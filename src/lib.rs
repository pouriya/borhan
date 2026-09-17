//! borhan's modules, as a library so that `tests/` can reach them. The binary in
//! `main.rs` is the command line over this crate and adds nothing to it.

pub mod api;
pub mod index;
pub mod mcp;
pub mod normalize;
pub mod search;
pub mod storage;
pub mod ulid;

/// Where `serve` listens when `server.toml` does not say otherwise.
pub const DEFAULT_LISTEN_ADDRESS: &str = "127.0.0.1:1995";
