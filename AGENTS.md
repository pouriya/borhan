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
| `make start-dev` | `make dev`, then runs `serve` with `--debug` |
| `make clippy` | `cargo clippy --all-targets --no-deps -- -D warnings` |
| `make check-style` | `cargo fmt --check` |
| `make fmt` | Rewrites formatting in place |
| `make check-arrow` | Fails if `Cargo.lock` has ≠1 arrow version, or a non-58 major |
| `make lint` | `clippy` + `check-style` + `check-arrow`, no build |
| `make test` | `cargo test --target …` |
| `make seed` | `seed-scan` + `seed-test`: fetch a corpus, scan it, search it |
| `make seed-fetch` | Clones the corpus into `seed/<name>/` (once; no-op if present) |
| `make seed-scan` | Wipes `home/`, inits it, adds every document as a message |
| `make seed-test` | `memory list`, three searches, and `memory get --json` of the top hit |
| `make seed-clean` | Drops `home/`, keeps the fetched corpus |
| `make clean` / `dist-clean` / `purge` | Drop `target/` / also `build/` / also `models/` and `seed/` |

`make model` is deliberately *not* part of `make all` — it is a 31 MB network
download, and only the directory-loader path needs it. The default embedder
requires nothing: its model is compiled into the binary.

### The seed corpus

`make seed` is the only thing that exercises storage on real volume: the
splitter, the vector index and search all behave differently at 24k rows than at
the handful a manual test types in.

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
take ~50s and produce ~11k paragraphs and ~24k sentences, well past the 1024
rows where the vector index gets built.

Two things the Makefile does that are not obvious:

- **`seed-scan` depends on `release`, not `dev`.** A debug build spends **1.6s
  of every invocation** loading the model against **0.17s** for release. Per
  document that is the difference between 1.8s and 0.2s, which is the difference
  between a coffee and an afternoon.
- **`--` goes before the text.** `memory add` takes the text as a positional,
  and a Markdown file that opens with a list item starts with `- `, which clap
  reads as a flag: `error: unexpected argument '- ' found`.

## Stack

| Crate | Version | Role |
|-------|---------|------|
| `model2vec-rs` | 0.2.1 | Static embeddings (no inference runtime, no GPU) |
| `lancedb` | 0.37.1 | Vector store — on-disk, columnar, ANN search |
| `rusqlite` | 0.40.2 | Metadata / keyword store, `bundled` SQLite |
| `tokio` | 1.53.1 | Async runtime (`lancedb` is async throughout) |
| `clap` | 4.6.6 | Command line, derive API |
| `tanzim` | 0.28.0 | Reads `server.toml`, with located errors |
| `toml_edit` | 0.22.27 | Writes `server.toml` in `init server` |
| `getrandom` | 0.4.3 | The 80 random bits of a ULID, straight from the OS |
| `chrono` | 0.4.45 | Renders `created_at` as ISO-8601 in `memory list`; no other date handling |
| `futures` | 0.3.31 | `TryStreamExt::try_next`, to read LanceDB's result stream |
| `pulldown-cmark` | 0.13.4 | Breaks stored Markdown into paragraphs and sentences |
| `serde_json` | 1.0.151 | Builds the `memory get --json` array; already in the tree |
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
- **`protoc` must be on `PATH` to build.** `lance-encoding`'s build script
  compiles `.proto` files and fails without it — `sudo apt-get install -y
  protobuf-compiler`, or `brew install protobuf`. Nothing borhan writes uses
  protobuf; it arrives through lancedb.

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

## Storage

`src/storage.rs` — `<home>/storage/`: `borhan.db`, and one
`embedding_<model>.lance` directory beside it per embedding model.

