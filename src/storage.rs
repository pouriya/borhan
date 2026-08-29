//! The local store: `<home>/storage/`, holding the SQLite database and, beside
//! it, one LanceDB table per embedding model.
//!
//! [`Storage::initialize`] is the only thing here that makes a storage — the
//! directory, the SQLite tables and the model's vector table, all of it "create
//! if missing" so that it can be run twice or run again after a crash.
//! Everything else goes through [`Storage::open`], which will not make the
//! directory. That is the whole of "nothing is created implicitly": an agent
//! with the wrong `--home` gets an error naming the missing mount instead of a
//! new, empty store that silently remembers nothing.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow_array::types::Float32Type;
use arrow_array::{Array, FixedSizeListArray, Float32Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use futures::TryStreamExt;
use lancedb::DistanceType;
use lancedb::index::Index;
use lancedb::index::scalar::BTreeIndexBuilder;
use lancedb::index::vector::IvfPqIndexBuilder;
use lancedb::query::{ExecutableQuery, QueryBase};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use rusqlite::Connection;

use crate::ulid::Ulid;

/// The one database file. LanceDB tables will land beside it, in the same
/// directory, under their own name.
const DATABASE: &str = "borhan.db";

/// Longest name a caller may hand to [`Storage::create`], in characters. What
/// lands in the column is this plus [`NAME_PREFIX`].
const NAME_LIMIT: usize = 40;

/// Put in front of every name before it is stored, so that the stored value
/// *is* the table name and every table borhan makes on a memory's behalf is
/// under this one namespace. Without it a memory called `memory` would name the
/// table this schema already owns, and nothing about the charset rule would
/// stop it.
const NAME_PREFIX: &str = "memory_";

/// Longest `memory.description`, in characters.
const DESCRIPTION_LIMIT: usize = 2000;

/// Longest `content` in a memory's own table, in characters.
///
/// Characters, not bytes, which is the same rule the two limits above use: a
/// byte limit would cut a Persian sentence at half the length of an English one
/// for no reason a writer could see.
const CONTENT_LIMIT: usize = 5000;

/// Lines of a code block that make one paragraph.
///
/// Code has no sentences to find in it, so a line is the sentence and this is
/// how many of them are held to be about one thing. A screenful: long enough
/// that a function usually lands whole, short enough that a 500-line file does
/// not become one vector that answers every query about it equally.
const CODE_LINES: usize = 20;

/// Longest `postfix` in a memory's own table, in characters.
///
/// A postfix is the whitespace one row was followed by, so this only has to be
/// long enough for the gaps a writer actually leaves. Sixteen newlines is
/// already a lot of them, and a run longer than that carries no more meaning
/// than the run that gets kept.
const POSTFIX_LIMIT: usize = 16;

/// Words a row needs before it is worth a vector.
///
/// A row is always written; this only decides whether one is embedded. A two-
/// word row is not an answer to anything, and worse than useless in a ranking:
/// a vector built from two tokens sits close to every query that mentions
/// either of them, so `}` and `Compiler` and `// code` take places that a
/// sentence saying something would have had. Measured on 24,343 sentences of
/// rust-lang/rfcs, a third of every top ten was a row under this line.
///
/// What is lost is the ability to find a heading or a stray line of code by
/// searching for it alone. The paragraph holding it is still embedded, still
/// found, and reads back with that line in it, which is the better answer
/// anyway.
const MINIMUM_WORDS: usize = 5;

/// Put in front of a model's name to make the LanceDB table its vectors live
/// in. One table per model is the whole migration story: a new model is a new
/// table beside the old one, embeddings are rebuilt into it at leisure, and
/// nothing has to be dropped to try one out.
///
/// Not [`NAME_PREFIX`]: that one means "the rows of one memory", and a model's
/// table is the opposite shape — every memory's vectors, one model.
const EMBEDDING_PREFIX: &str = "embedding_";

/// The column the embeddings themselves live in.
const VECTOR_COLUMN: &str = "vector";

/// Rows in a model's table before borhan indexes it.
///
/// LanceDB will not index an empty table — product quantisation trains 256
/// centroids and refuses with "Not enough rows to train PQ" below that — so the
/// index cannot be made when the table is. It is built by the first
/// [`Vectors::add`] that takes the table over this line instead. 1024 rather
/// than the 256 minimum because under a few thousand rows a flat scan of the
/// whole column beats an approximate lookup, so indexing earlier would cost
/// recall and buy nothing.
const INDEX_THRESHOLD: usize = 1024;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Could not create storage directory {path:?}")]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("Could not open SQLite database {path:?}")]
    Open {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },

    #[error("Could not create the schema in SQLite database {path:?}")]
    Schema {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },

    #[error("Memory name is {characters} characters, and it must be 1 to {NAME_LIMIT}")]
    NameLength { characters: usize },

    #[error("Memory name {name:?} contains {character:?}, and only a-z, 0-9 and _ are allowed")]
    NameCharacter { name: String, character: char },

    #[error("Memory {name:?} already exists")]
    Duplicate { name: String },

    #[error(
        "Memory description is {characters} characters, and it must be at most {DESCRIPTION_LIMIT}"
    )]
    Description { characters: usize },

    #[error("Could not look up memory {name:?} in SQLite database {path:?}")]
    Lookup {
        name: String,
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },

    #[error("Could not make an identifier for the memory")]
    Identifier {
        #[source]
        source: crate::ulid::Error,
    },

    #[error("Could not open a transaction in SQLite database {path:?}")]
    Transaction {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },

    #[error("Could not create table {name:?} in SQLite database {path:?}")]
    Table {
        name: String,
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },

    #[error("Could not insert the memory into SQLite database {path:?}")]
    Insert {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },

    #[error("Could not read the memories out of SQLite database {path:?}")]
    List {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },

    #[error("Memory {name:?} has a {length}-byte id, and a ULID is 16 bytes")]
    Corrupt { name: String, length: usize },

    #[error("Memory {name:?} is not stored under the {NAME_PREFIX:?} prefix that `create` writes")]
    Prefix { name: String },

    #[error("Could not count what is in table {table:?} of SQLite database {path:?}")]
    Count {
        table: String,
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },

    #[error(
        "Table {table:?} has a row of type {kind:?}, and a row is a session, a message, a paragraph or a sentence"
    )]
    Layer { table: String, kind: String },

    #[error("No memory named {name:?}")]
    Unknown { name: String },

    #[error("Adding a {kind} needs {field}")]
    Needs {
        kind: &'static str,
        field: &'static str,
    },

    #[error("Content is {characters} characters, and it must be 1 to {CONTENT_LIMIT}")]
    Content { characters: usize },

    #[error("Memory {memory:?} has no message {message:?} to hang a {kind} on")]
    NoMessage {
        memory: String,
        message: String,
        kind: &'static str,
    },

    #[error("Memory {memory:?} has no paragraph {paragraph} to hang a sentence on")]
    NoParagraph { memory: String, paragraph: Ulid },

    #[error("Memory {memory:?} already has a message {message:?}")]
    DuplicateMessage { memory: String, message: String },

    #[error("Model name is empty, and it has to name a LanceDB table")]
    ModelEmpty,

    #[error(
        "Model name {model:?} contains {character:?}, and only a-z, A-Z, 0-9, _ and - are allowed"
    )]
    ModelCharacter { model: String, character: char },

    #[error("Storage directory {path:?} is not valid UTF-8, and LanceDB is addressed by a URI")]
    PathEncoding { path: PathBuf },

    // `lancedb::Error` is over 130 bytes on its own, and every `Result` in this
    // module would carry that width on the success path too, so the LanceDB
    // sources below are boxed. The failures are all cold — a missing table, a
    // refused write — and one allocation on the way out of them is nothing.
    #[error("Could not open LanceDB in {path:?}")]
    Connect {
        path: PathBuf,
        #[source]
        source: Box<lancedb::Error>,
    },

    #[error("Could not list the LanceDB tables in {path:?}")]
    Tables {
        path: PathBuf,
        #[source]
        source: Box<lancedb::Error>,
    },

    #[error("Could not create LanceDB table {name:?} in {path:?}")]
    CreateTable {
        name: String,
        path: PathBuf,
        #[source]
        source: Box<lancedb::Error>,
    },

    // Says "the same --model" rather than naming one: what reaches here is the
    // model's own name, and what the user typed may have been the directory it
    // was loaded from, so any argument spelled out here would be a guess.
    #[error(
        "Storage {path:?} has no LanceDB table {name:?}: nothing has been embedded here with model {model}. Run `borhan init storage` with the same `--model` to make one."
    )]
    NoTable {
        name: String,
        model: String,
        path: PathBuf,
    },

    #[error("Could not open LanceDB table {name:?} in {path:?}")]
    OpenTable {
        name: String,
        path: PathBuf,
        #[source]
        source: Box<lancedb::Error>,
    },

    #[error("Could not read the schema of LanceDB table {name:?}")]
    TableSchema {
        name: String,
        #[source]
        source: Box<lancedb::Error>,
    },

    #[error(
        "LanceDB table {name:?} has no {VECTOR_COLUMN:?} column of fixed-width floats, so it was not made by borhan"
    )]
    TableShape { name: String },

    #[error(
        "Embedding for {id} is {dimensions} numbers and LanceDB table {name:?} holds {expected}: that is a different model"
    )]
    Dimensions {
        id: String,
        name: String,
        dimensions: usize,
        expected: usize,
    },

    #[error("Could not build a record batch for LanceDB table {name:?}")]
    Batch {
        name: String,
        #[source]
        source: Box<arrow_schema::ArrowError>,
    },

    #[error("Could not write {count} embeddings to LanceDB table {name:?}")]
    Add {
        count: usize,
        name: String,
        #[source]
        source: Box<lancedb::Error>,
    },

    #[error("Could not index column {column:?} of LanceDB table {name:?}")]
    IndexColumn {
        name: String,
        column: String,
        #[source]
        source: Box<lancedb::Error>,
    },

    #[error("Could not search LanceDB table {name:?}")]
    Search {
        name: String,
        #[source]
        source: Box<lancedb::Error>,
    },

    #[error("Search of LanceDB table {name:?} returned no usable {column:?} column")]
    Column { name: String, column: String },
}

/// One row of the `memory` table.
#[derive(Debug, Clone)]
pub struct Memory {
    pub id: Ulid,

    /// As the caller gave it to [`Storage::create`], with the [`NAME_PREFIX`]
    /// taken back off. The prefix exists to keep table names in one namespace,
    /// and that is nothing a reader has to look at.
    pub name: String,
    pub description: Option<String>,
    /// Milliseconds since the Unix epoch; the same instant as `id`'s timestamp.
    pub created_at: i64,

    /// What is in the memory's own table.
    pub counts: Counts,
}

