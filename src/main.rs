mod index;
mod normalize;
mod search;
mod storage;
mod ulid;

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
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
//       remote.toml         Present => this instance is a client of a server.
//       server.toml         Read by `serve`. Absent => every default applies.
//
// None of it is created implicitly: `init` is the only thing that writes the
// layout, so a missing directory always means "this machine was never set up",
// never "it was set up somewhere you did not look".
const STORAGE_DIRECTORY: &str = "storage";
const REMOTE_CONFIGURATION: &str = "remote.toml";
const SERVER_CONFIGURATION: &str = "server.toml";

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

    /// Point this instance at a running server by writing `remote.toml`.
    Remote {
        /// `HOST:PORT` of the running `borhan serve`.
        #[arg(long)]
        server: String,

        /// Token that server expects.
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

        /// Longer text, up to 2000 characters. Shown to the calling model, so
        /// it should say what is in here and what is not.
        #[arg(long)]
        description: Option<String>,

        /// Comma-separated language tags, like `fa,en`. Reported by `memory
        /// list` so a caller composing a query knows which languages a concept
        /// group is worth expanding into.
        #[arg(long, default_value = "fa,en")]
        languages: String,
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
    List,

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

        /// Emit one JSON object holding `hits`, `unknown` and `hints` instead
        /// of the table. All of it goes to standard output.
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
    },

    /// Split every stored message again and rebuild the index from scratch.
    ///
    /// The supported way to change the normalizer or the splitter: change it,
    /// bump the version, run this. Nothing is lost, because everything this
    /// destroys was derived from the messages, which are not touched.
    Rescan {
        /// The memory to rebuild.
        name: String,
    },
}

/// `<home>/remote.toml`. Present means this instance is a client.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Remote {
    /// `HOST:PORT` of the running server.
    pub server: String,

    /// Presented to the server when it is configured with a token. Parsed but
    /// not sent yet: no command routes to the server so far.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