```rust
Storage::initialize(dir, model, dims) -> Result<Initialized, Error>  // the only creator
Storage::open(directory) -> Result<Storage, Error>              // open + schema, no mkdir
storage.create(name, description) -> Result<Ulid, Error>        // one row + its own table
storage.add(memory, &Entry) -> Result<Vec<Row>, Error>          // rows in that memory
storage.get(memory, &[Ulid]) -> Result<Vec<Record>, Error>      // rows back, text reassembled
storage.walk(memory, session, message, paragraph, from, count)  // the children of what you name
    -> Result<Vec<Record>, Error>                               // no ULID needed
storage.list() -> Result<Vec<Memory>, Error>                    // every row, oldest first
                                                                // Memory.name has no prefix
                                                                // Memory.counts per type
storage.create_vectors(model, dims) -> Result<bool, Error>      // called by initialize
storage.open_vectors(model) -> Result<Vectors, Error>           // opens; never creates
vectors.add(&[Vector]) -> Result<(), Error>                     // + indexes past the threshold
vectors.search(memory, kind, query, limit) -> Result<Vec<Hit>, Error>
```

**`Storage::initialize` is the one thing in borhan that creates anything.** It
makes the directory, the SQLite tables and the model's LanceDB table, each
"create if missing", so running it twice is running it once and running it after
a crash repairs whatever did not land. `main.rs` calls it once, from `init
storage`, and `Initialized { existing, vectors }` is what the printed message is
built from. Everything else opens with `Storage::open`, which will not make the
directory — a typo'd `--home` has to be an error naming the missing mount, never
a new empty store that silently remembers nothing.

```sql
CREATE TABLE IF NOT EXISTS memory (
    id          BLOB(16)      NOT NULL PRIMARY KEY,
    ulid        TEXT          NOT NULL,
    name        VARCHAR(47)   NOT NULL,
    description VARCHAR(2000),
    created_at  INTEGER       NOT NULL
);
```

Every memory also gets a table of its own, named by its `memory.name` — the row
and the table are created in one transaction, so neither exists without the
other:

```sql
CREATE TABLE memory_<name> (
    id           BLOB(16)      NOT NULL PRIMARY KEY,
    ulid         TEXT          NOT NULL,
    type         VARCHAR(9)    NOT NULL,   -- session | message | paragraph | sentence
    session_id   VARCHAR(64)   NOT NULL,
    message_id   VARCHAR(64),
    paragraph_id BLOB(16),
    sentence_id  BLOB(16),
    position     INTEGER       NOT NULL,
    content      VARCHAR(5000),
    postfix      VARCHAR(16),              -- the whitespace that followed it
    role         VARCHAR(9),               -- user | assistant
    role_name    VARCHAR(64),              -- model id, or the user's name
    created_at   INTEGER       NOT NULL
);
CREATE INDEX memory_<name>_session   ON memory_<name> (session_id, position);
CREATE INDEX memory_<name>_message   ON memory_<name> (message_id, position);
CREATE INDEX memory_<name>_paragraph ON memory_<name> (paragraph_id, position);
```

- **The three indexes are "the children of this row, in order"** — the messages
  of a session, the paragraphs of a message, the sentences of a paragraph. That
  is what `storage.walk` asks for and what reading a transcript back *is*, so
  without them every step of a cursor scans the whole memory. `position` is the
  second column so the ordering comes out of the index instead of a sort. Index
  names are database-wide, hence the table name in front. Nothing indexes `type`
  or `role`: `memory list` groups by them once per listing, and one scan for a
  listing is not worth a fourth index on every write.

- **`id` and `ulid` are the same value**, in blob and text form. The text one is
  there so that reading the table by hand does not mean decoding blobs; it is
  deliberately unindexed, and code reads `id` — nothing keeps the copy honest.
- **`created_at` is unix milliseconds**, taken from `id`'s own timestamp rather
  than from a second clock reading, so the two can never disagree. ISO-8601 with
  a `Z` is a rendering, and belongs at the CLI and API boundary — the table
  keeps the number that sorts and compares.
- **Every rule about the data is in `create`, none of it in SQL.** No `CHECK`,
  no `UNIQUE`; the `VARCHAR(n)` widths are documentation, since SQLite reads
  them as affinity and enforces nothing. Validate there or not at all.
