mod embedding;

use std::env;
use std::fs;
use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use tracing_subscriber::{filter::LevelFilter, fmt};

use crate::embedding::Embedding;

/// Name of the environment variable holding the user's home directory.
///
/// Windows has no `$HOME`; the equivalent is `%USERPROFILE%`.
#[cfg(windows)]
const HOME_VARIABLE: &str = "USERPROFILE";
#[cfg(not(windows))]
const HOME_VARIABLE: &str = "HOME";

/// Appended to the user's home directory when `--storage-directory` is absent,
/// giving `~/borhan/storage` on Linux.
const DEFAULT_STORAGE_DIRECTORY: &str = "borhan/storage";

/// Where `serve` listens. Not configurable yet.
const LISTEN_ADDRESS: &str = "127.0.0.1:1995";

#[derive(Debug, Clone, Parser)]
#[command(about, version, author)]
pub struct CommandLine {
    /// Directory holding the SQLite database and the LanceDB tables.
    ///
    /// Created on startup if it does not already exist.
    #[arg(
        long,
        global = true,
        env = "BORHAN_STORAGE_DIRECTORY",
        default_value_os_t = default_storage_directory()
    )]
    pub storage_directory: PathBuf,

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
    pub command: Option<Command>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Start the HTTP server. Used when no subcommand is given.
    Serve,

    /// Inspect and exercise embedding models.
    Embedding {
        #[command(subcommand)]
        command: EmbeddingCommand,
    },
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

/// `~/borhan/storage`, resolving the home directory the way the `dirs` crate
/// does but without depending on it.
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
fn default_storage_directory() -> PathBuf {
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
        Some(home) => home.join(DEFAULT_STORAGE_DIRECTORY),
        // Neither the environment nor the OS gave us anything; fall back to a
        // path relative to the working directory so `--help` still renders.
        None => PathBuf::from(DEFAULT_STORAGE_DIRECTORY),
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

    if settings.storage_directory.is_dir() {
        tracing::debug!(
            msg = "Found storage directory",
            directory = ?settings.storage_directory
        );
    } else {
        fs::create_dir_all(&settings.storage_directory).with_context(|| {
            format!(
                "Could not create storage directory {:?}",
                settings.storage_directory
            )
        })?;
        tracing::info!(
            msg = "Created storage directory",
            directory = ?settings.storage_directory
        );
    }

    match settings.command.unwrap_or(Command::Serve) {
        Command::Serve => {
            let router =
                axum::Router::new().route("/", axum::routing::get(|| async { "Hello, world!" }));
            let listener = tokio::net::TcpListener::bind(LISTEN_ADDRESS)
                .await
                .with_context(|| format!("Could not listen on {LISTEN_ADDRESS}"))?;
            tracing::info!(msg = "Started HTTP server", address = LISTEN_ADDRESS);
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
