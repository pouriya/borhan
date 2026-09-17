# borhan

Memory store: **store → split → index → search → read back**. A binary, not a
library. A SQLite source of truth under a tantivy index; nothing is embedded and
nothing is downloaded at runtime.

## ⚠️ Read this before writing any code

**DO NOT CREATE NEW FILES UNLESS EXPLICITLY TOLD TO.** The module list under
Layout is the whole program and it is closed. Adding `settings.rs`, `utils.rs`,
`types.rs`, `lib.rs` or any further split is **not** an improvement to make on
your own initiative — it is a change to the project's shape, and that is the
author's decision. Add to the module the code belongs to and let it grow. If you
genuinely think a split is needed, say so and wait for an answer; do not split
and then explain.

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
`-D warnings` does not fail.
**Before reporting any task finished, `make all` must pass.** Quote its result;
do not claim it passed without running it.

## Make targets

| Target | What it does |
|--------|--------------|
| `make all` | **The gate**: `dev` + `clippy` + `test` + `check-style` |
| `make dev` | Debug build → `build/borhan-<version>-<target>-dev` |
| `make release` | Release build → `build/borhan-<version>-<target>` |
| `make dist` | Release archive + `.sha256` for `TARGET` → `build/dist/`: `.zip` for Windows, `.tar.gz` otherwise |
| `make docker` | Alpine image `borhan:<version>` + `borhan:latest`; `make release` runs inside it |
| `make version` | Prints the `Cargo.toml` version; the release workflow checks the tag against it |
| `make start-dev` | `make dev`, then runs `serve` with `--debug` |
| `make clippy` | `cargo clippy --all-targets --no-deps -- -D warnings` |
| `make check-style` | `cargo fmt --check` |
| `make fmt` | Rewrites formatting in place |
| `make lint` | `clippy` + `check-style`, no build |
| `make test` | `cargo test --target …` |
| `make seed` | `seed-scan` + `seed-test`: fetch a corpus, scan it, search it |
| `make seed-fetch` | Clones the corpus into `seed/<name>/` (once; no-op if present) |
| `make seed-scan` | Wipes `home/`, inits it, adds every document as a message |
| `make seed-test` | `memory list`, three searches, a lexicon lookup, and `memory cursor` of the top hit |
| `make seed-clean` | Drops `home/`, keeps the fetched corpus |
| `make clean` / `dist-clean` / `purge` | Drop `target/` / also `build/` / also `seed/` and `home/` |

### The seed corpus

`make seed` is the only thing that exercises storage on real volume: the
splitter, the index and search all behave differently at thousands of units than
at the handful a manual test types in.

| Variable | Default | Meaning |
|----------|---------|---------|
| `SEED_REPO` | `https://github.com/rust-lang/rfcs.git` | Any repository of Markdown |
| `SEED_PATH` | `text` | Directory inside it holding the `.md` files |
| `SEED_NAME` | `rfcs` | Memory name, and `seed/<name>/` |
| `SEED_LIMIT` | `200` | Documents to scan; `99999` for all 648 |
| `SEED_HOME` | `$(CURDIR)/home` | `BORHAN_HOME` for every seeded command |

**`BORHAN_HOME=./home`, never `~/.borhan`** — a scan of somebody else's
documents has no business landing in the developer's own store. `home/` and
`seed/` are both gitignored.

rust-lang/rfcs was picked over rust-lang/book on measurement, not taste. The
book's `src/*.md` holds **707 mdBook `{{#rustdoc_include}}` directives** where
the code should be, so scanning it would file placeholder lines as Rust. The
RFCs are self-contained: 648 documents, ~1.3M words, 4243 fenced blocks (473
over 20 lines, which is what drives the `CODE_LINES` chunking), 1783 table rows,
zero preprocessor directives.

One document is one message, named by **its path under `SEED_PATH`** with the
slashes turned into dashes, all under one session — so `--message` stays unique,
which `add` requires, and the corpus reads as one long conversation the cursor
can walk. The path and not the filename because rust-lang/rfcs is one flat
directory and almost nothing else is: github/docs has an `index.md` in nearly
every directory, and `basename` collides on **111** distinct names across its
3737 documents. For the same reason the loop is a `find`, not a `*.md` glob,
which would have matched nothing at all outside the top directory. 200 documents
take ~50s. 60 documents produce 3 087 units, which is enough for BM25's
document-frequency weighting to mean something.

Two things the Makefile does that are not obvious:

- **`seed-scan` depends on `release`, not `dev`.** A debug build is several
  times slower at tokenizing and indexing, and `seed-scan` runs the binary once
  per document, which is the difference between a coffee and an afternoon.
- **`--` goes before the text.** `memory add` takes the text as a positional,
  and a Markdown file that opens with a list item starts with `- `, which clap
  reads as a flag: `error: unexpected argument '- ' found`.

