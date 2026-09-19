use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use tracing_subscriber::{filter::LevelFilter, fmt};

use borhan::index::Index;
use borhan::search::Filter;
use borhan::storage::{Entry, Revision, Role, Storage};
use borhan::ulid::Ulid;

/// Name of the environment variable holding the user's home directory.
///
/// Windows has no `$HOME`; the equivalent is `%USERPROFILE%`.
#[cfg(windows)]
const HOME_VARIABLE: &str = "USERPROFILE";
#[cfg(not(windows))]
const HOME_VARIABLE: &str = "HOME";

/// Appended to the user's home directory when `--home` is absent, giving
/// `~/.borhan` on Linux.
const DEFAULT_HOME_DIRECTORY: &str = ".borhan";

// Everything borhan owns sits under `--home`, and nothing outside it:
//
//     ~/.borhan/
//       storage/            One directory per memory. Created by `init`.
//         <name>/borhan.db  The messages. Never derived, never rebuilt.
//         <name>/index/     The tantivy index. Entirely derived; `rescan` fodder.
//       server.toml         Listen address, token, and the operations the
//                           server refuses. Written by `init server` from a
//                           commented template. `serve` binds it; the CLI
//                           probes it and talks HTTP when that process
//                           answers, otherwise opens storage. The refusals
//                           bind only the served process — a local command is
//                           doing what its user could already do to these
//                           files by hand.
//
// None of it is created implicitly: `init` is the only thing that writes the
// layout, so a missing directory always means "this machine was never set up",
// never "it was set up somewhere you did not look".
const STORAGE_DIRECTORY: &str = "storage";
const SERVER_CONFIGURATION: &str = "server.toml";
const CRATE_VERSION: &str = env!("CARGO_PKG_VERSION");

// A skill is the one thing borhan writes outside `--home`, and it has to be.
// It is not borhan's data: it is a file another program reads, and that program
// looks in its own configuration directory and nowhere else. Putting it under
// `--home` would be tidy and would mean no agent ever loads it.
//
// Both names below are the agent-skill convention rather than borhan's choice:
// `<root>/skills/<name>/SKILL.md`, with the frontmatter at the top of the file
// naming the skill again.
const SKILL_DIRECTORY: &str = "skills";
const SKILL_FILE: &str = "SKILL.md";

/// Name of the remember skill on disk, and the slash command it becomes.
///
/// Prefixed, unlike the subcommand that prints it. `borhan skills remember` is
/// unambiguous because `borhan` is already on the line; `/remember`, in an agent
/// carrying a dozen other tools' skills, is not.
const REMEMBER_SKILL: &str = "borhan-remember";

/// Name of the survey skill on disk, and the slash command it becomes. Prefixed
/// for the same reason as [`REMEMBER_SKILL`].
const SURVEY_SKILL: &str = "borhan-survey";

/// Words of a snippet shown on one line of `memory search` results.
///
/// The snippet is already the best sentence of the unit rather than the whole
/// paragraph, so this is a backstop for a paragraph that has no sentence
/// boundaries in it — a long line of prose, or a table row. Whatever is shown
/// keeps the spacing it was stored with, with the newlines escaped, because a
/// hit has to stay one line for the columns beside it to line up.
const PREVIEW_WORDS: usize = 60;