- **A name is `a-z`, `0-9` and `_`, and unique.** It has to survive being used
  as a SQLite or LanceDB table name, where anything else needs quoting to be
  safe. Uniqueness is a `SELECT count(*)` before the insert — with nothing
  enforcing it in the database that is a race, and it is fine only because a
  single process owns the storage. The day two writers exist, add a `UNIQUE`
  index on `name`.
- **`create` stores `memory_` + the name it was given**, so the stored value
  *is* the table name and a caller can only ever name a table inside that
  namespace — never `memory` itself, never anything else in the database. It is
  also why a leading digit is allowed: `2024` is not an identifier, but
  `memory_2024` is. The 40-character limit is on what the caller passes, hence
  `VARCHAR(47)` on the column.
- **The prefix never leaves `storage.rs`.** `create` puts it on, `list` strips
  it back off, and everything outside — the CLI, the API, a future search —
  deals in the name the user typed. A row that is not under the prefix was not
  written by `create`, and `list` says so rather than guessing.
- **The implicit rowid stays.** An FTS5 index over `name`/`description` needs a
  rowid to point at (`content=memory`), so no `WITHOUT ROWID`.
- **`Storage::open` creates what is missing**, which is why the caller does the
  gating: `init storage` may call it, everything else calls `check_storage`
  first. See below.

### The per-memory table is a cursor

One row per session, message, paragraph and sentence, all four in the same
shape. Each row carries the ids of everything above it, and its own:

| `type` | `session_id` | `message_id` | `paragraph_id` | `sentence_id` | `content` |
|---|---|---|---|---|---|
| `session` | x | | | | |
| `message` | x | x | | | |
| `paragraph` | x | x | x | | |
| `sentence` | x | x | x | x | x |

- **LanceDB stores this table's `id`** against each embedding — sentences,
  paragraphs and messages are embedded, whole sessions are not — so a vector hit
  comes back into SQLite as `WHERE id = ?`, a primary-key lookup, at whatever
  level it matched. That row holds all four ids, and every walk starts there:
  the sentences of a paragraph share its `paragraph_id`, the paragraphs of a
  message share its `message_id`, and next/previous is `position` ± 1.
- **`position` is the reading order, not `id`.** A ULID is a millisecond plus 80
  random bits and there is no monotonic factory (see `src/ulid.rs`), so
  splitting a paragraph writes every sentence inside the same millisecond and
  `ORDER BY id` would be ordering the random halves. `position` counts from 0
  within the parent, and it survives re-ingesting a transcript. On a `session`
  row it is `0` — a session is not the nth of anything, and sessions arrive far
  enough apart for `id` to order them.
- **`paragraph_id` and `sentence_id` repeat `id` on their own row.** Redundant on
  purpose: everything belonging to a paragraph, the paragraph row included, is
  then one predicate — which is also all it takes to delete one.
- **`session_id` and `message_id` are text because they are somebody else's
  identifiers**, arriving with the transcript. 64 characters is a hyphenated
  UUID (36) with room for a feeder that does not use UUIDs.
- **Only `sentence` rows have `content`**, up to 5000 *characters* — a sentence
  is the unit that gets embedded, and a paragraph or message is read back by
  collecting its sentences. The original spacing and line breaks are not kept;
  reconstructing a transcript verbatim is not a goal.
- **No indexes yet**, same as `memory`. The walks above want
  `(session_id, position)`, `(message_id, position)` and
  `(paragraph_id, position)`; they land with the code that runs them.
- **No `IF NOT EXISTS` on the per-memory table.** The name was free a moment
  earlier, so a table already sitting under it is not one borhan made, and
  writing into columns nobody checked is worse than failing. The table name is
  interpolated, not bound — SQLite has no parameter for one — which is safe only
  because the name has already been reduced to `memory_` plus `[a-z0-9_]`.
- **`memory delete`, when it exists, must `DROP TABLE` in the same transaction**
  as the row delete, for the same reason `create` makes both at once — and
  delete the memory's rows from every `embedding_*` table.