## Stack

Every entry carries its reasoning in `Cargo.toml`, next to the dependency
itself; that file is the source of truth and this table is the index to it.

| Crate | Version | Role |
|-------|---------|------|
| `rusqlite` | 0.40.2 | The source of truth. `bundled`, so SQLite compiles from source |
| `tantivy` | 0.26.1 | The inverted index: dictionary, postings, positions, BM25, merges |
| `rust-stemmers` | 1.2.0 | Snowball English (Porter2), the second half of the normalizer |
| `unicode-normalization` | 0.1.24 | NFC, the first half. Persian arrives in both forms |
| `pulldown-cmark` | 0.13.4 | Breaks stored Markdown into units |
| `axum` | 0.8.9 | The HTTP server, and MCP over it |
| `tokio` | 1.53.1 | Async runtime for `serve` |
| `ureq` | 2.12.1 | The CLI talking to a running `serve`. Blocking on purpose |
| `clap` | 4.6.6 | Command line, derive API |
| `tanzim` | 0.28.0 | Reads `server.toml`, with located errors |
| `toml_edit` | 0.22.27 | Writes `server.toml` in `init server` |
| `serde` / `serde_json` | 1.0 | The configuration structs; every `--json` body and the MCP envelope |
| `getrandom` | 0.4.3 | The 80 random bits of a ULID, straight from the OS |
| `chrono` | 0.4.45 | Renders `created_at` as ISO-8601 in `memory list`; no other date handling |
| `thiserror` / `anyhow` | 2.0 / 1.0 | Module errors; the binary boundary |
| `tracing` + `tracing-subscriber` | 0.1 / 0.3 | Structured JSON logging to stderr |

## Hard constraints

- **Nothing downloads at runtime, and nothing is embedded.** There is no model,
  no vector store and no inference: search is BM25 over a tantivy index with a
  hand-written normalizer. `make seed` fetches a corpus, and that is the only
  network access anywhere in the tree.
- **The normalizer and the index version move together.** Changing `normalize.rs`
  or the splitter means bumping the rules version, and every existing index then
  answers with a `500` naming `memory rescan` until it is rebuilt. That error is
  the design working: an index quietly disagreeing with the query side about what
  a word folds to is a search that returns other things and says nothing.
- **The tokenizer and the scoring loop are not delegated.** tantivy has no notion
  of ZWNJ, ک/ی folding or Persian morphology, and best-in-clause, coverage across
  clauses and min-span proximity are not expressible in its stock query tree.
  Everything else about the index is tantivy's — including the query grammar and
  the BM25 of each word and phrase, which the scoring loop combines rather than
  recomputes.

## Identifiers

`src/ulid.rs` — **every primary key is a ULID**: 48 bits of milliseconds since
the Unix epoch, then 80 random bits, big-endian, stored as `BLOB(16)` and shown
as 26 Crockford base32 characters.

```rust
Ulid::new() -> Result<Ulid, Error>   // clock + getrandom, both fallible
ulid.bytes() -> &[u8; 16]            // the primary key
ulid.milliseconds() -> u64           // the timestamp back out of the key
ulid.to_string()                     // "01M07V7AGPANAFB7JRCRM60F15"
```

Byte order is time order, and SQLite compares blobs with `memcmp`, so
`ORDER BY id` is chronological and a time range is a contiguous key range. That
is the whole reason not to use a UUIDv4.

Written by hand rather than pulled from the `ulid` crate — it is a timestamp,
ten random bytes and an alphabet. **It has no monotonic factory**: two ULIDs
made in the same millisecond sort arbitrarily against each other. Add one when
something depends on within-millisecond ordering, not before.

`Ulid::parse` reads the 26-character form back, case-insensitively — a ULID that
has been through a shell or a copy-paste may arrive lowercased. `I`, `L` and `O`
are **not** folded onto `1` and `0`: the alphabet omits them so a human does not
misread one aloud, and accepting the mistake would file a row under an id nobody
can type twice. A first character above `7` is refused, since 26 characters
carry 130 bits and only 3 of the leading 5 fit.

## Storage

`src/storage.rs` — **one directory per memory**, `<home>/storage/<name>/`,
holding `borhan.db` and an `index/` beside it. Nothing is shared between
memories: no cross-memory table, no cross-memory index, and `memory delete` is
`rm -rf` of one directory plus the row that named it.