/// How much a memory holds, counted out of its own table.
#[derive(Debug, Clone, Copy, Default)]
pub struct Counts {
    pub sessions: u64,
    pub messages: u64,
    pub paragraphs: u64,
    pub sentences: u64,

    /// Messages whose `role` is `assistant`, and whose `role` is `user`.
    ///
    /// These need not add up to `messages`. `role` is not constrained by the
    /// schema — nothing in SQLite is — so a row written by something other than
    /// borhan can carry a third value or none at all, and it is counted in
    /// `messages` and in neither of these. A reader showing the two as
    /// percentages should expect them to fall short of 100 rather than assume
    /// one is `messages` minus the other.
    pub assistant: u64,
    pub user: u64,
}

/// Which layer of a transcript an embedding covers.
///
/// The `type` column of a memory's table has a fourth value, `session`, which
/// is not here on purpose: a session is a container, there is no text that *is*
/// one, and embedding the whole of a day's conversation would return it for
/// every query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Message,
    Paragraph,
    Sentence,
}

impl Kind {
    /// What goes in the column, and what a filter compares against.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Paragraph => "paragraph",
            Self::Sentence => "sentence",
        }
    }

    /// Read one back, from a CLI flag or an API field.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "message" => Some(Self::Message),
            "paragraph" => Some(Self::Paragraph),
            "sentence" => Some(Self::Sentence),
            _ => None,
        }
    }
}

/// Who spoke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "user" => Some(Self::User),
            "assistant" => Some(Self::Assistant),
            _ => None,
        }
    }
}

/// One row on its way into a memory's own table.
///
/// Which fields are needed depends on `kind`, and each layer is anchored to the
/// one above it:
///
/// - `Message` needs `session`, `message` and `role`. It is the only kind that
///   names a session, and adding one to a session nothing has mentioned yet
///   writes the `session` row too.
/// - `Paragraph` needs `message`, and takes the session, the role and the role
///   name off that message's row.
/// - `Sentence` needs `paragraph`, and takes everything off that paragraph's
///   row.
///
/// So a row cannot be written without its parent already being there, and the
/// ids on it cannot disagree with the ids above it — they are copied down, not
/// supplied twice.
#[derive(Debug, Clone)]
pub struct Entry {
    pub kind: Kind,

    /// The feeder's session identifier. Only read for a `Message`.
    pub session: Option<String>,

    /// The feeder's message identifier: which message this is, for a
    /// `Message`, or which one it belongs to, for a `Paragraph`.
    pub message: Option<String>,

    /// The paragraph a `Sentence` belongs to.
    pub paragraph: Option<Ulid>,

    /// Only read for a `Message`, where it is required.
    pub role: Option<Role>,

    /// The model's identifier or the user's name. Only read for a `Message`.
    pub role_name: Option<String>,

    /// The text, as Markdown. Broken into paragraphs and sentences by
    /// [`Storage::add`] unless `kind` is `Sentence`, which is taken as written.
    pub content: String,
}

/// One row [`Storage::add`] wrote, and the text a vector for it is made of.
///
/// The text is returned rather than read back out of the table because only a
/// sentence row keeps its content: a message's is the whole of what came in and
/// a paragraph's is its sentences joined, and both are wanted for embedding
/// even though neither is stored.
#[derive(Debug, Clone)]
pub struct Row {
    pub id: Ulid,
    pub kind: Kind,
    pub text: String,

    /// Whether a vector should be made of `text`, which is false for a row
    /// under [`MINIMUM_WORDS`]. The row is written either way — reassembling
    /// the paragraph above it needs it, and so does walking the cursor — it
    /// just does not become something a search can land on directly.
    pub embed: bool,
}

/// One row read back by [`Storage::get`], with its text put back together.
///
/// Only a sentence keeps its content, so `text` is reassembled for the layers
/// above it: a paragraph's is its sentences in `position` order, and a
/// message's is every sentence under it, ordered by its paragraph first and by
/// itself second. That two-level ordering is the reason `position` exists — a
/// message's sentences all number from zero inside their own paragraph, so
/// sorting them by `position` alone would interleave the paragraphs, and
/// sorting by `id` would scramble a split that happened inside one millisecond.
#[derive(Debug, Clone)]
pub struct Record {
    pub id: Ulid,
    pub kind: Kind,

    /// The feeder's identifiers, as they were handed to [`Storage::add`].
    pub session: String,
    pub message: Option<String>,

    /// The paragraph a sentence hangs off. `None` above that layer.
    pub paragraph: Option<Ulid>,

    /// Reading order among siblings: paragraphs within their message,
    /// sentences within their paragraph, messages within their session.
    pub position: i64,

    pub role: Option<Role>,
    pub role_name: Option<String>,

    /// Unix milliseconds, the same value the `id` leads with.
    pub created_at: i64,

    /// What the row says, whether or not the row is what stores it.
    pub text: String,
}

/// One embedding on its way into a model's table.
#[derive(Debug, Clone)]
pub struct Vector {
    /// The memory it belongs to, named as the user named it — no
    /// [`NAME_PREFIX`], which never leaves this module.
    pub memory: String,

    pub kind: Kind,

    /// The row in `memory_<memory>` this was embedded from. Stored as the
    /// 26-character text rather than the 16 bytes, because LanceDB filters are
    /// SQL strings and a text literal is something you can write in one.
    pub id: Ulid,

    pub embedding: Vec<f32>,
}

/// One result of [`Vectors::search`].
///
/// No `memory` field: a search is filtered to one memory, so it would be the
/// name the caller passed in, handed back.
#[derive(Debug, Clone)]
pub struct Hit {
    /// As stored. Not parsed back into a [`Kind`]: it is on its way to a screen
    /// or to a SQL predicate, and neither needs it typed.
    pub kind: String,

    /// The 26-character ULID of the row in `memory_<memory>`.
    pub id: String,

    /// Cosine distance, so 0 is identical and smaller is closer.
    pub distance: f32,
}

/// What [`Storage::initialize`] found already there, and what it had to make.
#[derive(Debug, Clone, Copy)]
pub struct Initialized {
    /// The storage directory was there before the call.
    pub existing: bool,

    /// The model's LanceDB table was made by this call.
    pub vectors: bool,
}

/// An open storage directory.
pub struct Storage {
    /// The directory itself: LanceDB is addressed by it, and it is what the
    /// SQLite errors name.
    directory: PathBuf,

    /// Only for error messages; the connection knows its own path.
    database: PathBuf,
    connection: Connection,
}

impl Storage {
    /// Make a storage directory, or finish making one that is half there:
    /// the directory, the SQLite database and its tables, and the LanceDB
    /// table for the model whose vectors it will hold.
    ///
    /// Every step is "create if missing", so running this twice is running it
    /// once, and running it after a crash repairs whatever did not land.
    /// [`Initialized`] says which parts were actually made, for the caller to
    /// report.
    ///
    /// This is the only thing in borhan that creates anything. Every other
    /// entry point goes through [`Storage::open`], which fails on a directory
    /// that is not there — otherwise a typo'd `--home` would quietly answer
    /// with an empty store instead of naming the mount that is missing.
    pub async fn initialize<P: AsRef<Path>>(
        directory: P,
        model: &str,
        dimensions: usize,
    ) -> Result<Initialized, Error> {
        let directory = directory.as_ref();
        // Read before anything is made, because afterwards there is no way to
        // tell "I just made this" from "it was already here".
        let existing = directory.is_dir();
        if let Err(source) = fs::create_dir_all(directory) {
            return Err(Error::CreateDirectory {
                path: directory.to_path_buf(),
                source,
            });
        }

        // Opening is what makes the database file and, in `open`, its tables.
        let storage = Self::open(directory)?;
        let vectors = storage.create_vectors(model, dimensions).await?;
        Ok(Initialized { existing, vectors })
    }

    /// Open `<directory>/borhan.db`, creating the file and the tables if they
    /// are missing — but not the directory. That one belongs to
    /// [`Storage::initialize`], so that opening a storage which was never made
    /// is an error rather than a new empty one.
    pub fn open<P: AsRef<Path>>(directory: P) -> Result<Self, Error> {
        let directory = directory.as_ref();
        let database = directory.join(DATABASE);
        let connection = match Connection::open(&database) {
            Ok(connection) => connection,
            Err(source) => {
                return Err(Error::Open {
                    path: database,
                    source,
                });
            }
        };

        // No `CHECK` and no `UNIQUE`: every rule about what a name may look
        // like, and whether one is already taken, lives in `create` and only
        // there. The widths in `VARCHAR(47)` are documentation — SQLite reads
        // them as affinity and enforces nothing — so the table describes the
        // shape and `create` is what holds it to it. 47 is the 40 characters a
        // caller may pass plus the `memory_` `create` puts in front of them.
        //
        // The table keeps its implicit rowid. An FTS5 index over `name` and
        // `description` needs one to point at (`content=memory`), and that is
        // the next thing to land here.
        let schema = "
            CREATE TABLE IF NOT EXISTS memory (
                id          BLOB(16)      NOT NULL PRIMARY KEY,
                ulid        TEXT          NOT NULL,
                name        VARCHAR(47)   NOT NULL,
                description VARCHAR(2000),
                created_at  INTEGER       NOT NULL
            );
        ";
        if let Err(source) = connection.execute_batch(schema) {
            return Err(Error::Schema {
                path: database,
                source,
            });
        }

        Ok(Self {
            directory: directory.to_path_buf(),
            database,
            connection,
        })
    }