### Writing into it: `storage.add`

One row per call, and **the parent is looked up, never described**:

| `--type` | needs | inherits from the parent row |
|---|---|---|
| `message` | `--session`, `--message`, `--role` | — |
| `paragraph` | `--message` | session, role, role name |
| `sentence` | `--paragraph` (a ULID) | session, message, role, role name |

- **An orphan is unwriteable.** There is no argument that could name a parent
  which is not there, so the ids on a row cannot contradict the ids above it —
  they are copied down, not supplied twice.
- **The `session` row is written for you**, by the first message naming a
  session that has none. A session has no text of its own, so it is never added
  directly and is not a `Kind`.
- **`position` is counted inside the transaction**, as the number of siblings
  already filed. Never supplied.
- **A duplicate `message_id` in one memory is refused**, because the paragraph
  lookup would otherwise resolve silently to whichever came first.
- **`content` is stored only on a `sentence` row, and nothing is lost by it.**
  Every text is broken all the way down, so the sentences under a paragraph
  *are* that paragraph and the sentences under a message are that message. A
  message and a paragraph are still embedded whole — that is what lets a search
  answer at the grain the question was asked at — but what is kept is the
  sentences.
- **A row under `MINIMUM_WORDS` (5) is written but not embedded.** `Row.embed`
  says which, and the CLI builds vectors only for those. A two-word row is not
  an answer to anything and is actively harmful in a ranking: a vector built
  from two tokens sits close to every query mentioning either of them, so `}`
  and `Compiler` and `// code` take the places a sentence saying something
  would have had. Measured on the 24,343 sentences of rust-lang/rfcs, a third of
  every top ten was a row under this line, and 8,699 rows — 36% — now need no
  vector at all. What is lost is finding a heading or a stray line of code by
  searching for it alone; the paragraph holding it is still embedded, still
  found, and reads back with that line in it. **A message is always embedded**,
  however short: one that short is somebody saying "yes", which is little to
  search for but is still the thing that was said.
- **The row goes in before the vector**, and they are not one transaction —
  SQLite and LanceDB cannot be. That order decides how a crash between them
  breaks: a row with no vector is invisible to search but still reachable by
  walking the cursor, while a vector with no row would put an id into search
  results that resolves to nothing.

### Reading it back: `storage.get`

`get(memory, &[Ulid]) -> Vec<Record>` is what turns a search result into
something a person can read, because **a hit is a ULID and a ULID says
nothing**. Both `memory get` and `memory search` go through it.

Only a sentence stores its own content, so the layers above it are reassembled
from the sentences underneath, each followed by its own `postfix`:

| Kind | Where its text comes from |
|------|---------------------------|
| `sentence` | its own `content` column |
| `paragraph` | its sentences and their postfixes, `ORDER BY position` |
| `message` | every sentence under it, `ORDER BY paragraph.position, sentence.position` |

**That two-level ordering on a message is the reason `position` exists.** A
sentence's `position` counts from zero inside its *own* paragraph, so ordering a
whole message by `position` alone would interleave the paragraphs, and ordering
by `id` would scramble any split that happened inside one millisecond — which is
every split, since `add` writes a whole message in one go. The join goes through
the paragraph row to get the outer key.

Two decisions to know about:

- **Sentences are joined by their `postfix`**, so the text comes back with the
  shape it went in with: code as lines, a heading above the prose it labels,
  blank lines between the paragraphs of a message. It is also byte for byte the
  string `add` built the paragraph's vector from, so what `get` prints is still
  what the search actually matched on. A sentence with no postfix — one added on
  its own, outside `split` — gets a space, and the trailing postfix is trimmed,
  since it is the gap to whatever comes after what was asked for.
- **Ids that are not there are left out, not raised.** A search resolving its
  own hits would otherwise lose a whole page because one vector outlived its
  row; `memory get` compares what it asked for against what it got and names
  each miss on stderr. **Session rows are not addressable** — they hold no text
  and are never embedded, so the query filters to the three real layers.