/// Keyword memory over stored conversations, for text that mixes Persian and
/// English in the same sentence.
///
/// borhan stores messages, splits them into paragraph-sized units and searches
/// those units by concept. It does not summarize, does not answer questions and
/// does not embed anything: what comes back is stored text and the address of
/// where it sits. A SQLite source of truth under a tantivy index.
///
/// WHAT IS STORED
///
/// A **memory** is a named corpus with a description saying what is in it and
/// what is not. It holds **sessions** — a thread, a channel, a document set —
/// which hold **messages**, one turn or one page each. Every message is split
/// into **units**: a paragraph, a heading, a list item, a table row, a fenced
/// code block. A unit is what search scores and returns, and its ULID is the
/// `cursor` that reads it back. That ULID is the only address worth keeping.
///
/// THE LOOP
///
/// Four commands, in this order. Skipping the second is the most common way to
/// end up with a result set that looks like answers and is not.
///
/// (1) `memory list` — what memories exist, how large each is, which languages
/// it was tagged with, and what its description says it does *not* hold.
///
/// (2) `memory lexicon <name> <words>…` — whether the words you are about to
/// search for are in that memory at all, and what they fold to. A search for a
/// word the memory has never seen returns other things rather than nothing,
/// and a result set full of other things looks exactly like one full of
/// answers.
///
/// (3) `memory search <name> <query>` — one query string, not a sentence.
/// Words in parentheses are one idea spelled every way this corpus might spell
/// it; separate parts are separate things being asked about.
///
/// (4) `memory cursor <name> <cursor>…` — read the hits back, at the width the
/// question needs: the unit itself, the units around it, or the whole messages
/// they came from. There is one reader, not one per depth.
///
/// A WORKED EXAMPLE
///
/// borhan memory list
///
/// borhan memory lexicon rfcs borrow mutable alias radiograph
///
/// borhan memory search rfcs '(borrow borrowed borrowing) (mutable mut) +(alias aliasing)'
///
/// borhan memory cursor rfcs 01M1BKH0YQ7YR34K2FKG3S4B0M --before 2 --after 2
///
/// Every command takes `--json` and prints exactly the object the HTTP API
/// returns. Full prose for each one is under `--help`, and `memory search
/// --help` and `memory cursor --help` are worth reading once before the first
/// query — they are where the rules that decide whether a search works are
/// written down.
///
/// THREE WAYS IN, ONE SET OF OPERATIONS
///
/// This command line is one of three surfaces over the same functions, so
/// nothing below is reachable from only one of them:
///
/// **MCP** — `borhan serve` speaks the Model Context Protocol at `POST /mcp`.
/// If your client can be configured with an MCP server, prefer it: the tool
/// schemas carry this guidance where the model will actually read it.
///
/// **This CLI** — `borhan <command> --help` at every level. Nothing is
/// abbreviated: `-h` and `--help` print the same long text.
///
/// **HTTP** — `borhan serve`, then `GET /` returns the whole REST API as
/// Markdown, with a `curl` line per endpoint and no token required to read it.
///
/// WHERE IT LIVES
///
/// Everything borhan owns sits under `--home`, or `BORHAN_HOME`, which defaults
/// to `~/.borhan`, and nothing sits outside it. **Nothing is created
/// implicitly**: `borhan init` writes the layout, so a missing storage
/// directory always means "this machine was never set up", never "it was set up
/// somewhere you did not look".
#[derive(Debug, Clone, Parser)]
#[command(version, author, disable_help_flag = true)]
pub struct CommandLine {
    /// Directory holding the storage and the configuration files.
    ///
    /// Never created implicitly — run `borhan init` first.
    #[arg(
        long,
        global = true,
        env = "BORHAN_HOME",
        default_value_os_t = default_home_directory()
    )]
    pub home: PathBuf,

    /// Enable trace level logging (shows target and location).
    #[arg(long, global = true)]
    pub trace: bool,

    /// Enable debug level logging (shows target).
    #[arg(long, global = true)]
    pub debug: bool,

    /// Enable info level logging: one line per operation, with its timings.
    ///
    /// Off by default. A command reports its own result on stdout, so the
    /// operation line is a second copy of something the reader already has —
    /// useful to a server writing to a journal, noise in a terminal.
    #[arg(long, global = true)]
    pub info: bool,

    /// Disable all logging.
    #[arg(long, global = true)]
    pub quiet: bool,

    /// Print help. `-h` and `--help` print the same thing: there is no
    /// abbreviated form, because the reader of a help text here is as likely to
    /// be a model composing its first query as a person who has run the command
    /// before, and the short form omits exactly what the first reader needs.
    #[arg(short = 'h', long = "help", global = true, action = clap::ArgAction::HelpLong)]
    pub help: Option<bool>,

    /// Absent prints this text. There is no implicit command: a bare `borhan`
    /// is someone who does not know what this is yet, and the answer to that
    /// is the guide, not a listing.
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Create the home directory and what lives inside it.
    ///
    /// Two separate things, and which one you want depends on the machine:
    /// `init storage` for a machine that keeps memories, `init server` for one
    /// that serves them. A machine that only talks to someone else's server
    /// needs neither — it needs a `server.toml` naming that server. Given no
    /// subcommand this prints the choice rather than guessing at it.
    Init {
        #[command(subcommand)]
        command: Option<InitCommand>,
    },

    /// Serve the HTTP API, and MCP over it, at the address in `server.toml`.
    ///
    /// Three things answer on that port. `/api/v1/…` is the REST API this
    /// command line talks to. `POST /mcp` is the Model Context Protocol, over
    /// the same operations and behind the same token. `GET /` is the whole REST
    /// API written out as Markdown — a `curl` line per endpoint — and it is the
    /// one route served without a token, so an agent handed nothing but an
    /// address can read it before it knows a token is wanted.
    ///
    /// Address, token and refusals come from `<home>/server.toml`, written by
    /// `init server`. Without that file it binds `127.0.0.1:1995` with no token
    /// and refuses nothing.
    ///
    /// While it is running, every `memory` command on this machine goes through
    /// it instead of opening storage directly — the CLI probes the same address
    /// and uses HTTP when something answers.
    Serve,

    /// Work with the memories themselves. Given no subcommand, prints this.
    ///
    /// Read a memory, in four commands, in this order:
    ///
    /// borhan memory list
    ///
    /// borhan memory lexicon <NAME> <WORDS>...
    ///
    /// borhan memory search <NAME> <QUERY>
    ///
    /// borhan memory cursor <NAME> <CURSORS>...
    ///
    /// `list` says which memories exist and what each one's description claims
    /// it holds and does not hold. `lexicon` says whether your words exist in
    /// that corpus before you spend a query on them — a word with a lemma count
    /// of 0 has never been seen there, and searching for it returns other
    /// things rather than nothing. `search` takes one query of ideas in
    /// parentheses, never a sentence. `cursor` reads the hits back at whatever width the question
    /// needs, and is the only reader.
    ///
    /// Write to a memory with three more:
    ///
    /// borhan memory create <NAME> --description "..." [--languages fa,en]
    ///
    /// borhan memory add <NAME> <TEXT> --session S [--message M] [--role user|assistant|tool]
    ///
    /// borhan memory rescan <NAME>
    ///
    /// `create` needs a description of more than ten words, because it is what
    /// a later caller reads to decide whether a question belongs here. `add`
    /// reads its text as Markdown and splits it into units. `rescan` rebuilds
    /// the index from the stored messages, which is the supported way to pick
    /// up a change to the splitter or the normalizer. `update` changes a
    /// description or the language tags; `delete` destroys a memory and is the
    /// one command here that cannot be undone.
    ///
    /// A WORKED EXAMPLE
    ///
    /// borhan memory lexicon rfcs borrow mutable alias
    ///
    /// borhan memory search rfcs '(borrow borrowed borrowing) (mutable mut) +(alias aliasing)'
    ///
    /// borhan memory cursor rfcs 01M1BKH0YQ7YR34K2FKG3S4B0M
    ///
    /// The third field of a hit's first line is its `cursor`; that is what the
    /// last command takes, and several of them can be read in one call.
    ///
    /// Every subcommand takes `--json` and prints exactly the object the HTTP
    /// API returns, pretty-printed. In text mode, results go to standard output
    /// and everything else — headers, warnings, counts — goes to standard
    /// error, so a pipeline reading stdout receives only results.
    ///
    /// Each subcommand's own `--help` is the full prose; `memory search --help`
    /// and `memory cursor --help` are the two worth reading before the first
    /// query.
    Memory {
        #[command(subcommand)]
        command: Option<MemoryCommand>,
    },

    /// Print the skills borhan ships, or install them for the agents on this
    /// machine. Given no subcommand, prints this.
    ///
    /// A skill is a Markdown file an agent reads to learn a workflow it was not
    /// trained on. borhan ships them because the hard part of this tool is not
    /// its API — that is in `--help` and in `GET /` — but knowing when to reach
    /// for it and what is worth putting in, and neither of those fits in a tool
    /// description.
    ///
    /// There are two:
    ///
    /// borhan skills remember             # print it
    ///
    /// borhan skills remember --install   # write it where the agents read
    ///
    /// borhan skills survey --install     # the same, for the codebase skill
    Skills {
        #[command(subcommand)]
        command: Option<SkillCommand>,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum SkillCommand {
    /// The end-of-conversation skill: read back over the session, pick what is
    /// worth keeping, and store it.
    ///
    /// One half of a pair. `survey` describes a project's code in a session
    /// named after the project; this records what happened in a session named
    /// after the conversation. Same memory, never the same session — a result
    /// line prints its session, and that is what tells a reader whether they
    /// are looking at how the code works or at what was decided one afternoon.
    ///
    /// Printed to standard output as a complete `SKILL.md`, frontmatter
    /// included, so redirecting it to a file gives a working skill and
    /// `--install` is a convenience rather than the only way in.
    ///
    /// What it tells the agent, in short. Find borhan: MCP first, because the
    /// tool schemas carry the argument rules where a model will read them, this
    /// command line second, and if neither is there then stop and say so rather
    /// than keeping it somewhere else. Check that storing is permitted
    /// *before* composing anything, because a server that refuses `add`
    /// does not list a writing tool at all and the discovery is otherwise made
    /// after the work. Read `memory list` and ask the user which
    /// memory to write to, every time. Keep decisions and the reasons for them
    /// rather than transcript. And file each message under the session, role and
    /// author it belongs to — the user and the agent are not the same author,
    /// and a paraphrase stored as `role: user` answers a later question
    /// confidently and wrongly.
    ///
    /// It takes memory names as its arguments — `/borhan-remember pouriya
    /// project_borhan` — which say where, not what, and do not remove the
    /// confirmation. A memory is a subject, so one conversation usually splits
    /// across several of them, and making that split is most of the work.
    Remember {
        /// Write the skill to every agent on this machine instead of printing
        /// it, replacing any copy already there.
        ///
        /// `~/.agents/skills` is written whether or not it exists already,
        /// because it is the vendor-neutral path that more than one agent
        /// reads. Every other root — `~/.claude`, `~/.codex`, `~/.hermes` — is
        /// written only when it is already there, so an agent you do not run
        /// never receives a tree it never asked for.
        ///
        /// Every destination is reported, including the ones skipped and why.
        /// "opencode: covered" and "opencode: missing" look identical in
        /// silence, and only one of them is fine.
        #[arg(long)]
        install: bool,
    },

    /// The codebase skill: read a project, then file what each part of it does
    /// into a memory, one message per feature.
    ///
    /// What it tells the agent. Find borhan and check that both `add` and
    /// `replace` are permitted before reading a single file, because a survey
    /// is hours of reading that a `403` at the end throws away. Choose the
    /// memory with the user — a codebase belongs in a memory about projects,
    /// and if none exists, propose one and let the user write its description.
    /// Name the session after the project, and check that name against
    /// `memory outline` rather than against the filesystem: two clones of one
    /// repo collide in the memory while looking unique on disk.
    ///
    /// Then the part that matters. If the project has been surveyed before,
    /// `memory outline <memory> <session>` says what is already on file, and
    /// the three answers are append what is new, replace what has gone stale,
    /// and leave the rest alone — never wipe and start over. Each message is
    /// one feature: what it does, how, where it lives and why it is that way.
    /// Each *paragraph* has to stand alone, because a paragraph is the unit
    /// search returns, and one that says "it does this by calling handle()"
    /// names neither the feature nor the file and so answers nothing.
    ///
    /// It takes memory names as its arguments, the same as the remember skill,
    /// and they say where rather than what. With none given, or with any doubt
    /// about which session, both skills are told to list what exists and ask
    /// with options — through a question tool where the agent has one, numbered
    /// choices otherwise. Storing nothing is recoverable; storing in the wrong
    /// place fails silently and surfaces months later as four answers to one
    /// question.
    Survey {
        /// Write the skill to every agent on this machine instead of printing
        /// it, replacing any copy already there. Same destinations and the same
        /// report as `remember --install`.
        #[arg(long)]
        install: bool,
    },
}

/// An agent that reads skills from a directory in the user's home.
///
/// opencode is on this list with nowhere of its own to write, on purpose: it
/// reads `~/.agents/skills` and `~/.claude/skills` both, so a third copy under
/// its own configuration directory would load the same skill twice and list it
/// twice. It is named in the report rather than left out of it.
struct SkillReader {
    /// What the report calls it.
    agent: &'static str,

    /// Configuration root under the user's home. `None` means another entry
    /// already covers this agent, and [`SkillReader::reason`] says how.
    root: Option<&'static str>,

    /// Create the tree even when `root` is not there yet.
    always: bool,

    /// Why nothing is written. Empty for every entry that has a `root`, which
    /// is never the entry being explained.
    reason: &'static str,
}

const SKILL_READERS: [SkillReader; 5] = [
    SkillReader {
        agent: "agents",
        root: Some(".agents"),
        always: true,
        reason: "",
    },
    SkillReader {
        agent: "claude",
        root: Some(".claude"),
        always: false,
        reason: "",
    },
    SkillReader {
        agent: "codex",
        root: Some(".codex"),
        always: false,
        reason: "",
    },
    SkillReader {
        agent: "hermes",
        root: Some(".hermes"),
        always: false,
        reason: "",
    },
    SkillReader {
        agent: "opencode",
        root: None,
        always: false,
        reason: "reads ~/.agents/skills and ~/.claude/skills already, so a copy of \
                 its own would load the same skill twice",
    },
];

#[derive(Debug, Clone, Subcommand)]
pub enum InitCommand {
    /// Create the local storage: `<home>/storage`, where memories are kept.
    ///
    /// Safe to run again: an existing storage is not an error, and re-running
    /// rebuilds the index of every memory in it from the stored messages, the
    /// same work `memory rescan` does one memory at a time.
    Storage,

    /// Write `server.toml` so `serve` and the CLI share a listen address and token.
    ///
    /// The file is written from a commented template that lists every operation
    /// and says what each one does, so the way to change what this server will
    /// do afterwards is to open it and read it, not to remember flags.
    Server {
        /// `HOST:PORT` to bind, and the address the CLI probes.
        #[arg(long)]
        listen: String,

        /// Token clients must present as `Authorization: Bearer`. Optional.
        #[arg(long)]
        token: Option<String>,

        /// A store-changing operation `serve` must NOT perform: `create`,
        /// `update`, `add`, `replace`, `rescan` or `delete`. Repeat the flag
        /// for each one.
        ///
        /// Everything not named is allowed. Omit the flag and the server does
        /// all six; `--refuse delete --refuse replace` is a server that writes
        /// and never destroys.
        #[arg(long = "refuse", value_name = "NAME")]
        refuse: Vec<String>,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum MemoryCommand {
    /// Store a new memory and print the ULID it was filed under.
    Create {
        /// Short name, 1 to 40 characters of a-z, 0-9 and _, not already taken.
        /// It is the memory's directory name.
        name: String,

        /// Longer text, more than 10 words and up to 2000 characters. Shown to
        /// the calling model, so it should say what is in here and what is not.
        #[arg(long)]
        description: String,

        /// Comma-separated language tags, like `fa,en`. Reported by `memory
        /// list` so a caller composing a query knows which languages a concept
        /// group is worth expanding into.
        #[arg(long, default_value = "fa,en")]
        languages: String,

        /// Emit JSON instead of the ULID.
        #[arg(long)]
        json: bool,
    },

    /// List the memories, oldest first.
    ///
    /// Two lines per memory: five columns — the id, when it was created, the
    /// name, what is in it, the language tags — and under them the whole
    /// description, uncut.
    ///
    /// The name is the first argument of every other subcommand. The language
    /// tags are a hint from whoever created the memory about which languages
    /// the ideas of a query are worth spelling in — they are not enforced, and a
    /// memory tagged `fa` can still hold English. The description says what is
    /// in the memory and what is not, which is what to read before deciding
    /// this is the one to search.
    List {
        /// Emit JSON instead of text.
        #[arg(long)]
        json: bool,
    },

    /// Change a memory's description and/or language tags.
    Update {
        /// The memory to change.
        name: String,

        /// Replacement description. Same rules as `memory create`: more than
        /// 10 words, at most 2000 characters.
        #[arg(long)]
        description: Option<String>,

        /// Replacement language tags, like `fa,en`.
        #[arg(long)]
        languages: Option<String>,

        /// Emit JSON instead of the ULID.
        #[arg(long)]
        json: bool,
    },

    /// Remove a memory: its messages, its index and its directory.
    ///
    /// Irreversible, and there is nothing to fall back on. `rescan` can rebuild
    /// an index from the messages because the index is derived; nothing can
    /// rebuild the messages, so this is the only command in the tool that
    /// destroys something that was not a copy of something else.
    ///
    /// It therefore requires `--yes`. The flag is not a safety mechanism —
    /// nothing here can tell a name you meant from a name you mistyped — it is
    /// there so that deleting a memory cannot be a command you completed with a
    /// shell history search and a return key.
    ///
    /// Against a server that refuses `delete` this answers 403 and nothing is
    /// removed. Locally there is nothing to check: the files belong to whoever
    /// is running this.
    ///
    /// Prints the counts of what was destroyed, which is the last record of it.
    Delete {
        /// The memory to remove, by the name `memory list` prints.
        name: String,

        /// Required. Confirms that the memory and every message in it go.
        #[arg(long)]
        yes: bool,

        /// Emit JSON instead of the counts.
        #[arg(long)]
        json: bool,
    },

    /// Add a message to a memory, split into units and indexed.
    ///
    /// The text is read as Markdown: a paragraph, a heading, a list item, a
    /// table row and a fenced code block each become a unit, which is the
    /// granularity search scores and returns.
    Add {
        /// The memory to file it under. It has to exist already.
        name: String,

        /// The text, as Markdown.
        text: String,

        /// The feeder's session identifier — a thread id, a channel, a
        /// filename. The first message of a session writes the session row.
        #[arg(long)]
        session: String,

        /// The feeder's message identifier, if it has one. Two messages with
        /// the same one in the same session is an error, so a replay that
        /// overlaps what is already stored fails loudly.
        #[arg(long)]
        message: Option<String>,

        /// user, assistant or tool.
        #[arg(long, default_value = "user")]
        role: String,

        /// Display name of the author. Defaults to the role.
        #[arg(long)]
        author: Option<String>,

        /// Unix milliseconds. Defaults to now. Supplied rather than assumed
        /// because a transcript is usually replayed, not watched.
        #[arg(long)]
        ts: Option<i64>,

        /// Emit JSON instead of the ULID.
        #[arg(long)]
        json: bool,
    },

    /// What a memory holds, without reading any of it.
    ///
    /// With no session, every session in the memory. With one, that session's
    /// messages — their ids, who wrote them, how long they are and how many
    /// units they split into — and no bodies, so the answer stays readable
    /// when a session runs to a hundred messages.
    ///
    /// This is the question `memory search` cannot answer. Search finds text;
    /// this asks what exists. Before storing something under a name, it is how
    /// you find out whether that name is already taken and what is under it.
    Outline {
        /// The memory to look inside.
        name: String,

        /// A session, by the id the `session` column prints. Omit for the
        /// list of sessions.
        session: Option<String>,

        /// Emit JSON instead of the table.
        #[arg(long)]
        json: bool,
    },

    /// Rewrite a message that is already stored, keeping its place.
    ///
    /// For a description that has gone out of date: the thing it described
    /// changed, so the text is now wrong, and adding a corrected copy beside it
    /// would only mean a search returns both with nothing to tell them apart.
    ///
    /// In place, rather than by removing and re-adding: a message's ordinal
    /// within its session is what makes `memory cursor` a range scan, so the
    /// message keeps it and the units around it keep pointing at each other.
    /// The old body is not kept anywhere.
    ///
    /// Both the session and the message have to exist already — this never
    /// creates either, and `memory outline` is how you find out what they are
    /// called. The timestamp is replaced too, because the text is new: a
    /// correction that kept the old time would be ranked by recency as though
    /// it were as old as the thing it corrected.
    Replace {
        /// The memory holding it.
        name: String,

        /// The new text, as Markdown.
        text: String,

        /// The session holding the message.
        #[arg(long)]
        session: String,

        /// The message to rewrite, by the id it was stored under.
        #[arg(long)]
        message: String,

        /// Unix milliseconds. Defaults to now.
        #[arg(long)]
        ts: Option<i64>,

        /// Emit JSON instead of the ULID.
        #[arg(long)]
        json: bool,
    },

    /// Search a memory with one query.
    ///
    /// The query is a single argument, so quote it: the shell would otherwise
    /// take the parentheses and split the words.
    ///
    /// borhan memory search notes '(error fault خطا) +(token jwt) -expired'
    ///
    /// WORDS. A word matches every unit that contains it in any inflection:
    /// `borrow` also finds `borrowed` and `borrowing`, `خطا` also finds
    /// `خطاها`. Case does not decide a match, but a unit spelling the word
    /// exactly as you did ranks above one holding another form of it.
    ///
    /// ONE IDEA: PARENTHESES. Words inside one pair of parentheses are
    /// alternatives for a single idea — synonyms, both languages, the
    /// abbreviation, the misspelling the room actually uses — and only the best
    /// of them scores, so a unit containing all of them counts once, not three
    /// times. `(error fault خطا)` is one idea. Never put two different ideas in
    /// one pair.
    ///
    /// SEVERAL IDEAS: PARTS SIDE BY SIDE. Every top-level part — a word, a
    /// phrase, or a parenthesised idea — is a separate thing being asked about,
    /// and how many of them a unit matches, its coverage, is the largest term
    /// in the score. Three parts of two words each ask a far better question
    /// than one part of six. A query wrapped whole in one pair of parentheses
    /// is one idea. `error fault` is two ideas; `(error fault)` is one.
    ///
    /// REQUIRED AND EXCLUDED: + AND -. A `+` directly in front of a part drops
    /// every unit that does not match it; a `-` drops every unit that does. No
    /// space after the sign. Put `+` on the one idea that makes a result worth
    /// reading, not on every part. `AND`, `OR` and `NOT`, in capitals, also
    /// work: `a AND b` is `+a +b`, `NOT a` is `-a`, and `a OR b` is the same as
    /// `a b`. Lowercase `and`, `or` and `not` are ordinary words.
    ///
    /// PHRASES: "…". Words in double quotes must appear together and in that
    /// order: `"borrow checker"`. `~N` after the closing quote lets up to N
    /// other words sit between them, so `"rotate token"~2` matches "rotate the
    /// API token". `*` after the closing quote reads the last word as the start
    /// of a word, so `"borrow check"*` matches "borrow checker". A phrase is two
    /// words or more.
    ///
    /// WEIGHT: ^N. `^2` after a word, phrase or parenthesised idea doubles what
    /// it contributes and `^0.5` halves it. Weight reorders hits; it never
    /// decides which units match.
    ///
    /// FIELDS. With no field a word is looked up three ways at once — exactly
    /// as written, folded to its root, and in the rest of the message the unit
    /// came from — and the best of the three counts. A field restricts it to
    /// one: `surface:JWT_SECRET` is the exact spelling, case included, which is
    /// what an identifier wants; `lemma:borrowing` is the folded form only;
    /// `context:rotation` is only the rest of the message. A field in front of
    /// parentheses applies to every word inside: `surface:(JWT_SECRET API_KEY)`.
    /// `surface: IN [JWT_SECRET API_KEY]` means the same thing.
    ///
    /// CHARACTERS THAT NEED CARE. `: ( ) [ ] { } ^ " '` and the backslash mean
    /// something in a query. Inside a word, put a backslash in front of them —
    /// `http\://host` — or quote the phrase. A word cannot start with `+` or `-`.
    ///
    /// NOT SUPPORTED, and refused with a sentence saying what to write instead:
    /// regular expressions (`/…/`); a `*` on a single word (`rot*` — list the
    /// forms in parentheses, `(rotate rotated rotation)`); ranges (`[a TO b]`,
    /// `>a`); `*` on its own; and `session:`, `ts:` or `role:` inside the query,
    /// which are the `--session`, `--after`/`--before` and `--role` flags.
    ///
    /// Do not paste a sentence in. Reduce it to the two to four things that
    /// have to co-occur, then expand each one into its spellings. So "why does
    /// the borrow checker reject this mutable alias" becomes three ideas, the
    /// last one required:
    ///
    /// '(borrow borrowck borrowing) (mutable mut) +(alias aliasing)'
    ///
    /// Two lines come back per hit, under a header that names the columns:
    ///
    /// 0.847  2/3  01J8…  session  message  75 words  [(site OR location),~acuity]
    ///
    /// "the best sentence of the unit, quoted"
    ///
    /// `score` is relative to the best hit in this result set, which is always
    /// 1.000; it orders these hits and means nothing next to the score of a
    /// different search. `cover` is how many top-level parts the unit matched
    /// out of how many were asked, and it is the more trustworthy of the two —
    /// prefer 3/3 at a middling score over 1/3 at a high one. `cursor` is what
    /// `memory cursor` takes.
    ///
    /// `matched` names those parts, spelled the way the query wrote them. A bare
    /// label means the unit contains one of that part's words, and you will find
    /// it in the quoted line. A `~` in front means the part was reached only
    /// through the surrounding units of the same message: the idea is somewhere
    /// in that message, but not on this line, and quoting this line for it would
    /// be wrong. It still counts toward `cover`, because it is still evidence —
    /// read it as a pointer to `memory cursor` rather than as an answer.
    ///
    /// Hits go to standard output and nothing else does. The header, the
    /// unknown-word and fuzzy lines, `No hits.` and the closing vocabulary line
    /// all go to standard error, so a pipeline reading stdout receives only
    /// results.
    ///
    /// Read the unknown-word lines. `unknown: "cva" (in "(cva stroke)") matched
    /// nothing` is the difference between "this memory disagrees with you" and
    /// "this memory has never heard that word", and only the second is a reason
    /// to search again with different wording. `memory lexicon` answers the
    /// same question before a search rather than after it.
    ///
    /// The closing line lists frequent terms that appear across these results
    /// and were not asked for. It is the cheapest source of a better second
    /// query: it is how you learn that the corpus says `x-ray` where you said
    /// `radiograph`.
    Search {
        /// The memory to search, by the name `memory list` prints.
        name: String,

        /// The query, as one argument: '(error fault) +token -expired'. See
        /// above for what the parentheses, signs, quotes and fields mean.
        #[arg(allow_hyphen_values = true)]
        query: String,

        /// Also match words one letter away from a word this memory has never
        /// seen, for words of five letters or more: `borow` finds `borrow`.
        /// Such a match counts for half, words the memory does have are never
        /// expanded, and a `fuzzy:` line on standard error says which word was
        /// taken for which — use that spelling next time.
        #[arg(long)]
        fuzzy: bool,

        /// Hits to return, at most.
        #[arg(long, default_value_t = 10)]
        limit: usize,

        /// Units returned from any one message. Twenty hits from one message is
        /// a wasted result set; the cursor is how you read the rest of it.
        #[arg(long, default_value_t = 2)]
        max_per_message: usize,

        /// Confine the search to one session, by its ULID — which is what
        /// the `session` field of a hit holds under `--json`. The `session`
        /// *column* of the text output is the feeder's own name for it
        /// (`session_ref`), and that is not accepted here.
        #[arg(long)]
        session: Option<String>,

        /// Only messages at or after this unix-millisecond timestamp.
        #[arg(long)]
        after: Option<i64>,

        /// Only messages at or before this unix-millisecond timestamp.
        #[arg(long)]
        before: Option<i64>,

        /// Repeatable: user, assistant or tool.
        #[arg(long = "role")]
        roles: Vec<String>,

        /// Emit one JSON object holding `hit_list`, `unknown_list`,
        /// `fuzzy_list`, `hint_list` and `stats` instead of the table. All of it goes to standard output.
        ///
        /// Ids in a hit are spelled the way `memory cursor` spells them:
        /// `session` and `message` are ULIDs and are what another call
        /// accepts, `session_ref` and `message_ref` are the feeder's own
        /// names and are what the text columns show.
        #[arg(long)]
        json: bool,
    },

    /// Read a hit back, at whatever width the question needs.
    ///
    /// The second half of the retrieval loop: recall a gist from a partial cue,
    /// then elaborate around it deliberately. No scoring and no snippets — the
    /// caller has already decided this region is worth reading.
    ///
    /// The window is counted in **units**, not messages, because a unit is a
    /// thirtieth of a message in a corpus fed from PDFs and pulling the whole
    /// page to re-read one paragraph is how a context window is wasted:
    ///
    /// borhan memory cursor notes 01J8…                     # the unit and two either side
    ///
    /// borhan memory cursor notes 01J8… --before 0 --after 0  # only that unit
    ///
    /// borhan memory cursor notes 01J8… --messages            # the whole messages instead
    ///
    /// Several cursors may be read at once and the windows are merged, so a
    /// page of hits costs one call rather than one per hit. A cursor this
    /// memory no longer holds is named on standard error rather than failing
    /// the read.
    Cursor {
        /// The memory the hits came from.
        name: String,

        /// One or more `cursor` values, as `memory search` printed them: the
        /// third field of the first line of each hit.
        #[arg(required = true, num_args = 1..)]
        cursors: Vec<String>,

        /// Units before each anchor, or messages before it with `--messages`.
        #[arg(long, default_value_t = 2)]
        before: i64,

        /// Units after each anchor, or messages after it with `--messages`.
        #[arg(long, default_value_t = 2)]
        after: i64,

        /// Count the window in whole messages and return every unit of them.
        #[arg(long)]
        messages: bool,

        /// Emit JSON instead of text.
        #[arg(long)]
        json: bool,
    },

    /// Ask what a word looks like in this memory before searching for it.
    ///
    /// borhan memory lexicon notes error errors خطا
    ///
    /// One line per word:
    ///
    /// errors  surface=errors (12)  lemma=error (175)  context=(0)
    ///
    /// `surface` is that exact spelling and the number of units holding it.
    /// `lemma` is what the word folds to — Snowball for English, affix
    /// stripping and letter folding for Persian — and the number of units
    /// holding anything that folds to the same thing. The lemma count is the
    /// one that matters, because it is what search ranks on.
    ///
    /// `context` counts units the term was propagated into from a neighbour
    /// rather than occurring in. A term with a high context count and a low
    /// lemma count is one the memory talks around without naming.
    ///
    /// A lemma count of 0 means this memory has never seen the idea. Searching
    /// for it anyway returns other things rather than nothing, and after the
    /// fact there is no way to tell those apart from an answer.
    ///
    /// It is also how to find the word a corpus actually uses: if `radiograph`
    /// is 0, try `x-ray`; if `بریدگی` is 0, try `زخم`.
    Lexicon {
        /// The memory to look in.
        name: String,

        /// Words to look up, any number of them, in any language the memory
        /// holds. They are looked up exactly as written, so pass the word you
        /// were about to search with, not a stem of it.
        words: Vec<String>,

        /// Emit JSON instead of text.
        #[arg(long)]
        json: bool,
    },

    /// Split every stored message again and rebuild the index from scratch.
    ///
    /// The supported way to change the normalizer or the splitter: change it,
    /// bump the version, run this. Nothing is lost, because everything this
    /// destroys was derived from the messages, which are not touched.
    Rescan {
        /// The memory to rebuild.
        name: String,

        /// Emit JSON instead of the counts.
        #[arg(long)]
        json: bool,
    },
}

/// `<home>/server.toml`. `serve` binds `listen`; the CLI probes the same
/// address and uses HTTP when that process answers.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    /// Address to bind, and the address the CLI probes. [`borhan::DEFAULT_LISTEN_ADDRESS`]
    /// when unset for `serve`; the CLI treats a missing value as "use storage".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub listen: Option<String>,

    /// A `serve` elsewhere, as a URL: `http://host:port` or `https://host`.
    /// Read by the CLI only — `serve` binds [`Server::listen`] and nothing
    /// else — and when it is set the CLI goes there instead of to `listen`.
    ///
    /// The two are not alternatives for the same fact. `listen` is where a
    /// server puts its socket, which is why it is bare `HOST:PORT`: a bind
    /// address has no scheme and no path. This is where a client finds a server
    /// that somebody else started, possibly behind a TLS terminator, on a host
    /// this machine reaches by name. That is a URL, and the scheme is the half
    /// that `listen` cannot express.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_address: Option<String>,

    /// Connect to an `https://` [`Server::remote_address`] without checking
    /// the certificate: neither its chain nor the hostname it was issued for.
    ///
    /// For a server presenting a self-signed or internal-CA certificate, which
    /// is what a memory on a LAN or behind a corporate CA usually has. What it
    /// costs is the whole of what TLS was doing: anything that can route the
    /// connection can terminate it, present any certificate, and read the token
    /// in the `Authorization` header as it goes past. Turning this on over a
    /// network that is not already trusted hands over the bearer token that
    /// governs every operation on the store.
    ///
    /// Has no effect on `http://`, where there is nothing to verify.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_skip_tls_verify: Option<bool>,

    /// Token clients must present as `Authorization: Bearer`. Unset means open.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,

    /// Store-changing operations `serve` will refuse: any of `create`,
    /// `update`, `add`, `replace`, `rescan` and `delete`. Reads are not on the
    /// list and are governed by `token`.
    ///
    /// A list of what is withheld, not of what is granted. Unset or empty means
    /// a server that does everything, which is the right default for the common
    /// case of a store on the machine of the person who started it — they can
    /// already `rm` the directory. Naming an operation here is how someone says
    /// *not this one*, and it is worth saying only when the caller is not that
    /// person.
    ///
    /// The direction matters beyond taste: a granting list silently withholds
    /// every operation invented after it was written, so the file that said
    /// `["create", "add"]` last year is a file that refuses this year's
    /// `replace` on behalf of an author who never considered it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refuse: Option<Vec<String>>,
}

impl Server {
    /// Everything except what [`Server::refuse`] names.
    ///
    /// An unknown name is an error and not a warning, and this is the direction
    /// in which that matters most: a skipped name in a refusal list is an
    /// operation left switched **on**. `refuse = ["delte"]` ignored is a server
    /// that deletes, and nothing about the day it does will point back here.
    fn allowed(&self) -> anyhow::Result<Vec<borhan::api::Permission>> {
        let mut refused = Vec::new();
        if let Some(names) = &self.refuse {
            for name in names {
                match borhan::api::Permission::parse(name) {
                    Some(permission) => refused.push(permission),
                    None => {
                        let known: Vec<&str> = borhan::api::Permission::ALL
                            .iter()
                            .map(|permission| permission.as_str())
                            .collect();
                        anyhow::bail!("{name:?} in refuse is not one of {}", known.join(", "));
                    }
                }
            }
        }
        let mut allowed = Vec::new();
        for permission in borhan::api::Permission::ALL {
            if !refused.contains(&permission) {
                allowed.push(permission);
            }
        }
        Ok(allowed)
    }

    /// Where the CLI should send requests, and whether missing it is fatal.
    ///
    /// `Some((origin, required))`. `required` is the difference between the two
    /// sources: a [`Server::remote_address`] that does not answer is an error,
    /// a [`Server::listen`] that does not answer is a local store.
    ///
    /// That asymmetry is about which store the fallback lands in, not about
    /// which line someone wrote. `listen` describes a `serve` on this machine
    /// over this same `--home`, so the two paths reach the same bytes and going
    /// direct when nothing answers costs nothing. A `remote_address` is a
    /// different machine and a different store, and falling back there would
    /// write the message into an empty local memory, print every sign of
    /// success, and leave someone searching the team's server for it.
    fn client_origin(&self) -> anyhow::Result<Option<(String, bool)>> {
        if let Some(address) = &self.remote_address {
            let address = address.trim();
            let rest = match address.split_once("://") {
                Some(("http" | "https", rest)) => rest,
                Some((scheme, _)) => anyhow::bail!(
                    "remote_address {address:?} has scheme {scheme:?}: it must be http or https"
                ),
                None => anyhow::bail!(
                    "remote_address {address:?} is not a URL: write the scheme too, as in \
                     \"http://{address}\""
                ),
            };
            if rest.trim_end_matches('/').is_empty() {
                anyhow::bail!("remote_address {address:?} has no host");
            }
            // Trailing slash off, because every path this client asks for
            // starts with one and `//api/v1/health` is a different route to
            // anything in front of the server. Trimmed after the scheme is
            // read, so that `https://` is a missing host and not a missing
            // scheme.
            return Ok(Some((address.trim_end_matches('/').to_string(), true)));
        }
        let Some(listen) = &self.listen else {
            return Ok(None);
        };
        Ok(Some((format!("http://{}", dialable(listen)), false)))
    }
}

/// A bind address, rewritten as something to connect to.
///
/// `0.0.0.0` and `[::]` are answers to a different question. They mean *every
/// interface on this machine* to `bind`, and as a destination they are not an
/// address at all: Windows refuses to connect to them outright, and Linux only
/// reaches the local machine by a convention nothing promises to keep. The
/// machine the CLI wants is the one it is running on, so the wildcard becomes
/// the loopback of its own family and the port is carried over untouched.
///
/// Anything else is passed through, including a `listen` that names one
/// interface: a server bound to `192.168.1.10:1995` is not listening on
/// loopback, and rewriting that one would break the connection it describes.
fn dialable(listen: &str) -> String {
    // Split on the last colon, and only when what follows is a port. `[::]:1995`
    // splits correctly; bare `[::]` would otherwise split inside the address.
    let (host, port) = match listen.rsplit_once(':') {
        Some((host, port))
            if !port.is_empty() && port.chars().all(|digit| digit.is_ascii_digit()) =>
        {
            (host, Some(port))
        }
        _ => (listen, None),
    };
    let host = match host {
        "0.0.0.0" => "127.0.0.1",
        "[::]" | "[::0]" => "[::1]",
        host => host,
    };
    match port {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    }
}

impl CommandLine {
    /// The most verbose flag given wins, except `--quiet`, which wins over
    /// everything.
    ///
    /// The default is `WARN` and not `INFO`. Every operation logs an `info`
    /// line carrying what it did and how long each phase took, which is what a
    /// server's journal is for and is exactly duplicated, for a one-shot
    /// command, by the result the command already prints. Defaulting to `INFO`
    /// meant every `memory search` answered twice: once as a table for the
    /// person, once as JSON for nobody. `warn` still shows the things a silent
    /// success would hide — a fallback taken, an error swallowed.
    pub fn logging_level(&self) -> LevelFilter {
        if self.quiet {
            LevelFilter::OFF
        } else if self.trace {
            LevelFilter::TRACE
        } else if self.debug {
            LevelFilter::DEBUG
        } else if self.info {
            LevelFilter::INFO
        } else {
            LevelFilter::WARN
        }
    }
}

/// Cut a text to [`PREVIEW_WORDS`] and escape what would break the column
/// alignment. Whatever survives keeps the spacing it was stored with, so a code
/// block still reads as a code block.
fn preview(text: &str) -> String {
    let mut words = 0;
    let mut cut = text.len();
    for (at, character) in text.char_indices() {
        if !character.is_whitespace() {
            continue;
        }
        words += 1;
        if words >= PREVIEW_WORDS {
            cut = at;
            break;
        }
    }
    let mut preview = text[..cut]
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t");
    if cut < text.len() {
        preview.push('…');
    }
    preview
}

/// `~/.borhan`, resolving the home directory the way the `dirs` crate does but
/// without depending on it.
///
/// `dirs` v6.0.0 dispatches per platform, and all three branches end in
/// `dirs-sys` v0.5.0:
///
/// ```text
/// // dirs/src/lin.rs  (Linux, BSDs, …)
/// pub fn home_dir() -> Option<PathBuf> { dirs_sys::home_dir() }
/// // dirs/src/mac.rs  (macOS)
/// pub fn home_dir() -> Option<PathBuf> { dirs_sys::home_dir() }
/// // dirs/src/win.rs  (Windows)
/// pub fn home_dir() -> Option<PathBuf> { dirs_sys::known_folder_profile() }
///
/// // dirs-sys/src/lib.rs, module `target_unix_not_redox`
/// pub fn home_dir() -> Option<PathBuf> {
///     return env::var_os("HOME")
///         .and_then(|h| if h.is_empty() { None } else { Some(h) })
///         .or_else(|| unsafe { fallback() })
///         .map(PathBuf::from);
///
///     unsafe fn fallback() -> Option<OsString> {
///         // libc::getpwuid_r(libc::getuid(), ...) then reads passwd.pw_dir
///     }
/// }
///
/// // dirs-sys/src/lib.rs, module `target_windows`
/// pub fn known_folder_profile() -> Option<PathBuf> {
///     known_folder(Shell::FOLDERID_Profile)  // SHGetKnownFolderPath
/// }
/// ```
///
/// So on every platform the shape is "environment variable first, OS call as
/// fallback". Three deliberate differences from the original:
///
/// 1. Written with `if let` instead of the combinator chain, per the code style
///    in AGENTS.md. The behaviour is identical: the variable wins, but an
///    *empty* value is treated as unset and falls through to the OS call.
/// 2. The OS-call fallback is delegated to `std::env::home_dir` rather than
///    reimplemented. `dirs-sys` rolls its own because it supports Rust versions
///    where `std::env::home_dir` was deprecated for being wrong on Windows —
///    it read `%HOME%`/`%USERPROFILE%` only, with no `SHGetKnownFolderPath`
///    fallback. That was fixed and the deprecation lifted in Rust 1.85, and std
///    now performs the same `getpwuid_r` lookup on Unix and the same
///    `SHGetKnownFolderPath(FOLDERID_Profile)` call on Windows. Delegating
///    keeps the semantics without taking `libc` and `windows-sys` dependencies.
/// 3. `dirs` does not read an environment variable at all on Windows; the
///    explicit `%USERPROFILE%` read here matches what std does before its
///    known-folder fallback, so the result is the same while keeping one code
///    path for every platform.
fn default_home_directory() -> PathBuf {
    match user_home() {
        Some(home) => home.join(DEFAULT_HOME_DIRECTORY),
        // Neither the environment nor the OS gave us anything; fall back to a
        // path relative to the working directory so `--help` still renders.
        None => PathBuf::from(DEFAULT_HOME_DIRECTORY),
    }
}

/// The user's home directory itself, by the rule documented on
/// [`default_home_directory`] above.
///
/// Separate from that function, rather than inlined into it, because
/// `skills --install` wants the same directory for the opposite reason: it
/// writes into the *other* programs' configuration, which sits beside
/// `~/.borhan` and not inside it.
fn user_home() -> Option<PathBuf> {
    if let Some(value) = env::var_os(HOME_VARIABLE)
        && !value.is_empty()
    {
        return Some(PathBuf::from(value));
    }
    // Unix: getpwuid_r. Windows: SHGetKnownFolderPath(FOLDERID_Profile).
    env::home_dir()
}

/// Read one of the TOML files under `--home` into `T`.
///
/// Flattening tanzim's error with `{:#}` is the whole point: that is the form
/// carrying the source, line, column and the caret pointing at the bad value,
/// and it is lost if the error is only wrapped as a `#[source]`.
fn read_configuration<T: serde::de::DeserializeOwned>(path: &Path) -> anyhow::Result<T> {
    match tanzim::Config::default()
        .with_source(tanzim::source::file(path.to_string_lossy()))
        .try_deserialize()
    {
        Ok(configuration) => Ok(configuration),
        Err(error) => Err(anyhow::anyhow!("Could not read {path:?}\n{error:#}")),
    }
}

/// The gate in front of every command that needs `<home>/storage` to be there.
///
/// The check is one line; the message is the reason this exists. An agent in a
/// container has two very different fixes available, and the wrong one — `init`
/// — *succeeds*: it makes an empty store, hides every memory the user already
/// has, and reports nothing. So the mount is spelled out ahead of it.
fn check_storage(home: &Path, storage: &Path, command: &str) -> anyhow::Result<()> {
    if storage.is_dir() {
        return Ok(());
    }
    let home = home.display();
    anyhow::bail!(
        "Storage directory {storage:?} does not exist, and borhan never creates it \
         implicitly.\n\
         \n\
         Everything borhan owns lives under --home, which is {home} right now \
         (set --home, or the BORHAN_HOME environment variable, to move it).\n\
         \n\
         If this machine simply has no borhan data yet, create it:\n\
         \n    borhan --home {home} init storage\n\
         \n\
         If you are an agent in a container or sandbox and the memories belong to a \
         user on the host, do NOT run init — it would make an empty store and every \
         existing memory would stay invisible. The user's directory has to be \
         visible from in here, either by mounting it at {home}:\n\
         \n    docker run -v /home/<user>/.borhan:{home} ...\n\
         \n\
         or by pointing borhan at wherever it is already mounted:\n\
         \n    borhan --home /path/to/mounted/.borhan {command}",
    )
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut settings = CommandLine::parse();

    // Before the subscriber, so that a help text is never interleaved with a
    // log line. `print_long_help` and not the short form for the same reason
    // `--help` is mapped to `HelpLong`: the reader here is as likely to be a
    // model that has never run this before as a person who has, and the short
    // form omits exactly what the first reader needs.
    let Some(command) = settings.command.take() else {
        <CommandLine as clap::CommandFactory>::command().print_long_help()?;
        return Ok(());
    };

    let level = settings.logging_level();
    let show_target = matches!(level, LevelFilter::DEBUG | LevelFilter::TRACE);
    let show_location = level == LevelFilter::TRACE;
    // One line per thing that happened, and every line self-contained.
    //
    // No `FmtSpan`: those synthetic `new`/`close` events tripled the line count
    // and carried a `message` of literally "close", which is nothing anybody
    // can search for or alert on. A span here exists to name and nest the work,
    // not to narrate its own lifetime; the one event a span emits before it
    // closes already carries the durations.
    //
    // `spans` is kept and `span` dropped, which is the opposite of what the
    // duplication suggests. Every field used to be written three times —
    // flattened onto the event, again under `span`, and again under `spans` —
    // but that was because the spans carried the measurements. They carry no
    // fields now, only names, so `spans` costs about sixty bytes and buys the
    // thing a trace view is for: the path the work took, `[http.server,
    // memory.search]`, on the line that reports the result. `span` would just
    // repeat the last element of it.
    //
    // Correlation does not depend on any of that: `trace_id` is set explicitly
    // as a field on every event, so a Loki query is `{...} | json |
    // trace_id = "..."` and needs no span context to have survived.
    fmt::Subscriber::builder()
        .with_max_level(level)
        .json()
        .flatten_event(true)
        .with_current_span(false)
        .with_span_list(true)
        .log_internal_errors(true)
        .with_level(true)
        .with_file(show_location)
        .with_line_number(show_location)
        .with_target(show_target)
        .with_thread_ids(false)
        .with_thread_names(false)
        .with_writer(std::io::stderr)
        .init();

    let storage = settings.home.join(STORAGE_DIRECTORY);

    match command {
        Command::Init { command } => {
            // No implicit `storage`, for the same reason `memory` and `skills`
            // have no implicit subcommand. The two things `init` writes are a
            // store and a server configuration, and they are not variations on
            // one another: a machine that only talks to someone else's server
            // wants the second and specifically not the first. A bare `init`
            // that quietly made a store would hand that machine an empty
            // memory it never asked for.
            let Some(command) = command else {
                let mut root = <CommandLine as clap::CommandFactory>::command();
                root.build();
                match root.find_subcommand_mut("init") {
                    Some(init) => init.print_long_help()?,
                    None => root.print_long_help()?,
                }
                return Ok(());
            };
            match command {
                InitCommand::Storage => {
                    let existing = storage.is_dir();
                    fs::create_dir_all(&storage)
                        .with_context(|| format!("Could not create {storage:?}"))?;
                    tracing::info!(directory = ?storage, existing = existing, "initialized storage");
                    if !existing {
                        println!("Initialized {}", storage.display());
                        return Ok(());
                    }

                    // Re-initializing an existing storage is not an error and not a
                    // no-op: the directory being there says nothing about whether
                    // the indexes inside it were built by the normalizer that is
                    // about to read them. So a second `init` rebuilds every memory
                    // from its messages, which is what the systemd unit's
                    // `ExecStartPre` leans on — one line that sets a machine up the
                    // first time and keeps the indexes honest on every boot after.
                    println!("Already initialized: {}", storage.display());
                    let (memories, _) = borhan::api::list(&storage, &Ulid::new()?)?;
                    for memory in &memories {
                        let store = Storage::open(&storage, &memory.name)?;
                        let built = Index::attach(&store.directory.join(borhan::index::DIRECTORY))?;
                        let writer = built.writer()?;
                        let store = Mutex::new(store);
                        let writer = Mutex::new(writer);
                        let (messages, units, _) = borhan::api::rescan(
                            &store,
                            &built,
                            &writer,
                            &memory.name,
                            &Ulid::new()?,
                        )?;
                        println!(
                            "Rescanned {}: {messages} messages, {units} units",
                            memory.name
                        );
                    }
                    Ok(())
                }

                InitCommand::Server {
                    listen,
                    token,
                    refuse,
                } => {
                    let server_configuration = settings.home.join(SERVER_CONFIGURATION);
                    if server_configuration.is_file() {
                        anyhow::bail!(
                            "{server_configuration:?} already exists. Remove it before writing \
                         another listen address."
                        );
                    }
                    let server = Server {
                        listen: Some(listen.clone()),
                        // Client-side settings, and this writes the file for a
                        // machine that is about to run `serve`. The template
                        // carries both, commented, for the machine that is not.
                        remote_address: None,
                        remote_skip_tls_verify: None,
                        token: token.clone(),
                        refuse: match refuse.is_empty() {
                            true => None,
                            false => Some(refuse.clone()),
                        },
                    };
                    // Parsed before the file is written, so a misspelled name is a
                    // message here rather than a `serve` that will not start.
                    let allowed = server.allowed()?;

                    // Rendered from the template rather than serialized from
                    // `server`, because serializing loses every comment — and the
                    // comments are most of what this file is. A generated
                    // `refuse = []` says nothing; the template's twenty lines above
                    // it say what the six names are, which two destroy, and why the
                    // list is of refusals.
                    let token_line = match &token {
                        Some(token) => format!("token = {}", quoted(token)),
                        None => "# token = \"a-long-random-string\"".to_string(),
                    };
                    let mut names = Vec::new();
                    for name in &refuse {
                        names.push(quoted(name));
                    }
                    let configuration = include_str!("server.toml")
                        .replace("@LISTEN@", &listen)
                        .replace("@TOKEN@", &token_line)
                        .replace("@REFUSE@", &names.join(", "));

                    fs::create_dir_all(&settings.home).with_context(|| {
                        format!("Could not create home directory {:?}", settings.home)
                    })?;
                    let mut options = fs::OpenOptions::new();
                    options.write(true).create_new(true);
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::OpenOptionsExt;
                        options.mode(0o600);
                    }
                    let mut file = options
                        .open(&server_configuration)
                        .with_context(|| format!("Could not create {server_configuration:?}"))?;
                    file.write_all(configuration.as_bytes())
                        .with_context(|| format!("Could not write {server_configuration:?}"))?;

                    tracing::info!(
                        configuration = ?server_configuration,
                        listen = listen,
                        token = server.token.is_some(),
                        refused = refuse.join(","),
                        allowed = allowed.len(),
                        "initialized server configuration",
                    );
                    println!(
                        "Initialized {} listening at {}",
                        server_configuration.display(),
                        listen
                    );
                    Ok(())
                }
            }
        }

        Command::Serve => {
            check_storage(&settings.home, &storage, "serve")?;

            let server_configuration = settings.home.join(SERVER_CONFIGURATION);
            let mut server = Server::default();
            if server_configuration.is_file() {
                server = read_configuration(&server_configuration)?;
                tracing::debug!(configuration = ?server_configuration, "read server configuration");
            }
            let permissions = server.allowed()?;
            let address = match server.listen {
                Some(listen) => listen,
                None => borhan::DEFAULT_LISTEN_ADDRESS.to_string(),
            };

            let router = borhan::api::router(borhan::api::App::new(
                storage.clone(),
                server.token.clone(),
                permissions.clone(),
            ));
            let listener = tokio::net::TcpListener::bind(&address)
                .await
                .with_context(|| format!("Could not listen on {address}"))?;
            let allowed: Vec<&str> = permissions
                .iter()
                .map(|permission| permission.as_str())
                .collect();
            tracing::info!(
                server.address = address,
                storage = ?storage,
                token = server.token.is_some(),
                permissions = allowed.join(","),
                "started HTTP server",
            );
            axum::serve(listener, router)
                .await
                .context("HTTP server stopped")?;
            Ok(())
        }

        Command::Memory { command } => {
            // No implicit `list`. Two readers of the same identifier is the
            // mistake `memory get` was, and a bare verb that quietly does one
            // of nine things is the same mistake spelled differently: the
            // caller who typed it did not choose `list`, and the caller who
            // needed to be told what the nine are gets a listing instead.
            let Some(command) = command else {
                // `build` first: until it runs, a subcommand fetched out of
                // the tree has neither the propagated global options nor a
                // `bin_name`, and prints `Usage: memory` — a command that does
                // not exist — instead of `Usage: borhan memory`.
                let mut root = <CommandLine as clap::CommandFactory>::command();
                root.build();
                match root.find_subcommand_mut("memory") {
                    Some(memory) => memory.print_long_help()?,
                    None => root.print_long_help()?,
                }
                return Ok(());
            };
            let remote = probe_server(&settings.home)?;
            match command {
                MemoryCommand::Create {
                    name,
                    description,
                    languages,
                    json,
                } => {
                    if let Some(remote) = &remote {
                        let response = request(
                            remote,
                            "POST",
                            "/api/v1/memory",
                            Some(serde_json::json!({
                                "name": name,
                                "description": description,
                                "languages": languages,
                            })),
                        )?;
                        print_http(&response, json, |body| {
                            println!("{}", body["id"].as_str().unwrap_or(""));
                            Ok(())
                        })?;
                    } else {
                        check_storage(&settings.home, &storage, "memory create ...")?;
                        let trace = Ulid::new()?;
                        let (id, stats) =
                            borhan::api::create(&storage, &name, &description, &languages, &trace)?;
                        if json {
                            print_pretty(&borhan::api::id_json(&id, &stats))?;
                        } else {
                            println!("{id}");
                        }
                    }
                    Ok(())
                }

                MemoryCommand::List { json } => {
                    if let Some(remote) = &remote {
                        let response = request(remote, "GET", "/api/v1/memory_list", None)?;
                        print_http(&response, json, print_memory_list_json)?;
                    } else {
                        check_storage(&settings.home, &storage, "memory list")?;
                        let trace = Ulid::new()?;
                        let (memories, stats) = borhan::api::list(&storage, &trace)?;
                        if json {
                            print_pretty(&borhan::api::list_json(&memories, &stats))?;
                        } else {
                            print_memory_list(&memories);
                        }
                    }
                    Ok(())
                }

                MemoryCommand::Update {
                    name,
                    description,
                    languages,
                    json,
                } => {
                    if description.is_none() && languages.is_none() {
                        anyhow::bail!("Pass --description and/or --languages");
                    }
                    if let Some(remote) = &remote {
                        let mut payload = serde_json::Map::new();
                        if let Some(description) = &description {
                            payload.insert(
                                "description".to_string(),
                                serde_json::Value::String(description.clone()),
                            );
                        }
                        if let Some(languages) = &languages {
                            payload.insert(
                                "languages".to_string(),
                                serde_json::Value::String(languages.clone()),
                            );
                        }
                        let response = request(
                            remote,
                            "PATCH",
                            &format!("/api/v1/memory/{name}"),
                            Some(serde_json::Value::Object(payload)),
                        )?;
                        print_http(&response, json, |body| {
                            println!("{}", body["id"].as_str().unwrap_or(""));
                            Ok(())
                        })?;
                    } else {
                        check_storage(&settings.home, &storage, "memory update ...")?;
                        let store = Storage::open(&storage, &name)?;
                        let trace = Ulid::new()?;
                        let (id, stats) = borhan::api::update(
                            &store,
                            &name,
                            description.as_deref(),
                            languages.as_deref(),
                            &trace,
                        )?;
                        if json {
                            print_pretty(&borhan::api::id_json(&id, &stats))?;
                        } else {
                            println!("{id}");
                        }
                    }
                    Ok(())
                }

                MemoryCommand::Delete { name, yes, json } => {
                    if !yes {
                        anyhow::bail!(
                            "Refusing to delete {name:?} without --yes. This removes every \
                             message in it and cannot be undone."
                        );
                    }
                    if let Some(remote) = &remote {
                        let response =
                            request(remote, "DELETE", &format!("/api/v1/memory/{name}"), None)?;
                        print_http(&response, json, |body| {
                            print_deleted(
                                body["name"].as_str().unwrap_or(&name),
                                body["sessions"].as_u64().unwrap_or(0),
                                body["messages"].as_u64().unwrap_or(0),
                                body["units"].as_u64().unwrap_or(0),
                            );
                            Ok(())
                        })?;
                    } else {
                        check_storage(&settings.home, &storage, "memory delete ...")?;
                        let trace = Ulid::new()?;
                        let (memory, stats) = borhan::api::delete(&storage, &name, None, &trace)?;
                        if json {
                            print_pretty(&serde_json::json!({
                                "id": memory.id.to_string(),
                                "name": memory.name,
                                "sessions": memory.sessions,
                                "messages": memory.messages,
                                "units": memory.units,
                                "stats": stats,
                            }))?;
                        } else {
                            print_deleted(
                                &memory.name,
                                memory.sessions,
                                memory.messages,
                                memory.units,
                            );
                        }
                    }
                    Ok(())
                }

                MemoryCommand::Add {
                    name,
                    text,
                    session,
                    message,
                    role,
                    author,
                    ts,
                    json,
                } => {
                    if let Some(remote) = &remote {
                        let mut payload = serde_json::json!({
                            "session": session,
                            "role": role,
                            "body": text,
                        });
                        if let Some(message) = &message {
                            payload["message"] = serde_json::Value::String(message.clone());
                        }
                        if let Some(author) = &author {
                            payload["author"] = serde_json::Value::String(author.clone());
                        }
                        if let Some(ts) = ts {
                            payload["ts"] = serde_json::json!(ts);
                        }
                        let response = request(
                            remote,
                            "POST",
                            &format!("/api/v1/memory/{name}/message_list"),
                            Some(payload),
                        )?;
                        print_http(&response, json, |body| {
                            if let Some(units) = body["units"].as_u64() {
                                eprintln!("{units} units");
                            }
                            println!("{}", body["id"].as_str().unwrap_or(""));
                            Ok(())
                        })?;
                    } else {
                        check_storage(&settings.home, &storage, "memory add ...")?;
                        let Some(role) = Role::parse(&role) else {
                            anyhow::bail!("Role {role:?} is not one of user, assistant or tool");
                        };
                        let store = Storage::open(&storage, &name)?;
                        let built = Index::open(&store)?;
                        let ts = match ts {
                            Some(ts) => ts,
                            None => Ulid::now(),
                        };
                        let author = match &author {
                            Some(author) => author.as_str(),
                            None => role.as_str(),
                        };
                        let writer = built.writer()?;
                        let store = Mutex::new(store);
                        let writer = Mutex::new(writer);
                        let entry = Entry {
                            session: &session,
                            message: message.as_deref(),
                            author,
                            role,
                            ts,
                            body: &text,
                        };
                        let trace = Ulid::new()?;
                        let (written, stats) =
                            borhan::api::add(&store, &built, &writer, &entry, &name, &trace)?;
                        if json {
                            print_pretty(&serde_json::json!({
                                "id": written.message.to_string(),
                                "units": written.units.len(),
                                "stats": stats,
                            }))?;
                        } else {
                            eprintln!("{} units", written.units.len());
                            println!("{}", written.message);
                        }
                    }
                    Ok(())
                }

                MemoryCommand::Outline {
                    name,
                    session,
                    json,
                } => {
                    if let Some(remote) = &remote {
                        let mut payload = serde_json::json!({});
                        if let Some(session) = &session {
                            payload["session"] = serde_json::Value::String(session.clone());
                        }
                        let response = request(
                            remote,
                            "POST",
                            &format!("/api/v1/memory/{name}/outline"),
                            Some(payload),
                        )?;
                        print_http(&response, json, print_outline_json)?;
                    } else {
                        check_storage(&settings.home, &storage, "memory outline ...")?;
                        let store = Storage::open(&storage, &name)?;
                        let trace = Ulid::new()?;
                        let (outline, stats) =
                            borhan::api::outline(&store, &name, session.as_deref(), &trace)?;
                        if json {
                            print_pretty(&borhan::api::outline_json(&outline, &stats))?;
                        } else {
                            print_outline(&outline);
                        }
                    }
                    Ok(())
                }

                MemoryCommand::Replace {
                    name,
                    text,
                    session,
                    message,
                    ts,
                    json,
                } => {
                    if let Some(remote) = &remote {
                        let mut payload = serde_json::json!({
                            "session": session,
                            "message": message,
                            "body": text,
                        });
                        if let Some(ts) = ts {
                            payload["ts"] = serde_json::json!(ts);
                        }
                        let response = request(
                            remote,
                            "POST",
                            &format!("/api/v1/memory/{name}/message"),
                            Some(payload),
                        )?;
                        print_http(&response, json, |body| {
                            if let Some(units) = body["units"].as_u64() {
                                eprintln!("{units} units");
                            }
                            println!("{}", body["id"].as_str().unwrap_or(""));
                            Ok(())
                        })?;
                    } else {
                        check_storage(&settings.home, &storage, "memory replace ...")?;
                        let store = Storage::open(&storage, &name)?;
                        let built = Index::open(&store)?;
                        let ts = match ts {
                            Some(ts) => ts,
                            None => Ulid::now(),
                        };
                        let writer = built.writer()?;
                        let store = Mutex::new(store);
                        let writer = Mutex::new(writer);
                        let revision = Revision {
                            session: &session,
                            message: &message,
                            ts,
                            body: &text,
                        };
                        let trace = Ulid::new()?;
                        let (id, units, reindexed, stats) = borhan::api::replace(
                            &store, &built, &writer, &revision, &name, &trace,
                        )?;
                        if json {
                            print_pretty(&serde_json::json!({
                                "id": id.to_string(),
                                "units": units,
                                "reindexed": reindexed,
                                "stats": stats,
                            }))?;
                        } else {
                            eprintln!("{units} units, {reindexed} messages reindexed");
                            println!("{id}");
                        }
                    }
                    Ok(())
                }

                MemoryCommand::Search {
                    name,
                    query,
                    fuzzy,
                    limit,
                    max_per_message,
                    session,
                    after,
                    before,
                    roles,
                    json,
                } => {
                    if let Some(remote) = &remote {
                        let mut payload = serde_json::json!({
                            "query": query,
                            "fuzzy": fuzzy,
                            "limit": limit,
                            "max_per_message": max_per_message,
                        });
                        if let Some(session) = &session {
                            payload["session"] = serde_json::Value::String(session.clone());
                        }
                        if let Some(after) = after {
                            payload["after"] = serde_json::json!(after);
                        }
                        if let Some(before) = before {
                            payload["before"] = serde_json::json!(before);
                        }
                        if !roles.is_empty() {
                            payload["role_list"] = serde_json::json!(roles);
                        }
                        let response = request(
                            remote,
                            "POST",
                            &format!("/api/v1/memory/{name}/search"),
                            Some(payload),
                        )?;
                        print_http(&response, json, print_search_json)?;
                    } else {
                        check_storage(&settings.home, &storage, "memory search ...")?;
                        let mut filter = Filter {
                            after,
                            before,
                            ..Filter::default()
                        };
                        if let Some(session) = &session {
                            match Ulid::parse(session) {
                                Ok(session) => filter.session = Some(session),
                                Err(error) => {
                                    return Err(anyhow::Error::new(error).context(format!(
                                        "--session takes a session ULID, and {session:?} is not one"
                                    )));
                                }
                            }
                        }
                        for role in &roles {
                            let Some(role) = Role::parse(role) else {
                                anyhow::bail!(
                                    "Role {role:?} is not one of user, assistant or tool"
                                );
                            };
                            filter.roles.push(role);
                        }
                        let store = Storage::open(&storage, &name)?;
                        let built = Index::open(&store)?;
                        let trace = Ulid::new()?;
                        let (outcome, stats) = borhan::api::search(
                            &store,
                            &built,
                            &name,
                            (&query, fuzzy),
                            &filter,
                            (limit, max_per_message),
                            &trace,
                        )?;
                        if json {
                            print_pretty(&borhan::api::search_json(&outcome, &stats))?;
                        } else {
                            print_search(&outcome);
                        }
                    }
                    Ok(())
                }

                MemoryCommand::Cursor {
                    name,
                    cursors,
                    before,
                    after,
                    messages,
                    json,
                } => {
                    let body = serde_json::json!({
                        "cursor_list": cursors,
                        "before": before,
                        "after": after,
                        "messages": messages,
                    });
                    if let Some(remote) = &remote {
                        let response = request(
                            remote,
                            "POST",
                            &format!("/api/v1/memory/{name}/cursor"),
                            Some(body),
                        )?;
                        print_http(&response, json, print_cursor_json)?;
                    } else {
                        check_storage(&settings.home, &storage, "memory cursor ...")?;
                        let mut units = Vec::new();
                        for cursor in &cursors {
                            match Ulid::parse(cursor) {
                                Ok(unit) => units.push(unit),
                                Err(error) => {
                                    return Err(anyhow::Error::new(error).context(format!(
                                        "{cursor:?} is not a cursor from a search hit"
                                    )));
                                }
                            }
                        }
                        let store = Storage::open(&storage, &name)?;
                        let trace = Ulid::new()?;
                        let (rows, missing, stats) = borhan::api::cursor(
                            &store, &name, &units, before, after, messages, &trace,
                        )?;
                        for unit in &missing {
                            eprintln!("No unit {unit}");
                        }
                        if json {
                            print_pretty(&borhan::api::cursor_json(
                                &rows, &units, &missing, messages, &stats,
                            ))?;
                        } else {
                            print_cursor(&rows, &units, messages);
                        }
                    }
                    Ok(())
                }

                MemoryCommand::Lexicon { name, words, json } => {
                    if words.is_empty() {
                        anyhow::bail!("Pass at least one word to look up");
                    }
                    if let Some(remote) = &remote {
                        let response = request(
                            remote,
                            "POST",
                            &format!("/api/v1/memory/{name}/lexicon"),
                            Some(serde_json::json!({ "word_list": words })),
                        )?;
                        print_http(&response, json, print_lexicon_json)?;
                    } else {
                        check_storage(&settings.home, &storage, "memory lexicon ...")?;
                        let store = Storage::open(&storage, &name)?;
                        let built = Index::open(&store)?;
                        let trace = Ulid::new()?;
                        let (rows, stats) = borhan::api::lexicon(&built, &name, &words, &trace)?;
                        if json {
                            print_pretty(&borhan::api::lexicon_json(&rows, &stats))?;
                        } else {
                            print_lexicon(&rows);
                        }
                    }
                    Ok(())
                }

                MemoryCommand::Rescan { name, json } => {
                    if let Some(remote) = &remote {
                        let response = request(
                            remote,
                            "POST",
                            &format!("/api/v1/memory/{name}/rescan"),
                            None,
                        )?;
                        print_http(&response, json, |body| {
                            eprintln!(
                                "{} messages, {} units",
                                body["messages"].as_u64().unwrap_or(0),
                                body["units"].as_u64().unwrap_or(0)
                            );
                            Ok(())
                        })?;
                    } else {
                        check_storage(&settings.home, &storage, "memory rescan ...")?;
                        let store = Storage::open(&storage, &name)?;
                        let built = Index::attach(&store.directory.join(borhan::index::DIRECTORY))?;
                        let writer = built.writer()?;
                        let store = Mutex::new(store);
                        let writer = Mutex::new(writer);
                        let trace = Ulid::new()?;
                        let (messages, units, stats) =
                            borhan::api::rescan(&store, &built, &writer, &name, &trace)?;
                        if json {
                            print_pretty(&serde_json::json!({
                                "messages": messages,
                                "units": units,
                                "stats": stats,
                            }))?;
                        } else {
                            eprintln!("{messages} messages, {units} units");
                        }
                    }
                    Ok(())
                }
            }
        }

        Command::Skills { command } => {
            // Same reasoning as `memory`: no implicit subcommand. A bare
            // `skills` is someone who does not yet know what is on offer, and
            // quietly printing one of them is not the answer to that.
            let Some(command) = command else {
                let mut root = <CommandLine as clap::CommandFactory>::command();
                root.build();
                match root.find_subcommand_mut("skills") {
                    Some(skills) => skills.print_long_help()?,
                    None => root.print_long_help()?,
                }
                return Ok(());
            };

            // Both arms differ only in which file and which name, so the
            // work is done once below rather than twice here. The pair is what
            // makes that worth doing: with one skill an inlined loop was the
            // simpler thing.
            let (name, skill, install) = match command {
                SkillCommand::Remember { install } => {
                    // The whole file, frontmatter included, so that whether it
                    // is redirected by hand or written by `--install` the
                    // result is the same bytes.
                    (REMEMBER_SKILL, include_str!("skills/remember.md"), install)
                }
                SkillCommand::Survey { install } => {
                    (SURVEY_SKILL, include_str!("skills/survey.md"), install)
                }
            };
            if !install {
                print!("{skill}");
                return Ok(());
            }

            let Some(home) = user_home() else {
                anyhow::bail!(
                    "Could not find your home directory, and --install writes into \
                     the agent configuration directories under it. Set \
                     {HOME_VARIABLE}, or place the skill yourself:\n\
                     \n    borhan skills ... > \
                     ~/.agents/{SKILL_DIRECTORY}/{name}/{SKILL_FILE}",
                );
            };

            let mut installed = 0;
            for reader in SKILL_READERS {
                let Some(root) = reader.root else {
                    println!("skipped    {:<9} {}", reader.agent, reader.reason);
                    continue;
                };
                let root = home.join(root);
                if !reader.always && !root.is_dir() {
                    println!(
                        "skipped    {:<9} {} is not there, so {} is not set up here",
                        reader.agent,
                        root.display(),
                        reader.agent,
                    );
                    continue;
                }

                let directory = root.join(SKILL_DIRECTORY).join(name);
                let file = directory.join(SKILL_FILE);
                // Read before the write, because "replaced" and "installed"
                // are the difference between a copy the user had edited and
                // one they never had.
                let replaced = file.is_file();
                fs::create_dir_all(&directory)
                    .with_context(|| format!("Could not create {directory:?}"))?;
                fs::write(&file, skill).with_context(|| format!("Could not write {file:?}"))?;
                installed += 1;
                let verb = match replaced {
                    true => "replaced",
                    false => "installed",
                };
                println!("{verb:<10} {:<9} {}", reader.agent, file.display());
            }

            tracing::info!(skill = name, destinations = installed, "installed skill");
            eprintln!();
            eprintln!(
                "{installed} destination(s). Each agent offers /{name} from its next \
                 session; nothing under --home was touched.",
            );
            Ok(())
        }
    }
}

/// A `serve` that answered, and everything needed to keep talking to it.
///
/// The agent is carried rather than rebuilt per request because it holds the
/// TLS configuration — a rebuilt one would verify certificates that
/// [`Server::remote_skip_tls_verify`] said not to — and because it pools the
/// connection, which for an `https://` origin saves a second handshake on a
/// command that makes two calls.
struct Remote {
    origin: String,
    token: Option<String>,
    agent: ureq::Agent,
}

/// The HTTP client, configured from `server.toml`.
///
/// `http_status_as_error(false)` because a 404 here is an answer and not a
/// failure: the body carries the server's `error` message, its `X-Trace-Id` and
/// its version, all of which this CLI reports, and all of which are thrown away
/// by a client that turns a status into an `Err` before the body is read.
fn client(server: &Server) -> ureq::Agent {
    let mut tls = ureq::tls::TlsConfig::builder();
    if server.remote_skip_tls_verify.unwrap_or_default() {
        tls = tls.disable_verification(true);
    }
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(120)))
        .tls_config(tls.build())
        .build()
        .new_agent()
}

