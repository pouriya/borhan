mod embedding;
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

use crate::embedding::Embedding;
use crate::storage::Storage;
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
//       storage/      SQLite database and LanceDB tables. Created by `init`.
//       remote.toml   Present => this instance is a client of a running server.
//       server.toml   Read by `serve`. Absent => every default applies.
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

/// Words of a row's text shown on one line of `memory search` results, before
/// an ellipsis takes over.
///
/// Enough that a sentence and most paragraphs arrive whole, so the ranking can
/// be read without a second command, and short enough that a message — which is
/// a whole document — does not bury the hits under it. Whatever is shown keeps
/// the spacing it was stored with, with the newlines escaped, because a hit has
/// to stay one line for the columns beside it to line up.
const PREVIEW_WORDS: usize = 100;

/// How many times `--limit` a search asks LanceDB for, so that the rows thrown
/// away afterwards — past the distance floor, or the same text as a nearer hit
/// — come out of the surplus instead of out of the page. Three, because on a
/// corpus that repeats itself about one hit in eight was a duplicate, and three
/// times over covers far worse than that without asking for a page of vectors
/// nobody will look at.
const OVERFETCH: usize = 3;

#[derive(Debug, Clone, Parser)]
#[command(about, version, author)]
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

    /// Inspect and exercise embedding models.
    Embedding {
        #[command(subcommand)]
        command: EmbeddingCommand,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum InitCommand {
    /// Create the local storage. The default when `init` is given no subcommand.
    Storage {
        /// Model whose vectors this storage will hold; "default" is the one
        /// built into the binary. Each model gets its own LanceDB table, so
        /// re-running with a second model adds one beside the first rather than
        /// replacing it.
        #[arg(long, default_value = embedding::DEFAULT_MODEL)]
        model: String,
    },

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
        /// Stored, and used as a table name, with a `memory_` in front.
        name: String,

        /// Longer text, up to 2000 characters.
        #[arg(long)]
        description: Option<String>,
    },

    /// List the memories, oldest first. The default when `memory` is given no
    /// subcommand.
    List,

    /// Add a message to a memory, broken into paragraphs and sentences.
    ///
    /// The text is read as Markdown: a heading, a bullet, a table row and a
    /// fenced code block each become a paragraph, and prose is cut into
    /// sentences. Every piece is stored and embedded, so a search can answer at
    /// whichever grain the question was asked at.
    Add {
        /// The memory to file it under. It has to exist already.
        name: String,

        /// The text, as Markdown.
        text: String,

        /// Which layer to attach at. The splitting happens below it, so
        /// `paragraph` hangs new paragraphs off an existing message and
        /// `sentence` stores exactly what it is given.
        #[arg(long = "type", default_value = "message")]
        kind: String,

        /// The feeder's session identifier — a UUID, usually. Required for a
        /// message; a paragraph and a sentence take it off their parent. The
        /// first message of a session also writes the session row.
        #[arg(long)]
        session: Option<String>,

        /// The feeder's message identifier. Required for a message, and for a
        /// paragraph, which is how it finds the message it belongs to.
        #[arg(long)]
        message: Option<String>,

        /// The ULID of the paragraph a sentence belongs to, as `memory add
        /// --type paragraph` printed it. Required for a sentence.
        #[arg(long)]
        paragraph: Option<String>,

        /// user or assistant. Required for a message; inherited below it.
        #[arg(long)]
        role: Option<String>,

        /// Who that was: a model identifier, or the user's name.
        #[arg(long = "role-name")]
        role_name: Option<String>,

        /// "default" for the model built into the binary, or a model directory.
        /// It must be one this storage was initialized with.
        #[arg(long, default_value = embedding::DEFAULT_MODEL)]
        model: String,
    },

    /// Print rows, with their text put back together.
    ///
    /// Either by ULID, as many as you care to give it, in any order, printed
    /// back in the order asked — so the output of `memory search` pipes
    /// straight in and keeps its ranking.
    ///
    /// Or by walking, with `--session`, `--message` and `--paragraph`, which
    /// take the feeder's own identifiers and need no ULID at all. Each names
    /// something and gets back the layer under it: a session gives its
    /// messages, a message its paragraphs, a paragraph its sentences. That is
    /// how you read around a hit rather than only at it.
    Get {
        /// The memory the rows belong to.
        name: String,

        /// One or more 26-character ULIDs, as `memory search` printed them.
        /// A session's id is not one of them: a session stores no text.
        ids: Vec<String>,

        /// The session to walk, named as whatever fed borhan named it. On its
        /// own it gives that session's messages.
        #[arg(long)]
        session: Option<String>,

        /// The message to walk, within `--session`: gives its paragraphs.
        #[arg(long, requires = "session")]
        message: Option<String>,

        /// The paragraph to walk, by ULID: gives its sentences. Needs no
        /// session, since a ULID is already unique across the memory.
        #[arg(long)]
        paragraph: Option<String>,

        /// Which child to start at, counted from 0 in reading order. Past the
        /// end is an empty result, which is how you find out where the end is.
        #[arg(long, default_value_t = 0)]
        from: i64,

        /// How many to print. A window, because a session can hold a whole
        /// corpus and a message a whole document.
        #[arg(long, default_value_t = 20)]
        count: i64,

        /// Print a JSON array instead of text blocks: one object per row, with
        /// the cursor columns alongside the text.
        #[arg(long)]
        json: bool,
    },

    /// Print what in a memory is closest to a text.
    Search {
        /// The memory to search.
        name: String,

        /// What to search for.
        text: String,

        /// Only look at one layer: message or sentence. Without it a sentence
        /// and its message compete for the same places in the results.
        /// Paragraphs are not embedded and so are never found by a search,
        /// whatever this says; anything stored before that changed still is.
        #[arg(long = "type")]
        kind: Option<String>,

        /// How many to print.
        #[arg(long, default_value_t = 10)]
        limit: usize,

        /// Drop anything further than this. Distance, so 0 is identical and
        /// smaller is closer; without it a search returns its `--limit` rows
        /// however far away they are, and "nothing here matches" looks exactly
        /// like a good answer. What counts as far depends on the model and on
        /// what is stored, so there is no default worth guessing.
        #[arg(long)]
        max_distance: Option<f32>,

        /// "default" for the model built into the binary, or a model directory.
        /// Searching with a different model than you stored with finds nothing:
        /// the vectors are in another table.
        #[arg(long, default_value = embedding::DEFAULT_MODEL)]
        model: String,
    },
}