### Walking it: `storage.walk`

`walk(memory, session, message, paragraph, from, count) -> Vec<Record>` is the
other way in, and **the one that needs no ULID**. A search hands back ids;
everything else a reader wants — the message before this one, the rest of this
document, what a session actually holds — is "the children of something I can
name", and the names are the feeder's own.

| Given | What comes back |
|-------|-----------------|
| `session` | the messages of that session |
| `session` + `message` | the paragraphs of that message |
| `paragraph` (a ULID) | the sentences of that paragraph |

Each answer is the layer under the deepest thing named. A paragraph needs no
session, since a ULID is already unique across the memory. `from` and `count`
are a window counted in `position`, and **a `from` past the end is an empty
result, not an error** — walking off the end is how a reader finds out where the
end is.

- **It collects ids and then calls `get`**, rather than being a second, wider
  query. Reassembling a row out of the rows under it is the whole of what `get`
  does, and doing it twice is how two readers start disagreeing about what a
  paragraph's text is.
- **The parent is bound as a `rusqlite::types::Value`**, not as bytes. The three
  parent columns are not one type — a paragraph id is a blob, the feeder's two
  are text — and SQLite compares a blob to a string as *unequal* rather than as
  an error, so binding the wrong one silently finds nothing.

### Splitting: what a paragraph and a sentence are

**Stored text is read as Markdown**, because what borhan is fed is a chat
transcript and that is what those are written in. A **paragraph is a CommonMark
block** — a paragraph, one list item, one table row, a fenced code block — not a
run between blank lines. Splitting on blank lines would run a bullet list into
one lump and cut a code block wherever the code happened to breathe; both are
worse things to embed than what the author actually wrote.

- **A heading joins the block under it**, and is the one exception to
  block-per-paragraph. `## Motivation` on its own answers no question and sits
  close to every query that says "motivation"; in front of the prose it labels,
  it says what that prose is about — to a reader and to a vector alike. Two
  headings in a row both join the first block with words in it. A heading with
  nothing under it is a paragraph of its own.
- **Every sentence stores the whitespace that followed it**, its `postfix`: a
  space inside a paragraph, a newline between lines of code, a blank line
  between blocks, as many blank lines as were actually written. Content plus
  postfix, concatenated, is the text back with its shape. Only the whitespace is
  kept — everything else between two blocks is the next one's markup — and it is
  capped at `POSTFIX_LIMIT` (16) characters. What is not recovered: the markup
  that was dropped, the line wrapping inside a paragraph (which CommonMark
  itself calls insignificant), and the very first prefix, the `#` of a heading
  or the `- ` of the first list item, which nothing needs to read the text back.
- **Markup is dropped, not stored.** `**bold**` is `bold`, a link is its text.
  Nothing reaching a model or a screen has a bracket the author did not type.
- **Code blocks keep their literal lines**, indentation included — in code the
  punctuation *is* the content. One sentence per line, blank lines dropped, and
  one paragraph per `CODE_LINES` (20) lines: a screenful, long enough that a
  function usually lands whole and short enough that a 500-line file does not
  become one vector answering every question about it equally. A 45-line block
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
- **A table row is one sentence**, cells joined with ` | `. Not a sentence each:
  `lancedb` alone is half a fact, `lancedb | vectors` answers something. The
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

`Ulid::parse` reads the 26-character form back, case-insensitively — a ULID that
has been through a shell or a copy-paste may arrive lowercased. `I`, `L` and `O`
are **not** folded onto `1` and `0`: the alphabet omits them so a human does not
misread one aloud, and accepting the mistake would file a row under an id nobody
can type twice. A first character above `7` is refused, since 26 characters
carry 130 bits and only 3 of the leading 5 fit.

### Counting: what `memory list` shows

`Memory.counts` is filled by one `SELECT type, role, count(*) … GROUP BY type,
role` per memory. That is a scan of each table, since nothing indexes `type` —
fine for a listing on a terminal; the day it is not, the fix is an index on
`(type, role)` or a counts row the writer keeps current, not a cleverer query.