fn probe_server(home: &Path) -> anyhow::Result<Option<Remote>> {
    let path = home.join(SERVER_CONFIGURATION);
    if !path.is_file() {
        tracing::debug!(reason = "no server.toml", "using local storage");
        return Ok(None);
    }
    let server: Server = read_configuration(&path)?;
    let Some((origin, required)) = server.client_origin()? else {
        tracing::debug!(reason = "server.toml has no listen", "using local storage");
        return Ok(None);
    };
    let agent = client(&server);
    let url = format!("{origin}/api/v1/health");
    // One second, and separate from the 120 the agent carries for real work: a
    // probe is asking whether anything is there, and the answer to that arrives
    // fast or not at all.
    let mut req = agent
        .get(&url)
        .config()
        .timeout_global(Some(Duration::from_secs(1)))
        .build();
    if let Some(token) = &server.token {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    match req.call() {
        Ok(response) if response.status() == 200 => {
            tracing::debug!(server.address = origin.as_str(), "using HTTP server");
            Ok(Some(Remote {
                origin,
                token: server.token,
                agent,
            }))
        }
        Ok(response) => {
            if required {
                anyhow::bail!(
                    "{url} answered {} and not 200. remote_address names the store this \
                     command would have used; refusing to fall back to the local one.",
                    response.status().as_u16(),
                );
            }
            tracing::debug!(
                reason = "health was not 200",
                http.response.status_code = response.status().as_u16(),
                "using local storage",
            );
            Ok(None)
        }
        Err(error) => {
            if required {
                return Err(anyhow::Error::new(error).context(format!(
                    "No answer from {url}. remote_address names the store this command \
                     would have used; refusing to fall back to the local one."
                )));
            }
            tracing::debug!(reason = "no response", error = %error, "using local storage");
            Ok(None)
        }
    }
}

struct ClientResponse {
    version: Option<String>,
    body: serde_json::Value,
}

fn versions_match(remote: Option<&str>) -> bool {
    match remote {
        Some(version) => version == CRATE_VERSION,
        None => false,
    }
}

fn print_pretty(body: &serde_json::Value) -> anyhow::Result<()> {
    let text = match serde_json::to_string_pretty(body) {
        Ok(text) => text,
        Err(error) => {
            return Err(anyhow::Error::new(error).context("Could not encode JSON"));
        }
    };
    println!("{text}");
    Ok(())
}

fn print_trace(trace: Option<&str>) {
    if let Some(trace) = trace
        && !trace.is_empty()
    {
        eprintln!("X-Trace-Id: {trace}");
    }
}

fn print_http(
    response: &ClientResponse,
    json: bool,
    print_text: impl FnOnce(&serde_json::Value) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    if json || !versions_match(response.version.as_deref()) {
        return print_pretty(&response.body);
    }
    print_text(&response.body)
}

/// The ceiling on a response body held in memory before it is parsed.
///
/// Well above the client's default, because the body on the other side of this
/// is `memory get --json` over a memory holding years of conversation, and the
/// failure a low limit produces is not a truncated answer but a parse error on
/// a command that works fine against local storage.
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024 * 1024;

fn header(response: &ureq::http::Response<ureq::Body>, name: &str) -> Option<String> {
    let value = response.headers().get(name)?;
    value.to_str().ok().map(str::to_string)
}

fn request(
    remote: &Remote,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> anyhow::Result<ClientResponse> {
    let url = format!("{}{path}", remote.origin);
    let (http_path, http_query) = match path.split_once('?') {
        Some((path, query)) => (path, query),
        None => (path, ""),
    };
    let request_bytes = match &body {
        Some(value) => value.to_string().len() as u64,
        None => 0,
    };
    // `http.client` is the mirror of the server's `http.server`, down to the
    // field names, so one Grafana panel can show both sides of a call and the
    // `trace_id` that joins them is the server's own — read off `X-Trace-Id`
    // and logged here, so the CLI line and the two server lines are one query.
    let _span = tracing::info_span!("http.client").entered();
    let started = std::time::Instant::now();
    let mut builder = ureq::http::Request::builder()
        .method(method)
        .uri(url.as_str());
    if let Some(token) = &remote.token {
        builder = builder.header("Authorization", format!("Bearer {token}"));
    }
    // `Content-Type` by hand because this goes through the http-crate request
    // rather than the client's own `send_json`, and axum's `Json` extractor
    // dispatches on the header: without it every write is a 415 and not a
    // parse error, which reads like the server refusing the operation.
    let response = match &body {
        Some(value) => {
            let request = builder
                .header("Content-Type", "application/json")
                .body(value.to_string())
                .with_context(|| format!("Could not build {method} {url}"))?;
            remote.agent.run(request)
        }
        None => {
            let request = builder
                .body(())
                .with_context(|| format!("Could not build {method} {url}"))?;
            remote.agent.run(request)
        }
    };
    let total_ms = started.elapsed().as_millis() as u64;
    match response {
        Ok(mut response) => {
            // Headers first, and owned: reading the body takes the response
            // mutably, and every one of these is still wanted afterwards.
            let code = response.status().as_u16();
            let response_bytes: u64 = match header(&response, "content-length") {
                Some(value) => value.parse().unwrap_or_default(),
                None => 0,
            };
            let version = header(&response, "x-borhan-version");
            let mut trace = header(&response, "x-trace-id");

            if code < 400 {
                tracing::info!(
                    trace_id = trace.as_deref().unwrap_or("-"),
                    http.request.method = method,
                    url.path = http_path,
                    url.query = http_query,
                    http.request.body.size = request_bytes,
                    http.response.status_code = code,
                    http.response.body.size = response_bytes,
                    total_ms = total_ms,
                    "sent request",
                );
                let body = response
                    .body_mut()
                    .with_config()
                    .limit(MAX_RESPONSE_BYTES)
                    .read_json::<serde_json::Value>();
                let body = match body {
                    Ok(value) => value,
                    Err(error) => {
                        return Err(anyhow::Error::new(error).context("Could not read JSON"));
                    }
                };
                return Ok(ClientResponse { version, body });
            }

            let body = response
                .body_mut()
                .with_config()
                .limit(MAX_RESPONSE_BYTES)
                .read_json::<serde_json::Value>();
            let body = match body {
                Ok(value) => value,
                Err(_) => serde_json::json!({}),
            };
            if trace.is_none()
                && let Some(value) = body
                    .get("stats")
                    .and_then(|stats| stats.get("trace"))
                    .and_then(|trace| trace.as_str())
            {
                trace = Some(value.to_string());
            }
            macro_rules! report {
                ($level:ident, $said:literal) => {
                    tracing::$level!(
                        trace_id = trace.as_deref().unwrap_or("-"),
                        http.request.method = method,
                        url.path = http_path,
                        url.query = http_query,
                        http.request.body.size = request_bytes,
                        http.response.status_code = code,
                        http.response.body.size = response_bytes,
                        total_ms = total_ms,
                        $said,
                    )
                };
            }
            if code >= 500 {
                report!(error, "server failed the request");
            } else {
                report!(warn, "server rejected the request");
            }
            print_trace(trace.as_deref());
            if !versions_match(version.as_deref()) {
                print_pretty(&body)?;
                anyhow::bail!("HTTP {code}");
            }
            let message = match body.get("error").and_then(|error| error.as_str()) {
                Some(message) => message.to_string(),
                None => format!("HTTP {code}"),
            };
            anyhow::bail!("{message}")
        }
        Err(error) => {
            // No status and no `X-Trace-Id`: the request never reached a
            // handler, so there is no server-side line to join this to and the
            // error string is the whole of what happened.
            tracing::error!(
                http.request.method = method,
                url.path = http_path,
                url.query = http_query,
                http.request.body.size = request_bytes,
                total_ms = total_ms,
                error = %error,
                "no response from server",
            );
            Err(anyhow::Error::new(error).context(format!("{method} {url}")))
        }
    }
}

fn column_widths(titles: &[&str], rows: &[Vec<String>]) -> Vec<usize> {
    let mut widths = Vec::new();
    for title in titles {
        widths.push(title.chars().count());
    }
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    widths
}

fn columns_line(cells: &[String], widths: &[usize]) -> String {
    let mut line = String::new();
    for (at, cell) in cells.iter().enumerate() {
        if at > 0 {
            line.push_str("  ");
        }
        let width = match widths.get(at) {
            Some(width) => *width,
            None => cell.chars().count(),
        };
        line.push_str(&format!("{cell:<width$}"));
    }
    line
}

fn print_column_header(titles: &[&str], widths: &[usize]) {
    let mut cells = Vec::new();
    for title in titles {
        cells.push((*title).to_string());
    }
    eprintln!("{}", columns_line(&cells, widths));
}

fn print_deleted(name: &str, sessions: u64, messages: u64, units: u64) {
    println!("Deleted {name}: {sessions} sessions, {messages} messages, {units} units.");
}

fn print_memory_list(memories: &[borhan::storage::Memory]) {
    if memories.is_empty() {
        eprintln!("No memories yet — `borhan memory create <name>`.");
        return;
    }
    // Five columns and the description underneath, the same two-line shape
    // `memory search` and `memory cursor` use, rather than six columns with the
    // description cut to sixty characters.
    //
    // The description is the one field here written for a reader: it is what
    // says which memory this is and, more usefully, what is *not* in it, and a
    // caller choosing between memories has to read all of it to choose. Sixty
    // characters reliably ended mid-clause, so the listing showed the half of
    // the sentence that says what the memory holds and cut the half that says
    // where it stops — and the only way to see the rest was a command that
    // requires already knowing which memory you wanted.
    //
    // It cannot be a sixth column either: the text runs to two thousand
    // characters and the padding is computed from the widest cell, so one long
    // description sets the width of a table nothing else in it needs.
    const TITLES: [&str; 5] = ["id", "created", "name", "counts", "languages"];
    let mut rows = Vec::new();
    let mut bodies = Vec::new();
    for memory in memories {
        let created = chrono::DateTime::from_timestamp_millis(memory.created_at);
        let created = match created {
            Some(created) => created.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            None => "-".to_string(),
        };
        let counts = format!(
            "{} sessions, {} messages, {} units",
            memory.sessions, memory.messages, memory.units
        );
        rows.push(vec![
            memory.id.to_string(),
            created,
            memory.name.clone(),
            counts,
            memory.languages.clone(),
        ]);
        bodies.push(match &memory.description {
            Some(description) => description.clone(),
            None => "(no description)".to_string(),
        });
    }
    print_block_list(&TITLES, &rows, &bodies);
}

fn print_table(titles: &[&str], rows: &[Vec<String>]) {
    if rows.is_empty() {
        return;
    }
    let widths = column_widths(titles, rows);
    print_column_header(titles, &widths);
    for row in rows {
        println!("{}", columns_line(row, &widths));
    }
}

fn print_block_list(titles: &[&str], rows: &[Vec<String>], bodies: &[String]) {
    if rows.is_empty() {
        return;
    }
    let widths = column_widths(titles, rows);
    print_column_header(titles, &widths);
    for (row, body) in rows.iter().zip(bodies) {
        println!("{}", columns_line(row, &widths));
        println!("{body}");
        println!();
    }
}

fn print_outline(outline: &borhan::api::Outline) {
    match outline {
        borhan::api::Outline::Sessions(sessions) => {
            if sessions.is_empty() {
                eprintln!("No sessions yet — `borhan memory add <name> ... --session <id>`.");
                return;
            }
            const TITLES: [&str; 5] = ["session", "started", "ended", "messages", "units"];
            let mut rows = Vec::new();
            for session in sessions {
                rows.push(vec![
                    session.reference.clone(),
                    stamp(Some(session.started_at)),
                    stamp(session.ended_at),
                    session.messages.to_string(),
                    session.units.to_string(),
                ]);
            }
            print_table(&TITLES, &rows);
        }
        borhan::api::Outline::Messages(messages) => {
            const TITLES: [&str; 7] = [
                "message",
                "seq",
                "role",
                "author",
                "written",
                "characters",
                "units",
            ];
            let mut rows = Vec::new();
            for message in messages {
                let reference = match &message.reference {
                    Some(reference) => reference.clone(),
                    // A message stored without one. It can still be read, but
                    // it cannot be named again, so it cannot be replaced.
                    None => "-".to_string(),
                };
                rows.push(vec![
                    reference,
                    message.seq.to_string(),
                    message.role.as_str().to_string(),
                    message.author.clone(),
                    stamp(Some(message.ts)),
                    message.characters.to_string(),
                    message.units.to_string(),
                ]);
            }
            print_table(&TITLES, &rows);
        }
    }
}

/// Unix milliseconds as a date a person can read, and `-` for no time at all.
/// One TOML basic string, quoted and escaped.
///
/// `format!("{text:?}")` is nearly this and not quite: Rust escapes an
/// unprintable character as `\u{1}`, which TOML does not accept. The permission
/// names are known-safe, but the token is whatever was typed on the command
/// line, and a token that lands in the file wrongly quoted is a server that
/// will not start.
fn quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            character if (character as u32) < 0x20 || character as u32 == 0x7f => {
                out.push_str(&format!("\\u{:04X}", character as u32));
            }
            character => out.push(character),
        }
    }
    out.push('"');
    out
}