```rust
Storage::create(root, name, description, languages) -> Result<(Storage, Ulid), Error>
Storage::open(root, name)   -> Result<Storage, Error>   // opens; never creates
Storage::list(root)         -> Result<Vec<Memory>, Error>
Storage::delete(root, name) -> Result<Memory, Error>
storage.describe()          -> Result<Memory, Error>    // + the three counts
storage.add(&Entry)         -> Result<Written, Error>   // one message, split into units
storage.replace(&Revision)  -> Result<(Ulid, Ulid, Vec<Indexed>), Error>  // one message, rewritten
storage.sessions()          -> Result<Vec<SessionRow>, Error>       // what `outline` lists
storage.messages(session)   -> Result<Vec<MessageRow>, Error>       // one session's, no bodies
storage.resplit()           -> Result<Vec<(Ulid, Written)>, Error>   // what `rescan` runs
storage.locate(&[Ulid])     -> Result<Vec<Located>, Error>          // units by id
storage.around(unit, before, after, whole) -> Result<Vec<Located>, Error>
storage.sentences(unit)     -> Result<Vec<(usize, usize)>, Error>   // for the snippet
```

**`Storage::open` will not create the directory.** A typo'd `--home` has to be an
error naming the missing mount, never a new empty store that silently remembers
nothing. `init` is the only thing that writes the layout; see `check_storage` in
`main.rs`, whose message spells out both fixes because an agent in a container
has two and the wrong one *succeeds*.

```sql
memory (id, ulid, name, description, languages, created_at)
session (id, ulid, external_ref UNIQUE, started_at, ended_at)
message (id, ulid, session_id, external_ref, seq, author, role, ts, body,
         UNIQUE (session_id, seq))
unit     (id, message_id, seq, byte_start, byte_end, UNIQUE (message_id, seq))
sentence (unit_id, seq, byte_start, byte_end, PRIMARY KEY (unit_id, seq))
index_meta    (key, value)
```

- **`message.body` is the only copy of the text.** `unit` and `sentence` hold
  byte offsets into it and no content of their own, which is why `Located::text`
  is a slice and not a column. A unit's text therefore cannot drift from the
  message it came from, and re-splitting is rewriting offsets rather than
  rewriting text.
- **Offsets are bytes, not characters.** Persian is two bytes per character in
  UTF-8, so `length(body)` in SQLite — which counts characters on TEXT — is not
  the same number, and `length(cast(body as blob))` is what a size check wants.
- **`seq` is the reading order, not `id`.** A ULID is a millisecond plus 80
  random bits with no monotonic factory (see `src/ulid.rs`), and `add` writes a
  whole message inside one millisecond, so `ORDER BY id` would be ordering the
  random halves. `message.seq` counts within its session and `unit.seq` within
  its message, both gapless — which is what lets `around` turn a window counted
  in units into one indexed range scan over messages.
- **`external_ref` is somebody else's identifier**, arriving with the
  transcript: a thread id, a filename, a page number. It is `UNIQUE` per session
  on `message`, so replaying a transcript that overlaps what is stored fails
  loudly instead of duplicating. It is what a hit reports as `session_ref` and
  `message_ref`; the ULID beside it is what another call accepts.
- **`role` is an integer**, not text: `Role::code`/`Role::from_code`.
- **Nothing here records reads.** There is no table of queries and no table of
  which results were expanded, and adding one is not a small change: it would
  make `search` and `cursor` writers, so a store mounted read-only, or a server
  configured without write permission, would start failing on reads. What a read
  is worth saying goes to stderr.
- **`replace` rewrites a body in place and keeps the `seq`.** Not remove-and-add:
  the ordinal is gapless within a session and that is what makes `around` a range
  scan, so a hole in it would silently shorten every window spanning the gap.
  It keeps the author and the role too — a replacement corrects what a message
  *says*, never who said it — and takes a new `ts`, because a correction ranked
  by the time of the thing it corrects is ranked wrong.
- **`replace` returns the whole session, and the caller reindexes all of it.**
  A unit document carries a `session` term and no message term, so
  `Index::forget` is the narrowest deletion tantivy can be asked for. The
  alternative was a `message` field, which changes the schema `open_or_create`
  compares against and so breaks every index already on disk with a tantivy
  error rather than the `Stale` path that names `memory rescan`. Cost is
  proportional to the session; the forget and the rewrite share one commit, so a
  reader never sees the session half-present.
- **`server.toml` lists what the server refuses, not what it permits.**
  `refuse = ["delete", "replace"]`; absent or empty means all six operations are
  allowed, which is right for a store on the machine of whoever started the
  server — they can already `rm` it. An allowing list would have withheld every
  operation invented after the file was written, which is how `replace` shipped
  switched off on servers whose owners had never heard of it. An unknown name in
  `refuse` stops the server at startup: skipping it would leave an operation
  *on*.
- **A `403` says what is refused and never how to un-refuse it.** The old text
  named the key and the file; a model read that as a repair instruction and
  looped trying to edit the configuration and restart the server. The party
  reading the error is the party the refusal is aimed at, and the party who can
  change it is not in that conversation. Same rule for the MCP tool list: a
  refused operation has no tool, not a disabled tool with a note.