    /// Store a new memory, give it its own table, and return the ULID it was
    /// filed under. The row and the table are one transaction: a memory
    /// without a table, or a table nothing knows about, is not a state
    /// anything downstream has to handle.
    ///
    /// `name` is `a-z`, `0-9` and `_`, and unique across the table: it
    /// identifies the memory to a human, and it has to survive being used as a
    /// SQLite or LanceDB table name, where anything else would need quoting to
    /// be safe. What goes in the column is [`NAME_PREFIX`] followed by what was
    /// passed, so a caller can only ever name a table inside that namespace.
    pub fn create(&self, name: &str, description: Option<&str>) -> Result<Ulid, Error> {
        check_name(name)?;
        if let Some(description) = description {
            let characters = description.chars().count();
            if characters > DESCRIPTION_LIMIT {
                return Err(Error::Description { characters });
            }
        }

        // The name as it is stored, which is also the name of any table this
        // memory gets later. The prefix is what keeps a caller out of the rest
        // of the database, and it doubles as the reason a leading digit is
        // allowed here: `2024` on its own is not a SQLite identifier, but
        // `memory_2024` is.
        let stored = format!("{NAME_PREFIX}{name}");

        // Last, because it is the only rule that costs a query. Nothing in the
        // database enforces it, so it is a race between two writers by
        // construction — one process is what borhan is built around, though:
        // the server owns the storage, and the CLI either owns it or talks to
        // that server. Add a `UNIQUE` index on `name` the day two things can
        // write at once.
        let taken = self.connection.query_row(
            "SELECT count(*) FROM memory WHERE name = ?1",
            [&stored],
            |row| row.get::<_, i64>(0),
        );
        match taken {
            Ok(0) => {}
            Ok(_) => {
                return Err(Error::Duplicate {
                    name: name.to_string(),
                });
            }
            Err(source) => {
                return Err(Error::Lookup {
                    name: name.to_string(),
                    path: self.database.clone(),
                    source,
                });
            }
        }

        let id = match Ulid::new() {
            Ok(id) => id,
            Err(source) => return Err(Error::Identifier { source }),
        };
        // Milliseconds since the Unix epoch, read back out of the ULID rather
        // than from a second clock reading, so the column can never disagree
        // with the key beside it. The cast is lossless — a ULID timestamp is 48
        // bits — and SQLite has no unsigned integer to store it as anyway.
        // Rendering it as ISO-8601 is the CLI's and the API's job, not the
        // table's: sorting, `BETWEEN` and arithmetic all want the number.
        let created_at = id.milliseconds() as i64;

        // The row and the memory's own table go in together. Either half on its
        // own is a state every later command would have to know about: a memory
        // that cannot be written to, or a table `memory list` never mentions.
        let transaction = match self.connection.unchecked_transaction() {
            Ok(transaction) => transaction,
            Err(source) => {
                return Err(Error::Transaction {
                    path: self.database.clone(),
                    source,
                });
            }
        };

        // The memory's own table: one row per session, message, paragraph and
        // sentence, all four in the same shape, so that a hit coming back from
        // LanceDB can be walked in any direction without a join.
        //
        // Every row carries the ids of everything above it, and its own:
        //
        //     type       session_id  message_id  paragraph_id  sentence_id  content
        //     session    x
        //     message    x           x
        //     paragraph  x           x           x
        //     sentence   x           x           x             x            x
        //
        // so a row *is* a cursor. LanceDB stores this table's `id` against each
        // embedding — sentences, paragraphs and messages get one, whole sessions
        // do not — which makes the way in a primary key lookup at any level.
        // From that row: the sentences of a paragraph share its `paragraph_id`,
        // the paragraphs of a message share its `message_id`, and the next or
        // previous one is `position` ± 1.
        //
        // `position` is which sentence of the paragraph, paragraph of the
        // message or message of the session this is, counted from 0, and it is
        // what the reading order actually is. `id` cannot be: a ULID is a
        // millisecond plus 80 random bits, splitting a paragraph writes all its
        // sentences inside one millisecond, and `ORDER BY id` would then be the
        // order of the random halves. On a `session` row it is 0 — a session is
        // not the nth of anything, and sessions arrive far enough apart that
        // `id` orders them.
        //
        // `paragraph_id` and `sentence_id` are ULIDs, and on the row that *is*
        // the paragraph or the sentence they repeat its `id`. Redundant on
        // purpose: everything belonging to a paragraph, the paragraph included,
        // is then one predicate — which is also what deleting one takes.
        //
        // `session_id` and `message_id` are text, because they are somebody
        // else's identifiers: they come in with the transcript. 64 characters is
        // room for a UUID and its hyphens, which is 36, and for whatever a
        // feeder that does not use UUIDs hands over instead.
        //
        // Only sentences hold `content`, since a sentence is the unit that gets
        // embedded; a paragraph or a message is read back by collecting them.
        //
        // `postfix` is what the author wrote *after* that content and before
        // whatever came next: a space, a newline, a blank line, as many blank
        // lines as they left. It is what makes collecting them lossless.
        // Without it a paragraph read back is its sentences with a space
        // between them, which turns a code block into one line and a heading
        // into the first words of the prose under it; with it, the text comes
        // back shaped the way it went in. Only the whitespace is kept, because
        // everything else between two blocks is the next one's markup, and
        // markup is not what a row stores. What is lost is the very first
        // prefix — the `#` of a heading, the `- ` of the first list item —
        // which nothing needs to read the text back.
        //
        // `role` and `role_name` say who spoke and, if it was a model, which
        // one; a `session` row has neither.
        //
        // Three indexes, one per "the children of this row, in order": the
        // messages of a session, the paragraphs of a message, the sentences of
        // a paragraph. That is what [`Storage::walk`] asks for and what reading
        // a transcript back *is*, so without them every step of a cursor is a
        // scan of the whole memory. Each carries `position` as its second
        // column so the ordering comes out of the index rather than a sort.
        // Named after the table because index names are database-wide.
        //
        // Nothing indexes `type` or `role`: `memory list` groups by them once
        // per listing, and one scan for a listing is not worth a fourth index
        // on every write. Widths, the `type` and `role` vocabularies and which
        // columns a given `type` fills are documentation: SQLite enforces none
        // of it, and the rules live in the Rust that writes the rows.
        //
        // Interpolated rather than bound because a table name cannot be a
        // parameter in SQLite. It is safe because `stored` is `memory_` and the
        // characters the loop above let through, so there is nothing in it to
        // quote or to escape. No `IF NOT EXISTS`: the name was free a moment
        // ago, so a table already sitting there is not one borhan made, and
        // writing rows into columns nobody has looked at is worse than stopping.
        let table = format!(
            "
            CREATE TABLE {stored} (
                id           BLOB(16)      NOT NULL PRIMARY KEY,
                ulid         TEXT          NOT NULL,
                type         VARCHAR(9)    NOT NULL,
                session_id   VARCHAR(64)   NOT NULL,
                message_id   VARCHAR(64),
                paragraph_id BLOB(16),
                sentence_id  BLOB(16),
                position     INTEGER       NOT NULL,
                content      VARCHAR(5000),
                postfix      VARCHAR(16),
                role         VARCHAR(9),
                role_name    VARCHAR(64),
                created_at   INTEGER       NOT NULL
            );
            CREATE INDEX {stored}_session   ON {stored} (session_id, position);
            CREATE INDEX {stored}_message   ON {stored} (message_id, position);
            CREATE INDEX {stored}_paragraph ON {stored} (paragraph_id, position);
        "
        );
        if let Err(source) = transaction.execute_batch(&table) {
            return Err(Error::Table {
                name: stored,
                path: self.database.clone(),
                source,
            });
        }

        // `id` is the key everything joins on; `ulid` is the same value in the
        // text form, carried along so that reading the table by hand does not
        // mean decoding blobs. Deliberately unindexed.
        let statement = "
            INSERT INTO memory (id, ulid, name, description, created_at)
            VALUES (?1, ?2, ?3, ?4, ?5)
        ";
        if let Err(source) = transaction.execute(
            statement,
            rusqlite::params![
                &id.bytes()[..],
                id.to_string(),
                stored,
                description,
                created_at
            ],
        ) {
            return Err(Error::Insert {
                path: self.database.clone(),
                source,
            });
        }
        if let Err(source) = transaction.commit() {
            return Err(Error::Transaction {
                path: self.database.clone(),
                source,
            });
        }
        Ok(id)
    }