fn stamp(ts: Option<i64>) -> String {
    let Some(ts) = ts else {
        return "-".to_string();
    };
    match chrono::DateTime::from_timestamp_millis(ts) {
        Some(time) => time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        None => "-".to_string(),
    }
}

fn print_outline_json(body: &serde_json::Value) -> anyhow::Result<()> {
    if let Some(list) = body.get("session_list").and_then(|value| value.as_array()) {
        let mut sessions = Vec::new();
        for session in list {
            let id = match Ulid::parse(session["id"].as_str().unwrap_or("")) {
                Ok(id) => id,
                Err(_) => continue,
            };
            sessions.push(borhan::storage::SessionRow {
                id,
                reference: session["session"].as_str().unwrap_or("").to_string(),
                started_at: session["started_at"].as_i64().unwrap_or(0),
                ended_at: session["ended_at"].as_i64(),
                messages: session["messages"].as_u64().unwrap_or(0),
                units: session["units"].as_u64().unwrap_or(0),
            });
        }
        print_outline(&borhan::api::Outline::Sessions(sessions));
        return Ok(());
    }

    let Some(list) = body.get("message_list").and_then(|value| value.as_array()) else {
        anyhow::bail!("server response has neither session_list nor message_list");
    };
    let mut messages = Vec::new();
    for message in list {
        let id = match Ulid::parse(message["id"].as_str().unwrap_or("")) {
            Ok(id) => id,
            Err(_) => continue,
        };
        let role = match Role::parse(message["role"].as_str().unwrap_or("")) {
            Some(role) => role,
            None => Role::User,
        };
        messages.push(borhan::storage::MessageRow {
            id,
            reference: message["message"].as_str().map(str::to_string),
            seq: message["seq"].as_i64().unwrap_or(0),
            author: message["author"].as_str().unwrap_or("").to_string(),
            role,
            ts: message["ts"].as_i64().unwrap_or(0),
            characters: message["characters"].as_u64().unwrap_or(0),
            units: message["units"].as_u64().unwrap_or(0),
        });
    }
    print_outline(&borhan::api::Outline::Messages(messages));
    Ok(())
}