- **An unrecognised `type` is an error; an unrecognised `role` is not.** A fifth
  `type` means the row is not one of the four things a memory is made of. A
  fifth speaker is odd but legible, so it counts in `messages` and in neither
  share — **which is why the two percentages need not add up to 100**, and why
  neither can be derived as the other's complement.
- **The heading goes to stderr**, the rows to stdout, for the same reason the
  "no memories" sentence does: a pipe reads nothing but data, a terminal still
  gets told what the columns are.
- A memory with no messages shows `-` for the share rather than `0%/0%`, which
  would read as a fact about who spoke.

### Vectors: one LanceDB table per model

`embedding_<model>`, holding every memory's vectors for that one model. A new
model is a new table beside the old one, so migrating is re-embedding at leisure
with nothing dropped, and two models can be compared side by side.

| column | type | |
|---|---|---|
| `memory` | `Utf8` | the name the user gave, no `memory_` prefix |
| `type` | `Utf8` | `message`, `paragraph` or `sentence` — see `Kind` |
| `id` | `Utf8` | 26-character ULID of the row in `memory_<memory>` |
| `vector` | `FixedSizeList<Float32, D>` | `D` is the model's `dimensions()` |

- **`session` is not a `Kind`.** Sessions are containers; there is no text that
  *is* one, and a whole day's conversation embedded as a single vector would
  come back for every query.
- **`type` earns its place at query time.** A sentence, the paragraph holding it
  and the message holding that are three vectors of nearly the same words. Left
  unfiltered they take three of the top results — visibly, in testing. `--type`
  picks the granularity.
- **`id` is text, not the 16 bytes.** LanceDB filters are SQL expression
  strings, and `id = '01M08...'` is something you can write in one. The `ulid`
  column in SQLite exists for the same reason.
- **The model name is taken verbatim, `[A-Za-z0-9_-]` only.** `potion-retrieval-32M`
  keeps its hyphens and capitals; `.` and `/` are refused, which also stops a
  `--model` directory name from walking out of the storage directory. Lowercasing
  would collide `potion-base-8M` with `potion_base_8m`.
- **Indexes are built lazily, past `INDEX_THRESHOLD` (1024 rows), all three at
  once.** They cannot be made with the table: PQ trains 256 centroids and lance
  refuses below that with `Not enough rows to train PQ`. 1024 rather than 256
  because a flat scan of a few hundred vectors beats an approximate lookup
  anyway. Verified by dropping the constant to 256, loading 300 rows and
  watching three index directories appear under `_indices/`.
- **Cosine, said twice.** `IvfPqIndexBuilder` *and* the query default to L2, and
  a table too small to have an index is scanned flat through the second path —
  so leaving either unsaid picks the wrong metric for some queries and not
  others. The model normalises its output, so cosine and dot rank identically.
- **The scalar `BTree` indexes on `memory` and `type` are not decoration.** A
  search is always filtered to one memory; an approximate vector index answers a
  filtered query by walking partitions, so a memory that is a small slice of the
  table has its rows scattered and real hits get missed. With the scalar index
  the filter resolves first.
- **Rows written after the index exists are still found**, by scanning the
  unindexed tail — verified: a row added after indexing came back as the top
  hit. They are not *in* the index until an `optimize`, which nothing calls yet.
- **`lancedb::Error` is boxed in the error enum.** It is over 130 bytes, and
  clippy's `result_large_err` fires on returning it by value; every `Result` in
  the module would carry that width on the success path too.

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

Commands that touch no storage (`embedding load`, `embedding do` — model-only
work) run fine without `init`.

### Local vs HTTP

`server.toml` is the only configuration file. `init server --listen HOST:PORT [--token T]` writes it (`create_new`, mode `0600`). `serve` reads it to bind; if the file is missing it binds `127.0.0.1:1995` with no token. Every memory command probes that listen address (`GET /api/v1/health`, about 1s). If the file is missing, `listen` is unset, or the server does not answer, the command opens storage itself.