    /// Break a text into a memory's own table and return every row written,
    /// each with the text a vector for it should be made of.
    ///
    /// What arrives is treated as **Markdown**, because what borhan is fed is a
    /// chat transcript and that is what those are written in. A CommonMark
    /// parser is what tells a fenced code block from a bullet list from a run
    /// of prose, and splitting on blank lines cannot: it would glue a heading to
    /// the paragraph under it, cut a code block wherever the code happened to
    /// breathe, and run a whole list together into one unsearchable lump.
    ///
    /// The parent is looked up rather than described: a paragraph is given a
    /// message id and inherits the session, the role and the role name off that
    /// message's row; a sentence is given a paragraph and inherits all of it.
    /// That is what makes an orphan unwriteable — there is no argument that
    /// could name a parent which is not there — and it is why the ids on a row
    /// cannot contradict the ids above it.
    ///
    /// `kind` says which layer the text is being attached at, and the splitting
    /// happens below it: a `Message` becomes a message row, the paragraphs
    /// under it and the sentences under those; a `Paragraph` becomes whatever
    /// paragraphs its text holds, with their sentences, hung off an existing
    /// message; a `Sentence` is taken as written and split no further.
    ///
    /// `position` is counted here too, so rows land in the order they arrive
    /// and `ORDER BY position` reads the transcript back — which `ORDER BY id`
    /// cannot, because one paragraph's sentences are all written inside the same
    /// millisecond and a ULID has nothing but random bits to separate them.
    ///
    /// **Only sentence rows keep content**, and nothing is lost by it: every
    /// text is broken all the way down, so the sentences under a paragraph are
    /// that paragraph, and the sentences under a message are that message.
    pub fn add(&self, memory: &str, entry: &Entry) -> Result<Vec<Row>, Error> {
        check_name(memory)?;
        if entry.content.trim().is_empty() {
            return Err(Error::Content { characters: 0 });
        }

        let table = self.table(memory)?;

        // Everything the row needs that was not passed in, read off the parent.
        // `session` and `message` end up holding the feeder's identifiers for
        // every kind, so the inserts below do not care which one it is.
        let (session, message, role, role_name) = match entry.kind {
            Kind::Message => {
                let session = match &entry.session {
                    Some(session) => session.clone(),
                    None => {
                        return Err(Error::Needs {
                            kind: "message",
                            field: "a session id",
                        });
                    }
                };
                let message = match &entry.message {
                    Some(message) => message.clone(),
                    None => {
                        return Err(Error::Needs {
                            kind: "message",
                            field: "a message id",
                        });
                    }
                };
                // Required, not defaulted: a message nobody spoke is not a
                // thing, and a guess here would quietly skew the shares that
                // `memory list` reports.
                let role = match entry.role {
                    Some(role) => role,
                    None => {
                        return Err(Error::Needs {
                            kind: "message",
                            field: "a role",
                        });
                    }
                };

                // Two messages under one id would make the paragraph lookup
                // below ambiguous, and it resolves silently to whichever came
                // first — so it is refused here instead.
                let query = format!(
                    "SELECT count(*) FROM {table} WHERE type = 'message' AND message_id = ?1"
                );
                let existing = self
                    .connection
                    .query_row(&query, [&message], |row| row.get::<_, i64>(0));
                match existing {
                    Ok(0) => {}
                    Ok(_) => {
                        return Err(Error::DuplicateMessage {
                            memory: memory.to_string(),
                            message,
                        });
                    }
                    Err(source) => {
                        return Err(Error::Count {
                            table,
                            path: self.database.clone(),
                            source,
                        });
                    }
                }
                (session, message, Some(role), entry.role_name.clone())
            }

            Kind::Paragraph => {
                let message = match &entry.message {
                    Some(message) => message.clone(),
                    None => {
                        return Err(Error::Needs {
                            kind: "paragraph",
                            field: "the message id it belongs to",
                        });
                    }
                };
                let query = format!(
                    "SELECT session_id, role, role_name FROM {table}
                     WHERE type = 'message' AND message_id = ?1"
                );
                let parent = self.connection.query_row(&query, [&message], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                });
                match parent {
                    Ok((session, role, role_name)) => {
                        let role = match role {
                            Some(role) => Role::parse(&role),
                            None => None,
                        };
                        (session, message, role, role_name)
                    }
                    Err(rusqlite::Error::QueryReturnedNoRows) => {
                        return Err(Error::NoMessage {
                            memory: memory.to_string(),
                            message,
                            kind: "paragraph",
                        });
                    }
                    Err(source) => {
                        return Err(Error::Count {
                            table,
                            path: self.database.clone(),
                            source,
                        });
                    }
                }
            }

            Kind::Sentence => {
                let paragraph = match entry.paragraph {
                    Some(paragraph) => paragraph,
                    None => {
                        return Err(Error::Needs {
                            kind: "sentence",
                            field: "the paragraph it belongs to",
                        });
                    }
                };
                // A sentence is stored as written, so this is the one path
                // where the limit is a refusal rather than somewhere to cut.
                let characters = entry.content.chars().count();
                if characters > CONTENT_LIMIT {
                    return Err(Error::Content { characters });
                }
                // By `id`, not by `paragraph_id`: on a paragraph's own row the
                // two are the same value, and `id` is the primary key.
                let query = format!(
                    "SELECT session_id, message_id, role, role_name FROM {table}
                     WHERE type = 'paragraph' AND id = ?1"
                );
                let parent = self
                    .connection
                    .query_row(&query, [&paragraph.bytes()[..]], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, Option<String>>(1)?,
                            row.get::<_, Option<String>>(2)?,
                            row.get::<_, Option<String>>(3)?,
                        ))
                    });
                match parent {
                    Ok((session, message, role, role_name)) => {
                        let message = match message {
                            Some(message) => message,
                            // A paragraph row always has one; this is a row
                            // written by something other than `add`.
                            None => {
                                return Err(Error::NoParagraph {
                                    memory: memory.to_string(),
                                    paragraph,
                                });
                            }
                        };
                        let role = match role {
                            Some(role) => Role::parse(&role),
                            None => None,
                        };
                        (session, message, role, role_name)
                    }
                    Err(rusqlite::Error::QueryReturnedNoRows) => {
                        return Err(Error::NoParagraph {
                            memory: memory.to_string(),
                            paragraph,
                        });
                    }
                    Err(source) => {
                        return Err(Error::Count {
                            table,
                            path: self.database.clone(),
                            source,
                        });
                    }
                }
            }
        };

        // Split before the transaction opens: it reads nothing from the
        // database, and a text that turns out to hold nothing worth storing
        // should say so without having taken a write lock to find out.
        let paragraphs = match entry.kind {
            Kind::Sentence => Vec::new(),
            _ => split(&entry.content),
        };
        if entry.kind != Kind::Sentence && paragraphs.is_empty() {
            // Markdown that is all structure and no words — a horizontal rule,
            // an empty list — leaves nothing to embed.
            return Err(Error::Content { characters: 0 });
        }

        let statement = format!(
            "INSERT INTO {table}
                 (id, ulid, type, session_id, message_id, paragraph_id,
                  sentence_id, position, content, postfix, role, role_name,
                  created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)"
        );
        let transaction = match self.connection.unchecked_transaction() {
            Ok(transaction) => transaction,
            Err(source) => {
                return Err(Error::Transaction {
                    path: self.database.clone(),
                    source,
                });
            }
        };

        let mut written = Vec::new();

        // The session row, if this is the first message of one. Nothing else
        // creates it: a session has no text of its own, so it is never added
        // directly, and without a row here a cursor walking up from a message
        // would have nothing to land on. It gets no vector either — see [`Kind`].
        if entry.kind == Kind::Message {
            let query =
                format!("SELECT count(*) FROM {table} WHERE type = 'session' AND session_id = ?1");
            let sessions = transaction.query_row(&query, [&session], |row| row.get::<_, i64>(0));
            let sessions = match sessions {
                Ok(sessions) => sessions,
                Err(source) => {
                    return Err(Error::Count {
                        table,
                        path: self.database.clone(),
                        source,
                    });
                }
            };
            if sessions == 0 {
                let id = match Ulid::new() {
                    Ok(id) => id,
                    Err(source) => return Err(Error::Identifier { source }),
                };
                // Position 0: a session is not the nth of anything. Role and
                // role name are left out too — a session has no speaker.
                let session_row = transaction.execute(
                    &statement,
                    rusqlite::params![
                        &id.bytes()[..],
                        id.to_string(),
                        "session",
                        &session,
                        None::<String>,
                        None::<Vec<u8>>,
                        None::<Vec<u8>>,
                        0,
                        None::<String>,
                        None::<String>,
                        None::<String>,
                        None::<String>,
                        id.milliseconds() as i64
                    ],
                );
                if let Err(source) = session_row {
                    return Err(Error::Insert {
                        path: self.database.clone(),
                        source,
                    });
                }
            }
        }

        // Which sibling the top row is. Counted inside the transaction, so the
        // number cannot be stale by the time the row lands.
        let (query, sibling) = match entry.kind {
            Kind::Message => (
                format!("SELECT count(*) FROM {table} WHERE type = 'message' AND session_id = ?1"),
                session.clone(),
            ),
            Kind::Paragraph => (
                format!(
                    "SELECT count(*) FROM {table} WHERE type = 'paragraph' AND message_id = ?1"
                ),
                message.clone(),
            ),
            Kind::Sentence => (
                format!(
                    "SELECT count(*) FROM {table} WHERE type = 'sentence' AND paragraph_id = ?1"
                ),
                // Bound as a blob below rather than through this string.
                String::new(),
            ),
        };
        let counted = match entry.paragraph {
            Some(paragraph) if entry.kind == Kind::Sentence => {
                transaction.query_row(&query, [&paragraph.bytes()[..]], |row| row.get::<_, i64>(0))
            }
            _ => transaction.query_row(&query, [&sibling], |row| row.get::<_, i64>(0)),
        };
        let mut position = match counted {
            Ok(position) => position,
            Err(source) => {
                return Err(Error::Count {
                    table,
                    path: self.database.clone(),
                    source,
                });
            }
        };

        // A sentence added on its own: one row, the text as it was given.
        if entry.kind == Kind::Sentence {
            let id = match Ulid::new() {
                Ok(id) => id,
                Err(source) => return Err(Error::Identifier { source }),
            };
            let paragraph = match entry.paragraph {
                Some(paragraph) => paragraph,
                None => {
                    return Err(Error::Needs {
                        kind: "sentence",
                        field: "the paragraph it belongs to",
                    });
                }
            };
            let row = transaction.execute(
                &statement,
                rusqlite::params![
                    &id.bytes()[..],
                    id.to_string(),
                    Kind::Sentence.as_str(),
                    &session,
                    Some(&message),
                    paragraph.bytes().to_vec(),
                    id.bytes().to_vec(),
                    position,
                    Some(entry.content.as_str()),
                    // A space, because nothing here knows what will follow it:
                    // a sentence added on its own has no next block to read a
                    // gap from, and a space is what keeps it off the words of
                    // whatever lands after it.
                    Some(" "),
                    role.map(|role| role.as_str()),
                    &role_name,
                    id.milliseconds() as i64
                ],
            );
            if let Err(source) = row {
                return Err(Error::Insert {
                    path: self.database.clone(),
                    source,
                });
            }
            written.push(Row {
                id,
                kind: Kind::Sentence,
                text: entry.content.clone(),
                embed: entry.content.split_whitespace().count() >= MINIMUM_WORDS,
            });
        } else {
            // A message row first, when that is the layer being attached at.
            // Its text is the whole of what came in: a message-level vector is
            // meant to answer "which message was this discussed in", so it is
            // embedded whole even though what is *kept* is its sentences.
            if entry.kind == Kind::Message {
                let id = match Ulid::new() {
                    Ok(id) => id,
                    Err(source) => return Err(Error::Identifier { source }),
                };
                let row = transaction.execute(
                    &statement,
                    rusqlite::params![
                        &id.bytes()[..],
                        id.to_string(),
                        Kind::Message.as_str(),
                        &session,
                        Some(&message),
                        None::<Vec<u8>>,
                        None::<Vec<u8>>,
                        position,
                        None::<String>,
                        None::<String>,
                        role.map(|role| role.as_str()),
                        &role_name,
                        id.milliseconds() as i64
                    ],
                );
                if let Err(source) = row {
                    return Err(Error::Insert {
                        path: self.database.clone(),
                        source,
                    });
                }
                written.push(Row {
                    id,
                    kind: Kind::Message,
                    text: entry.content.clone(),
                    // A message is always embedded. It is a whole turn of a
                    // conversation, and one that short is somebody saying
                    // "yes" -- which is little to search for but is still the
                    // thing that was said.
                    embed: true,
                });
                // Paragraphs of a brand new message start at 0; paragraphs
                // added to an existing one carry on from what it already has,
                // which is what `position` already holds.
                position = 0;
            }

            for paragraph in &paragraphs {
                let id = match Ulid::new() {
                    Ok(id) => id,
                    Err(source) => return Err(Error::Identifier { source }),
                };
                // Content and postfix, straight through: the paragraph's vector
                // is built from the same string [`Storage::get`] hands back, so
                // what a search matched on is what a reader is shown. The last
                // postfix is the gap to the next paragraph and belongs between
                // them, not at the end of this one.
                let mut text = String::new();
                for (sentence, postfix) in paragraph {
                    text.push_str(sentence);
                    text.push_str(postfix);
                }
                let text = text.trim_end().to_string();
                let row = transaction.execute(
                    &statement,
                    rusqlite::params![
                        &id.bytes()[..],
                        id.to_string(),
                        Kind::Paragraph.as_str(),
                        &session,
                        Some(&message),
                        // Its own id, repeated in the column naming its layer,
                        // so "everything under this paragraph" stays one
                        // predicate.
                        id.bytes().to_vec(),
                        None::<Vec<u8>>,
                        position,
                        None::<String>,
                        None::<String>,
                        role.map(|role| role.as_str()),
                        &role_name,
                        id.milliseconds() as i64
                    ],
                );
                if let Err(source) = row {
                    return Err(Error::Insert {
                        path: self.database.clone(),
                        source,
                    });
                }
                let words = text.split_whitespace().count();
                written.push(Row {
                    id,
                    kind: Kind::Paragraph,
                    text,
                    embed: words >= MINIMUM_WORDS,
                });
                position += 1;

                for (index, (sentence, postfix)) in paragraph.iter().enumerate() {
                    let sentence_id = match Ulid::new() {
                        Ok(sentence_id) => sentence_id,
                        Err(source) => return Err(Error::Identifier { source }),
                    };
                    let row = transaction.execute(
                        &statement,
                        rusqlite::params![
                            &sentence_id.bytes()[..],
                            sentence_id.to_string(),
                            Kind::Sentence.as_str(),
                            &session,
                            Some(&message),
                            id.bytes().to_vec(),
                            sentence_id.bytes().to_vec(),
                            index as i64,
                            Some(sentence.as_str()),
                            Some(postfix.as_str()),
                            role.map(|role| role.as_str()),
                            &role_name,
                            sentence_id.milliseconds() as i64
                        ],
                    );
                    if let Err(source) = row {
                        return Err(Error::Insert {
                            path: self.database.clone(),
                            source,
                        });
                    }
                    written.push(Row {
                        id: sentence_id,
                        kind: Kind::Sentence,
                        text: sentence.clone(),
                        embed: sentence.split_whitespace().count() >= MINIMUM_WORDS,
                    });
                }
            }
        }

        if let Err(source) = transaction.commit() {
            return Err(Error::Transaction {
                path: self.database.clone(),
                source,
            });
        }
        Ok(written)
    }

    /// The table a memory lives in, once the name is known to be well formed
    /// and the memory known to be there.
    ///
    /// Both readers of a memory's own table need the same three things, and
    /// the check has to happen before the name is interpolated into SQL, which
    /// is the whole reason [`check_name`] exists.
    fn table(&self, memory: &str) -> Result<String, Error> {
        check_name(memory)?;
        let table = format!("{NAME_PREFIX}{memory}");
        let taken = self.connection.query_row(
            "SELECT count(*) FROM memory WHERE name = ?1",
            [&table],
            |row| row.get::<_, i64>(0),
        );
        match taken {
            Ok(0) => Err(Error::Unknown {
                name: memory.to_string(),
            }),
            Ok(_) => Ok(table),
            Err(source) => Err(Error::Lookup {
                name: memory.to_string(),
                path: self.database.clone(),
                source,
            }),
        }
    }

    /// Read rows back by id, in the order they were asked for.
    ///
    /// This is what turns a search result into something a person can read: a
    /// hit is a ULID, and a ULID says nothing.
    ///
    /// Ids that are not there are left out rather than reported. The two
    /// callers both do better with that than with an error — a search resolving
    /// its own hits would otherwise lose a whole page because one vector
    /// outlived its row, and `memory get` can see it asked for more than it
    /// got. Session rows are not addressable here: they hold no text and are
    /// never embedded, so there is nothing to hand back.
    ///
    /// The reassembled text of a paragraph is its sentences with their
    /// postfixes put back — which is exactly the string its vector was built
    /// from in [`Storage::add`], so what this prints is what the search
    /// actually matched on, down to the whitespace. A code block comes back as
    /// lines, a heading comes back above the prose it labels, and paragraphs of
    /// a message come back with the blank lines between them.
    pub fn get(&self, memory: &str, ids: &[Ulid]) -> Result<Vec<Record>, Error> {
        let table = self.table(memory)?;

        let query = format!(
            "SELECT type, session_id, message_id, paragraph_id, position,
                    content, role, role_name, created_at
               FROM {table}
              WHERE id = ?1 AND type IN ('message', 'paragraph', 'sentence')"
        );
        let sentences_of_paragraph = format!(
            "SELECT content, postfix FROM {table}
              WHERE type = 'sentence' AND paragraph_id = ?1
              ORDER BY position"
        );
        // Two levels of ordering, through the paragraph row: a sentence's
        // `position` counts from zero inside its own paragraph, so ordering a
        // whole message by it alone would interleave the paragraphs.
        let sentences_of_message = format!(
            "SELECT sentence.content, sentence.postfix
               FROM {table} AS sentence
               JOIN {table} AS paragraph
                 ON paragraph.id = sentence.paragraph_id
                AND paragraph.type = 'paragraph'
              WHERE sentence.type = 'sentence' AND sentence.message_id = ?1
              ORDER BY paragraph.position, sentence.position"
        );

        let mut records = Vec::with_capacity(ids.len());
        for id in ids {
            let found = self.connection.query_row(&query, [&id.bytes()[..]], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<Vec<u8>>>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, i64>(8)?,
                ))
            });
            let (kind, session, message, paragraph, position, content, role, role_name, created_at) =
                match found {
                    Ok(row) => row,
                    Err(rusqlite::Error::QueryReturnedNoRows) => continue,
                    Err(source) => {
                        return Err(Error::Lookup {
                            name: memory.to_string(),
                            path: self.database.clone(),
                            source,
                        });
                    }
                };

            let kind = match Kind::parse(&kind) {
                Some(kind) => kind,
                None => return Err(Error::Layer { table, kind }),
            };
            // `BLOB(16)` is affinity and not a rule, so a column written by
            // anything other than `add` can be the wrong width.
            let paragraph = match paragraph {
                Some(bytes) => {
                    let length = bytes.len();
                    match <[u8; 16]>::try_from(bytes) {
                        Ok(bytes) => Some(Ulid::from_bytes(bytes)),
                        Err(_) => {
                            return Err(Error::Corrupt {
                                name: memory.to_string(),
                                length,
                            });
                        }
                    }
                }
                None => None,
            };
            let role = match role {
                Some(role) => match Role::parse(&role) {
                    Some(role) => Some(role),
                    None => return Err(Error::Layer { table, kind: role }),
                },
                None => None,
            };

            // Only a sentence stores what it says; the layers above it are put
            // back together out of the sentences underneath.
            let text = match kind {
                Kind::Sentence => content.unwrap_or_default(),
                Kind::Paragraph => self.sentences(&sentences_of_paragraph, [&id.bytes()[..]])?,
                Kind::Message => match &message {
                    Some(message) => self.sentences(&sentences_of_message, [message])?,
                    None => String::new(),
                },
            };

            records.push(Record {
                id: *id,
                kind,
                session,
                message,
                paragraph,
                position,
                role,
                role_name,
                created_at,
                text,
            });
        }
        Ok(records)
    }

    /// Read the children of one row, in the order they were written.
    ///
    /// This is the other way in, and the one that does not need a ULID. A
    /// search hands back ids; everything else a reader wants — the message
    /// before this one, the rest of this document, what a session actually
    /// holds — is "the children of something I can name", and the names are
    /// the feeder's own. Which layer comes back is decided by how much is
    /// given, so that each answer is the layer below the deepest thing named:
    ///
    /// | Given | What comes back |
    /// |-------|-----------------|
    /// | `session` | the messages of that session |
    /// | `session` and `message` | the paragraphs of that message |
    /// | `paragraph` | the sentences of that paragraph |
    ///
    /// A paragraph is named by ULID and needs no session, since a ULID is
    /// already unique across the memory.
    ///
    /// `from` and `count` are a window on that list, counted in `position` —
    /// which is the reading order and the reason the column exists. A `from`
    /// past the end is an empty result and not an error: walking off the end of
    /// a document is how a reader finds out where it ends.
    pub fn walk(
        &self,
        memory: &str,
        session: Option<&str>,
        message: Option<&str>,
        paragraph: Option<Ulid>,
        from: i64,
        count: i64,
    ) -> Result<Vec<Record>, Error> {
        let table = self.table(memory)?;

        // Ids first and the text after, through [`Storage::get`], rather than
        // one wider query: reassembling a row out of the rows under it is the
        // whole of what `get` does, and doing it twice is how the two readers
        // start disagreeing about what a paragraph's text is.
        //
        // The parent is a `Value` and not bytes, because the three columns are
        // not one type: a paragraph id is a blob and the feeder's two are text,
        // and SQLite compares a blob to a string as unequal rather than as an
        // error — so binding the wrong one finds nothing and says nothing.
        let (query, parent) = match (paragraph, session, message) {
            (Some(paragraph), _, _) => (
                format!(
                    "SELECT id FROM {table}
                      WHERE type = 'sentence' AND paragraph_id = ?1
                      ORDER BY position LIMIT ?2 OFFSET ?3"
                ),
                rusqlite::types::Value::Blob(paragraph.bytes().to_vec()),
            ),
            (None, Some(_), Some(message)) => (
                format!(
                    "SELECT id FROM {table}
                      WHERE type = 'paragraph' AND message_id = ?1
                      ORDER BY position LIMIT ?2 OFFSET ?3"
                ),
                rusqlite::types::Value::Text(message.to_string()),
            ),
            (None, Some(session), None) => (
                format!(
                    "SELECT id FROM {table}
                      WHERE type = 'message' AND session_id = ?1
                      ORDER BY position LIMIT ?2 OFFSET ?3"
                ),
                rusqlite::types::Value::Text(session.to_string()),
            ),
            // The caller named nothing to walk from. Checked at the boundary
            // that built the arguments, so this is the impossible fourth case
            // rather than a state a user can reach.
            (None, None, _) => {
                return Err(Error::Needs {
                    kind: "walk",
                    field: "a session or a paragraph to walk from",
                });
            }
        };

        let mut statement = match self.connection.prepare(&query) {
            Ok(statement) => statement,
            Err(source) => {
                return Err(Error::List {
                    path: self.database.clone(),
                    source,
                });
            }
        };
        let found = statement.query_map(rusqlite::params![parent, count, from], |row| {
            row.get::<_, Vec<u8>>(0)
        });
        let found = match found {
            Ok(found) => found,
            Err(source) => {
                return Err(Error::List {
                    path: self.database.clone(),
                    source,
                });
            }
        };

        let mut ids = Vec::new();
        for row in found {
            let bytes = match row {
                Ok(bytes) => bytes,
                Err(source) => {
                    return Err(Error::List {
                        path: self.database.clone(),
                        source,
                    });
                }
            };
            // `BLOB(16)` is affinity and not a rule, so a column written by
            // anything other than `add` can be the wrong width.
            let length = bytes.len();
            match <[u8; 16]>::try_from(bytes) {
                Ok(bytes) => ids.push(Ulid::from_bytes(bytes)),
                Err(_) => {
                    return Err(Error::Corrupt {
                        name: memory.to_string(),
                        length,
                    });
                }
            }
        }
        self.get(memory, &ids)
    }

    /// Run one of [`Storage::get`]'s two child queries and put back together
    /// what comes back: each sentence followed by the text it was followed by.
    ///
    /// A sentence with no postfix stored is one that arrived on its own rather
    /// than out of [`split`], and gets a space so that it does not run into the
    /// next one. The trailing postfix is the gap to whatever comes *after* what
    /// was asked for, and goes with it.
    fn sentences<P: rusqlite::Params>(&self, query: &str, parameters: P) -> Result<String, Error> {
        let mut statement = match self.connection.prepare(query) {
            Ok(statement) => statement,
            Err(source) => {
                return Err(Error::List {
                    path: self.database.clone(),
                    source,
                });
            }
        };
        let rows = statement.query_map(parameters, |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
            ))
        });
        let rows = match rows {
            Ok(rows) => rows,
            Err(source) => {
                return Err(Error::List {
                    path: self.database.clone(),
                    source,
                });
            }
        };

        let mut text = String::new();
        for row in rows {
            match row {
                Ok((Some(content), postfix)) => {
                    text.push_str(&content);
                    match postfix {
                        Some(postfix) if !postfix.is_empty() => text.push_str(&postfix),
                        _ => text.push(' '),
                    }
                }
                Ok((None, _)) => {}
                Err(source) => {
                    return Err(Error::List {
                        path: self.database.clone(),
                        source,
                    });
                }
            }
        }
        Ok(text.trim_end().to_string())
    }

    /// Every memory, oldest first, with names as the caller wrote them: the
    /// [`NAME_PREFIX`] goes on in [`Storage::create`] and comes off here, so it
    /// never leaves this module.
    ///
    /// `ORDER BY id` is chronological: a ULID leads with its timestamp and
    /// SQLite compares blobs with `memcmp`, so the primary key already sorts
    /// the way a reader expects and `created_at` needs no index.
    ///
    /// Reads the whole table into memory. That is the right shape while a
    /// memory is a name and a paragraph and the table is a few thousand rows;
    /// the day it is not, this grows a limit and an offset rather than a
    /// streaming iterator, because the caller is a CLI printing a page.
    pub fn list(&self) -> Result<Vec<Memory>, Error> {
        // `id` is read rather than `ulid`: the blob is the value the table is
        // keyed and ordered by, and the text column is a copy kept for human
        // eyes. Nothing checks that the two agree, so a program reads the one
        // that decides.
        let query = "SELECT id, name, description, created_at FROM memory ORDER BY id";
        let mut statement = match self.connection.prepare(query) {
            Ok(statement) => statement,
            Err(source) => {
                return Err(Error::List {
                    path: self.database.clone(),
                    source,
                });
            }
        };
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3)?,
            ))
        });
        let rows = match rows {
            Ok(rows) => rows,
            Err(source) => {
                return Err(Error::List {
                    path: self.database.clone(),
                    source,
                });
            }
        };

        let mut memories = Vec::new();
        for row in rows {
            let (id, name, description, created_at) = match row {
                Ok(row) => row,
                Err(source) => {
                    return Err(Error::List {
                        path: self.database.clone(),
                        source,
                    });
                }
            };
            // `BLOB(16)` is affinity, not a rule, so a row that came from
            // somewhere other than `create` can be any length.
            let length = id.len();
            let id = match <[u8; 16]>::try_from(id) {
                Ok(bytes) => Ulid::from_bytes(bytes),
                Err(_) => return Err(Error::Corrupt { name, length }),
            };
            // The memory's own table is named by the value in this column, so
            // hold on to it before the prefix comes off.
            let table = name.clone();

            // Off again on the way out: the prefix is how the table keeps
            // callers inside one namespace, and the caller only ever knew the
            // name it passed in. A row without it was not written by `create`,
            // and guessing what it means is worse than saying so.
            let name = match name.strip_prefix(NAME_PREFIX) {
                Some(name) => name.to_string(),
                None => return Err(Error::Prefix { name }),
            };
            // Run over the name again on the way out, not because `create` let
            // anything through, but because the table name below is pasted into
            // SQL and cannot be bound — a parameter cannot be an identifier.
            // After this it is `memory_` and `[a-z0-9_]`, with nothing in it to
            // quote or escape.
            check_name(&name)?;

            // One query per memory. That is a scan of each table, since nothing
            // indexes `type` — fine while `memory list` is a handful of
            // memories on a terminal, and the day it is not, the fix is an
            // index on `(type, role)` or a counts row kept up to date by the
            // writer, not a cleverer query.
            let query = format!("SELECT type, role, count(*) FROM {table} GROUP BY type, role");
            let mut counters = match self.connection.prepare(&query) {
                Ok(counters) => counters,
                Err(source) => {
                    return Err(Error::Count {
                        table,
                        path: self.database.clone(),
                        source,
                    });
                }
            };
            let grouped = counters.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            });
            let grouped = match grouped {
                Ok(grouped) => grouped,
                Err(source) => {
                    return Err(Error::Count {
                        table,
                        path: self.database.clone(),
                        source,
                    });
                }
            };

            let mut counts = Counts::default();
            for group in grouped {
                let (kind, role, total) = match group {
                    Ok(group) => group,
                    Err(source) => {
                        return Err(Error::Count {
                            table,
                            path: self.database.clone(),
                            source,
                        });
                    }
                };
                // `count(*)` cannot be negative, and SQLite has no unsigned
                // integer to have returned it as.
                let total = total as u64;
                match kind.as_str() {
                    "session" => counts.sessions += total,
                    "message" => {
                        counts.messages += total;
                        // An unrecognised role is counted in `messages` and in
                        // neither share, which is the one place this is lenient:
                        // `role` says who spoke and a fifth speaker is odd but
                        // legible, while a fifth `type` would mean the row is
                        // not one of the four things a memory is made of.
                        match role.as_deref() {
                            Some("assistant") => counts.assistant += total,
                            Some("user") => counts.user += total,
                            _ => {}
                        }
                    }
                    "paragraph" => counts.paragraphs += total,
                    "sentence" => counts.sentences += total,
                    _ => return Err(Error::Layer { table, kind }),
                }
            }

            memories.push(Memory {
                id,
                name,
                description,
                created_at,
                counts,
            });
        }
        Ok(memories)
    }

    /// Make the LanceDB table one model's vectors live in, unless it is already
    /// there; the bool says which happened.
    ///
    /// Only `init storage` calls this. Everything else opens the table with
    /// [`Storage::open_vectors`] and fails if it is missing, which is the same
    /// rule the SQLite side follows: borhan creates nothing behind the user's
    /// back, because the alternative is a wrong `--model` quietly starting a
    /// second, empty index instead of saying so.
    pub async fn create_vectors(&self, model: &str, dimensions: usize) -> Result<bool, Error> {
        let name = embedding_table(model)?;
        let connection = self.connect().await?;
        let names = match connection.table_names().execute().await {
            Ok(names) => names,
            Err(source) => {
                return Err(Error::Tables {
                    path: self.directory.clone(),
                    source: Box::new(source),
                });
            }
        };
        for existing in &names {
            if existing == &name {
                return Ok(false);
            }
        }

        // `memory` is the name the user gave, without the SQLite prefix, and
        // `id` is the 26-character ULID of the row in `memory_<memory>` this
        // was embedded from — text on both counts, because a LanceDB filter is
        // a SQL string and a text literal is something you can write into one.
        //
        // `type` is the layer ([`Kind`]), so that a search can ask for
        // sentences and not be crowded out by the paragraph and the message
        // that contain the same words.
        //
        // The width of `vector` is the model's, which is what keeps the tables
        // honest: point `--model` at a different model whose directory happens
        // to share a name and the write is rejected here rather than silently
        // mixing 256- and 512-dimension vectors.
        let schema = Arc::new(Schema::new(vec![
            Field::new("memory", DataType::Utf8, false),
            Field::new("type", DataType::Utf8, false),
            Field::new("id", DataType::Utf8, false),
            Field::new(
                VECTOR_COLUMN,
                DataType::FixedSizeList(
                    Arc::new(Field::new("item", DataType::Float32, true)),
                    dimensions as i32,
                ),
                false,
            ),
        ]));
        if let Err(source) = connection.create_empty_table(&name, schema).execute().await {
            return Err(Error::CreateTable {
                name,
                path: self.directory.clone(),
                source: Box::new(source),
            });
        }
        Ok(true)
    }

    /// Open one model's table. Does not create it — see
    /// [`Storage::create_vectors`].
    pub async fn open_vectors(&self, model: &str) -> Result<Vectors, Error> {
        let name = embedding_table(model)?;
        let connection = self.connect().await?;
        let names = match connection.table_names().execute().await {
            Ok(names) => names,
            Err(source) => {
                return Err(Error::Tables {
                    path: self.directory.clone(),
                    source: Box::new(source),
                });
            }
        };
        // Asked separately rather than reading it off `open_table`'s error,
        // because "you have not embedded anything with this model" is the
        // likely cause and it has a one-line fix worth naming.
        let mut found = false;
        for existing in &names {
            if existing == &name {
                found = true;
            }
        }
        if !found {
            return Err(Error::NoTable {
                name,
                model: model.to_string(),
                path: self.directory.clone(),
            });
        }

        let table = match connection.open_table(&name).execute().await {
            Ok(table) => table,
            Err(source) => {
                return Err(Error::OpenTable {
                    name,
                    path: self.directory.clone(),
                    source: Box::new(source),
                });
            }
        };
        Ok(Vectors { name, table })
    }

    /// LanceDB's handle on the storage directory. Cheap enough to make per
    /// command — it is a directory listing, not a server — and not held on
    /// [`Storage`] because that would make opening the SQLite database async.
    async fn connect(&self) -> Result<lancedb::Connection, Error> {
        let uri = match self.directory.to_str() {
            Some(uri) => uri,
            None => {
                return Err(Error::PathEncoding {
                    path: self.directory.clone(),
                });
            }
        };
        match lancedb::connect(uri).execute().await {
            Ok(connection) => Ok(connection),
            Err(source) => Err(Error::Connect {
                path: self.directory.clone(),
                source: Box::new(source),
            }),
        }
    }
}