/// `<home>/server.toml`, read by `serve` only. Absent means every default.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    /// Address to bind. [`DEFAULT_LISTEN_ADDRESS`] when unset.
    pub listen: Option<String>,

    /// Token clients must present. Parsed but not enforced yet: the only route
    /// is a placeholder, and there is nothing worth protecting behind it.
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
    let remote_configuration = settings.home.join(REMOTE_CONFIGURATION);

    // Presence is the switch, so the existence check comes first and tanzim only
    // runs when there is a file to read.
    let mut remote = None;
    if remote_configuration.is_file() {
        let configuration: Remote = read_configuration(&remote_configuration)?;
        tracing::debug!(
            msg = "Running as a client",
            configuration = ?remote_configuration,
            server = configuration.server,
            token = configuration.token.is_some()
        );
        remote = Some(configuration);
    } else {
        tracing::debug!(msg = "Running against local storage", directory = ?storage);
    }

    match settings.command {
        Command::Init { command } => match command.unwrap_or(InitCommand::Storage) {
            InitCommand::Storage => {
                if let Some(remote) = remote {
                    anyhow::bail!(
                        "{remote_configuration:?} makes this instance a client of {}, and a \
                         client owns no storage — there is nothing here to initialize. \
                         Initialize the machine running `borhan serve` instead, or remove \
                         {remote_configuration:?} to keep memories in {storage:?} locally.",
                        remote.server
                    );
                }

                // The whole of making a storage, now that a memory is a
                // directory rather than a table: there is nothing to shape
                // ahead of time, so this is a mkdir and the announcement that
                // it happened.
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

            InitCommand::Remote { server, token } => {
                if let Some(remote) = remote {
                    anyhow::bail!(
                        "{remote_configuration:?} already points this instance at {}. Remove it \
                         before pointing it at another server.",
                        remote.server
                    );
                }
                // TODO: reach the server before writing anything — hit
                // `http://{server}/api/v1/auth` with the token and require a 201
                // back, so a wrong address or a rejected token fails here rather
                // than at the first command that needs the server. Nothing in
                // the tree makes client-side HTTP requests yet, so writing the
                // file is all this does for now.
                let remote = Remote { server, token };
                let configuration = match toml_edit::ser::to_string_pretty(&remote) {
                    Ok(configuration) => configuration,
                    // Two strings; there is no shape here that TOML cannot hold.
                    Err(error) => {
                        return Err(anyhow::Error::new(error)
                            .context("Could not render the remote configuration"));
                    }
                };

                fs::create_dir_all(&settings.home).with_context(|| {
                    format!("Could not create home directory {:?}", settings.home)
                })?;
                // `create_new` refuses to clobber an existing file, and the unix
                // mode keeps the token out of a world-readable file from the
                // moment it exists rather than a chmod later.
                let mut options = fs::OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                let mut file = options
                    .open(&remote_configuration)
                    .with_context(|| format!("Could not create {remote_configuration:?}"))?;
                file.write_all(configuration.as_bytes())
                    .with_context(|| format!("Could not write {remote_configuration:?}"))?;

                tracing::info!(
                    msg = "Initialized remote",
                    configuration = ?remote_configuration,
                    server = remote.server,
                    token = remote.token.is_some()
                );
                println!(
                    "Initialized {} pointing at {}",
                    remote_configuration.display(),
                    remote.server
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
                axum::Router::new().route("/", axum::routing::get(|| async { "Hello, world!" }));
            let listener = tokio::net::TcpListener::bind(&address)
                .await
                .with_context(|| format!("Could not listen on {address}"))?;
            tracing::info!(
                msg = "Started HTTP server",
                address = address,
                storage = ?storage,
                // Nothing enforces it yet; logged so a misread server.toml is
                // visible before it matters.
                token = server.token.is_some()
            );
            axum::serve(listener, router)
                .await
                .context("HTTP server stopped")?;
            Ok(())
        }

        Command::Memory { command } => {
            if let Some(remote) = remote {
                // TODO: send this to the server instead of refusing — talk to
                // it over the HTTP API and print what it answers with. Every
                // memory command lands here, so this is the one place client
                // mode has to grow.
                anyhow::bail!(
                    "{remote_configuration:?} makes this instance a client of {}, and routing \
                     commands to a server is not implemented yet. Remove \
                     {remote_configuration:?} to work against {storage:?} locally.",
                    remote.server
                );
            }

            match command.unwrap_or(MemoryCommand::List) {
                MemoryCommand::Create {
                    name,
                    description,
                    languages,
                } => {
                    check_storage(&settings.home, &storage, "memory create ...")?;

                    tracing::debug!(
                        msg = "Creating memory",
                        name = name,
                        description = description.is_some()
                    );
                    let (store, id) =
                        Storage::create(&storage, &name, description.as_deref(), &languages)?;
                    // The index is made now rather than on the first `add`, so
                    // that a memory is either wholly there or wholly not.
                    let built = Index::attach(&store.directory.join(index::DIRECTORY))?;
                    store.set_meta(index::VERSION_KEY, &normalize::VERSION.to_string())?;
                    store.set_meta(index::BUILT_KEY, &Ulid::now().to_string())?;
                    drop(built);

                    tracing::info!(msg = "Created memory", ulid = %id, name = name);
                    println!("{id}");
                    Ok(())
                }

                MemoryCommand::List => {
                    check_storage(&settings.home, &storage, "memory list")?;

                    let memories = Storage::list(&storage)?;
                    tracing::info!(msg = "Listed memories", count = memories.len());
                    if memories.is_empty() {
                        eprintln!("No memories yet — `borhan memory create <name>`.");
                        return Ok(());
                    }

                    let mut widths = (0, 0, 0);
                    let mut rows = Vec::new();
                    for memory in &memories {
                        let created = chrono::DateTime::from_timestamp_millis(memory.created_at);
                        let created = match created {
                            Some(created) => {
                                created.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                            }
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
                                    let cut: String =
                                        line.chars().take(SUMMARY_LIMIT).collect::<String>();
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
                } => {
                    check_storage(&settings.home, &storage, "memory add ...")?;

                    let Some(role) = Role::parse(&role) else {
                        anyhow::bail!("Role {role:?} is not one of user, assistant or tool");
                    };
                    let mut store = Storage::open(&storage, &name)?;
                    let built = Index::open(&store)?;

                    let ts = match ts {
                        Some(ts) => ts,
                        None => Ulid::now(),
                    };
                    let author = match &author {
                        Some(author) => author.as_str(),
                        None => role.as_str(),
                    };
                    tracing::debug!(
                        msg = "Adding message",
                        memory = name,
                        session = session,
                        bytes = text.len()
                    );

                    // Layer one first and on its own. If indexing fails after
                    // this, the message is still stored and `rescan` recovers
                    // it; the other order loses it.
                    let written = store.add(&Entry {
                        session: &session,
                        message: message.as_deref(),
                        author,
                        role,
                        ts,
                        body: &text,
                    })?;

                    let mut writer = built.writer()?;
                    built.add(&writer, &written, role.code(), ts, &text)?;
                    built.commit(&mut writer)?;

                    tracing::info!(
                        msg = "Added message",
                        memory = name,
                        ulid = %written.message,
                        seq = written.seq,
                        units = written.units.len()
                    );
                    eprintln!("{} units", written.units.len());
                    println!("{}", written.message);
                    Ok(())
                }

                MemoryCommand::Get { name, ids, json } => {
                    check_storage(&settings.home, &storage, "memory get ...")?;
                    if ids.is_empty() {
                        anyhow::bail!("Pass at least one unit ULID to read back");
                    }

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
                    let located = store.locate(&units)?;
                    for id in &units {
                        if !located.iter().any(|row| row.unit == *id) {
                            eprintln!("No unit {id}");
                        }
                    }

                    if json {
                        let mut array = Vec::new();
                        for row in &located {
                            array.push(serde_json::json!({
                                "unit": row.unit.to_string(),
                                "message": row.message.to_string(),
                                "message_ref": row.message_ref,
                                "session": row.session.to_string(),
                                "session_ref": row.session_ref,
                                "seq": row.seq,
                                "unit_seq": row.unit_seq,
                                "author": row.author,
                                "role": row.role.as_str(),
                                "ts": row.ts,
                                "text": row.text(),
                            }));
                        }
                        println!("{}", serde_json::Value::Array(array));
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
                    check_storage(&settings.home, &storage, "memory search ...")?;

                    let mut parsed = Vec::new();
                    for group in &groups {
                        parsed.push(parse_group(group)?);
                    }
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
                            anyhow::bail!("Role {role:?} is not one of user, assistant or tool");
                        };
                        filter.roles.push(role);
                    }

                    let store = Storage::open(&storage, &name)?;
                    let built = Index::open(&store)?;
                    tracing::debug!(
                        msg = "Searching",
                        memory = name,
                        groups = parsed.len(),
                        limit = limit
                    );
                    let outcome =
                        search::search(&store, &built, &parsed, &filter, limit, max_per_message)?;

                    // Logged before it is printed, so that the cursor call that
                    // follows a search has something to attach itself to.
                    let mut returned = Vec::new();
                    for hit in &outcome.hits {
                        returned.push(serde_json::json!({
                            "unit": hit.unit.to_string(),
                            "score": hit.score,
                            "coverage": [hit.coverage.0, hit.coverage.1],
                        }));
                    }
                    let asked = serde_json::to_string(&serde_json::Value::Array(
                        parsed
                            .iter()
                            .map(|group| {
                                serde_json::json!({
                                    "label": group.label,
                                    "words": group.words,
                                    "required": group.required,
                                })
                            })
                            .collect(),
                    ))?;
                    let logged = serde_json::to_string(&serde_json::Value::Array(returned))?;
                    store.log_search(&asked, &logged)?;

                    tracing::info!(
                        msg = "Searched",
                        memory = name,
                        hits = outcome.hits.len(),
                        unknown = outcome.unknown.len()
                    );

                    if json {
                        let mut array = Vec::new();
                        for hit in &outcome.hits {
                            array.push(serde_json::json!({
                                "cursor": hit.cursor,
                                "unit": hit.unit.to_string(),
                                "score": hit.score,
                                "raw": hit.raw,
                                "coverage": [hit.coverage.0, hit.coverage.1],
                                "matched": hit.matched,
                                "session": hit.session,
                                "message": hit.message,
                                "author": hit.author,
                                "role": hit.role.as_str(),
                                "ts": hit.ts,
                                "words": hit.words,
                                "snippet": hit.snippet,
                            }));
                        }
                        let unknown: Vec<serde_json::Value> = outcome
                            .unknown
                            .iter()
                            .map(|unknown| {
                                serde_json::json!({
                                    "group": unknown.group,
                                    "word": unknown.word,
                                })
                            })
                            .collect();
                        let hints: Vec<serde_json::Value> = outcome
                            .hints
                            .iter()
                            .map(|(term, df)| serde_json::json!({"term": term, "units": df}))
                            .collect();
                        println!(
                            "{}",
                            serde_json::json!({
                                "hits": array,
                                "unknown": unknown,
                                "hints": hints,
                            })
                        );
                        return Ok(());
                    }

                    for unknown in &outcome.unknown {
                        eprintln!(
                            "unknown: {:?} (group {:?}) matched nothing",
                            unknown.word, unknown.group
                        );
                    }
                    if outcome.hits.is_empty() {
                        eprintln!("No hits.");
                    }

                    // Column widths are measured from the result set rather
                    // than fixed, because a session and a message are whatever
                    // the feeder decided to call them, and a header that does
                    // not sit over the column it names is worse than no header.
                    //
                    // The header goes to stderr, where everything else this
                    // command says *about* its answer already goes — the
                    // unknown words, `No hits.`, the vocabulary line. Standard
                    // output stays hits and nothing else, so `head -n 1 | awk
                    // '{print $3}'` still means "the cursor of the top hit".
                    const HEADER: [&str; 6] =
                        ["score", "cover", "cursor", "session", "message", "size"];
                    let rows: Vec<[String; 6]> = outcome
                        .hits
                        .iter()
                        .map(|hit| {
                            [
                                format!("{:.3}", hit.score),
                                format!("{}/{}", hit.coverage.0, hit.coverage.1),
                                hit.cursor.to_string(),
                                hit.session.to_string(),
                                hit.message.as_deref().unwrap_or("-").to_string(),
                                format!("{} words", hit.words),
                            ]
                        })
                        .collect();
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
                        // Quoted so that the sentence is one selectable run:
                        // a double-click takes a word and a triple-click takes
                        // the line, but the quotes are what make the boundary
                        // of the text visible when it ends in whitespace or
                        // starts with a dash.
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
                    Ok(())
                }

                MemoryCommand::Cursor {
                    name,
                    cursor,
                    before,
                    after,
                    json,
                } => {
                    check_storage(&settings.home, &storage, "memory cursor ...")?;

                    let unit = match Ulid::parse(&cursor) {
                        Ok(unit) => unit,
                        Err(error) => {
                            return Err(anyhow::Error::new(error)
                                .context(format!("{cursor:?} is not a cursor from a search hit")));
                        }
                    };
                    let store = Storage::open(&storage, &name)?;
                    let messages = store.around(unit, before, after)?;
                    // The label this produces is the whole reason the log
                    // exists: the caller reaching for a hit is the caller
                    // telling us that hit was the right one.
                    store.log_expansion(unit)?;

                    tracing::info!(
                        msg = "Expanded a cursor",
                        memory = name,
                        unit = %unit,
                        messages = messages.len()
                    );

                    if json {
                        let mut array = Vec::new();
                        for message in &messages {
                            array.push(serde_json::json!({
                                "message": message.id.to_string(),
                                "message_ref": message.reference,
                                "seq": message.seq,
                                "author": message.author,
                                "role": message.role.as_str(),
                                "ts": message.ts,
                                "anchor": message.anchor,
                                "body": message.body,
                            }));
                        }
                        println!("{}", serde_json::Value::Array(array));
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
                    Ok(())
                }

                MemoryCommand::Lexicon { name, words } => {
                    check_storage(&settings.home, &storage, "memory lexicon ...")?;
                    if words.is_empty() {
                        anyhow::bail!("Pass at least one word to look up");
                    }

                    let store = Storage::open(&storage, &name)?;
                    let built = Index::open(&store)?;
                    let searcher = built.reader.searcher();

                    for word in &words {
                        let segmented = normalize::segment(word);
                        let script = match segmented.first() {
                            Some(word) => word.script,
                            None => normalize::Script::Other,
                        };
                        let surface = normalize::surface(word);
                        let lemma = normalize::lemma(word, script);

                        let counts = [
                            (built.fields.surface, surface.as_str()),
                            (built.fields.lemma, lemma.as_str()),
                            (built.fields.context, lemma.as_str()),
                        ];
                        let mut frequencies = [0u64; 3];
                        for (at, (field, text)) in counts.iter().enumerate() {
                            if text.is_empty() {
                                continue;
                            }
                            let term = tantivy::Term::from_field_text(*field, text);
                            frequencies[at] = searcher.doc_freq(&term)?;
                        }
                        println!(
                            "{word}  surface={surface} ({})  lemma={lemma} ({})  context=({})",
                            frequencies[0], frequencies[1], frequencies[2]
                        );
                    }
                    Ok(())
                }

                MemoryCommand::Rescan { name } => {
                    check_storage(&settings.home, &storage, "memory rescan ...")?;

                    let mut store = Storage::open(&storage, &name)?;
                    // Deliberately not `Index::open`: this is the command whose
                    // entire job is to make a stale index current, so refusing
                    // to open a stale one here would leave no way out.
                    let built = Index::attach(&store.directory.join(index::DIRECTORY))?;
                    let mut writer = built.writer()?;
                    built.clear(&mut writer)?;
                    built.commit(&mut writer)?;

                    let written = store.resplit()?;
                    let mut units = 0;
                    for (message, written) in &written {
                        let (body, role, ts) = store.message(*message)?;
                        built.add(&writer, written, role, ts, &body)?;
                        units += written.units.len();
                    }
                    built.commit(&mut writer)?;
                    store.set_meta(index::VERSION_KEY, &normalize::VERSION.to_string())?;
                    store.set_meta(index::BUILT_KEY, &Ulid::now().to_string())?;

                    tracing::info!(
                        msg = "Rescanned memory",
                        memory = name,
                        messages = written.len(),
                        units = units
                    );
                    eprintln!("{} messages, {units} units", written.len());
                    Ok(())
                }
            }
        }
    }
}
