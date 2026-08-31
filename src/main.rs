mod api;
mod index;
mod normalize;
mod search;
mod storage;
mod ulid;

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::{filter::LevelFilter, fmt};

use crate::index::Index;
use crate::search::{Filter, Group};
use crate::storage::{Entry, Role, Storage};
use crate::ulid::Ulid;

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
//       server.toml         Listen address and token. Written by `init server`.
//                           `serve` binds it; the CLI probes it and talks HTTP
//                           when that process answers, otherwise opens storage.
//
// None of it is created implicitly: `init` is the only thing that writes the
// layout, so a missing directory always means "this machine was never set up",
// never "it was set up somewhere you did not look".
const STORAGE_DIRECTORY: &str = "storage";
const SERVER_CONFIGURATION: &str = "server.toml";
const CRATE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Where `serve` listens when `server.toml` does not say otherwise.
const DEFAULT_LISTEN_ADDRESS: &str = "127.0.0.1:1995";

/// Characters of a description `memory list` shows before cutting it off. The
/// listing is one line per memory, so the full text is not what is wanted here.
const SUMMARY_LIMIT: usize = 60;

/// Words of a snippet shown on one line of `memory search` results.
///
/// The snippet is already the best sentence of the unit rather than the whole
/// paragraph, so this is a backstop for a paragraph that has no sentence
/// boundaries in it — a long line of prose, or a table row. Whatever is shown
/// keeps the spacing it was stored with, with the newlines escaped, because a
/// hit has to stay one line for the columns beside it to line up.
const PREVIEW_WORDS: usize = 60;