/// One model's vectors: every memory's, one table.
pub struct Vectors {
    /// `embedding_<model>`, for error messages.
    name: String,
    table: lancedb::Table,
}

impl Vectors {
    /// Store embeddings, and index the table once there are enough of them.
    pub async fn add(&self, vectors: &[Vector]) -> Result<(), Error> {
        if vectors.is_empty() {
            return Ok(());
        }

        let schema = match self.table.schema().await {
            Ok(schema) => schema,
            Err(source) => {
                return Err(Error::TableSchema {
                    name: self.name.clone(),
                    source: Box::new(source),
                });
            }
        };
        // The model's width, read off the table rather than passed in, so the
        // check below compares against what is actually stored.
        let expected = match schema.field_with_name(VECTOR_COLUMN) {
            Ok(field) => match field.data_type() {
                DataType::FixedSizeList(_, width) => *width as usize,
                _ => {
                    return Err(Error::TableShape {
                        name: self.name.clone(),
                    });
                }
            },
            Err(_) => {
                return Err(Error::TableShape {
                    name: self.name.clone(),
                });
            }
        };

        let mut memories = Vec::with_capacity(vectors.len());
        let mut kinds = Vec::with_capacity(vectors.len());
        let mut identifiers = Vec::with_capacity(vectors.len());
        let mut embeddings = Vec::with_capacity(vectors.len());
        for vector in vectors {
            // Checked here rather than left to arrow, which panics on a ragged
            // fixed-size list instead of returning.
            if vector.embedding.len() != expected {
                return Err(Error::Dimensions {
                    id: vector.id.to_string(),
                    name: self.name.clone(),
                    dimensions: vector.embedding.len(),
                    expected,
                });
            }
            memories.push(vector.memory.clone());
            kinds.push(vector.kind.as_str());
            identifiers.push(vector.id.to_string());
            let mut embedding = Vec::with_capacity(expected);
            for number in &vector.embedding {
                embedding.push(Some(*number));
            }
            embeddings.push(Some(embedding));
        }

        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(StringArray::from(memories)),
                Arc::new(StringArray::from(kinds)),
                Arc::new(StringArray::from(identifiers)),
                Arc::new(
                    FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
                        embeddings,
                        expected as i32,
                    ),
                ),
            ],
        );
        let batch = match batch {
            Ok(batch) => batch,
            Err(source) => {
                return Err(Error::Batch {
                    name: self.name.clone(),
                    source: Box::new(source),
                });
            }
        };
        if let Err(source) = self.table.add(vec![batch]).execute().await {
            return Err(Error::Add {
                count: vectors.len(),
                name: self.name.clone(),
                source: Box::new(source),
            });
        }
        self.index().await
    }

    /// Build the indexes, once, when the table is big enough to want them.
    ///
    /// Rows written after this runs are not *in* the vector index until a
    /// `optimize` folds them in; LanceDB still finds them by scanning the
    /// unindexed tail, so a search stays correct and only gets slower.
    async fn index(&self) -> Result<(), Error> {
        let rows = match self.table.count_rows(None).await {
            Ok(rows) => rows,
            Err(source) => {
                return Err(Error::IndexColumn {
                    name: self.name.clone(),
                    column: VECTOR_COLUMN.to_string(),
                    source: Box::new(source),
                });
            }
        };
        if rows < INDEX_THRESHOLD {
            return Ok(());
        }
        let indices = match self.table.list_indices().await {
            Ok(indices) => indices,
            Err(source) => {
                return Err(Error::IndexColumn {
                    name: self.name.clone(),
                    column: VECTOR_COLUMN.to_string(),
                    source: Box::new(source),
                });
            }
        };
        if !indices.is_empty() {
            return Ok(());
        }

        // Cosine because the model normalises its output, which makes cosine
        // and dot rank identically and both of them cheaper to reason about
        // than L2 — and because the builder's default is L2, so leaving it
        // unsaid would pick the other one.
        let vectors = self
            .table
            .create_index(
                &[VECTOR_COLUMN],
                Index::IvfPq(IvfPqIndexBuilder::default().distance_type(DistanceType::Cosine)),
            )
            .execute()
            .await;
        if let Err(source) = vectors {
            return Err(Error::IndexColumn {
                name: self.name.clone(),
                column: VECTOR_COLUMN.to_string(),
                source: Box::new(source),
            });
        }

        // The scalar pair matters more than it looks. A search is always
        // filtered to one memory, and an approximate vector index answers a
        // filtered query by walking partitions — so when a memory is a small
        // slice of the table, its rows are scattered and real hits get missed.
        // With these, the filter is resolved first and the vector search runs
        // over what survives.
        for column in ["memory", "type"] {
            let scalar = self
                .table
                .create_index(&[column], Index::BTree(BTreeIndexBuilder::default()))
                .execute()
                .await;
            if let Err(source) = scalar {
                return Err(Error::IndexColumn {
                    name: self.name.clone(),
                    column: column.to_string(),
                    source: Box::new(source),
                });
            }
        }
        Ok(())
    }

    /// The `limit` rows of `memory` closest to `query`, nearest first.
    ///
    /// `kind` narrows it to one layer. Without it a sentence, the paragraph
    /// holding it and the message holding that are three separate rows of
    /// nearly the same text, and they will take three of the results.
    pub async fn search(
        &self,
        memory: &str,
        kind: Option<Kind>,
        query: &[f32],
        limit: usize,
    ) -> Result<Vec<Hit>, Error> {
        // The filter is a SQL string with the name pasted into it, so the name
        // has to have been through the same gate `create` uses — after which it
        // is `[a-z0-9_]` and holds no quote to break out with.
        check_name(memory)?;
        let mut filter = format!("memory = '{memory}'");
        if let Some(kind) = kind {
            filter.push_str(&format!(" AND type = '{}'", kind.as_str()));
        }

        let search = match self.table.query().nearest_to(query) {
            Ok(search) => search,
            Err(source) => {
                return Err(Error::Search {
                    name: self.name.clone(),
                    source: Box::new(source),
                });
            }
        };
        // Said again here: the index knows it was built for cosine, but a table
        // too small to have one is scanned flat, and that path defaults to L2.
        let stream = search
            .distance_type(DistanceType::Cosine)
            .only_if(filter)
            .limit(limit)
            .execute()
            .await;
        let mut stream = match stream {
            Ok(stream) => stream,
            Err(source) => {
                return Err(Error::Search {
                    name: self.name.clone(),
                    source: Box::new(source),
                });
            }
        };

        let mut hits = Vec::new();
        loop {
            let batch = match stream.try_next().await {
                Ok(Some(batch)) => batch,
                Ok(None) => break,
                Err(source) => {
                    return Err(Error::Search {
                        name: self.name.clone(),
                        source: Box::new(source),
                    });
                }
            };
            let kinds = self.text(&batch, "type")?;
            let identifiers = self.text(&batch, "id")?;
            // LanceDB adds this one to the results; it is not in the schema.
            let distances = match batch.column_by_name("_distance") {
                Some(column) => match column.as_any().downcast_ref::<Float32Array>() {
                    Some(distances) => distances.clone(),
                    None => {
                        return Err(Error::Column {
                            name: self.name.clone(),
                            column: "_distance".to_string(),
                        });
                    }
                },
                None => {
                    return Err(Error::Column {
                        name: self.name.clone(),
                        column: "_distance".to_string(),
                    });
                }
            };

            for row in 0..batch.num_rows() {
                hits.push(Hit {
                    kind: kinds.value(row).to_string(),
                    id: identifiers.value(row).to_string(),
                    distance: distances.value(row),
                });
            }
        }
        Ok(hits)
    }

    /// One text column of a result batch, or the error naming which one was
    /// not what the schema promised.
    fn text(&self, batch: &RecordBatch, column: &str) -> Result<StringArray, Error> {
        if let Some(values) = batch.column_by_name(column)
            && let Some(values) = values.as_any().downcast_ref::<StringArray>()
        {
            return Ok(values.clone());
        }
        Err(Error::Column {
            name: self.name.clone(),
            column: column.to_string(),
        })
    }
}