- **`index_meta` holds the normalization rules version.** An index built by an
  older normalizer answers with a `500` naming `memory rescan` rather than
  quietly disagreeing with the query side about what a word folds to.

### Counting: what `memory list` shows

`describe()` runs three `count(*)` queries — sessions, messages, units — per
memory. That is a scan, which is fine for a listing on a terminal; the day it is
not, the fix is a counts row the writer keeps current, not a cleverer query.
**The heading goes to stderr and the rows to stdout**, so a pipe reads nothing
but data and a terminal is still told what the columns are.

### Splitting: what a unit and a sentence are

**Stored text is read as Markdown**, because what borhan is fed is a chat
transcript and that is what those are written in. A **unit is a CommonMark
block** — a paragraph, one list item, one table row, a fenced code block — not a
run between blank lines. Splitting on blank lines would run a bullet list into
one lump and cut a code block wherever the code happened to breathe; both are
worse things to index than what the author actually wrote.

A unit is what search scores and returns, and its ULID is the `cursor`. A
sentence is a span inside a unit, and exists for one reason: the snippet on a
hit is the best *sentence* of the unit rather than the whole block.

- **A heading joins the block under it**, and is the one exception to
  block-per-unit. `## Motivation` on its own answers no question and sits
  close to every query that says "motivation"; in front of the prose it labels,
  it says what that prose is about. Two headings in a row both join the first
  block with words in it. A heading with nothing under it is a unit of its own.
- **Nothing is copied.** A unit and a sentence are byte offsets into
  `message.body`, so the text comes back with the spacing it went in with and
  cannot drift from the message it came from.
- **Markup is dropped, not stored.** `**bold**` is `bold`, a link is its text.
  Nothing reaching a model or a screen has a bracket the author did not type.
- **Code blocks keep their literal lines**, indentation included — in code the
  punctuation *is* the content. One sentence per line, blank lines dropped, and
  one unit per `CODE_LINES` (20) lines: a screenful, long enough that a
  function usually lands whole and short enough that a 500-line file does not
  become one unit answering every question about it equally. A 45-line block
  splits 20/20/5.
- **An HTML block keeps its words and loses its tags.** `<details>`,
  `<summary>`, a hand-written `<table>`, an MDX-style `<Tabs>` — the tags go the
  way `#` and `- ` go, and the prose between them stays. Its own path in the
  splitter because a comment can run across several `Event::Html` lines, so
  there is nothing to strip until the block ends. `<!-- … -->` is dropped whole:
  `<!-- prettier-ignore -->` is a note to a tool, not to a reader. **A stripped
  tag becomes a space** where it sat between two non-spaces, because
  `<td>a</td><td>b</td>` on one line is two cells and dropping the markup
  outright leaves `ab`, a word nobody wrote and nobody can search for. Inline
  `<b>` is dropped without a space, since the text around it arrives on its own
  and already reads as one sentence; inline `<br>` breaks the line, wherever it
  was written.
- **A table row is one unit**, cells joined with ` | `. Not one per cell:
  `rusqlite` alone is half a fact, `rusqlite | the source of truth` answers
  something. The
  separator goes in front of each cell so no punctuation is invented.
- **A sentence ends at `.` `!` `?` `؟` `。` `！` `？` `…` `؛` followed by
  whitespace or the end of the block.** Requiring the whitespace is what keeps
  `3.14` and `e.g.` in one piece. Runs (`?!`, `...`) end one sentence, not three.
  A hard break is a boundary; a soft-wrapped line inside a paragraph is not.
- **Abbreviations are not understood.** "Dr. Smith" is two sentences. The fix is
  a real segmenter with a per-language model, not a longer list of special
  cases; until that is worth a dependency the cost is one short extra row.
- **A line with no terminator inside `CONTENT_LIMIT` is cut at the limit**
  rather than dropped or refused, because losing the tail is worse and failing
  a whole transcript over one long line is worse still.

## Home directory

Everything borhan owns sits under `--home` (`BORHAN_HOME`, default `~/.borhan`),
and nothing sits outside it:

```
~/.borhan/
  storage/      One directory per memory. Written by `init storage`.
  server.toml   Listen address and token. Written by `init server`.
                `serve` binds it; the CLI probes it and uses HTTP when that
                process answers, otherwise opens storage itself.
                Absent for `serve` => 127.0.0.1:1995 and no token.
```

**Nothing is created implicitly.** `init` is the only thing that writes the
layout, so a missing `storage/` always means "never set up here", never "set up
somewhere you did not look". `check_storage` in `main.rs` is that gate, and it
runs in front of every storage-touching command; its error spells out both fixes
— run `init`, or mount the user's real `~/.borhan` into the sandbox and pass
`--home`. The message is aimed at agents running in containers, where the
difference between the two matters: `init` there silently produces an *empty*
store and hides every existing memory.