fn print_memory_list_json(body: &serde_json::Value) -> anyhow::Result<()> {
    let Some(list) = body.get("memory_list").and_then(|value| value.as_array()) else {
        anyhow::bail!("server response has no memory_list");
    };
    if list.is_empty() {
        eprintln!("No memories yet — `borhan memory create <name>`.");
        return Ok(());
    }
    let mut memories = Vec::new();
    for memory in list {
        let id = match Ulid::parse(memory["id"].as_str().unwrap_or("")) {
            Ok(id) => id,
            Err(_) => continue,
        };
        memories.push(borhan::storage::Memory {
            id,
            name: memory["name"].as_str().unwrap_or("").to_string(),
            description: memory["description"].as_str().map(str::to_string),
            languages: memory["languages"].as_str().unwrap_or("").to_string(),
            created_at: memory["created_at"].as_i64().unwrap_or(0),
            sessions: memory["sessions"].as_u64().unwrap_or(0),
            messages: memory["messages"].as_u64().unwrap_or(0),
            units: memory["units"].as_u64().unwrap_or(0),
        });
    }
    print_memory_list(&memories);
    Ok(())
}

fn matched_cell(matched: &[String], nearby: &[String]) -> String {
    let mut cells: Vec<String> = matched.to_vec();
    for label in nearby {
        cells.push(format!("~{label}"));
    }
    format!("[{}]", cells.join(","))
}

