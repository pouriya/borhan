# borhan

Memory store: **embed → store → search**. A binary, not a library.

## ⚠️ Read this before writing any code

**DO NOT CREATE NEW FILES UNLESS EXPLICITLY TOLD TO.** The whole program lives in
`src/main.rs`. Splitting it into `settings.rs`, `utils.rs`, `store.rs`, `lib.rs`,
a `mod` tree, or anything else is **not** an improvement to make on your own
initiative — it is a change to the project's shape, and that is the author's
decision. Add to `main.rs` and let it grow. If you genuinely think a split is
needed, say so and wait for an answer; do not split and then explain.

**INLINE EVERYTHING UNLESS YOU KNOW IT WILL BE REUSED.** Not "might be reused
later", not "is cleaner as its own function" — *known*, concrete, present reuse.
One caller means no function: paste the body into the call site. This applies to
functions, types, traits, constants, and modules alike. Abstraction is earned by
a second caller that exists today, never by anticipation.

Corollary: when a helper's only caller disappears, or two helpers end up in a
single chain used once, collapse them back together.

**USE `make`. DO NOT CALL `cargo` DIRECTLY.** Every check this project cares
about is a make target, and the bare `cargo` equivalents silently skip some of
them — `cargo build` does not pin `--target`, `cargo clippy` without
`-D warnings` does not fail, and nothing else runs `check-arrow` at all.
**Before reporting any task finished, `make all` must pass.** Quote its result;
do not claim it passed without running it.

## Make targets

| Target | What it does |
|--------|--------------|
| `make all` | **The gate**: `dev` + `clippy` + `test` + `check-style` + `check-arrow` |
| `make model` | Downloads a fixture model into `models/` (once; no-op if present) |
| `make dev` | Debug build → `build/borhan-<version>-<target>-dev` |
| `make release` | Release build → `build/borhan-<version>-<target>` |
| `make start-dev` | `make dev`, then runs the binary with `--debug` |
| `make clippy` | `cargo clippy --all-targets --no-deps -- -D warnings` |
| `make check-style` | `cargo fmt --check` |
| `make fmt` | Rewrites formatting in place |
| `make check-arrow` | Fails if `Cargo.lock` has ≠1 arrow version, or a non-58 major |
| `make lint` | `clippy` + `check-style` + `check-arrow`, no build |
| `make test` | `cargo test --target …` |
| `make clean` / `dist-clean` / `purge` | Drop `target/` / also `build/` / also `models/` |

`make model` is deliberately *not* part of `make all` — it is a 31 MB network
download, and only the directory-loader path needs it. The default embedder
requires nothing: its model is compiled into the binary.

## Stack

| Crate | Version | Role |
|-------|---------|------|
| `model2vec-rs` | 0.2.1 | Static embeddings (no inference runtime, no GPU) |
| `lancedb` | 0.37.1 | Vector store — on-disk, columnar, ANN search |
| `rusqlite` | 0.40.2 | Metadata / keyword store, `bundled` SQLite |
| `tokio` | 1.53.1 | Async runtime (`lancedb` is async throughout) |
| `clap` | 4.6.6 | Command line, derive API |
| `tracing` + `tracing-subscriber` | 0.1 / 0.3 | Structured JSON logging to stderr |

## Hard constraints

- **arrow stays on 58.x.** `lancedb` 0.37.1 pins `arrow ^58`. arrow 59 exists and
  will resolve fine, but its types do not unify with lancedb's — you get
  `expected arrow_schema::Schema, found arrow_schema::Schema`, with no mention of
  versions anywhere in the error. `make check-arrow` enforces this; if it fails,
  `cargo tree -i arrow` shows who pulled the second one.
- **Nothing downloads at runtime.** `model2vec-rs` is built with `local-only`,
  which drops `hf-hub` and `ureq` from the graph. The default model is compiled
  into the binary; `models/` only holds fixtures for the directory loader.
- **`git lfs` must be installed before cloning.** `*.safetensors` is LFS-tracked
  via `.gitattributes`. Without it you get pointer files and a runtime
  `HeaderTooLarge` — see the Model section.
- **`fancy-regex`, not `onig`.** The default `onig` feature pulls the oniguruma
  C library; `fancy-regex` is pure Rust. Do not re-enable default features on
  `model2vec-rs`.

## Model