### Local vs HTTP

`server.toml` is the only configuration file. `init server --listen HOST:PORT [--token T]` writes it (`create_new`, mode `0600`). `serve` reads it to bind; if the file is missing it binds `127.0.0.1:1995` with no token. Every memory command probes that listen address (`GET /api/v1/health`, about 1s). If the file is missing, `listen` is unset, or the server does not answer, the command opens storage itself.

A token in `server.toml` is required on every HTTP request as `Authorization: Bearer`. The same file is what the CLI sends.

The same server serves `GET /` — `src/guide.md`, the whole REST API as Markdown
— outside the token gate, and speaks MCP at `POST /mcp`, behind the same token. Every tool is one of `api.rs`'s `run_*` functions — the ones the REST handlers call, deserializing into the same body structs — so a tool and its endpoint cannot drift. There is one read tool per question and no two that overlap — `memory_cursor` covers every width of read — so a model never spends a turn choosing between them. `tools/list` shows the write tools unless `server.toml` refuses them, and `tools/call` checks again through the same `App::permit`, because a client is entitled to cache that list. Resources are the memories: one each, carrying the description and the counts, and reading one returns that memory's metadata plus the lemmas it actually uses — which is what a caller has to write its query in.

Handshake era (`2025-11-25`, back to `2024-11-05`), not the ratified `2026-07-28`, because every client this is for — Claude Code, Cursor, Codex, OpenCode, Hermes — is on the handshake era today. `server/discover` is therefore answered with `404` and an **empty body**: a newer client falls back to `initialize` only when the body is *not* a recognized modern JSON-RPC error, so a `-32601` there would stop the fallback rather than trigger it. `GET` and `DELETE` get `405`, and `Origin` must be absent or loopback.

Every HTTP response sets `Server: borhan/<version> (<repository>)` and `X-Borhan-Version` from the crate version and repository (`Cargo.toml`). The CLI compares that header to its own version. A mismatch prints prettified JSON and does not try to format it. A match with `--json` decodes, re-encodes and pretty-prints the body. A match without `--json` prints the usual text. `X-Trace-Id` is shown only when the server answers 4xx or 5xx.