/// One CommonMark block on its way to becoming a paragraph, with where it sat
/// in the source so that what followed it can be kept.
struct Block {
    /// The block's sentences, markup already gone.
    sentences: Vec<String>,
    /// Kept exactly as written, one sentence per line, and cut into paragraphs
    /// of at most [`CODE_LINES`] lines.
    code: bool,
    /// A heading does not become a paragraph of its own: the block after it
    /// joins it. A title with nothing under it is not what anybody is looking
    /// for, and the prose under it is much easier to place with its title in
    /// front of it — which is as true of a vector as it is of a reader.
    heading: bool,
    /// Where the block's source starts and ends. Everything between one
    /// block's `end` and the next one's `start` is what the author put between
    /// them, and that is what becomes a postfix.
    start: usize,
    end: usize,
}

/// Break a Markdown text into paragraphs, each already broken into sentences,
/// each sentence carrying the text that followed it.
///
/// Empty paragraphs never come back, so an empty result means the text held no
/// words — a horizontal rule, an empty list, nothing but markup.
///
/// The unit of a paragraph is a CommonMark *block*, not a run between blank
/// lines: a paragraph, one item of a list, one row of a table, a fenced code
/// block. That is the whole reason a parser is here. Splitting on blank lines
/// would run a bullet list together into one lump and cut a code block wherever
/// the code happened to breathe — and both of those are worse units to embed
/// than what the author actually wrote.
///
/// A heading is the exception, and joins the block under it. It is a label for
/// that block and nothing on its own: `## Motivation` answers no question, and
/// as a row of its own it is two words that sit close to every query mentioning
/// motivation.
///
/// The second half of each pair is the **postfix**: the whitespace the author
/// left between this sentence and whatever came next — a space inside a
/// paragraph, a newline between the lines of code, a blank line between blocks,
/// as many blank lines as were actually written. Concatenating content and
/// postfix through a paragraph or a whole message gives the text back with its
/// shape, which is what [`Storage::get`] prints and what a paragraph's vector is
/// built from. What that cannot give back is the markup this deliberately drops
/// and the line wrapping inside a paragraph, which CommonMark itself treats as
/// insignificant. Nor the very first prefix, which is markup too — the `- ` of
/// the first list item, the `#` of a heading.
///
/// Markup is dropped, not stored: what a row keeps is the words. `**bold**` is
/// `bold`, a link is its text, and nothing that reaches a model or a screen has
/// a bracket in it that the author did not type. Code blocks are the exception
/// and keep their literal lines, because in code the punctuation *is* the
/// content.
///
/// What this deliberately does not do is understand abbreviations. "Dr. Smith"
/// is two sentences here. The fix for that is a real sentence segmenter with a
/// per-language model, not a longer list of special cases, and until one is
/// worth the dependency the cost is one short extra row.
fn split(text: &str) -> Vec<Vec<(String, String)>> {
    let mut options = Options::empty();
    // Tables and task lists because transcripts are full of them, and without
    // these their syntax arrives as literal `|` and `[ ]` in the prose.
    // Strikethrough so that struck text is still text rather than `~~`.
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_STRIKETHROUGH);

    let mut blocks: Vec<Block> = Vec::new();
    // The prose of the block being read, with its markup already gone.
    let mut current = String::new();
    // Set while inside a fenced or indented code block, where the text is kept
    // exactly as written instead.
    let mut code = false;

    // Offsets and not just events, because the postfix is the source between
    // two blocks and no event carries it: the parser reports the words, and
    // the whitespace around them is exactly what it is there to throw away.
    for (event, range) in Parser::new_ext(text, options).into_offset_iter() {
        // A code block is its own path: its text is kept as written, and one
        // block of it can become several paragraphs.
        if let Event::End(TagEnd::CodeBlock) = event {
            // One sentence per line, blank lines dropped. A file of code is not
            // prose and has no sentences to find in it; a line is the unit that
            // gets read, quoted and searched for.
            let mut lines = Vec::new();
            for line in current.lines() {
                if !line.trim().is_empty() {
                    lines.push(cut(line.trim_end()));
                }
            }
            if !lines.is_empty() {
                blocks.push(Block {
                    sentences: lines,
                    code: true,
                    heading: false,
                    start: range.start,
                    end: range.end,
                });
            }
            current.clear();
            code = false;
            continue;
        }

        // An HTML block is markup wrapped around prose: a `<details>`, a
        // `<div class="note">`, a hand-written `<table>`. Its own path because
        // the tags have to come off in one piece -- a comment can span several
        // `Event::Html` lines, so there is nothing to strip until the block
        // ends.
        if let Event::End(TagEnd::HtmlBlock) = event {
            // Tags are how the words were laid out, not something written to be
            // read, and they go the way `#` and `- ` go. A comment is not read
            // either: `<!-- prettier-ignore -->` is a note to a tool.
            let mut stripped = String::new();
            let mut rest = current.as_str();
            while let Some(open) = rest.find('<') {
                stripped.push_str(&rest[..open]);
                let tag = &rest[open..];
                let (skip, close) = match tag.starts_with("<!--") {
                    true => (4, "-->"),
                    false => (1, ">"),
                };
                match tag[skip..].find(close) {
                    Some(end) => {
                        // A tag is a gap between words, not nothing: `<td>a</td>
                        // <td>b</td>` on one line is two cells, and dropping the
                        // markup outright would leave `ab`, a word nobody wrote
                        // and nobody can search for.
                        match stripped.chars().next_back() {
                            Some(last) if !last.is_whitespace() => stripped.push(' '),
                            _ => {}
                        }
                        rest = &tag[skip + end + close.len()..];
                    }
                    // A `<` with no `>` after it is a less-than sign.
                    None => {
                        stripped.push_str(tag);
                        rest = "";
                        break;
                    }
                }
            }
            stripped.push_str(rest);
            if let Some(sentences) = prose(&stripped) {
                blocks.push(Block {
                    sentences,
                    code: false,
                    heading: false,
                    start: range.start,
                    end: range.end,
                });
            }
            current.clear();
            continue;
        }

        // Every other block boundary: whatever has been collected is one block,
        // and the next one starts here. Both ends are matched because a block
        // can open right after another closes; an `End` gives the boundary the
        // block really reached, a `Start` only where the next one begins.
        let boundary = match &event {
            Event::End(TagEnd::Heading(_)) => Some((true, range.start, range.end)),
            Event::End(TagEnd::Paragraph | TagEnd::Item | TagEnd::TableRow | TagEnd::TableHead) => {
                Some((false, range.start, range.end))
            }
            Event::Start(
                Tag::CodeBlock(_)
                | Tag::Paragraph
                | Tag::Heading { .. }
                | Tag::Item
                | Tag::TableRow
                | Tag::TableHead
                | Tag::HtmlBlock,
            ) => Some((false, range.start, range.start)),
            _ => None,
        };
        if let Some((heading, start, end)) = boundary {
            if let Some(sentences) = prose(&current) {
                blocks.push(Block {
                    sentences,
                    code: false,
                    heading,
                    start,
                    end,
                });
            }
            current.clear();
            if matches!(event, Event::Start(Tag::CodeBlock(_))) {
                code = true;
            }
            continue;
        }

        match event {
            // Inline text, and inline code, which is prose about code and reads
            // as part of the sentence around it.
            Event::Text(text) | Event::Code(text) => current.push_str(&text),

            // One line of an HTML block, its line break included. Collected
            // raw and taken apart when the block ends.
            Event::Html(html) => current.push_str(&html),

            // An inline tag is markup around text that arrives on its own, so
            // `<b>` goes and `inline` stays. Except a break, which is a break
            // wherever it is written.
            Event::InlineHtml(html) => {
                let tag = html.trim_start_matches('<').trim_start_matches('/');
                if tag.starts_with("br") {
                    current.push('\n');
                }
            }

            // A wrapped line inside one paragraph is not a boundary; a hard
            // break is one the author asked for.
            Event::SoftBreak => current.push(if code { '\n' } else { ' ' }),
            Event::HardBreak => current.push('\n'),

            // A row is one sentence, its cells divided the way the author
            // divided them. Not a sentence each: "lancedb" on its own row is
            // half a fact, and it is "lancedb | vectors" that answers a
            // question. The separator goes in front so the row has no trailing
            // one, and no punctuation is invented that was never written.
            Event::Start(Tag::TableCell) if !current.is_empty() => current.push_str(" | "),

            _ => {}
        }
    }
    if let Some(sentences) = prose(&current) {
        blocks.push(Block {
            sentences,
            code: false,
            heading: false,
            start: text.len(),
            end: text.len(),
        });
    }

    let mut paragraphs: Vec<Vec<(String, String)>> = Vec::new();
    // The paragraph being built. Usually one block, but a heading hands its
    // sentences to the block after it and this is where they wait.
    let mut sentences: Vec<(String, String)> = Vec::new();
    for (index, block) in blocks.iter().enumerate() {
        // What the author left between this block and the next: one newline
        // inside a list, two between paragraphs, more where somebody wanted the
        // gap. Only the whitespace of it — the rest is the next block's own
        // markup, the `- ` of a list item or the `#` of a heading, and markup
        // is not what a row keeps. The last block is followed by nothing.
        let mut gap = String::new();
        if let Some(next) = blocks.get(index + 1) {
            if let Some(between) = text.get(block.end..next.start) {
                for character in between.chars() {
                    if !character.is_whitespace() || gap.chars().count() == POSTFIX_LIMIT {
                        break;
                    }
                    gap.push(character);
                }
            }
            // Two blocks with nothing between them cannot happen in Markdown,
            // and a range that did not land on a character boundary is the
            // parser disagreeing with itself. Either way, keep the words apart.
            if gap.is_empty() {
                gap.push_str("\n\n");
            }
        }

        let separator = if block.code { "\n" } else { " " };
        // A code block longer than a screenful is several paragraphs: a
        // 500-line file as one vector answers every query about it equally.
        // Prose is one paragraph however many sentences it holds.
        let size = if block.code {
            CODE_LINES
        } else {
            block.sentences.len()
        };
        let chunks: Vec<&[String]> = block.sentences.chunks(size.max(1)).collect();
        for (number, chunk) in chunks.iter().enumerate() {
            for (position, sentence) in chunk.iter().enumerate() {
                let postfix = if position + 1 < chunk.len() {
                    separator.to_string()
                } else if number + 1 < chunks.len() {
                    // The cut is borhan's, not the author's: the lines run on.
                    String::from("\n")
                } else {
                    gap.clone()
                };
                sentences.push((sentence.clone(), postfix));
            }
            if !block.heading {
                paragraphs.push(std::mem::take(&mut sentences));
            }
        }
    }
    // A heading with nothing under it: the last thing in the text, or followed
    // only by markup that held no words.
    if !sentences.is_empty() {
        paragraphs.push(sentences);
    }
    paragraphs
}