fn print_search(outcome: &borhan::search::Outcome) {
    for unknown in &outcome.unknown {
        eprintln!(
            "unknown: {:?} (in {:?}) matched nothing",
            unknown.word, unknown.clause
        );
    }
    for fuzzy in &outcome.fuzzy {
        eprintln!(
            "fuzzy: {:?} matched nothing, taken as {}",
            fuzzy.word,
            fuzzy.matched.join(", ")
        );
    }
    if outcome.hits.is_empty() {
        eprintln!("No hits.");
        return;
    }
    const TITLES: [&str; 7] = [
        "score", "cover", "cursor", "session", "message", "size", "matched",
    ];
    let mut rows = Vec::new();
    let mut bodies = Vec::new();
    for hit in &outcome.hits {
        rows.push(vec![
            format!("{:.3}", hit.score),
            format!("{}/{}", hit.coverage.0, hit.coverage.1),
            hit.cursor.to_string(),
            hit.session_ref.to_string(),
            hit.message_ref.as_deref().unwrap_or("-").to_string(),
            format!("{} words", hit.words),
            matched_cell(&hit.matched, &hit.nearby),
        ]);
        bodies.push(format!("\"{}\"", preview(&hit.snippet)));
    }
    print_block_list(&TITLES, &rows, &bodies);
    if !outcome.hints.is_empty() {
        let mut hints = Vec::new();
        for (term, units) in &outcome.hints {
            hints.push(format!("{term} ({units})"));
        }
        eprintln!("also in these results: {}", hints.join(", "));
    }
}