/// Memory store: a SQLite source of truth under a tantivy index.
///
/// A memory holds messages. Every message is split into units — a paragraph, a
/// heading, a list item, a table row, a fenced code block — and a unit is what
/// search scores and returns. Nothing is created implicitly: `borhan init`
/// writes the layout, and every command reads `--home`, or `BORHAN_HOME`,
/// which defaults to `~/.borhan`.
///
/// If none of this is familiar yet, the order to work in is:
///
/// `memory list` — what memories exist, how large each is, and which languages
/// it was tagged with.
///
/// `memory lexicon <name> <words>…` — whether the words you are about to
/// search for are in that memory at all, and what they fold to. This is the
/// step most callers skip and should not: a search for a word the memory has
/// never seen returns other things rather than nothing, and a result set full
/// of other things looks exactly like a result set full of answers.
///
/// `memory search <name> <groups>…` — concept groups, not a sentence.
///
/// `memory cursor <name> <cursor>` — read the messages around a hit, once
/// search has told you where to look.
///
/// `memory get <name> <ids>…` — the full text of units, by id.
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

    /// Disable all logging.
    #[arg(long, global = true)]
    pub quiet: bool,

    /// Print help. `-h` and `--help` print the same thing: there is no
    /// abbreviated form, because the reader of a help text here is as likely to
    /// be a model composing its first query as a person who has run the command
    /// before, and the short form omits exactly what the first reader needs.
    #[arg(short = 'h', long = "help", global = true, action = clap::ArgAction::HelpLong)]
    pub help: Option<bool>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Create the home directory and what lives inside it.
    Init {
        #[command(subcommand)]
        command: Option<InitCommand>,
    },

    /// Serve the HTTP API that remote clients talk to.
    Serve,

    /// Work with the memories themselves.
    Memory {
        #[command(subcommand)]
        command: Option<MemoryCommand>,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum InitCommand {
    /// Create the local storage. The default when `init` is given no subcommand.
    Storage,

    /// Write `server.toml` so `serve` and the CLI share a listen address and token.
    Server {
        /// `HOST:PORT` to bind, and the address the CLI probes.
        #[arg(long)]
        listen: String,

        /// Token clients must present as `Authorization: Bearer`. Optional.
        #[arg(long)]
        token: Option<String>,
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

    /// List the memories, oldest first. The default when `memory` is given no
    /// subcommand.
    ///
    /// One line per memory, six columns: the id, when it was created, the name,
    /// what is in it, the language tags, and the description cut to 60
    /// characters.
    ///
    /// The name is the first argument of every other subcommand. The language
    /// tags are a hint from whoever created the memory about which languages a
    /// concept group is worth expanding into — they are not enforced, and a
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

    /// Print units back by id, in the order asked.
    ///
    /// A search result shows one sentence of a unit; this shows the unit.
    ///
    /// borhan memory get notes 01J8… 01J9…
    Get {
        /// The memory the units are in.
        name: String,

        /// Unit ULIDs: the `cursor` column of a `memory search` result, which
        /// is the third field of the first line of each hit.
        ids: Vec<String>,

        /// Emit JSON instead of text.
        #[arg(long)]
        json: bool,
    },

    /// Search a memory with concept groups.
    ///
    /// A group is a comma-separated list of words that mean the same thing,
    /// across languages if that is what the memory holds. Groups are the
    /// arguments; there is no flag, and nothing to repeat:
    ///
    /// borhan memory search notes error,fault,خطا '!timeout'
    ///
    /// Words inside one group are alternatives competing for a single slot and
    /// only the best of them scores, so one group should hold every spelling,
    /// inflection and translation of one idea, and never two different ideas.
    /// Separate groups are separate things being asked about, and how many of
    /// them a unit matches — its coverage — is the largest term in the score.
    /// Three groups of two words each ask a far better question than one group
    /// of six.
    ///
    /// A leading `!` makes a group required: units that miss it are dropped
    /// rather than ranked lower. `label=word,word` names a group so the result
    /// line can report which ones hit; unlabelled, the first word is the label.
    ///
    /// Do not paste a sentence in. Reduce it to the two to four things that
    /// have to co-occur, then expand each one into its synonyms. So "why does
    /// the borrow checker reject this mutable alias" becomes three groups:
    ///
    /// borrow,borrowck,borrowing mutable,mutably,mut alias,aliasing
    ///
    /// Two lines come back per hit, under a header that names the columns:
    ///
    /// 0.847  2/3  01J8…  session  message  75 words  [site,sx]
    ///
    /// "the best sentence of the unit, quoted"
    ///
    /// `score` is relative to the best hit in this result set, which is always
    /// 1.000; it orders these hits and means nothing next to the score of a
    /// different search. `cover` is how many groups the unit matched out of how
    /// many were asked, and it is the more trustworthy of the two — prefer 3/3
    /// at a middling score over 1/3 at a high one. `cursor` is what both
    /// `memory cursor` and `memory get` take.
    ///
    /// Hits go to standard output and nothing else does. The header, the
    /// unknown-word lines, `No hits.` and the closing vocabulary line all go to
    /// standard error, so a pipeline reading stdout receives only results.
    ///
    /// Read the unknown-word lines. `unknown: "cva" (group "dx") matched
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

        /// One or more groups, each a comma-separated list of alternatives.
        /// Quote a group only when it starts with `!`, which the shell would
        /// otherwise take.
        #[arg(required = true, num_args = 1..)]
        groups: Vec<String>,

        /// Hits to return, at most.
        #[arg(long, default_value_t = 10)]
        limit: usize,

        /// Units returned from any one message. Twenty hits from one message is
        /// a wasted result set; the cursor is how you read the rest of it.
        #[arg(long, default_value_t = 2)]
        max_per_message: usize,

        /// Confine the search to one session, by its ULID.
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

        /// Emit one JSON object holding `hit_list`, `unknown_list`, `hint_list`
        /// and `stats` instead of the table. All of it goes to standard output.
        #[arg(long)]
        json: bool,
    },

    /// Read the messages around a hit.
    ///
    /// The second half of the retrieval loop: recall a gist from a partial cue,
    /// then elaborate around it deliberately. No scoring and no snippets — the
    /// caller has already decided this region is worth reading.
    ///
    /// borhan memory cursor notes 01J8… --before 2 --after 2
    Cursor {
        /// The memory the hit came from.
        name: String,

        /// The `cursor` of a hit, as `memory search` printed it: the third
        /// field of the first line of the hit.
        cursor: String,

        /// Messages to include before the anchor. Whole messages, not units, so
        /// `--before 0 --after 0` returns the one message the hit came from.
        #[arg(long, default_value_t = 2)]
        before: i64,

        /// Messages to include after the anchor.
        #[arg(long, default_value_t = 2)]
        after: i64,

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
    /// Address to bind, and the address the CLI probes. [`DEFAULT_LISTEN_ADDRESS`]
    /// when unset for `serve`; the CLI treats a missing value as "use storage".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub listen: Option<String>,

    /// Token clients must present as `Authorization: Bearer`. Unset means open.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

impl CommandLine {
    pub fn logging_level(&self) -> LevelFilter {
        if self.quiet {
            LevelFilter::OFF
        } else if self.trace {
            LevelFilter::TRACE
        } else if self.debug {
            LevelFilter::DEBUG
        } else {
            LevelFilter::INFO
        }
    }
}

/// One group argument.
///
/// `label=a,b,c` names the group, a leading `!` makes it required, and the
/// label defaults to the first word so that a result set is readable without
/// the caller having named anything.
fn parse_group(text: &str) -> anyhow::Result<Group> {
    let (required, rest) = match text.strip_prefix('!') {
        Some(rest) => (true, rest),
        None => (false, text),
    };

    // Split on the first `=` only, so a label is a label and everything after
    // it is words. A word containing `=` is not a word.
    let (label, list) = match rest.split_once('=') {
        Some((label, list)) => (Some(label.trim().to_string()), list),
        None => (None, rest),
    };

    let mut words = Vec::new();
    for word in list.split(',') {
        let word = word.trim();
        if word.is_empty() {
            continue;
        }
        words.push(word.to_string());
    }
    if words.is_empty() {
        anyhow::bail!("Group {text:?} has no words in it");
    }

    let label = match label {
        Some(label) if !label.is_empty() => label,
        _ => words[0].clone(),
    };
    Ok(Group {
        label,
        words,
        required,
    })
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
    let mut home = None;
    if let Some(value) = env::var_os(HOME_VARIABLE)
        && !value.is_empty()
    {
        home = Some(PathBuf::from(value));
    }
    if home.is_none() {
        // Unix: getpwuid_r. Windows: SHGetKnownFolderPath(FOLDERID_Profile).
        home = env::home_dir();
    }
    match home {
        Some(home) => home.join(DEFAULT_HOME_DIRECTORY),
        // Neither the environment nor the OS gave us anything; fall back to a
        // path relative to the working directory so `--help` still renders.
        None => PathBuf::from(DEFAULT_HOME_DIRECTORY),
    }
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
         \n    borhan --home {home} init\n\
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
    let settings = CommandLine::parse();

    let level = settings.logging_level();
    let show_target = matches!(level, LevelFilter::DEBUG | LevelFilter::TRACE);
    let show_location = level == LevelFilter::TRACE;
    fmt::Subscriber::builder()
        .with_max_level(level)
        .json()
        .flatten_event(true)
        .with_span_events(FmtSpan::CLOSE)
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

    match settings.command {
        Command::Init { command } => match command.unwrap_or(InitCommand::Storage) {
            InitCommand::Storage => {
                let existing = storage.is_dir();
                fs::create_dir_all(&storage)
                    .with_context(|| format!("Could not create {storage:?}"))?;
                tracing::info!(msg = "Initialized storage", directory = ?storage, existing = existing);
                if existing {
                    println!("Already initialized: {}", storage.display());
                } else {
                    println!("Initialized {}", storage.display());
                }
                Ok(())
            }

            InitCommand::Server { listen, token } => {
                let server_configuration = settings.home.join(SERVER_CONFIGURATION);
                if server_configuration.is_file() {
                    anyhow::bail!(
                        "{server_configuration:?} already exists. Remove it before writing \
                         another listen address."
                    );
                }
                let server = Server {
                    listen: Some(listen.clone()),
                    token,
                };
                let configuration = match toml_edit::ser::to_string_pretty(&server) {
                    Ok(configuration) => configuration,
                    Err(error) => {
                        return Err(anyhow::Error::new(error)
                            .context("Could not render the server configuration"));
                    }
                };

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
                    msg = "Initialized server configuration",
                    configuration = ?server_configuration,
                    listen = listen,
                    token = server.token.is_some()
                );
                println!(
                    "Initialized {} listening at {}",
                    server_configuration.display(),
                    listen
                );
                Ok(())
            }
        },

        Command::Serve => {
            check_storage(&settings.home, &storage, "serve")?;

            let server_configuration = settings.home.join(SERVER_CONFIGURATION);
            let mut server = Server::default();
            if server_configuration.is_file() {
                server = read_configuration(&server_configuration)?;
                tracing::debug!(msg = "Read server configuration", configuration = ?server_configuration);
            }
            let address = match server.listen {
                Some(listen) => listen,
                None => DEFAULT_LISTEN_ADDRESS.to_string(),
            };

            let router =
                crate::api::router(crate::api::App::new(storage.clone(), server.token.clone()));
            let listener = tokio::net::TcpListener::bind(&address)
                .await
                .with_context(|| format!("Could not listen on {address}"))?;
            tracing::info!(
                msg = "Started HTTP server",
                address = address,
                storage = ?storage,
                token = server.token.is_some()
            );
            axum::serve(listener, router)
                .await
                .context("HTTP server stopped")?;
            Ok(())
        }

        Command::Memory { command } => {
            let origin = probe_server(&settings.home)?;
            let command = command.unwrap_or(MemoryCommand::List { json: false });
            match command {
                MemoryCommand::Create {
                    name,
                    description,
                    languages,
                    json,
                } => {
                    if let Some((origin, token)) = &origin {
                        let response = request(
                            origin,
                            token.as_deref(),
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
                        let (id, stats) =
                            crate::api::create(&storage, &name, &description, &languages)?;
                        if json {
                            print_pretty(&crate::api::id_json(&id, &stats))?;
                        } else {
                            println!("{id}");
                        }
                    }
                    Ok(())
                }

                MemoryCommand::List { json } => {
                    if let Some((origin, token)) = &origin {
                        let response =
                            request(origin, token.as_deref(), "GET", "/api/v1/memory_list", None)?;
                        print_http(&response, json, print_memory_list_json)?;
                    } else {
                        check_storage(&settings.home, &storage, "memory list")?;
                        let (memories, stats) = crate::api::list(&storage)?;
                        if json {
                            print_pretty(&crate::api::list_json(&memories, &stats))?;
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
                    if let Some((origin, token)) = &origin {
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
                            origin,
                            token.as_deref(),
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
                        let (id, stats) = crate::api::update(
                            &store,
                            &name,
                            description.as_deref(),
                            languages.as_deref(),
                        )?;
                        if json {
                            print_pretty(&crate::api::id_json(&id, &stats))?;
                        } else {
                            println!("{id}");
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
                    if let Some((origin, token)) = &origin {
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
                            origin,
                            token.as_deref(),
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
                        let (written, stats) =
                            crate::api::add(&store, &built, &writer, &entry, &name)?;
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

                MemoryCommand::Get { name, ids, json } => {
                    if ids.is_empty() {
                        anyhow::bail!("Pass at least one unit ULID to read back");
                    }
                    if let Some((origin, token)) = &origin {
                        let response = request(
                            origin,
                            token.as_deref(),
                            "POST",
                            &format!("/api/v1/memory/{name}/unit_list"),
                            Some(serde_json::json!({ "id_list": ids })),
                        )?;
                        print_http(&response, json, print_units_json)?;
                    } else {
                        check_storage(&settings.home, &storage, "memory get ...")?;
                        let store = Storage::open(&storage, &name)?;
                        let mut units = Vec::new();
                        for id in &ids {
                            match Ulid::parse(id) {
                                Ok(id) => units.push(id),
                                Err(error) => {
                                    return Err(anyhow::Error::new(error)
                                        .context(format!("{id:?} is not a ULID")));
                                }
                            }
                        }
                        let (located, stats) = crate::api::get(&store, &units)?;
                        for id in &units {
                            if !located.iter().any(|row| row.unit == *id) {
                                eprintln!("No unit {id}");
                            }
                        }
                        if json {
                            print_pretty(&crate::api::get_json(&located, &stats))?;
                        } else {
                            for row in &located {
                                println!(
                                    "{}  {}  {}  unit {}",
                                    row.unit,
                                    row.role.as_str(),
                                    row.session_ref,
                                    row.unit_seq
                                );
                                println!("{}", row.text());
                                println!();
                            }
                        }
                    }
                    Ok(())
                }

                MemoryCommand::Search {
                    name,
                    groups,
                    limit,
                    max_per_message,
                    session,
                    after,
                    before,
                    roles,
                    json,
                } => {
                    let mut parsed = Vec::new();
                    for group in &groups {
                        parsed.push(parse_group(group)?);
                    }
                    if let Some((origin, token)) = &origin {
                        let mut group_list = Vec::new();
                        for group in &parsed {
                            group_list.push(serde_json::json!({
                                "label": group.label,
                                "word_list": group.words,
                                "required": group.required,
                            }));
                        }
                        let mut payload = serde_json::json!({
                            "group_list": group_list,
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
                            origin,
                            token.as_deref(),
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
                        let (outcome, stats) = crate::api::search(
                            &store,
                            &built,
                            &name,
                            &parsed,
                            &filter,
                            limit,
                            max_per_message,
                        )?;
                        if json {
                            print_pretty(&crate::api::search_json(&outcome, &stats))?;
                        } else {
                            print_search(&outcome);
                        }
                    }
                    Ok(())
                }

                MemoryCommand::Cursor {
                    name,
                    cursor,
                    before,
                    after,
                    json,
                } => {
                    if let Some((origin, token)) = &origin {
                        let path = format!(
                            "/api/v1/memory/{name}/cursor/{cursor}?before={before}&after={after}"
                        );
                        let response = request(origin, token.as_deref(), "GET", &path, None)?;
                        print_http(&response, json, print_cursor_json)?;
                    } else {
                        check_storage(&settings.home, &storage, "memory cursor ...")?;
                        let unit = match Ulid::parse(&cursor) {
                            Ok(unit) => unit,
                            Err(error) => {
                                return Err(anyhow::Error::new(error).context(format!(
                                    "{cursor:?} is not a cursor from a search hit"
                                )));
                            }
                        };
                        let store = Storage::open(&storage, &name)?;
                        let (messages, stats) =
                            crate::api::cursor(&store, &name, unit, before, after)?;
                        if json {
                            print_pretty(&crate::api::cursor_json(&messages, &stats))?;
                        } else {
                            for message in &messages {
                                let mark = if message.anchor { "→" } else { " " };
                                println!(
                                    "{mark} {}  {}  {}  seq {}",
                                    message.id,
                                    message.author,
                                    message.role.as_str(),
                                    message.seq
                                );
                                println!("{}", message.body);
                                println!();
                            }
                        }
                    }
                    Ok(())
                }

                MemoryCommand::Lexicon { name, words, json } => {
                    if words.is_empty() {
                        anyhow::bail!("Pass at least one word to look up");
                    }
                    if let Some((origin, token)) = &origin {
                        let response = request(
                            origin,
                            token.as_deref(),
                            "POST",
                            &format!("/api/v1/memory/{name}/lexicon"),
                            Some(serde_json::json!({ "word_list": words })),
                        )?;
                        print_http(&response, json, print_lexicon_json)?;
                    } else {
                        check_storage(&settings.home, &storage, "memory lexicon ...")?;
                        let store = Storage::open(&storage, &name)?;
                        let built = Index::open(&store)?;
                        let (rows, stats) = crate::api::lexicon(&built, &name, &words)?;
                        if json {
                            print_pretty(&crate::api::lexicon_json(&rows, &stats))?;
                        } else {
                            for row in &rows {
                                println!(
                                    "{}  surface={} ({})  lemma={} ({})  context=({})",
                                    row.word,
                                    row.surface,
                                    row.surface_units,
                                    row.lemma,
                                    row.lemma_units,
                                    row.context_units
                                );
                            }
                        }
                    }
                    Ok(())
                }

                MemoryCommand::Rescan { name, json } => {
                    if let Some((origin, token)) = &origin {
                        let response = request(
                            origin,
                            token.as_deref(),
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
                        let built = Index::attach(&store.directory.join(crate::index::DIRECTORY))?;
                        let writer = built.writer()?;
                        let store = Mutex::new(store);
                        let writer = Mutex::new(writer);
                        let (messages, units, stats) =
                            crate::api::rescan(&store, &built, &writer, &name)?;
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
    }
}

fn probe_server(home: &Path) -> anyhow::Result<Option<(String, Option<String>)>> {
    let path = home.join(SERVER_CONFIGURATION);
    if !path.is_file() {
        tracing::debug!(msg = "Running against local storage");
        return Ok(None);
    }
    let server: Server = read_configuration(&path)?;
    let Some(listen) = server.listen else {
        tracing::debug!(msg = "server.toml has no listen, using local storage");
        return Ok(None);
    };
    let origin = format!("http://{listen}");
    let url = format!("{origin}/api/v1/health");
    let mut req = ureq::get(&url);
    req = req.timeout(Duration::from_secs(1));
    if let Some(token) = &server.token {
        req = req.set("Authorization", &format!("Bearer {token}"));
    }
    match req.call() {
        Ok(response) if response.status() == 200 => {
            tracing::debug!(msg = "Using HTTP server", origin = origin.as_str());
            Ok(Some((origin, server.token)))
        }
        Ok(response) => {
            tracing::debug!(
                msg = "Server health was not 200, using local storage",
                status = response.status()
            );
            Ok(None)
        }
        Err(error) => {
            tracing::debug!(msg = "Server did not respond, using local storage", error = %error);
            Ok(None)
        }
    }
}

struct ClientResponse {
    version: Option<String>,
    server: Option<String>,
    trace: Option<String>,
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

fn print_http_headers(response: &ClientResponse) {
    if let Some(server) = &response.server {
        eprintln!("Server: {server}");
    }
    if let Some(version) = &response.version {
        eprintln!("X-Borhan-Version: {version}");
    }
    if let Some(trace) = &response.trace {
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
    print_http_headers(response);
    print_text(&response.body)
}

fn request(
    origin: &str,
    token: Option<&str>,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> anyhow::Result<ClientResponse> {
    let url = format!("{origin}{path}");
    let mut req = ureq::request(method, &url);
    req = req.timeout(Duration::from_secs(120));
    if let Some(token) = token {
        req = req.set("Authorization", &format!("Bearer {token}"));
    }
    let response = match body {
        Some(body) => req.send_json(body),
        None => req.call(),
    };
    match response {
        Ok(response) => {
            let version = response.header("x-borhan-version").map(str::to_string);
            let server = response.header("server").map(str::to_string);
            let trace = response.header("x-trace-id").map(str::to_string);
            if let Some(trace) = &trace {
                tracing::debug!(msg = "Server trace", trace = trace.as_str());
            }
            let body = match response.into_json::<serde_json::Value>() {
                Ok(value) => value,
                Err(error) => return Err(anyhow::Error::new(error).context("Could not read JSON")),
            };
            Ok(ClientResponse {
                version,
                server,
                trace,
                body,
            })
        }
        Err(ureq::Error::Status(code, response)) => {
            let version = response.header("x-borhan-version").map(str::to_string);
            let body = match response.into_json::<serde_json::Value>() {
                Ok(value) => value,
                Err(_) => serde_json::json!({}),
            };
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
        Err(error) => Err(anyhow::Error::new(error).context(format!("{method} {url}"))),
    }
}

fn print_memory_list(memories: &[crate::storage::Memory]) {
    if memories.is_empty() {
        eprintln!("No memories yet — `borhan memory create <name>`.");
        return;
    }
    let mut widths = (0, 0, 0);
    let mut rows = Vec::new();
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
        let summary = match &memory.description {
            Some(description) => {
                let line = description.lines().next().unwrap_or("");
                if line.chars().count() > SUMMARY_LIMIT {
                    let cut: String = line.chars().take(SUMMARY_LIMIT).collect::<String>();
                    format!("{cut}…")
                } else {
                    line.to_string()
                }
            }
            None => String::new(),
        };
        widths.0 = widths.0.max(memory.name.len());
        widths.1 = widths.1.max(counts.len());
        widths.2 = widths.2.max(memory.languages.len());
        rows.push((
            memory.id.to_string(),
            created,
            memory.name.clone(),
            counts,
            memory.languages.clone(),
            summary,
        ));
    }
    for (id, created, name, counts, languages, summary) in rows {
        println!(
            "{id}  {created}  {name:<0$}  {counts:<1$}  {languages:<2$}  {summary}",
            widths.0, widths.1, widths.2
        );
    }
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
        memories.push(crate::storage::Memory {
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

fn print_units_json(body: &serde_json::Value) -> anyhow::Result<()> {
    let Some(list) = body.get("unit_list").and_then(|value| value.as_array()) else {
        anyhow::bail!("server response has no unit_list");
    };
    for row in list {
        println!(
            "{}  {}  {}  unit {}",
            row["unit"].as_str().unwrap_or(""),
            row["role"].as_str().unwrap_or(""),
            row["session_ref"].as_str().unwrap_or(""),
            row["unit_seq"].as_i64().unwrap_or(0)
        );
        println!("{}", row["text"].as_str().unwrap_or(""));
        println!();
    }
    Ok(())
}

fn print_search(outcome: &crate::search::Outcome) {
    for unknown in &outcome.unknown {
        eprintln!(
            "unknown: {:?} (group {:?}) matched nothing",
            unknown.word, unknown.group
        );
    }
    if outcome.hits.is_empty() {
        eprintln!("No hits.");
    }
    const HEADER: [&str; 6] = ["score", "cover", "cursor", "session", "message", "size"];
    let mut rows: Vec<[String; 6]> = Vec::new();
    for hit in &outcome.hits {
        rows.push([
            format!("{:.3}", hit.score),
            format!("{}/{}", hit.coverage.0, hit.coverage.1),
            hit.cursor.to_string(),
            hit.session.to_string(),
            hit.message.as_deref().unwrap_or("-").to_string(),
            format!("{} words", hit.words),
        ]);
    }
    let mut widths = HEADER.map(|title| title.len());
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let lay = |cells: &[String; 6], last: &str| {
        let mut line = String::new();
        for (cell, width) in cells.iter().zip(widths) {
            line.push_str(&format!("{cell:<width$}  "));
        }
        line.push_str(last);
        line
    };
    if !rows.is_empty() {
        let titles = HEADER.map(|title| title.to_string());
        eprintln!("{}", lay(&titles, "matched"));
    }
    for (row, hit) in rows.iter().zip(&outcome.hits) {
        println!("{}", lay(row, &format!("[{}]", hit.matched.join(","))));
        println!("\"{}\"", preview(&hit.snippet));
        println!();
    }
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
                "unknown: {:?} (group {:?}) matched nothing",
                item["word"].as_str().unwrap_or(""),
                item["group"].as_str().unwrap_or("")
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
    const HEADER: [&str; 6] = ["score", "cover", "cursor", "session", "message", "size"];
    let mut rows: Vec<[String; 6]> = Vec::new();
    let mut matched_list = Vec::new();
    let mut snippets = Vec::new();
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
        rows.push([
            format!("{:.3}", hit["score"].as_f64().unwrap_or(0.0)),
            cover,
            hit["cursor"].as_str().unwrap_or("").to_string(),
            hit["session"].as_str().unwrap_or("").to_string(),
            hit["message"].as_str().unwrap_or("-").to_string(),
            format!("{} words", hit["words"].as_u64().unwrap_or(0)),
        ]);
        let mut matched = Vec::new();
        if let Some(list) = hit["matched_list"].as_array() {
            for label in list {
                if let Some(label) = label.as_str() {
                    matched.push(label.to_string());
                }
            }
        }
        matched_list.push(matched);
        snippets.push(hit["snippet"].as_str().unwrap_or("").to_string());
    }
    let mut widths = HEADER.map(|title| title.len());
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let lay = |cells: &[String; 6], last: &str| {
        let mut line = String::new();
        for (cell, width) in cells.iter().zip(widths) {
            line.push_str(&format!("{cell:<width$}  "));
        }
        line.push_str(last);
        line
    };
    let titles = HEADER.map(|title| title.to_string());
    eprintln!("{}", lay(&titles, "matched"));
    for (at, row) in rows.iter().enumerate() {
        println!("{}", lay(row, &format!("[{}]", matched_list[at].join(","))));
        println!("\"{}\"", preview(&snippets[at]));
        println!();
    }
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

fn print_cursor_json(body: &serde_json::Value) -> anyhow::Result<()> {
    let Some(list) = body.get("message_list").and_then(|value| value.as_array()) else {
        anyhow::bail!("server response has no message_list");
    };
    for message in list {
        let mark = if message["anchor"].as_bool().unwrap_or(false) {
            "→"
        } else {
            " "
        };
        println!(
            "{mark} {}  {}  {}  seq {}",
            message["message"].as_str().unwrap_or(""),
            message["author"].as_str().unwrap_or(""),
            message["role"].as_str().unwrap_or(""),
            message["seq"].as_i64().unwrap_or(0)
        );
        println!("{}", message["body"].as_str().unwrap_or(""));
        println!();
    }
    Ok(())
}

fn print_lexicon_json(body: &serde_json::Value) -> anyhow::Result<()> {
    let Some(list) = body.get("word_list").and_then(|value| value.as_array()) else {
        anyhow::bail!("server response has no word_list");
    };
    for row in list {
        println!(
            "{}  surface={} ({})  lemma={} ({})  context=({})",
            row["word"].as_str().unwrap_or(""),
            row["surface"].as_str().unwrap_or(""),
            row["surface_units"].as_u64().unwrap_or(0),
            row["lemma"].as_str().unwrap_or(""),
            row["lemma_units"].as_u64().unwrap_or(0),
            row["context_units"].as_u64().unwrap_or(0)
        );
    }
    Ok(())
}