/// Break one block of prose into sentences, or `None` if it holds no words.
///
/// A sentence ends at `.`, `!`, `?` or their counterparts in the scripts borhan
/// is likely to be fed — Persian `؟`, the full-width CJK three — when what
/// follows is whitespace or the end of the block. That last condition is what
/// keeps `3.14` and `e.g.` in one piece as long as nothing separates them.
fn prose(text: &str) -> Option<Vec<String>> {
    let terminators = ['.', '!', '?', '؟', '。', '！', '？', '…', '؛'];
    let characters: Vec<char> = text.chars().collect();

    let mut sentences = Vec::new();
    let mut current = String::new();
    let mut index = 0;
    while index < characters.len() {
        let character = characters[index];
        index += 1;
        current.push(character);

        // A hard break is the author ending a line on purpose.
        let mut boundary = character == '\n';
        if terminators.contains(&character) {
            // `?!` and `...` end one sentence, not three.
            while index < characters.len() && terminators.contains(&characters[index]) {
                current.push(characters[index]);
                index += 1;
            }
            boundary = match characters.get(index) {
                Some(next) => next.is_whitespace(),
                None => true,
            };
        }
        // Nothing in the text says where to break, so break where the column
        // ends. Losing the tail would be worse, and refusing the whole text
        // over one long line worse still.
        if current.chars().count() >= CONTENT_LIMIT {
            boundary = true;
        }

        if boundary && !current.trim().is_empty() {
            sentences.push(current.trim().to_string());
            current.clear();
        }
    }
    if !current.trim().is_empty() {
        sentences.push(current.trim().to_string());
    }

    if sentences.is_empty() {
        return None;
    }
    Some(sentences)
}