fn print_search_json(body: &serde_json::Value) -> anyhow::Result<()> {
    if let Some(unknown) = body.get("unknown_list").and_then(|value| value.as_array()) {
        for item in unknown {
            eprintln!(
                "unknown: {:?} (in {:?}) matched nothing",
                item["word"].as_str().unwrap_or(""),
                item["clause"].as_str().unwrap_or("")
            );
        }
    }
    if let Some(fuzzy) = body.get("fuzzy_list").and_then(|value| value.as_array()) {
        for item in fuzzy {
            let mut matched = Vec::new();
            if let Some(list) = item["matched_list"].as_array() {
                for term in list {
                    if let Some(term) = term.as_str() {
                        matched.push(term);
                    }
                }
            }
            eprintln!(
                "fuzzy: {:?} matched nothing, taken as {}",
                item["word"].as_str().unwrap_or(""),
                matched.join(", ")
            );
        }
    }
    let Some(hits) = body.get("hit_list").and_then(|value| value.as_array()) else {
        anyhow::bail!("server response has no hit_list");
    };
    if hits.is_empty() {
        eprintln!("No hits.");
        return Ok(());
    }
    const TITLES: [&str; 7] = [
        "score", "cover", "cursor", "session", "message", "size", "matched",
    ];
    let mut rows = Vec::new();
    let mut bodies = Vec::new();
    for hit in hits {
        let coverage = hit["coverage"].as_array();
        let cover = match coverage {
            Some(coverage) if coverage.len() == 2 => {
                format!(
                    "{}/{}",
                    coverage[0].as_u64().unwrap_or(0),
                    coverage[1].as_u64().unwrap_or(0)
                )
            }
            _ => "-/-".to_string(),
        };
        let labels = |key: &str| {
            let mut labels = Vec::new();
            if let Some(list) = hit[key].as_array() {
                for label in list {
                    if let Some(label) = label.as_str() {
                        labels.push(label.to_string());
                    }
                }
            }
            labels
        };
        let matched = labels("matched_list");
        let nearby = labels("nearby_list");
        rows.push(vec![
            format!("{:.3}", hit["score"].as_f64().unwrap_or(0.0)),
            cover,
            hit["cursor"].as_str().unwrap_or("").to_string(),
            hit["session_ref"].as_str().unwrap_or("").to_string(),
            hit["message_ref"].as_str().unwrap_or("-").to_string(),
            format!("{} words", hit["words"].as_u64().unwrap_or(0)),
            matched_cell(&matched, &nearby),
        ]);
        bodies.push(format!(
            "\"{}\"",
            preview(hit["snippet"].as_str().unwrap_or(""))
        ));
    }
    print_block_list(&TITLES, &rows, &bodies);
    if let Some(hints) = body.get("hint_list").and_then(|value| value.as_array())
        && !hints.is_empty()
    {
        let mut parts = Vec::new();
        for hint in hints {
            parts.push(format!(
                "{} ({})",
                hint["term"].as_str().unwrap_or(""),
                hint["units"].as_u64().unwrap_or(0)
            ));
        }
        eprintln!("also in these results: {}", parts.join(", "));
    }
    Ok(())
}