The default model is **`minishlab/potion-retrieval-32M`** — 131 MB,
**512 dimensions**, `normalize: true`, embedding matrix `F32 [63091, 512]`.
It is fine-tuned for asymmetric query→document retrieval, which is what memory
recall is.

It lives in `src/embedding/potion-retrieval-32M/` and is pulled into the binary
with `include_bytes!`, so a `borhan` binary embeds text with no files on disk and
no network. The cost is real and deliberate: the debug binary is ~235 MB. Do not
add a second embedded model without asking.

### The model is in Git LFS — install it before cloning

`model.safetensors` is 124 MB, over GitHub's 100 MB hard limit, so it is stored
via **Git LFS**. `.gitattributes` tracks `*.safetensors`.

```sh
sudo apt-get install -y git-lfs   # or: brew install git-lfs
git lfs install                   # once per machine
git clone git@github.com:pouriya/borhan.git
```

**Cloning without git-lfs installed leaves a ~130-byte pointer file in place of
the model.** `include_bytes!` happily embeds the pointer text, the build
succeeds, and the failure only shows up at runtime as:

```
Error: Could not load the built-in embedding model "potion-retrieval-32M"

Caused by:
    0: failed to parse safetensors
    1: HeaderTooLarge
```

If you see that, you are missing git-lfs — run `git lfs install && git lfs pull`.

Watch the quota: GitHub's free tier gives 1 GB of LFS storage and **1 GB/month of
LFS bandwidth**, and every fresh clone or CI run pulls the full 124 MB. That is
roughly eight clones a month. If CI starts cloning this repo regularly, either
pay for a data pack or move the model out of git and fetch it in a build step.

**It is English-only** — the tokenizer is `bge-base-en-v1.5`. Non-Latin text
degrades to `[UNK]`. The only multilingual model2vec is
`potion-multilingual-128M` at 537 MB; switching means changing the dimension and
re-embedding everything already stored.

`models/` (gitignored, populated by `make model`) holds `potion-base-8M`
(31 MB, 256d) purely as a fixture for exercising the directory loader.

## Embedding module

`src/embedding/mod.rs` — one file plus the model directory.

```rust
pub trait Embedding: Send + Sync {
    fn name(&self) -> &str;
    fn dimensions(&self) -> usize;
    fn embed(&self, texts: &[String]) -> Vec<Vec<f32>>;
}

pub struct Embedded;    // default: include_bytes! -> StaticModel::from_bytes
pub struct Directory;   // Directory::new(path) -> StaticModel::from_pretrained

pub fn load(name: &str) -> Result<Box<dyn Embedding>, Error>;
```

`load` treats `"default"` (the `DEFAULT_MODEL` const) as the embedded model and
anything else as a directory path.

- **`embed` is infallible.** model2vec is a lookup table plus pooling — there is
  no inference step to fail. Everything that can go wrong happens at load time,
  which is what `Error` covers. Do not add a `Result` to `embed` "just in case".
- **`dimensions()` is probed, not read.** `StaticModel` keeps its shape private
  and exposes no accessor, so `probe_dimensions` embeds `"a"` once at load time
  and measures the result. If model2vec-rs ever exposes the width, use that.
- **`Directory::new` checks for each required file itself**, because
  model2vec-rs reports a missing directory, a missing `config.json` and a
  missing `model.safetensors` with the same opaque message.

## CLI

Logging flags and `--storage-directory` are `global = true`, so they work before
or after a subcommand. **`serve` is the default when no subcommand is given.**

```
borhan                                  # == borhan serve
borhan serve                            # axum hello-world on 127.0.0.1:1995
borhan embedding load <DIR>             # load a model dir, report name + dims
borhan embedding do [--model M] <TEXT>  # embed TEXT; M is "default" or a dir
```

`do` is a Rust keyword, hence `#[command(name = "do")]` on the `Do` variant.

## Layout

```
src/main.rs                          CLI + runtime setup. See the rule at the top.
src/embedding/mod.rs                 Embedding trait, both impls, Error
src/embedding/potion-retrieval-32M/  Committed model, embedded via include_bytes!
Makefile                             Every build/check entry point. Use it, not cargo.
scripts/fetch-model.sh               Fetches a model dir (via `make model`)
models/                              Test-fixture models (gitignored)
build/                               Named binaries from make dev/release (gitignored)
```

`main.rs` holds: `CommandLine`/`Command`/`EmbeddingCommand` (clap derive),
`logging_level()`, `default_storage_directory()` (with the vendored `dirs` logic
inlined into it), and `main()`.