Both `serve` and the CLI read the file with [`tanzim`](https://docs.rs/tanzim) into `Server`, via `read_configuration`. The helper formats tanzim's error with `{:#}` — that is the form carrying source, line, column and the caret; wrapping it as a `#[source]` throws all of it away. `init server` serializes the same `Server` struct back out with `toml_edit`.

## Three ways in

borhan is used by models more than by people, and a model reaches it one of
three ways. All three are the same `run_*` functions in `api.rs`, so an
operation is never available through only one of them — but each surface has to
teach itself, because whoever arrives has arrived at exactly one of them and has
nothing else to read.

| Surface | Entry point | Where its guide text lives |
|---------|-------------|----------------------------|
| **MCP** (best case) | `POST /mcp`, configured into the client | `initialize`'s `instructions` and the tool `description`s, in `src/mcp.rs` |
| **CLI** | `borhan`, `borhan memory`, `--help` at every level | the clap doc comments in `src/main.rs` |
| **HTTP** | `GET /` on a running `serve` | `src/guide.md`, served verbatim |

**Each one must stand alone.** A model given only an MCP server never sees
`--help`; a model given only a URL never sees a tool schema. So the same handful
of facts — parentheses hold one idea and separate parts are separate ideas, read
`coverage` and not `score`, `lemma_units` of 0 means the corpus has never heard
the word, one call reads every cursor — are written out in all three places on
purpose. That duplication is the design, not drift to be factored out.

**Three rules when changing any of this:**

1. **Add a route, edit `src/guide.md`.** Nothing checks that the two agree, and
   a documented endpoint that does not exist is worse than an undocumented one.
2. **Rename a JSON field, grep all three.** `src/guide.md`, the tool schemas in
   `src/mcp.rs`, and the doc comments in `src/main.rs`.
3. **No surface may need another one to be usable.** If the answer to "how would
   a model discover this" is "read the CLI help", it is not documented.

`GET /` is served **outside the token gate** — it is registered after
`.layer(token_gate)` in `router`, which is what makes axum skip the layer for
it. It is documentation, not data, and an agent handed nothing but an address
has to be able to read it before it can know a token is wanted. Everything the
document describes stays gated.

Bare `borhan` and bare `borhan memory` both print their long help and exit `0`.
Neither has an implicit action: a caller who typed `borhan memory` did not
choose `list`, and a caller who needed to be told what the nine subcommands are
would get a listing instead of an answer. This is the same rule as One reader,
one level up — a verb that quietly does one of nine things is a choice the
caller did not know they were making.

## CLI

Logging flags and `--home` are `global = true`, so they work before or after a
subcommand.

```
borhan init                             # == borhan init storage
borhan init storage                     # create ~/.borhan and ~/.borhan/storage
borhan init server --listen H:P \
                  [--token T]           # write ~/.borhan/server.toml (0600)
borhan serve                            # HTTP API + MCP + GET / , address from server.toml
borhan memory create <NAME> \
                  --description T       # store a memory, print its ULID
                  [--languages L]       # description is required, more than 10 words
                  [--json]
borhan                                  # print the long help and exit 0
borhan memory                           # print the memory help and exit 0
borhan memory list [--json]             # ULID, created, name, counts, languages, description
borhan memory outline <NAME> [SESSION] [--json]
borhan memory update <NAME> \
                  [--description T] [--languages L] [--json]
borhan memory add <NAME> <TEXT> \
                  --session S [--message M]
                  [--role user|assistant|tool] [--author N] [--ts MS]
                  [--json]              # split into units, index, print the message ULID
borhan memory replace <NAME> <TEXT> \
                  --session S --message M [--ts MS] [--json]
borhan memory search <NAME> <QUERY> \
                  [--fuzzy] [--limit N] \
                  [--json]              # one query string, coverage, cursor
borhan memory cursor <NAME> <ULID>... \
                  [--before N] [--after N] [--messages] [--json]
borhan memory lexicon <NAME> <WORDS>... [--json]
borhan memory rescan <NAME> [--json]
borhan memory delete <NAME> --yes [--json]
borhan skills remember|survey [--install]
```

`--json` prints the same wrapped object the HTTP API returns, pretty-printed.
Text mode does not print stats on stdout.

`memory add` reads the text as Markdown, splits it into units, writes them to
SQLite and the tantivy index, prints the message ULID on stdout and the unit
count on stderr.

### One reader

`memory cursor` is the only way to read stored text back, at every width a
caller might want it: `--before 0 --after 0` is the unit itself, the default of
two is the paragraphs either side, and `--messages` counts the window in whole
messages instead. There is deliberately no second command for reading a unit by
id, because two readers taking the same ULID — which is what `memory get` was —
made every caller stop and pick one, and a model picking between them picks
wrong.

**The window is counted in units, not messages.** A unit is a thirtieth of a
message in a corpus fed from PDFs (2 577 bytes against 84 in a Persian chat corpus,
6 967 against 133 in `rfcs`), so pulling the whole page to re-read one paragraph
costs eleven to forty times the context for text the caller did not ask for.
`--messages` is still there for when the paragraph does not say who was talking,
and it costs what it always did.

Several cursors are read in one call and the windows merged, because a page of
hits is one question and over MCP each call is another turn of the model. A
cursor this memory no longer holds comes back in `missing_list` rather than
failing the read — a batch carried over from an older result set should read the
ones that still resolve — while a string that is not a ULID at all is a `400`,
because that is the caller malformed rather than the memory changed.

Results are grouped by message: the fields that say *where* a unit sits are
identical for every unit of a message, and a whole-message window is 122 units
in that Persian chat corpus. Flat, that response was 57 KB of JSON around 5 KB of text.
Each message carries its `unit_list` — the ids are what a caller passes back to
move again — or its `body` when the window was counted in messages, where those
ids would buy nothing.

**A widened read writes nothing.** Whether the window was widened is a field on
the stderr line and nothing else; `cursor` touches no table, the same as
`search`.

**A hit and a cursor row spell their ids the same way.** In both, `session` and
`message` are ULIDs — what another call accepts, and `session` is exactly what
the `session` filter of the next search wants — while `session_ref` and
`message_ref` are the feeder's own names, which are what the text columns show
and what nothing accepts as input. They used to be spelled the opposite way
round on a search hit, so `hit.session` could not be fed back into a search at
all. Two fields with one name meaning two things is the same failure as two
tools taking one identifier.

### What the text output looks like

`memory search` prints two lines per hit under a header naming the columns:

```
score  cover  cursor  session  message  size  matched
"the best sentence of that unit, quoted"
```

- **`cover` before `score`.** Coverage is a fact — top-level parts matched over
  parts asked. The score is squashed to `0..1` against the top hit of *this* query and
  orders these hits and nothing else.
- **`matched` marks a nearby part with `~`.** Labels are the query's top-level
  parts, spelled back from the parsed tree. A bare label means the word is in
  the quoted line; `~` means it was reached through the surrounding units of the
  same message. It still counts toward coverage — it is still evidence — but
  quoting the line for it would be wrong. See `matched_cell` in `main.rs`.
- The `session` and `message` columns are the feeder's refs, sized to their
  contents like `memory list`'s columns, because a session id is a UUID in one
  deployment and a filename in another.
- The preview is cut at `PREVIEW_WORDS` (60) with an `…`, keeps the spacing it
  was stored with, and escapes `\n`, `\r` and `\t` so a code block still reads
  as a code block while the hit stays one line.

**Hits go to stdout and nothing else does.** The header, the unknown-word and
fuzzy lines, `No hits.` and the closing vocabulary line all go to stderr, so a pipeline
reading stdout receives only results. `memory create` prints the ULID and
nothing else, so it pipes. `memory list` prints one aligned line per memory;
with no memories stdout stays empty and the sentence goes to stderr, so a pipe
reads an empty list rather than prose.

**Read the unknown-word lines.** `unknown: "cva" (in "(cva stroke)") matched nothing`
is the difference between "this memory disagrees with you" and "this memory has
never heard that word", and only the second is a reason to search again with
different wording. The closing `also in these results:` line is the frequent
terms these hits share that were not asked for — the cheapest source of a better
second query, and how a caller learns that the corpus says `x-ray` where they
said `radiograph`. Both come back as lemmas.

## Search queries

`src/search.rs` — a search is **one string**, the same on every surface: `query`
in the JSON body and the MCP tool, the positional `<QUERY>` on the command line.
There is no list-of-groups form any more; a body that still sends `group_list`
is a `400` that shows the equivalent query, because a model holding an old
example should be told what replaced it rather than that `query` is missing.

- **tantivy's grammar parses it; borhan compiles it.** `query_grammar::parse_query`
  gives the tree, and `compile` turns every node into two things: a query for the
  driver, which finds candidates and enforces `+`, `-` and the filters without
  scoring, and a node of tantivy `Weight`s that the collector scores unit by unit.
  `QueryParser` is not used: it would add up every field a bare word matched on,
  and it has no tiers.
- **Top-level parts are what coverage counts.** A word, a phrase or a
  parenthesised clause is one idea; excluded parts are not counted. Inside a
  clause, `OR` and plain spaces take the best, `+`/`AND` add, `^N` multiplies. A
  leaf is the best of its probes — exact on `surface` (1.0), folded on `lemma`
  (0.9), a typo on `lemma` (0.5), borrowed on `context` (0.35) — so a phrase is
  scored as a phrase, by tantivy's phrase BM25, and not as its best word.
- **Outer parentheses are recovered from the text.** The grammar discards them,
  so `(a b)` and `a b` parse to one tree. A query wrapped whole in one pair,
  optionally after `+` or before `^N`, is one idea; everything else is split at
  the top level. Labels are rebuilt from the tree for the same reason, so
  `error OR fault` comes back as `(error OR fault)`.
- **What the grammar accepts and this refuses, each with a sentence naming the
  fix:** regular expressions, a `*` on one word (the grammar keeps `rot*` as the
  word `rot*`, the segmenter drops the `*`, and the search would silently be for
  `rot`), ranges, `*` alone and `field:*`, any field other than `surface`,
  `lemma` and `context`, and `session:`/`ts:`/`role:`, which stay request fields
  so a query never has to spell a role code. A parse failure re-runs the lenient
  parser only to say where it failed.
- **`fuzzy` expands only words the memory has never seen**, of five letters or
  more, to the lemmas one edit away (a transposition counts as one). The
  dictionary is walked by hand rather than through `FuzzyTermQuery`, because
  that query scores every expansion the same and names none of them, and
  `fuzzy_list` has to say which word was taken for which.
- **Caller-facing documentation teaches the syntax as borhan's own.** The guide,
  the tool schema and `--help` do not name tantivy or any other query language,
  and they say outright that regular expressions are not supported.

## Layout

```
src/main.rs        CLI, clap derive, logging setup, the HTTP client half. See the rule at the top.
src/api.rs         Every operation, and the axum router that serves them
src/mcp.rs         MCP over the HTTP server, at POST /mcp
src/guide.md       The REST guide, served verbatim at GET /. Not code — prose, include_str!'d
src/server.toml    The commented server.toml template `init server` fills in and writes
src/skills/*.md    The agent skills `skills <name> --install` writes out
src/storage.rs     SQLite: the memory directory, the schema, add/locate/around
src/index.rs       The tantivy schema, the writer and the reader
src/search.rs      The query syntax, coverage, scoring, snippets, hints
src/normalize.rs   NFC, ZWNJ, ک/ی folding, Persian affixes, Snowball English
src/ulid.rs        Ulid, the primary key everywhere
Makefile           Every build/check entry point. Use it, not cargo.
borhan.service     systemd unit template for `make systemd-install`
install.sh         The curl | sh installer for Linux and macOS; downloads what `make dist` built
install.ps1        The irm | iex installer for Windows; also adds the binary to the user PATH
Dockerfile         Alpine image for `serve`; first start writes a server.toml on 0.0.0.0:1995
.github/workflows  ci.yml runs `make all` on Linux and Windows, smoke-tests install.ps1 and
                   the image; release.yml runs `make dist` per target and pushes the
                   image to ghcr.io on a v* tag
README.md          For users: what borhan is, install, quick start
CONTRIBUTING.md    For contributors: build, gate, release
LICENSE            MIT
seed/              Corpus cloned by `make seed` (gitignored)
home/              BORHAN_HOME for `make seed` (gitignored)
build/             Named binaries from make dev/release (gitignored)
```

The non-Rust files under `src/` are there rather than in a `docs/` directory
because `include_str!` resolves relative to the file that calls it: they compile
into the binary, so a `serve` running from a copied binary on a machine with no
repository still serves its guide, writes its skills and can write a
`server.toml` with all of its comments intact.

`src/server.toml` is a template, not a config: `@LISTEN@`, `@TOKEN@` and
`@REFUSE@` are substituted by `init server`. It is not serialized from the
`Server` struct because serializing drops every comment, and the comments — what
the six operation names mean, which two destroy — are most of what the file is
for.

`main.rs` holds: `CommandLine`/`Command`/`InitCommand`/`MemoryCommand` (clap
derive), the `Server` configuration struct (serde derive), `logging_level()`,
`default_home_directory()` (with the vendored `dirs` logic inlined into it),
`read_configuration()`, `check_storage()`, `probe_server()`, the printers, and
`main()`.

`--home` defaults to `~/.borhan` (`$HOME` on Unix, `%USERPROFILE%` on Windows);
see the Home directory section for what lives inside it.

## Vendored code

The home-directory resolution inside `default_home_directory` is copied from
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
  are `thiserror` enums with `#[source]` (see `storage::Error`); they convert
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

No `#[cfg(test)]` blocks in `src/` — ever. All tests live in `tests/`, and there
are none yet, so `make test` currently proves only that the crate builds under
the test profile. Naming:
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
| *(none)* | `WARN` | — |
| `--info` | `INFO` | — |
| `--debug` | `DEBUG` | target |
| `--trace` | `TRACE` | target, file, line |

`--quiet` wins over `--trace`, which wins over `--debug`, which wins over
`--info`.

The default is `WARN`, so **the per-operation `info` line is off unless asked
for**. Every operation logs one, with its timings, and for a one-shot command
that is a second copy of the result already on stdout. A server wants it: the
systemd unit passes `--info`, and so does `make start-dev` by way of `--debug`.

### Level guide

| Level | When to use |
|-------|-------------|
| `info` | Important success event (storage created, index built, batch committed) |
| `warn` | Intentionally ignored error, or a fallback being taken |
| `error` | Failure the binary recovers from — a failure it does *not* recover from is returned from `main` as `Err(String)`, not logged |
| `debug` | Before attempting something important — include the inputs/params that affect the outcome |
| `trace` | After completing a low-level operation — include rich detail about what was produced |

### HTTP and operations

Every memory operation opens a named span (`List`, `Search`, `Cursor`, …) with
`op` (lowercase, for grouping), `trace` (the ULID), `memory` when there is one,
and the numeric timings that ran (`total_ms`, `search_ms`, `fetch_ms`, …). The
same numbers are on the matching `info` event. CLI and HTTP share this span.

HTTP adds a parent `Http` span (`op = "http"`) and one access-log event
`msg = "HTTP request"` with:

- `http_method`, `http_path` (the raw URI), `http_route` (the matched template,
  low cardinality), `http_query`
- `http_status` (numeric)
- `http_request_bytes`, `http_response_bytes` (`Content-Length`, else 0)
- `total_ms`, `trace`

2xx is `info`, 4xx is `warn`, 5xx is `error`. The CLI talking to `serve` emits
the same fields as `msg = "HTTP client request"` on a `Client` span, and records
the server's `trace` so the two processes join.

Span `NEW` and `CLOSE` events are enabled so busy/idle time is in the JSON too.

### Format rules

Every log call must start with a `msg` field whose value begins with a capital
letter. Additional structured fields follow as `key=value` pairs. Use `?` for
`Debug` (paths, options) and `%` for `Display`.

```rust
tracing::info!(msg = "Created storage directory", directory = ?settings.storage_directory);
tracing::warn!(msg = "Skipped unreadable memory file", path = ?path, error = %error);
tracing::debug!(msg = "Opened index", memory = %name, segments = segments.len());
tracing::trace!(msg = "Committed units", table = "unit", rows = written.units.len());
```

Because the default is `WARN` and `--quiet` sets `OFF`, never rely on a log line
to communicate something the user must see — return it from `main` or write it
to stdout. `info` and below are for an operator reading a journal, not for the
person who typed the command.