/// A line of code, cut to what the column can hold.
///
/// Only a minified bundle or a base64 blob reaches this; real code does not
/// have 5000-character lines. The tail is dropped rather than wrapped, because
/// a second row holding the middle of a line is not something anyone would
/// search for or want to read.
fn cut(line: &str) -> String {
    let mut text = String::new();
    for character in line.chars().take(CONTENT_LIMIT) {
        text.push(character);
    }
    text
}

/// The rule every memory name goes through, on the way in and on the way back
/// out into a LanceDB filter.
fn check_name(name: &str) -> Result<(), Error> {
    let characters = name.chars().count();
    if characters == 0 || characters > NAME_LIMIT {
        return Err(Error::NameLength { characters });
    }
    for character in name.chars() {
        if !character.is_ascii_lowercase() && !character.is_ascii_digit() && character != '_' {
            return Err(Error::NameCharacter {
                name: name.to_string(),
                character,
            });
        }
    }
    Ok(())
}

/// The LanceDB table a model's vectors live in.
///
/// A model name is looser than a memory name — `potion-retrieval-32M` has
/// hyphens and capitals — so it is taken as written, and only what cannot be
/// part of a table name is refused. That includes `.` and `/`, which matters
/// because a `--model` pointing at a directory takes its name from that path.
fn embedding_table(model: &str) -> Result<String, Error> {
    if model.is_empty() {
        return Err(Error::ModelEmpty);
    }
    for character in model.chars() {
        if !character.is_ascii_alphanumeric() && character != '_' && character != '-' {
            return Err(Error::ModelCharacter {
                model: model.to_string(),
                character,
            });
        }
    }
    Ok(format!("{EMBEDDING_PREFIX}{model}"))
}