Storage defaults to `~/borhan/storage` (`$HOME` on Unix, `%USERPROFILE%` on
Windows), overridable with `--storage-directory` or `BORHAN_STORAGE_DIRECTORY`.

## Vendored code

The home-directory resolution inside `default_storage_directory` is copied from
`dirs`/`dirs-sys` rather than depended on. **When vendoring, the doc comment must
quote the original source, name the crate and version, and list every deliberate
deviation.** Do not vendor silently.

## Code style conventions

- **Plain `for` loops over iterator method chains.** Prefer a `for` loop to
  `.map`/`.filter`/`.fold`/`.collect` chains. (When you do index a slice, still
  use `for x in &xs` / `.iter().enumerate()` to satisfy `needless_range_loop`.)
- **`match` over combinators for `Result`.** Use an explicit `match` instead of
  `map_err`, `and_then`, `or_else`, etc.
- **`if let Some(...)` over combinators for `Option`.** Use `if let` / `match`
  instead of `map`, `and_then`, `unwrap_or_else`, etc.
- **Don't extract single-use helpers.** If a function is called from exactly one
  place, inline it. See the rule at the top of this file — this is the same rule,
  and it is not negotiable.
- **`anyhow` at the binary boundary, `thiserror` inside modules.**
  `fn main() -> anyhow::Result<()>`, so every fallible call is just `?` plus
  `.context(...)`/`.with_context(...)` — never hand-roll a source-chain walker,
  and never convert errors to `String` to propagate them. Module-level errors
  are `thiserror` enums with `#[source]` (see `embedding::Error`); they convert
  into `anyhow::Error` through `?` for free, and `main`'s `Debug` output prints
  the whole `Caused by:` chain.

## Lint & style conventions

- **No `#[allow(...)]` anywhere** — fix the root cause instead of suppressing a
  lint. (`collapsible_if` wants edition-2024 let-chains: `if let Some(x) = … && cond`.)
- **`make clippy` is the gate** — `-D warnings`, so warnings in tests and
  examples fail the build too.
- **`clippy::type_complexity`** — extract a named `pub type` alias rather than
  spelling out nested generic types in signatures.
- **`needless_range_loop`** — iterate with `for x in &xs` / `.iter().enumerate()`,
  not `for i in 0..xs.len()`.

## Testing

No `#[cfg(test)]` blocks in `src/` — ever. All tests live in `tests/`. Naming:
module `x` → `tests/x.rs`; submodule `a::b` → `tests/a_b.rs`. A test needing a
private item with no public path is deleted, not kept inline and not exposed via
a new `pub`.

## Logging conventions

`tracing` only — no `log`, no `cfg_if`, no feature gates. The subscriber is
installed in `main` and writes **JSON to stderr**; stdout stays free for program
output. Verbosity comes from `CommandLine::logging_level()`:

| Flag | Level | Extra fields |
|------|-------|--------------|
| `--quiet` | `OFF` | — |
| *(none)* | `INFO` | — |
| `--debug` | `DEBUG` | target |
| `--trace` | `TRACE` | target, file, line |

`--quiet` wins over `--trace`, which wins over `--debug`.

### Level guide

| Level | When to use |
|-------|-------------|
| `info` | Important success event (storage created, index built, batch committed) |
| `warn` | Intentionally ignored error, or a fallback being taken |
| `error` | Failure the binary recovers from — a failure it does *not* recover from is returned from `main` as `Err(String)`, not logged |
| `debug` | Before attempting something important — include the inputs/params that affect the outcome |
| `trace` | After completing a low-level operation — include rich detail about what was produced |

### Format rules

Every log call must start with a `msg` field whose value begins with a capital
letter. Additional structured fields follow as `key=value` pairs. Use `?` for
`Debug` (paths, options) and `%` for `Display`.

```rust
tracing::info!(msg = "Created storage directory", directory = ?settings.storage_directory);
tracing::warn!(msg = "Skipped unreadable memory file", path = ?path, error = %error);
tracing::debug!(msg = "Embedding batch", count = texts.len(), dimensions = DIM);
tracing::trace!(msg = "Committed vectors", table = "memory", rows = batch.num_rows());
```

Because `--quiet` sets `OFF`, never rely on a log line to communicate something
the user must see — return it from `main` or write it to stdout.