A token in `server.toml` is required on every HTTP request as `Authorization: Bearer`. The same file is what the CLI sends.

Every HTTP response sets `Server: borhan/<version>` and `X-Borhan-Version` to the crate version (`Cargo.toml`). The CLI compares that header to its own version. A mismatch prints prettified JSON and does not try to format it. A match with `--json` decodes, re-encodes and pretty-prints the body. A match without `--json` prints the usual text. `X-Trace-Id` is shown only when the server answers 4xx or 5xx.

Both `serve` and the CLI read the file with [`tanzim`](https://docs.rs/tanzim) into `Server`, via `read_configuration`. The helper formats tanzim's error with `{:#}` — that is the form carrying source, line, column and the caret; wrapping it as a `#[source]` throws all of it away. `init server` serializes the same `Server` struct back out with `toml_edit`.

## CLI

Logging flags and `--home` are `global = true`, so they work before or after a
subcommand. **A subcommand is required** — there is no implicit server.

```
borhan init                             # == borhan init storage
borhan init storage                     # create ~/.borhan and ~/.borhan/storage
borhan init server --listen H:P \
                  [--token T]           # write ~/.borhan/server.toml (0600)
borhan serve                            # HTTP API, address from server.toml
borhan memory create <NAME> \
                  --description T       # store a memory, print its ULID
                  [--languages L]       # description is required, more than 10 words
                  [--json]
borhan memory                           # == borhan memory list
borhan memory list [--json]             # ULID, created, name, counts, languages, description
borhan memory update <NAME> \
                  [--description T] [--languages L] [--json]
borhan memory add <NAME> <TEXT> \
                  --session S [--message M]
                  [--role user|assistant|tool] [--author N] [--ts MS]
                  [--json]              # split into units, index, print the message ULID
borhan memory get <NAME> <ULID>... \
                  [--json]              # units back, in the order asked
borhan memory search <NAME> <GROUPS>... \
                  [--limit N] [--json]  # concept groups, coverage, cursor
borhan memory cursor <NAME> <ULID> \
                  [--before N] [--after N] [--json]
borhan memory lexicon <NAME> <WORDS>... [--json]
borhan memory rescan <NAME> [--json]
```

`--json` prints the same wrapped object the HTTP API returns, pretty-printed.
Text mode does not print stats on stdout.

`memory add` reads the text as Markdown, splits it into units, writes them to
SQLite and the tantivy index, prints the message ULID on stdout and the unit
count on stderr.

`memory get` takes unit ULIDs and prints them back in the order asked. An id
that resolves to nothing is named on stderr, one line each.

The same command **walks** when given `--session`, `--message` or `--paragraph`
instead of ULIDs, which is how you read *around* a hit rather than only at it:
`--session S` gives that session's messages, `--session S --message M` gives
that message's paragraphs, `--paragraph ULID` gives that paragraph's sentences.
`--from` and `--count` are a window in reading order, defaulting to `0` and
`20`, because a session can hold a whole corpus and a message a whole document.
**ULIDs and walking are mutually exclusive** and asking for both is an error
naming the two things it could have meant — validated in Rust rather than with
a clap group, so the message can say what to do instead.

**Every hit carries the feeder's `session` and `message` identifiers**, whatever
layer it is: a paragraph and a sentence inherit both from the message they were
split out of, and `add` requires them on every message — which, over the CLI, is
every `add`, since `--type` defaults to `message`. Those two columns are the
join back to whatever fed borhan in the first place, so a result can be turned
into the surrounding conversation without a second query here. They are sized to
their contents, like `memory list`'s columns, because a session id is a UUID in
one deployment and a filename in another.

`memory search` prints one line per hit —
`score  type  id  session  message  paragraph  sentence  words  text` — followed
by a blank line, because a hundred words of preview wraps and without the gap a
ranking reads as one block of text. **All four ids go out**, so a row says where
it sits without a second query; a `-` is a layer the hit is *above*, not one
that is missing, so a paragraph has no sentence and a message has neither.

The text is cut at `PREVIEW_WORDS` (100) with an `…` — enough that a sentence
and most paragraphs arrive whole, few enough that a message, which is a whole
document, does not bury the hits under it. **The word count is the size of the
whole row, not of the preview.** The preview keeps the spacing it was stored
with, with `\n`, `\r` and `\t` escaped, so a code block still reads as a code
block while the hit stays one line and the columns stay lined up.

**The first row is the nearest**, said explicitly with a sort rather than taken
on trust: LanceDB returns one batch per partition, and the floor, the duplicate
check and the page all treat the first row they see as the best one.

Three things narrow what gets printed, in this order:

- **`--max-distance D` drops anything further than `D`.** No default, because
  what counts as far depends on the model and on what is stored — on
  rust-lang/rfcs with the built-in model, right answers landed at 0.13–0.28 and
  beginner-phrased misses at a median of 0.50, so a floor near 0.45 separates
  them. Everything being past the floor prints its own sentence, which is not
  the same as the memory being empty and does not say so.
- **The same text twice is one answer**, however many rows hold it. A corpus
  repeats a line of code, a licence header, a stock sentence; each copy is its
  own row with its own vector. The nearest copy survives, since the sort already
  put it first.
- **`OVERFETCH` (3) × `--limit` is what LanceDB is actually asked for**, so the
  rows those two steps throw away come out of the surplus instead of out of the
  page. Asking for exactly `--limit` and then dropping some is how a full page
  turns into four rows.

The header goes to stderr and the rows to stdout, as in `memory
list`, so a pipe reads results and nothing else. When the
memory has nothing stored under that model, stdout stays empty and the sentence
goes to stderr. An empty result really does mean empty: there is no
distance floor, so a search returns everything it has up to `--limit`. Searching
with a different `--model` than you stored with finds nothing — the vectors are
in another table — and that is what the message says.

`memory create` prints the ULID on stdout and nothing else, so it pipes.

`memory list` prints one aligned line per memory and nothing else; with no
memories stdout stays empty and the sentence goes to stderr, so a pipe reads an
empty list rather than prose. The description column is the first line of the
description, cut at `SUMMARY_LIMIT` characters with a `…` — a listing is a
summary, and one memory has to stay one row.

`do` is a Rust keyword, hence `#[command(name = "do")]` on the `Do` variant.

## Layout

```
src/main.rs                          CLI + runtime setup. See the rule at the top.
src/ulid.rs                          Ulid, the primary key everywhere
src/storage.rs                       SQLite + LanceDB: the storage directory, both stores
src/embedding/mod.rs                 Embedding trait, both impls, Error
src/embedding/potion-retrieval-32M/  Committed model, embedded via include_bytes!
Makefile                             Every build/check entry point. Use it, not cargo.
scripts/fetch-model.sh               Fetches a model dir (via `make model`)
scripts/fetch-seed.sh                Clones the seed corpus (via `make seed-fetch`)
models/                              Test-fixture models (gitignored)
seed/                                Corpus cloned by `make seed` (gitignored)
home/                                BORHAN_HOME for `make seed` (gitignored)
build/                               Named binaries from make dev/release (gitignored)
```

`main.rs` holds: `CommandLine`/`Command`/`InitCommand`/`MemoryCommand`/
`EmbeddingCommand` (clap derive), the `Remote`/`Server` configuration structs
(serde derive), `logging_level()`,
`default_home_directory()` (with the vendored `dirs` logic inlined into it),
`read_configuration()`, `check_storage()`, and `main()`.

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
tracing::debug!(msg = "Embedding batch", count = texts.len(), dimensions = DIM);
tracing::trace!(msg = "Committed vectors", table = "memory", rows = batch.num_rows());
```

Because `--quiet` sets `OFF`, never rely on a log line to communicate something
the user must see — return it from `main` or write it to stdout.