fn print_cursor(rows: &[borhan::storage::Located], asked: &[Ulid], whole: bool) {
    let mut table = Vec::new();
    let mut bodies = Vec::new();
    if whole {
        const TITLES: [&str; 5] = ["hit", "message", "author", "role", "seq"];
        let mut at = None;
        for row in rows {
            if at == Some(row.message) {
                continue;
            }
            at = Some(row.message);
            let hit = rows
                .iter()
                .any(|other| other.message == row.message && asked.contains(&other.unit));
            table.push(vec![
                if hit {
                    "\u{2192}".to_string()
                } else {
                    String::new()
                },
                row.message.to_string(),
                row.author.clone(),
                row.role.as_str().to_string(),
                row.seq.to_string(),
            ]);
            bodies.push(row.body.clone());
        }
        print_block_list(&TITLES, &table, &bodies);
        return;
    }
    const TITLES: [&str; 5] = ["hit", "unit", "role", "session", "seq"];
    for row in rows {
        table.push(vec![
            if asked.contains(&row.unit) {
                "\u{2192}".to_string()
            } else {
                String::new()
            },
            row.unit.to_string(),
            row.role.as_str().to_string(),
            row.session_ref.clone(),
            format!("{}.{}", row.seq, row.unit_seq),
        ]);
        bodies.push(row.text().to_string());
    }
    print_block_list(&TITLES, &table, &bodies);
}

fn print_cursor_json(body: &serde_json::Value) -> anyhow::Result<()> {
    let Some(list) = body.get("message_list").and_then(|value| value.as_array()) else {
        anyhow::bail!("server response has no message_list");
    };
    if let Some(missing) = body.get("missing_list").and_then(|value| value.as_array()) {
        for unit in missing {
            eprintln!("No unit {}", unit.as_str().unwrap_or(""));
        }
    }
    let mark = |value: &serde_json::Value| {
        if value["anchor"].as_bool().unwrap_or(false) {
            "\u{2192}".to_string()
        } else {
            String::new()
        }
    };
    let mut table = Vec::new();
    let mut bodies = Vec::new();
    if list.iter().any(|message| message.get("body").is_some()) {
        const TITLES: [&str; 5] = ["hit", "message", "author", "role", "seq"];
        for message in list {
            table.push(vec![
                mark(message),
                message["message"].as_str().unwrap_or("").to_string(),
                message["author"].as_str().unwrap_or("").to_string(),
                message["role"].as_str().unwrap_or("").to_string(),
                message["seq"].as_i64().unwrap_or(0).to_string(),
            ]);
            bodies.push(message["body"].as_str().unwrap_or("").to_string());
        }
        print_block_list(&TITLES, &table, &bodies);
        return Ok(());
    }
    const TITLES: [&str; 5] = ["hit", "unit", "role", "session", "seq"];
    for message in list {
        let Some(units) = message["unit_list"].as_array() else {
            continue;
        };
        for unit in units {
            table.push(vec![
                mark(unit),
                unit["unit"].as_str().unwrap_or("").to_string(),
                message["role"].as_str().unwrap_or("").to_string(),
                message["session_ref"].as_str().unwrap_or("").to_string(),
                format!(
                    "{}.{}",
                    message["seq"].as_i64().unwrap_or(0),
                    unit["unit_seq"].as_i64().unwrap_or(0)
                ),
            ]);
            bodies.push(unit["text"].as_str().unwrap_or("").to_string());
        }
    }
    print_block_list(&TITLES, &table, &bodies);
    Ok(())
}

fn print_lexicon(rows: &[borhan::api::Lexeme]) {
    const TITLES: [&str; 4] = ["word", "surface", "lemma", "context"];
    let mut table = Vec::new();
    for row in rows {
        table.push(vec![
            row.word.clone(),
            format!("{} ({})", row.surface, row.surface_units),
            format!("{} ({})", row.lemma, row.lemma_units),
            row.context_units.to_string(),
        ]);
    }
    print_table(&TITLES, &table);
}

fn print_lexicon_json(body: &serde_json::Value) -> anyhow::Result<()> {
    let Some(list) = body.get("word_list").and_then(|value| value.as_array()) else {
        anyhow::bail!("server response has no word_list");
    };
    const TITLES: [&str; 4] = ["word", "surface", "lemma", "context"];
    let mut table = Vec::new();
    for row in list {
        table.push(vec![
            row["word"].as_str().unwrap_or("").to_string(),
            format!(
                "{} ({})",
                row["surface"].as_str().unwrap_or(""),
                row["surface_units"].as_u64().unwrap_or(0)
            ),
            format!(
                "{} ({})",
                row["lemma"].as_str().unwrap_or(""),
                row["lemma_units"].as_u64().unwrap_or(0)
            ),
            row["context_units"].as_u64().unwrap_or(0).to_string(),
        ]);
    }
    print_table(&TITLES, &table);
    Ok(())
}
