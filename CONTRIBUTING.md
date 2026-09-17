# Contributing

The design, the conventions and the reasons behind both live in [AGENTS.md](AGENTS.md). Read it before changing code. It is written for human and AI contributors alike, and it is strict. This file covers how to build, check and release a checkout.

## Before you start

- **Open an issue first** for anything bigger than a fix. borhan's module list is closed on purpose (see the top of AGENTS.md), so a change that adds a file, a dependency or a new surface is a design conversation before it is a pull request.
- **Bugs:** include `borhan --version`, the exact command or request, what came back, and the `X-Trace-Id` header if a server answered. A search that ranks the wrong thing is a bug too; include the query and the unit you expected.

## Build

You need **Rust 1.90+**, **GNU make**, and a C compiler, because SQLite and zstd are compiled from source.

On **Windows**, run everything from Git Bash: the Makefile's recipes are sh. Git for Windows ships no make (`choco install make`, which is what CI does), and the C compiler is the one from Visual Studio's *Desktop development with C++* workload, which rustup's MSVC toolchain already asks for. `make systemd-install` and `make seed` are Unix-only.

```bash
make dev        # debug build   → build/borhan-<version>-<target>-dev
make release    # release build → build/borhan-<version>-<target>
```

**Use `make`, not `cargo`.** Every check is a make target, and the bare cargo commands skip some of them.

## The gate

```bash
make all        # dev build + clippy -D warnings + tests + rustfmt --check
```

`make all` must pass before a commit, and CI runs exactly that on Linux and Windows for every push and pull request. There is no check in CI you cannot run locally. `make fmt` fixes formatting.

CI also builds the Windows release zip, installs it with `install.ps1`, and uses the installed binary: store, search, then `rescan` and `delete` through a running `serve`. Nobody working on borhan runs Windows, so that job is the only place a Windows-only failure shows up before a user finds it.

A few rules clippy does not enforce, and review will:

- no new files or modules unless agreed in an issue;
- no single-use helper functions, inline them;
- `for` loops over iterator chains, `match` / `if let` over combinators;
- no `#[allow(...)]`, and no `#[cfg(test)]` in `src/`: tests live in `tests/`;
- a JSON array field's name ends in `_list`;
- a change to the API is written into all three surfaces, `src/guide.md`, `src/mcp.rs` and the `--help` text in `src/main.rs`, because each one has to stand alone.

## Try it on a real corpus

```bash
make seed       # clone rust-lang/rfcs, scan 200 documents into ./home, run searches
make start-dev  # serve ./home with debug logging
```

The seed never touches `~/.borhan`. `SEED_REPO`, `SEED_PATH`, `SEED_NAME` and `SEED_LIMIT` point it at any repository of Markdown; see AGENTS.md.

## Docker

```bash
make docker     # builds borhan:<version> and borhan:latest from the Dockerfile
```

The build runs `make release` inside `rust:alpine`, so it needs only Docker. `DOCKER_REGISTRY` (a prefix with its trailing slash) and `DOCKER_ALPINE_VERSION` change where the base images come from. CI builds and starts the image on every push; a `v*` tag pushes it to `ghcr.io/pouriya/borhan` as the version and `latest`.

## Commits

Short imperative subjects with a type prefix, matching the history: `feat: …`, `fix: …`, `ref: …`, `doc: …`. One logical change per commit.

## Releasing

1. Bump `version` in `Cargo.toml`, run `make all`, commit.
2. Tag and push: `git tag v0.2.0 && git push origin master v0.2.0`.

The release workflow refuses a tag that disagrees with `Cargo.toml`, then builds `make dist` for Linux x86_64/aarch64 (musl), macOS arm64/x86_64 and Windows x86_64, and publishes each archive with its `.sha256`: a `.tar.gz` for `install.sh`, a `.zip` for `install.ps1`. To build one by hand:

```bash
make dist TARGET=x86_64-unknown-linux-musl    # → build/dist/
```

Test the installers against it without publishing anything:

```bash
BORHAN_TARBALL=build/dist/borhan-<version>-<target>.tar.gz BORHAN_BIN_DIR=/tmp/borhan-bin sh install.sh
```

```powershell
$env:BORHAN_ARCHIVE = "build\dist\borhan-<version>-x86_64-pc-windows-msvc.zip"; $env:BORHAN_BIN_DIR = "$env:TEMP\borhan-bin"; ./install.ps1
```

## License

By contributing you agree that your contributions are licensed under the [MIT License](LICENSE).
