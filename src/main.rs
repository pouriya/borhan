mod embedding;

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use tracing_subscriber::{filter::LevelFilter, fmt};

use crate::embedding::Embedding;

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

    /// Inspect and exercise embedding models.
    Embedding {
        #[command(subcommand)]
        command: EmbeddingCommand,
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
                if storage.is_dir() {
                    tracing::debug!(msg = "Storage already initialized", directory = ?storage);
                    println!("Already initialized: {}", storage.display());
                    return Ok(());
                }
                // Creates `<home>` on the way to `<home>/storage`. Nothing else
                // to set up yet: the SQLite database and the LanceDB tables are
                // made when the first memory is written.
                fs::create_dir_all(&storage)
                    .with_context(|| format!("Could not create storage directory {storage:?}"))?;
                tracing::info!(msg = "Initialized storage", directory = ?storage);
                println!("Initialized {}", storage.display());
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
            if !storage.is_dir() {
                let home = settings.home.display();
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
                     \n    borhan --home /path/to/mounted/.borhan serve",
                );
            }

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