/// `<home>/remote.toml`.
///
/// Its *presence* is the mode switch: with the file there this instance owns no
/// storage of its own and talks to the `borhan serve` named by `server`;
/// without it, everything happens against `<home>/storage` locally.
///
/// Written by `init remote` and read on every startup, so the two directions go
/// through the same struct and cannot drift apart.
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

#[derive(Debug, Clone, Subcommand)]
pub enum EmbeddingCommand {
    /// Load a model from a directory and report what it found.
    Load {
        /// Directory holding config.json, tokenizer.json and model.safetensors.
        directory: PathBuf,
    },

    /// Embed a text and print the resulting vector.
    #[command(name = "do")]
    Do {
        /// "default" for the model built into the binary, or a model directory.
        #[arg(long, default_value = embedding::DEFAULT_MODEL)]
        model: String,

        /// The text to embed.
        text: String,
    },
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
        Command::Init { command } => match command.unwrap_or(InitCommand::Storage {
            model: embedding::DEFAULT_MODEL.to_string(),
        }) {
            InitCommand::Storage { model } => {
                if let Some(remote) = remote {
                    anyhow::bail!(
                        "{remote_configuration:?} makes this instance a client of {}, and a \
                         client owns no storage — there is nothing here to initialize. \
                         Initialize the machine running `borhan serve` instead, or remove \
                         {remote_configuration:?} to keep memories in {storage:?} locally.",
                        remote.server
                    );
                }
                // The model is loaded here because its width is what the vector
                // table is shaped by, and its name is what that table is called
                // — so `init` has to have the model in hand before it can make
                // one. Everything else about making a storage lives in
                // `Storage::initialize`, which is the single call below.
                tracing::debug!(msg = "Loading embedding model", model = model);
                let embedder = embedding::load(&model)?;
                let report =
                    Storage::initialize(&storage, embedder.name(), embedder.dimensions()).await?;
                tracing::info!(
                    msg = "Initialized storage",
                    directory = ?storage,
                    existing = report.existing,
                    model = embedder.name(),
                    dimensions = embedder.dimensions(),
                    vectors = report.vectors
                );

                if report.existing {
                    println!("Already initialized: {}", storage.display());
                } else {
                    println!("Initialized {}", storage.display());
                }
                if report.vectors {
                    println!(
                        "Added vectors for {} ({} dimensions)",
                        embedder.name(),
                        embedder.dimensions()
                    );
                } else {
                    println!(
                        "Already has vectors for {} ({} dimensions)",
                        embedder.name(),
                        embedder.dimensions()
                    );
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
                MemoryCommand::Create { name, description } => {
                    check_storage(&settings.home, &storage, "memory create ...")?;

                    tracing::debug!(
                        msg = "Creating memory",
                        name = name,
                        description = description.is_some()
                    );
                    let store = Storage::open(&storage)?;
                    let id = store.create(&name, description.as_deref())?;
                    tracing::info!(msg = "Created memory", ulid = %id, name = name);
                    println!("{id}");
                    Ok(())
                }

                MemoryCommand::List => {
                    check_storage(&settings.home, &storage, "memory list")?;

                    let store = Storage::open(&storage)?;
                    let memories = store.list()?;
                    tracing::info!(msg = "Listed memories", count = memories.len());
                    if memories.is_empty() {
                        // Nothing on stdout, so a pipe reading this sees an
                        // empty list rather than a sentence about one.
                        eprintln!("No memories in {}", storage.display());
                        return Ok(());
                    }

                    // Rendered first, then measured, then printed: the columns
                    // are as wide as what goes in them, and a number's width is
                    // not something to predict.
                    let mut rows = Vec::with_capacity(memories.len());
                    for memory in &memories {
                        let created =
                            match chrono::DateTime::from_timestamp_millis(memory.created_at) {
                                Some(created) => {
                                    created.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
                                }
                                // Only reachable for a row written by something
                                // other than `create`; show the number rather than
                                // refusing to list the rest of the table.
                                None => format!("{}ms", memory.created_at),
                            };
                        // First line only, cut to length: a description is up
                        // to 2000 characters and may hold newlines, and one
                        // memory has to stay one row.
                        let mut summary = String::new();
                        if let Some(description) = &memory.description
                            && let Some(first) = description.lines().next()
                        {
                            for character in first.chars().take(SUMMARY_LIMIT) {
                                summary.push(character);
                            }
                            // Bytes, not characters: `summary` is a prefix of
                            // `first`, which is a prefix of the whole
                            // description, so shorter means something was left
                            // out either way.
                            if summary.len() < first.len() || first.len() < description.len() {
                                summary.push('…');
                            }
                        }

                        // Rounded to whole percents, and they will not always
                        // add up to 100: `role` is not constrained by the
                        // schema, so a message written by something other than
                        // borhan can carry neither value and is counted in
                        // `messages` alone.
                        let counts = &memory.counts;
                        let share = if counts.messages == 0 {
                            // Nothing spoke, so there is no split to show. A
                            // pair of zero percents would read as a fact.
                            "-".to_string()
                        } else {
                            let percent =
                                |part: u64| (part * 200 + counts.messages) / (counts.messages * 2);
                            format!("{}%/{}%", percent(counts.assistant), percent(counts.user))
                        };
                        rows.push((
                            created,
                            memory.name.clone(),
                            counts.sessions.to_string(),
                            counts.messages.to_string(),
                            counts.paragraphs.to_string(),
                            counts.sentences.to_string(),
                            share,
                            summary,
                        ));
                    }

                    // Wide enough for the heading as well, since that is what
                    // the numbers line up under.
                    let mut name = "NAME".len();
                    let mut sessions = "SESS".len();
                    let mut messages = "MSG".len();
                    let mut paragraphs = "PARA".len();
                    let mut sentences = "SENT".len();
                    let mut share = "A/U".len();
                    for row in &rows {
                        // Names are ASCII by construction and so are the
                        // numbers, so their length in bytes is their width on
                        // screen and the columns line up.
                        if row.1.len() > name {
                            name = row.1.len();
                        }
                        if row.2.len() > sessions {
                            sessions = row.2.len();
                        }
                        if row.3.len() > messages {
                            messages = row.3.len();
                        }
                        if row.4.len() > paragraphs {
                            paragraphs = row.4.len();
                        }
                        if row.5.len() > sentences {
                            sentences = row.5.len();
                        }
                        if row.6.len() > share {
                            share = row.6.len();
                        }
                    }

                    // The heading goes to stderr for the same reason the "no
                    // memories" sentence does: stdout stays nothing but rows,
                    // so a pipe reads data and a terminal still gets told what
                    // the columns are.
                    eprintln!(
                        "{:26}  {:24}  {:name$}  {:>sessions$}  {:>messages$}  {:>paragraphs$}  \
                         {:>sentences$}  {:>share$}  DESCRIPTION",
                        "ULID", "CREATED", "NAME", "SESS", "MSG", "PARA", "SENT", "A/U"
                    );
                    for (index, memory) in memories.iter().enumerate() {
                        let row = &rows[index];
                        let line = format!(
                            "{}  {}  {:name$}  {:>sessions$}  {:>messages$}  {:>paragraphs$}  \
                             {:>sentences$}  {:>share$}  {}",
                            memory.id, row.0, row.1, row.2, row.3, row.4, row.5, row.6, row.7
                        );
                        println!("{}", line.trim_end());
                    }
                    Ok(())
                }

                MemoryCommand::Add {
                    name,
                    text,
                    kind,
                    session,
                    message,
                    paragraph,
                    role,
                    role_name,
                    model,
                } => {
                    check_storage(&settings.home, &storage, "memory add ...")?;
                    let kind = match storage::Kind::parse(&kind) {
                        Some(kind) => kind,
                        None => anyhow::bail!(
                            "Unknown type {kind:?}: a row is a message, a paragraph or a \
                             sentence. A session row is written for you, with the first \
                             message that names one."
                        ),
                    };
                    let role = match role {
                        Some(role) => match storage::Role::parse(&role) {
                            Some(role) => Some(role),
                            None => anyhow::bail!("Unknown role {role:?}: it is user or assistant"),
                        },
                        None => None,
                    };
                    let paragraph = match paragraph {
                        Some(paragraph) => Some(Ulid::parse(&paragraph)?),
                        None => None,
                    };

                    let store = Storage::open(&storage)?;
                    tracing::debug!(msg = "Loading embedding model", model = model);
                    let embedder = embedding::load(&model)?;
                    // Opened before anything is written, so that a storage
                    // holding no table for this model fails here rather than
                    // after the rows have landed.
                    let vectors = store.open_vectors(embedder.name()).await?;

                    // The rows first, because they are what mint the ids the
                    // vectors are filed under, and because splitting is where
                    // most of what can go wrong goes wrong. SQLite and LanceDB
                    // cannot be one transaction, so this order decides which
                    // way a crash between them breaks: rows with no vectors are
                    // invisible to search but still reachable by walking the
                    // cursor, while vectors with no rows would put ids into
                    // search results that resolve to nothing.
                    let rows = store.add(
                        &name,
                        &storage::Entry {
                            kind,
                            session,
                            message,
                            paragraph,
                            role,
                            role_name,
                            content: text,
                        },
                    )?;

                    // One call for the lot: model2vec is a lookup table, and
                    // the per-call cost is loading it, not the text. Rows too
                    // short to be worth a vector are left out here — they are
                    // written either way, and `add` has already said which.
                    let mut embedded = Vec::new();
                    let mut texts = Vec::new();
                    for row in &rows {
                        if row.embed {
                            embedded.push(row);
                            texts.push(row.text.clone());
                        }
                    }
                    let embeddings = embedder.embed(&texts);
                    if embeddings.len() != embedded.len() {
                        anyhow::bail!("Embedded {} of {} rows", embeddings.len(), embedded.len());
                    }
                    let mut written = Vec::with_capacity(embedded.len());
                    for (row, embedding) in embedded.iter().zip(embeddings) {
                        written.push(storage::Vector {
                            memory: name.clone(),
                            kind: row.kind,
                            id: row.id,
                            embedding,
                        });
                    }
                    vectors.add(&written).await?;

                    // Counted for the log and the summary line, since "added a
                    // message" says nothing about how much went in.
                    let mut paragraphs = 0;
                    let mut sentences = 0;
                    for row in &rows {
                        match row.kind {
                            storage::Kind::Paragraph => paragraphs += 1,
                            storage::Kind::Sentence => sentences += 1,
                            storage::Kind::Message => {}
                        }
                    }
                    tracing::info!(
                        msg = "Added to memory",
                        memory = name,
                        kind = kind.as_str(),
                        rows = rows.len(),
                        paragraphs = paragraphs,
                        sentences = sentences,
                        vectors = written.len(),
                        model = embedder.name()
                    );

                    // The top row's ULID on stdout and nothing else, so it
                    // pipes into the `--paragraph` of whatever goes in next.
                    // What it was broken into goes to stderr.
                    match rows.first() {
                        Some(row) => println!("{}", row.id),
                        // `add` returns at least the row it was asked for.
                        None => anyhow::bail!("Nothing was written"),
                    }
                    eprintln!(
                        "{} paragraph{}, {} sentence{}",
                        paragraphs,
                        if paragraphs == 1 { "" } else { "s" },
                        sentences,
                        if sentences == 1 { "" } else { "s" }
                    );
                    Ok(())
                }

                MemoryCommand::Get {
                    name,
                    ids,
                    session,
                    message,
                    paragraph,
                    from,
                    count,
                    json,
                } => {
                    check_storage(&settings.home, &storage, "memory get ...")?;

                    // Two ways in, and asking for both is asking for two
                    // different things at once. Checked here rather than with
                    // a clap group so the message can say what to do instead.
                    let walking = session.is_some() || paragraph.is_some();
                    if !ids.is_empty() && walking {
                        anyhow::bail!(
                            "Give ULIDs or give --session/--paragraph, not both: one reads \
                             the rows you name, the other reads what is under them."
                        );
                    }
                    if ids.is_empty() && !walking {
                        anyhow::bail!(
                            "Nothing to read: give one or more ULIDs, or --session NAME to \
                             walk a session, or --paragraph ULID to walk a paragraph."
                        );
                    }

                    // Parsed before the storage is opened: a typo in a ULID is
                    // the caller's mistake and should not read like the store
                    // is broken.
                    let mut parsed = Vec::with_capacity(ids.len());
                    for id in &ids {
                        parsed.push(Ulid::parse(id)?);
                    }
                    let paragraph = match &paragraph {
                        Some(paragraph) => Some(Ulid::parse(paragraph)?),
                        None => None,
                    };

                    let store = Storage::open(&storage)?;
                    let records = if walking {
                        store.walk(
                            &name,
                            session.as_deref(),
                            message.as_deref(),
                            paragraph,
                            from,
                            count,
                        )?
                    } else {
                        store.get(&name, &parsed)?
                    };
                    tracing::info!(
                        msg = "Read rows",
                        memory = name,
                        asked = parsed.len(),
                        found = records.len()
                    );

                    if json {
                        let mut array = Vec::with_capacity(records.len());
                        for record in &records {
                            let created =
                                match chrono::DateTime::from_timestamp_millis(record.created_at) {
                                    Some(created) => {
                                        created.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
                                    }
                                    None => record.created_at.to_string(),
                                };
                            array.push(serde_json::json!({
                                "id": record.id.to_string(),
                                "type": record.kind.as_str(),
                                "session": record.session,
                                "message": record.message,
                                "paragraph": record.paragraph.map(|id| id.to_string()),
                                "position": record.position,
                                "role": record.role.map(|role| role.as_str()),
                                "role_name": record.role_name,
                                "created_at": created,
                                "words": record.text.split_whitespace().count(),
                                "text": record.text,
                            }));
                        }
                        // An array even for one id, and an empty one when
                        // nothing resolved: whatever reads this should not have
                        // to branch on how many were asked for.
                        println!("{}", serde_json::Value::Array(array));
                    } else {
                        for (index, record) in records.iter().enumerate() {
                            // Blank line between rows, none before the first,
                            // so one id prints as one paragraph of text.
                            if index > 0 {
                                println!();
                            }
                            let mut location = record.session.clone();
                            if let Some(message) = &record.message {
                                location.push('/');
                                location.push_str(message);
                            }
                            let speaker = match (record.role, &record.role_name) {
                                (Some(role), Some(name)) => format!("{}/{name}", role.as_str()),
                                (Some(role), None) => role.as_str().to_string(),
                                (None, _) => String::from("-"),
                            };
                            println!(
                                "{}  {}  {}#{}  {}  {} words",
                                record.id,
                                record.kind.as_str(),
                                location,
                                record.position,
                                speaker,
                                record.text.split_whitespace().count()
                            );
                            println!("{}", record.text);
                        }
                    }

                    // Named one by one rather than counted: an id that resolved
                    // to nothing is either a typo or a vector that outlived its
                    // row, and both are things you want to see spelled out.
                    for id in &parsed {
                        let mut found = false;
                        for record in &records {
                            if record.id == *id {
                                found = true;
                            }
                        }
                        if !found {
                            eprintln!("No row {id} in memory {name:?}");
                        }
                    }
                    Ok(())
                }

                MemoryCommand::Search {
                    name,
                    text,
                    kind,
                    limit,
                    max_distance,
                    model,
                } => {
                    check_storage(&settings.home, &storage, "memory search ...")?;
                    let kind = match kind {
                        Some(kind) => match storage::Kind::parse(&kind) {
                            Some(kind) => Some(kind),
                            None => anyhow::bail!(
                                "Unknown type {kind:?}: a vector covers a message or a \
                                 sentence. Paragraphs and whole sessions are not embedded."
                            ),
                        },
                        None => None,
                    };

                    let store = Storage::open(&storage)?;
                    tracing::debug!(msg = "Loading embedding model", model = model);
                    let embedder = embedding::load(&model)?;
                    let query = match embedder.embed(&[text]).into_iter().next() {
                        Some(query) => query,
                        None => anyhow::bail!("Embedding produced no vector"),
                    };

                    let vectors = store.open_vectors(embedder.name()).await?;
                    // More than asked for, because two of the next steps throw
                    // rows away: the distance floor and the duplicate check.
                    // Asking for exactly `limit` and then dropping some of it
                    // is how a full page turns into four rows.
                    let mut hits = vectors
                        .search(&name, kind, &query, limit * OVERFETCH)
                        .await?;
                    tracing::info!(
                        msg = "Searched memory",
                        memory = name,
                        model = embedder.name(),
                        hits = hits.len()
                    );
                    if hits.is_empty() {
                        // Nothing on stdout, as in `memory list`. A search with
                        // no distance floor returns everything it has up to
                        // `limit`, so an empty result is not "nothing matched
                        // well enough" — there is nothing there at all.
                        let layer = match kind {
                            Some(kind) => format!(" of type {}", kind.as_str()),
                            None => String::new(),
                        };
                        eprintln!(
                            "Memory {name:?} has nothing{layer} stored under {}",
                            embedder.name()
                        );
                        return Ok(());
                    }
                    // Nearest first, said here rather than assumed: LanceDB
                    // hands back one batch per partition, and everything below
                    // — the floor, the duplicate check, the page — takes the
                    // first row it sees as the best one.
                    hits.sort_by(|left, right| left.distance.total_cmp(&right.distance));
                    if let Some(max) = max_distance {
                        hits.retain(|hit| hit.distance <= max);
                    }

                    // A hit is a ULID, and a ULID says nothing, so the rows go
                    // straight back out of SQLite. Only a sentence stores its
                    // own text; a paragraph and a message are put back together
                    // from the sentences under them, which is also exactly the
                    // string their vector was built from.
                    let mut parsed = Vec::with_capacity(hits.len());
                    for hit in &hits {
                        parsed.push(Ulid::parse(&hit.id)?);
                    }
                    let records = store.get(&name, &parsed)?;

                    // The same string twice is one answer, however many rows
                    // hold it — a corpus repeats a line of code, a licence
                    // header, a stock sentence, and each copy is its own row
                    // with its own vector and its own place in the ranking.
                    // The nearest copy is kept because the sort above already
                    // put it first.
                    let mut seen: Vec<&str> = Vec::new();
                    let mut ranked = Vec::with_capacity(limit);
                    for hit in &hits {
                        if ranked.len() == limit {
                            break;
                        }
                        let mut text = "";
                        for record in &records {
                            if record.id.to_string() == hit.id {
                                text = &record.text;
                            }
                        }
                        if seen.contains(&text) {
                            continue;
                        }
                        seen.push(text);
                        ranked.push(hit);
                    }
                    let hits = ranked;
                    if hits.is_empty() {
                        // Everything found was past the floor. Not the same as
                        // the memory being empty, so it does not say so.
                        eprintln!(
                            "Nothing in memory {name:?} is within {} of that",
                            match max_distance {
                                Some(max) => max.to_string(),
                                None => String::from("range"),
                            }
                        );
                        return Ok(());
                    }

                    // Rendered first, then measured, then printed, as in
                    // `memory list`: the session and message columns hold the
                    // feeder's own identifiers — a UUID, a filename — and their
                    // width is not something to predict.
                    //
                    // All four ids of a hit go out, so that the row says where
                    // it sits without a second query: whose session, which
                    // message of it, which paragraph, which sentence. A `-` is
                    // a layer the hit is above, not one that is missing — a
                    // paragraph has no sentence.
                    let mut rows = Vec::with_capacity(hits.len());
                    for hit in &hits {
                        let mut session = String::new();
                        let mut message = String::new();
                        let mut paragraph = String::from("-");
                        let mut sentence = String::from("-");
                        // Cut to the first words, because a message's text is
                        // the whole message and this is one row of a ranking.
                        // The count beside it is the honest size of what was
                        // matched, not of what is shown.
                        let mut preview = String::new();
                        let mut words = 0;
                        for record in &records {
                            if record.id.to_string() != hit.id {
                                continue;
                            }
                            session = record.session.clone();
                            message = match &record.message {
                                Some(message) => message.clone(),
                                // Only a row written by something other than
                                // `add`, which requires one for every layer.
                                None => String::from("-"),
                            };
                            if let Some(id) = record.paragraph {
                                paragraph = id.to_string();
                            }
                            if record.kind == storage::Kind::Sentence {
                                sentence = record.id.to_string();
                            }

                            // Walked by hand rather than through
                            // `split_whitespace`, because the spacing between
                            // the words is part of what is being shown: it is
                            // what tells a code block from a paragraph.
                            let mut end = None;
                            let mut inside = false;
                            for (offset, character) in record.text.char_indices() {
                                if character.is_whitespace() {
                                    inside = false;
                                    continue;
                                }
                                if inside {
                                    continue;
                                }
                                inside = true;
                                words += 1;
                                if words == PREVIEW_WORDS + 1 {
                                    end = Some(offset);
                                }
                            }
                            let shown = match end {
                                Some(end) => &record.text[..end],
                                None => &record.text,
                            };
                            // Escaped, not dropped: a hit has to stay one line
                            // for the columns to line up, and a code block that
                            // came back as prose would be a lie about what is
                            // stored.
                            for character in shown.trim().chars() {
                                match character {
                                    '\n' => preview.push_str("\\n"),
                                    '\r' => preview.push_str("\\r"),
                                    '\t' => preview.push_str("\\t"),
                                    character => preview.push(character),
                                }
                            }
                            if end.is_some() {
                                preview.push_str(" …");
                            }
                        }
                        rows.push((
                            session,
                            message,
                            paragraph,
                            sentence,
                            words.to_string(),
                            preview,
                        ));
                    }

                    // Wide enough for the heading as well, since that is what
                    // the values line up under. Characters, not bytes: a
                    // feeder's identifiers are its own and need not be ASCII.
                    let mut session = "session".len();
                    let mut message = "message".len();
                    let mut paragraph = "paragraph".len();
                    let mut sentence = "sentence".len();
                    let mut words = "words".len();
                    for row in &rows {
                        if row.0.chars().count() > session {
                            session = row.0.chars().count();
                        }
                        if row.1.chars().count() > message {
                            message = row.1.chars().count();
                        }
                        if row.2.len() > paragraph {
                            paragraph = row.2.len();
                        }
                        if row.3.len() > sentence {
                            sentence = row.3.len();
                        }
                        if row.4.len() > words {
                            words = row.4.len();
                        }
                    }

                    // Header to stderr and rows to stdout, as in `memory list`,
                    // so a pipe reading this gets results and nothing else.
                    eprintln!(
                        "{:6}  {:9}  {:26}  {:session$}  {:message$}  {:paragraph$}  \
                         {:sentence$}  {:>words$}  text",
                        "score",
                        "type",
                        "id",
                        "session",
                        "message",
                        "paragraph",
                        "sentence",
                        "words"
                    );
                    for (index, hit) in hits.iter().enumerate() {
                        let row = &rows[index];
                        println!(
                            "{:.4}  {:9}  {:26}  {:session$}  {:message$}  {:paragraph$}  \
                             {:sentence$}  {:>words$}  {}",
                            hit.distance,
                            hit.kind,
                            hit.id,
                            row.0,
                            row.1,
                            row.2,
                            row.3,
                            row.4,
                            row.5
                        );
                        // A blank line after every hit, the last one included:
                        // a hundred words of preview wraps across a terminal,
                        // and without it a ranking reads as one block of text
                        // with no telling where one result ends.
                        println!();
                    }
                    Ok(())
                }
            }
        }

        Command::Embedding { command } => match command {
            EmbeddingCommand::Load { directory } => {
                tracing::debug!(msg = "Loading embedding model", directory = ?directory);
                let model = embedding::Directory::new(&directory)?;
                tracing::info!(
                    msg = "Loaded embedding model",
                    model = model.name(),
                    dimensions = model.dimensions()
                );
                println!("{} ({} dimensions)", model.name(), model.dimensions());
                Ok(())
            }

            EmbeddingCommand::Do { model, text } => {
                tracing::debug!(msg = "Loading embedding model", model = model);
                let embedder = embedding::load(&model)?;
                tracing::debug!(
                    msg = "Embedding text",
                    model = embedder.name(),
                    characters = text.len()
                );
                match embedder.embed(&[text]).into_iter().next() {
                    Some(vector) => {
                        tracing::info!(
                            msg = "Embedded text",
                            model = embedder.name(),
                            dimensions = vector.len()
                        );
                        println!("{vector:?}");
                        Ok(())
                    }
                    // `embed` returns one vector per input and it was given one.
                    None => anyhow::bail!("Embedding produced no vector"),
                }
            }
        },
    }
}
