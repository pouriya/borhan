//! Layer one: the messages as they arrived, and nothing derived from them.
//!
//! One directory per memory, under `<home>/storage/<name>/`, holding a SQLite
//! database and — written by [`crate::index`], never by this module — a tantivy
//! index beside it. A memory is deleted by unlinking its directory, and its
//! document frequencies are its own, which is the point: `error` is a common
//! word in an infrastructure room and a rare one in a scheduling room, and an
//! IDF averaged across both is wrong for each.
//!
//! What lives here is the part that is never rebuilt. Messages arrive, are
//! written verbatim, and are then split into units and sentences that are
//! recorded as **byte offsets into the message body** rather than as copies of
//! the text. A unit and the message it came from cannot drift apart if there is
//! only one copy of the text, and slicing it back out is a join this code is
//! doing anyway to display a result.
//!
//! Everything in [`crate::index`] is reconstructible from this module by a full
//! rescan. Nothing in this module is reconstructible from anything.

use std::fs;
use std::path::{Path, PathBuf};

use pulldown_cmark::{Event, Options, Parser, Tag};
use rusqlite::Connection;

use crate::ulid::Ulid;

/// The one database file inside a memory's directory. The tantivy index sits
/// beside it under [`crate::index::DIRECTORY`].
const DATABASE: &str = "borhan.db";

/// Longest name a caller may hand to [`Storage::create`], in characters.
///
/// A name is a directory name and it is what every other command takes to find
/// the memory, so it is `a-z`, `0-9` and `_` and nothing else. No prefix is put
/// in front of it: unlike the old layout there are no shared tables for a name
/// to collide with, because a memory *is* its own directory.
const NAME_LIMIT: usize = 40;

/// Longest `memory.description`, in characters. Shown to the calling model, so
/// it is prose about what the memory holds rather than a label.
const DESCRIPTION_LIMIT: usize = 2000;

/// Longest `memory.languages`, in characters. A comma-separated list like
/// `fa,en`, reported by `memory list` so that a model composing a query knows
/// which languages are worth expanding a concept group into.
const LANGUAGES_LIMIT: usize = 64;

/// Longest feeder-supplied session or message identifier, in characters. A
/// UUID in one deployment and a filename in another.
const REFERENCE_LIMIT: usize = 64;

/// Lines of a fenced code block that make one unit.
///
/// Code has no sentences in it, so a line is the sentence and this is how many
/// of them are held to be about one thing. A screenful: long enough that a
/// function usually lands whole, short enough that a 500-line paste does not
/// become one unit that answers every query about it equally.
const CODE_LINES: usize = 20;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("memory name is empty")]
    Empty,

    #[error("memory name {name:?} is longer than {NAME_LIMIT} characters")]
    Long { name: String },

    #[error("memory name {name:?} has a character outside a-z, 0-9 and _")]
    Charset { name: String },

    #[error("description is longer than {DESCRIPTION_LIMIT} characters")]
    Description,

    #[error("description must be more than 10 words")]
    DescriptionShort,

    #[error("languages is longer than {LANGUAGES_LIMIT} characters")]
    Languages,

    #[error("{what} {reference:?} is longer than {REFERENCE_LIMIT} characters")]
    Reference {
        what: &'static str,
        reference: String,
    },

    #[error("memory {name:?} already exists at {path}")]
    Exists { name: String, path: PathBuf },

    #[error("no memory named {name:?} at {path} — `borhan memory create {name}` first")]
    Missing { name: String, path: PathBuf },

    #[error("could not create {path}")]
    Create {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("could not read {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("could not open the database at {path}")]
    Open {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },

    #[error("could not apply the schema to {path}")]
    Schema {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },

    #[error("could not write to {path}")]
    Write {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },

    #[error("could not query {path}")]
    Query {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },

    #[error("message {reference:?} is already stored in session {session:?}")]
    Duplicate { session: String, reference: String },

    #[error("no unit {id} in this memory")]
    Unknown { id: String },

    #[error("could not make a ULID")]
    Identifier {
        #[source]
        source: crate::ulid::Error,
    },
}

/// Who wrote a message. A small enum because it is a filter — "only what the
/// user said" is a question worth asking — and filters want an indexable
/// integer, not a string compared a hundred thousand times.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "user" => Some(Role::User),
            "assistant" => Some(Role::Assistant),
            "tool" => Some(Role::Tool),
            _ => None,
        }
    }

    pub fn code(&self) -> u64 {
        match self {
            Role::User => 0,
            Role::Assistant => 1,
            Role::Tool => 2,
        }
    }

    pub fn from_code(code: u64) -> Role {
        match code {
            0 => Role::User,
            2 => Role::Tool,
            _ => Role::Assistant,
        }
    }
}

/// One memory, as `memory list` shows it.
#[derive(Debug, Clone)]
pub struct Memory {
    pub id: Ulid,
    pub name: String,
    pub description: Option<String>,
    pub languages: String,
    pub created_at: i64,
    pub sessions: u64,
    pub messages: u64,
    pub units: u64,
}

/// A message on its way in.
#[derive(Debug, Clone)]
pub struct Entry<'a> {
    /// The feeder's session identifier — a thread id, a channel, a filename.
    pub session: &'a str,
    /// The feeder's message identifier, if it has one. Used to reject a
    /// double-send of the same message, and carried back out on every hit.
    pub message: Option<&'a str>,
    pub author: &'a str,
    pub role: Role,
    /// Unix milliseconds. Supplied rather than taken from the clock, because a
    /// transcript is usually being replayed rather than watched.
    pub ts: i64,
    /// The text, verbatim. Read as Markdown when it is split, stored untouched.
    pub body: &'a str,
}

/// A unit — a paragraph — as stored: an id and a byte range into the message.
#[derive(Debug, Clone)]
pub struct Unit {
    pub id: Ulid,
    pub start: usize,
    pub end: usize,
}

/// What [`Storage::add`] wrote, and what [`crate::index`] needs to index it.
#[derive(Debug, Clone)]
pub struct Written {
    pub session: Ulid,
    pub message: Ulid,
    pub seq: i64,
    pub units: Vec<Unit>,
}

/// A unit resolved back to everything a result line needs, in one query.
#[derive(Debug, Clone)]
pub struct Located {
    pub unit: Ulid,
    pub unit_seq: i64,
    pub message: Ulid,
    pub message_ref: Option<String>,
    pub session: Ulid,
    pub session_ref: String,
    pub seq: i64,
    pub author: String,
    pub role: Role,
    pub ts: i64,
    pub start: usize,
    pub end: usize,
    pub body: String,
}

impl Located {
    /// The unit's own text, sliced out of the message body it was never copied
    /// from.
    pub fn text(&self) -> &str {
        &self.body[self.start..self.end]
    }
}

/// A whole message, as the cursor tool returns it.
#[derive(Debug, Clone)]
pub struct Message {
    pub id: Ulid,
    pub reference: Option<String>,
    pub seq: i64,
    pub author: String,
    pub role: Role,
    pub ts: i64,
    pub body: String,
    /// True for the message the cursor pointed at, so the caller can see where
    /// in the window it landed.
    pub anchor: bool,
}

/// One memory's SQLite database.
pub struct Storage {
    pub directory: PathBuf,
    database: PathBuf,
    connection: Connection,
}

impl Storage {
    /// Make a memory: its directory, its database, and the one row describing
    /// it. Fails if the directory is already there, because a name that is
    /// taken is the one thing a caller has to be told about rather than
    /// silently joined to.
    pub fn create(
        root: &Path,
        name: &str,
        description: &str,
        languages: &str,
    ) -> Result<(Self, Ulid), Error> {
        check_name(name)?;
        check_description(description)?;
        if languages.chars().count() > LANGUAGES_LIMIT {
            return Err(Error::Languages);
        }

        let directory = root.join(name);
        if directory.exists() {
            return Err(Error::Exists {
                name: name.to_string(),
                path: directory,
            });
        }
        if let Err(source) = fs::create_dir_all(&directory) {
            return Err(Error::Create {
                path: directory,
                source,
            });
        }

        let storage = Self::attach(directory)?;
        let id = match Ulid::new() {
            Ok(id) => id,
            Err(source) => return Err(Error::Identifier { source }),
        };
        let written = storage.connection.execute(
            "INSERT INTO memory (id, ulid, name, description, languages, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                id.bytes().as_slice(),
                id.to_string(),
                name,
                description,
                languages,
                id.milliseconds() as i64,
            ],
        );
        if let Err(source) = written {
            return Err(Error::Write {
                path: storage.database.clone(),
                source,
            });
        }
        Ok((storage, id))
    }

    /// Change the description and/or the language tags on the one row this
    /// database holds. At least one of the two has to be `Some`; the caller
    /// has already decided that, because an empty patch is a client mistake
    /// rather than a storage one.
    pub fn update(&self, description: Option<&str>, languages: Option<&str>) -> Result<(), Error> {
        if let Some(text) = description {
            check_description(text)?;
        }
        if let Some(text) = languages
            && text.chars().count() > LANGUAGES_LIMIT
        {
            return Err(Error::Languages);
        }

        let written = match (description, languages) {
            (Some(description), Some(languages)) => self.connection.execute(
                "UPDATE memory SET description = ?1, languages = ?2",
                rusqlite::params![description, languages],
            ),
            (Some(description), None) => self.connection.execute(
                "UPDATE memory SET description = ?1",
                rusqlite::params![description],
            ),
            (None, Some(languages)) => self.connection.execute(
                "UPDATE memory SET languages = ?1",
                rusqlite::params![languages],
            ),
            (None, None) => return Ok(()),
        };
        if let Err(source) = written {
            return Err(Error::Write {
                path: self.database.clone(),
                source,
            });
        }
        Ok(())
    }

    /// Open an existing memory. Will not create one: a wrong `--home` or a
    /// misspelled name has to be an error naming what was looked for, never a
    /// new empty memory that silently remembers nothing.
    pub fn open(root: &Path, name: &str) -> Result<Self, Error> {
        check_name(name)?;
        let directory = root.join(name);
        if !directory.join(DATABASE).exists() {
            return Err(Error::Missing {
                name: name.to_string(),
                path: directory,
            });
        }
        Self::attach(directory)
    }

    /// Every memory under `root`, oldest first, with its counts.
    ///
    /// There is no registry to read: a memory is a directory, so the listing is
    /// the directory. A subdirectory without a database is skipped rather than
    /// reported, because that is what a half-finished `create` and a stray
    /// `mkdir` both look like.
    pub fn list(root: &Path) -> Result<Vec<Memory>, Error> {
        let entries = match fs::read_dir(root) {
            Ok(entries) => entries,
            Err(source) => {
                return Err(Error::Read {
                    path: root.to_path_buf(),
                    source,
                });
            }
        };

        let mut memories = Vec::new();
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(source) => {
                    return Err(Error::Read {
                        path: root.to_path_buf(),
                        source,
                    });
                }
            };
            let path = entry.path();
            if !path.join(DATABASE).exists() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let storage = Self::open(root, name)?;
            memories.push(storage.describe()?);
        }
        memories.sort_by_key(|memory| memory.created_at);
        Ok(memories)
    }

    /// Open the database and apply the schema, without touching the directory.
    fn attach(directory: PathBuf) -> Result<Self, Error> {
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

        // Every primary key is a ULID stored as a 16-byte blob: byte order is
        // time order and SQLite compares blobs with memcmp, so `ORDER BY id` is
        // chronological and a time range is a contiguous key range.
        //
        // `message.seq` is gapless within a session and it is what makes the
        // cursor tool a range scan. Timestamps collide, arrive out of order and
        // are supplied by the feeder; an ordinal this code assigns cannot.
        //
        // `unit` and `sentence` hold offsets and no text. The offsets are byte
        // offsets into `message.body`, which is the only copy of the text.
        //
        // The two log tables cost nothing today and are the entire training set
        // for a reranker later: a search that returns twenty hits followed by a
        // cursor call on the seventh is a relevance label generated for free
        // during normal operation, and it cannot be recovered afterwards.
        let schema = "
            CREATE TABLE IF NOT EXISTS memory (
                id          BLOB(16)    NOT NULL PRIMARY KEY,
                ulid        TEXT        NOT NULL,
                name        VARCHAR(40) NOT NULL,
                description VARCHAR(2000),
                languages   VARCHAR(64) NOT NULL,
                created_at  INTEGER     NOT NULL
            );

            CREATE TABLE IF NOT EXISTS session (
                id           BLOB(16)    NOT NULL PRIMARY KEY,
                ulid         TEXT        NOT NULL,
                external_ref VARCHAR(64) NOT NULL UNIQUE,
                started_at   INTEGER     NOT NULL,
                ended_at     INTEGER
            );

            CREATE TABLE IF NOT EXISTS message (
                id           BLOB(16)    NOT NULL PRIMARY KEY,
                ulid         TEXT        NOT NULL,
                session_id   BLOB(16)    NOT NULL,
                external_ref VARCHAR(64),
                seq          INTEGER     NOT NULL,
                author       VARCHAR(64) NOT NULL,
                role         INTEGER     NOT NULL,
                ts           INTEGER     NOT NULL,
                body         TEXT        NOT NULL,
                UNIQUE (session_id, seq)
            );
            CREATE INDEX IF NOT EXISTS message_ts ON message (ts);
            CREATE UNIQUE INDEX IF NOT EXISTS message_ref
                ON message (session_id, external_ref);

            CREATE TABLE IF NOT EXISTS unit (
                id         BLOB(16) NOT NULL PRIMARY KEY,
                message_id BLOB(16) NOT NULL,
                seq        INTEGER  NOT NULL,
                byte_start INTEGER  NOT NULL,
                byte_end   INTEGER  NOT NULL,
                UNIQUE (message_id, seq)
            );

            CREATE TABLE IF NOT EXISTS sentence (
                unit_id    BLOB(16) NOT NULL,
                seq        INTEGER  NOT NULL,
                byte_start INTEGER  NOT NULL,
                byte_end   INTEGER  NOT NULL,
                PRIMARY KEY (unit_id, seq)
            ) WITHOUT ROWID;

            CREATE TABLE IF NOT EXISTS index_meta (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS search_log (
                id            BLOB(16) NOT NULL PRIMARY KEY,
                ts            INTEGER  NOT NULL,
                groups_json   TEXT     NOT NULL,
                returned_json TEXT     NOT NULL
            );

            CREATE TABLE IF NOT EXISTS expansion_log (
                search_id BLOB(16) NOT NULL,
                unit_id   BLOB(16) NOT NULL,
                ts        INTEGER  NOT NULL,
                PRIMARY KEY (search_id, unit_id)
            ) WITHOUT ROWID;
        ";
        if let Err(source) = connection.execute_batch(schema) {
            return Err(Error::Schema {
                path: database,
                source,
            });
        }

        Ok(Self {
            directory,
            database,
            connection,
        })
    }

    /// The memory's own row, plus the three counts `memory list` prints.
    pub fn describe(&self) -> Result<Memory, Error> {
        let row = self.connection.query_row(
            "SELECT id, name, description, languages, created_at FROM memory LIMIT 1",
            [],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        );
        let (id, name, description, languages, created_at) = match row {
            Ok(row) => row,
            Err(source) => {
                return Err(Error::Query {
                    path: self.database.clone(),
                    source,
                });
            }
        };

        let mut counts = [0u64; 3];
        for (at, table) in ["session", "message", "unit"].iter().enumerate() {
            let query = format!("SELECT COUNT(*) FROM {table}");
            match self
                .connection
                .query_row(&query, [], |row| row.get::<_, i64>(0))
            {
                Ok(count) => counts[at] = count as u64,
                Err(source) => {
                    return Err(Error::Query {
                        path: self.database.clone(),
                        source,
                    });
                }
            }
        }

        Ok(Memory {
            id: identifier(&id),
            name,
            description,
            languages,
            created_at,
            sessions: counts[0],
            messages: counts[1],
            units: counts[2],
        })
    }

    /// Persist one message and the units and sentences it splits into.
    ///
    /// All of it in one transaction, and the raw message written first. A crash
    /// between the message and its units leaves a message that a rescan will
    /// pick up; a crash that left half a message's units behind would leave a
    /// unit that is invisible to search and undetectable afterwards.
    ///
    /// Indexing is *not* done here. The caller writes the returned units into
    /// tantivy, because that index is derived and this module only ever writes
    /// things that are not.
    pub fn add(&mut self, entry: &Entry<'_>) -> Result<Written, Error> {
        if entry.session.chars().count() > REFERENCE_LIMIT {
            return Err(Error::Reference {
                what: "session",
                reference: entry.session.to_string(),
            });
        }
        if let Some(reference) = entry.message
            && reference.chars().count() > REFERENCE_LIMIT
        {
            return Err(Error::Reference {
                what: "message",
                reference: reference.to_string(),
            });
        }

        let transaction = match self.connection.transaction() {
            Ok(transaction) => transaction,
            Err(source) => {
                return Err(Error::Write {
                    path: self.database.clone(),
                    source,
                });
            }
        };

        // The session, found or made. `external_ref` is unique, so the second
        // message of a conversation finds the row the first one wrote.
        let found = transaction.query_row(
            "SELECT id FROM session WHERE external_ref = ?1",
            rusqlite::params![entry.session],
            |row| row.get::<_, Vec<u8>>(0),
        );
        let session = match found {
            Ok(bytes) => identifier(&bytes),
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                let id = match Ulid::new() {
                    Ok(id) => id,
                    Err(source) => return Err(Error::Identifier { source }),
                };
                let written = transaction.execute(
                    "INSERT INTO session (id, ulid, external_ref, started_at)
                     VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![
                        id.bytes().as_slice(),
                        id.to_string(),
                        entry.session,
                        entry.ts
                    ],
                );
                if let Err(source) = written {
                    return Err(Error::Write {
                        path: self.database.clone(),
                        source,
                    });
                }
                id
            }
            Err(source) => {
                return Err(Error::Query {
                    path: self.database.clone(),
                    source,
                });
            }
        };

        // The next ordinal in this session. Gapless, assigned here, and never
        // taken from the feeder: the cursor tool asks for "three messages
        // before this one" and answers it as a range scan, which only works if
        // the numbers are dense and monotonic.
        let seq = match transaction.query_row(
            "SELECT COALESCE(MAX(seq), -1) + 1 FROM message WHERE session_id = ?1",
            rusqlite::params![session.bytes().as_slice()],
            |row| row.get::<_, i64>(0),
        ) {
            Ok(seq) => seq,
            Err(source) => {
                return Err(Error::Query {
                    path: self.database.clone(),
                    source,
                });
            }
        };

        let message = match Ulid::new() {
            Ok(id) => id,
            Err(source) => return Err(Error::Identifier { source }),
        };
        let written = transaction.execute(
            "INSERT INTO message
                (id, ulid, session_id, external_ref, seq, author, role, ts, body)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                message.bytes().as_slice(),
                message.to_string(),
                session.bytes().as_slice(),
                entry.message,
                seq,
                entry.author,
                entry.role.code() as i64,
                entry.ts,
                entry.body,
            ],
        );
        if let Err(source) = written {
            if let rusqlite::Error::SqliteFailure(error, _) = &source
                && error.code == rusqlite::ErrorCode::ConstraintViolation
                && let Some(reference) = entry.message
            {
                return Err(Error::Duplicate {
                    session: entry.session.to_string(),
                    reference: reference.to_string(),
                });
            }
            return Err(Error::Write {
                path: self.database.clone(),
                source,
            });
        }

        let units = write_units(&transaction, &self.database, message, entry.body)?;

        if let Err(source) = transaction.commit() {
            return Err(Error::Write {
                path: self.database.clone(),
                source,
            });
        }

        Ok(Written {
            session,
            message,
            seq,
            units,
        })
    }

    /// Drop every unit and sentence and split every stored message again, in
    /// `(session, seq)` order, returning what the caller has to reindex.
    ///
    /// This is the operation the whole layer split exists for. Change the
    /// normalizer or the splitter, bump [`crate::normalize::VERSION`], run
    /// this. It is supported rather than improvised because it will be run
    /// every time those rules are touched.
    pub fn resplit(&mut self) -> Result<Vec<(Ulid, Written)>, Error> {
        let transaction = match self.connection.transaction() {
            Ok(transaction) => transaction,
            Err(source) => {
                return Err(Error::Write {
                    path: self.database.clone(),
                    source,
                });
            }
        };

        if let Err(source) = transaction.execute_batch("DELETE FROM sentence; DELETE FROM unit;") {
            return Err(Error::Write {
                path: self.database.clone(),
                source,
            });
        }

        let mut messages = Vec::new();
        {
            let mut statement = match transaction.prepare(
                "SELECT m.id, m.session_id, m.seq, m.body
                 FROM message m JOIN session s ON s.id = m.session_id
                 ORDER BY s.id, m.seq",
            ) {
                Ok(statement) => statement,
                Err(source) => {
                    return Err(Error::Query {
                        path: self.database.clone(),
                        source,
                    });
                }
            };
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            });
            let rows = match rows {
                Ok(rows) => rows,
                Err(source) => {
                    return Err(Error::Query {
                        path: self.database.clone(),
                        source,
                    });
                }
            };
            for row in rows {
                match row {
                    Ok(row) => messages.push(row),
                    Err(source) => {
                        return Err(Error::Query {
                            path: self.database.clone(),
                            source,
                        });
                    }
                }
            }
        }

        let mut written = Vec::new();
        for (message, session, seq, body) in &messages {
            let message = identifier(message);
            let units = write_units(&transaction, &self.database, message, body)?;
            written.push((
                message,
                Written {
                    session: identifier(session),
                    message,
                    seq: *seq,
                    units,
                },
            ));
        }

        if let Err(source) = transaction.commit() {
            return Err(Error::Write {
                path: self.database.clone(),
                source,
            });
        }
        Ok(written)
    }

    /// The body, role and timestamp of a message: everything the index needs
    /// to write its units again, in one query rather than three.
    pub fn message(&self, message: Ulid) -> Result<(String, u64, i64), Error> {
        match self.connection.query_row(
            "SELECT body, role, ts FROM message WHERE id = ?1",
            rusqlite::params![message.bytes().as_slice()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)? as u64,
                    row.get::<_, i64>(2)?,
                ))
            },
        ) {
            Ok(row) => Ok(row),
            Err(source) => Err(Error::Query {
                path: self.database.clone(),
                source,
            }),
        }
    }

    /// Resolve units to everything a result line needs, in one query per unit.
    ///
    /// Units that no longer exist are skipped rather than reported: the caller
    /// got them from the index, and an id in the index that is not in the
    /// database means the two are out of step, which `rescan` fixes and a
    /// missing row in a result set does not.
    pub fn locate(&self, ids: &[Ulid]) -> Result<Vec<Located>, Error> {
        let mut located = Vec::with_capacity(ids.len());
        for id in ids {
            let row = self.connection.query_row(
                "SELECT u.seq, u.byte_start, u.byte_end,
                        m.id, m.external_ref, m.seq, m.author, m.role, m.ts, m.body,
                        s.id, s.external_ref
                 FROM unit u
                 JOIN message m ON m.id = u.message_id
                 JOIN session s ON s.id = m.session_id
                 WHERE u.id = ?1",
                rusqlite::params![id.bytes().as_slice()],
                |row| {
                    Ok(Located {
                        unit: *id,
                        unit_seq: row.get(0)?,
                        start: row.get::<_, i64>(1)? as usize,
                        end: row.get::<_, i64>(2)? as usize,
                        message: identifier(&row.get::<_, Vec<u8>>(3)?),
                        message_ref: row.get(4)?,
                        seq: row.get(5)?,
                        author: row.get(6)?,
                        role: Role::from_code(row.get::<_, i64>(7)? as u64),
                        ts: row.get(8)?,
                        body: row.get(9)?,
                        session: identifier(&row.get::<_, Vec<u8>>(10)?),
                        session_ref: row.get(11)?,
                    })
                },
            );
            match row {
                Ok(row) => located.push(row),
                Err(rusqlite::Error::QueryReturnedNoRows) => continue,
                Err(source) => {
                    return Err(Error::Query {
                        path: self.database.clone(),
                        source,
                    });
                }
            }
        }
        Ok(located)
    }

    /// The sentence offsets of a unit, in order. Used to cut a snippet tighter
    /// than the whole paragraph when a match sits in one sentence of it.
    pub fn sentences(&self, unit: Ulid) -> Result<Vec<(usize, usize)>, Error> {
        let mut statement = match self
            .connection
            .prepare("SELECT byte_start, byte_end FROM sentence WHERE unit_id = ?1 ORDER BY seq")
        {
            Ok(statement) => statement,
            Err(source) => {
                return Err(Error::Query {
                    path: self.database.clone(),
                    source,
                });
            }
        };
        let rows = statement.query_map(rusqlite::params![unit.bytes().as_slice()], |row| {
            Ok((
                row.get::<_, i64>(0)? as usize,
                row.get::<_, i64>(1)? as usize,
            ))
        });
        let rows = match rows {
            Ok(rows) => rows,
            Err(source) => {
                return Err(Error::Query {
                    path: self.database.clone(),
                    source,
                });
            }
        };
        let mut sentences = Vec::new();
        for row in rows {
            match row {
                Ok(row) => sentences.push(row),
                Err(source) => {
                    return Err(Error::Query {
                        path: self.database.clone(),
                        source,
                    });
                }
            }
        }
        Ok(sentences)
    }

    /// The messages around a unit: the cursor tool's whole implementation.
    ///
    /// A range scan on `(session_id, seq)`, which is exact and index-only
    /// because the ordinals are gapless. No scoring and no snippets — the
    /// caller has already decided this region is worth reading, and cutting it
    /// down again would be answering a question it did not ask.
    pub fn around(&self, unit: Ulid, before: i64, after: i64) -> Result<Vec<Message>, Error> {
        let anchor = self.connection.query_row(
            "SELECT m.session_id, m.seq FROM unit u JOIN message m ON m.id = u.message_id
             WHERE u.id = ?1",
            rusqlite::params![unit.bytes().as_slice()],
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?)),
        );
        let (session, seq) = match anchor {
            Ok(anchor) => anchor,
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                return Err(Error::Unknown {
                    id: unit.to_string(),
                });
            }
            Err(source) => {
                return Err(Error::Query {
                    path: self.database.clone(),
                    source,
                });
            }
        };

        let mut statement = match self.connection.prepare(
            "SELECT id, external_ref, seq, author, role, ts, body FROM message
             WHERE session_id = ?1 AND seq >= ?2 AND seq <= ?3
             ORDER BY seq",
        ) {
            Ok(statement) => statement,
            Err(source) => {
                return Err(Error::Query {
                    path: self.database.clone(),
                    source,
                });
            }
        };
        let rows = statement.query_map(
            rusqlite::params![session, seq - before, seq + after],
            |row| {
                let at: i64 = row.get(2)?;
                Ok(Message {
                    id: identifier(&row.get::<_, Vec<u8>>(0)?),
                    reference: row.get(1)?,
                    seq: at,
                    author: row.get(3)?,
                    role: Role::from_code(row.get::<_, i64>(4)? as u64),
                    ts: row.get(5)?,
                    body: row.get(6)?,
                    anchor: at == seq,
                })
            },
        );
        let rows = match rows {
            Ok(rows) => rows,
            Err(source) => {
                return Err(Error::Query {
                    path: self.database.clone(),
                    source,
                });
            }
        };
        let mut messages = Vec::new();
        for row in rows {
            match row {
                Ok(row) => messages.push(row),
                Err(source) => {
                    return Err(Error::Query {
                        path: self.database.clone(),
                        source,
                    });
                }
            }
        }
        Ok(messages)
    }

    /// Record a query and what it returned.
    ///
    /// Free to write and impossible to recover later: when the agent searches,
    /// gets twenty results and then reaches for the cursor on the seventh, that
    /// pair of rows is a relevance judgement produced by normal use.
    pub fn log_search(&self, groups: &str, returned: &str) -> Result<Ulid, Error> {
        let id = match Ulid::new() {
            Ok(id) => id,
            Err(source) => return Err(Error::Identifier { source }),
        };
        let written = self.connection.execute(
            "INSERT INTO search_log (id, ts, groups_json, returned_json) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                id.bytes().as_slice(),
                id.milliseconds() as i64,
                groups,
                returned
            ],
        );
        if let Err(source) = written {
            return Err(Error::Write {
                path: self.database.clone(),
                source,
            });
        }
        Ok(id)
    }

    /// Record that a unit returned by some earlier search was expanded.
    ///
    /// The search it belongs to is the most recent one that returned this unit,
    /// found here rather than passed in, because the caller of the cursor tool
    /// holds an opaque string and should not have to also carry a search id
    /// around to make the log work.
    pub fn log_expansion(&self, unit: Ulid) -> Result<(), Error> {
        let found = self.connection.query_row(
            "SELECT id FROM search_log WHERE returned_json LIKE ?1 ORDER BY id DESC LIMIT 1",
            rusqlite::params![format!("%{}%", unit)],
            |row| row.get::<_, Vec<u8>>(0),
        );
        let search = match found {
            Ok(search) => search,
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(()),
            Err(source) => {
                return Err(Error::Query {
                    path: self.database.clone(),
                    source,
                });
            }
        };
        let written = self.connection.execute(
            "INSERT OR IGNORE INTO expansion_log (search_id, unit_id, ts) VALUES (?1, ?2, ?3)",
            rusqlite::params![search, unit.bytes().as_slice(), Ulid::now()],
        );
        if let Err(source) = written {
            return Err(Error::Write {
                path: self.database.clone(),
                source,
            });
        }
        Ok(())
    }

    /// Read one `index_meta` value.
    pub fn meta(&self, key: &str) -> Result<Option<String>, Error> {
        match self.connection.query_row(
            "SELECT value FROM index_meta WHERE key = ?1",
            rusqlite::params![key],
            |row| row.get::<_, String>(0),
        ) {
            Ok(value) => Ok(Some(value)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(source) => Err(Error::Query {
                path: self.database.clone(),
                source,
            }),
        }
    }

    /// Write one `index_meta` value.
    pub fn set_meta(&self, key: &str, value: &str) -> Result<(), Error> {
        let written = self.connection.execute(
            "INSERT INTO index_meta (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = ?2",
            rusqlite::params![key, value],
        );
        if let Err(source) = written {
            return Err(Error::Write {
                path: self.database.clone(),
                source,
            });
        }
        Ok(())
    }
}

/// Split a message body and write its `unit` and `sentence` rows.
///
/// Shared by [`Storage::add`] and [`Storage::resplit`] — the two callers that
/// must not disagree, because the second one exists to reproduce the first.
fn write_units(
    transaction: &rusqlite::Transaction<'_>,
    database: &Path,
    message: Ulid,
    body: &str,
) -> Result<Vec<Unit>, Error> {
    let mut units = Vec::new();
    for (seq, block) in split(body).into_iter().enumerate() {
        let id = match Ulid::new() {
            Ok(id) => id,
            Err(source) => return Err(Error::Identifier { source }),
        };
        let written = transaction.execute(
            "INSERT INTO unit (id, message_id, seq, byte_start, byte_end)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                id.bytes().as_slice(),
                message.bytes().as_slice(),
                seq as i64,
                block.start as i64,
                block.end as i64,
            ],
        );
        if let Err(source) = written {
            return Err(Error::Write {
                path: database.to_path_buf(),
                source,
            });
        }

        for (at, (start, end)) in sentences(body, &block).into_iter().enumerate() {
            let written = transaction.execute(
                "INSERT INTO sentence (unit_id, seq, byte_start, byte_end)
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![id.bytes().as_slice(), at as i64, start as i64, end as i64],
            );
            if let Err(source) = written {
                return Err(Error::Write {
                    path: database.to_path_buf(),
                    source,
                });
            }
        }

        units.push(Unit {
            id,
            start: block.start,
            end: block.end,
        });
    }
    Ok(units)
}

/// One block of a message: a byte range, and whether it is code.
#[derive(Debug, Clone, Copy)]
struct Block {
    start: usize,
    end: usize,
    code: bool,
}

/// Cut a message body into units.
///
/// The text is read as Markdown, because that is what a chat transcript is, and
/// a CommonMark parser is what tells a fenced code block from a list item from
/// a run of prose — which is the difference between splitting on blank lines
/// and splitting on meaning. Everything is a byte range into the input; nothing
/// is copied.
///
/// The unit is the retrieval granularity, so the rule is the innermost block
/// that holds text: a paragraph, a heading, a table row, a list item, a fenced
/// block. A list item that contains a paragraph yields the paragraph; one that
/// does not — a tight list, where CommonMark puts the text straight in the item
/// — yields the item, cut short at whatever is nested inside it so no text is
/// counted twice.
fn split(text: &str) -> Vec<Block> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_TASKLISTS);

    let mut leaves: Vec<Block> = Vec::new();
    let mut items: Vec<(usize, usize)> = Vec::new();
    for (event, range) in Parser::new_ext(text, options).into_offset_iter() {
        let Event::Start(tag) = event else {
            continue;
        };
        match tag {
            Tag::Paragraph
            | Tag::Heading { .. }
            | Tag::HtmlBlock
            | Tag::TableRow
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition => leaves.push(Block {
                start: range.start,
                end: range.end,
                code: false,
            }),
            Tag::CodeBlock(_) => leaves.push(Block {
                start: range.start,
                end: range.end,
                code: true,
            }),
            Tag::Item => items.push((range.start, range.end)),
            _ => {}
        }
    }

    // Innermost items first, so an item that only contains other items is
    // trimmed against them rather than the other way round.
    items.sort_by_key(|(start, end)| end - start);
    for (start, end) in items {
        let mut cut = end;
        let mut inside = false;
        for block in &leaves {
            if block.start >= start && block.end <= end {
                inside = true;
                cut = cut.min(block.start);
            }
        }
        // An item whose whole content is a nested block contributes nothing of
        // its own; one with a line of its own in front of the nesting keeps
        // that line and nothing after it.
        if inside && text[start..cut].trim().is_empty() {
            continue;
        }
        leaves.push(Block {
            start,
            end: cut,
            code: false,
        });
    }

    leaves.sort_by_key(|block| (block.start, block.end));

    // Trim the edges and drop what is left of an empty block. The ranges a
    // parser hands back run to the start of the next block, so most of them end
    // in the newline that separated them.
    let mut units: Vec<Block> = Vec::new();
    for block in leaves {
        let slice = &text[block.start..block.end];
        let front = slice.len() - slice.trim_start().len();
        let back = slice.len() - slice.trim_end().len();
        if front + back >= slice.len() {
            continue;
        }
        let block = Block {
            start: block.start + front,
            end: block.end - back,
            code: block.code,
        };
        // A block fully inside one already taken is the outer block's text a
        // second time, and a duplicated unit is a duplicated hit.
        if let Some(last) = units.last()
            && block.start >= last.start
            && block.end <= last.end
        {
            continue;
        }
        units.push(block);
    }

    // A message with no Markdown structure at all — a single line with no
    // trailing newline is still a paragraph to CommonMark, but an empty body
    // is not, and a unitless message would be stored and never findable.
    if units.is_empty() && !text.trim().is_empty() {
        let front = text.len() - text.trim_start().len();
        let back = text.len() - text.trim_end().len();
        units.push(Block {
            start: front,
            end: text.len() - back,
            code: false,
        });
    }
    units
}

/// Cut a unit into sentences, as byte ranges into the whole message body.
///
/// Persian is more forgiving here than it looks: `؟`, `!` and `.` all end a
/// sentence and `؛` is a strong enough break to treat as one. What has to be
/// caught is the full stop that is not one — inside `3.14`, `e.g.` or a URL —
/// which is why a terminator only counts when what follows it is whitespace.
///
/// Code has no sentences, so a run of lines stands in for one.
fn sentences(text: &str, block: &Block) -> Vec<(usize, usize)> {
    let slice = &text[block.start..block.end];
    let mut cuts = Vec::new();

    if block.code {
        let mut start = 0;
        let mut lines = 0;
        for (at, character) in slice.char_indices() {
            if character != '\n' {
                continue;
            }
            lines += 1;
            if lines < CODE_LINES {
                continue;
            }
            cuts.push((start, at + 1));
            start = at + 1;
            lines = 0;
        }
        if start < slice.len() {
            cuts.push((start, slice.len()));
        }
    } else {
        let mut start = 0;
        let characters: Vec<(usize, char)> = slice.char_indices().collect();
        for (at, (offset, character)) in characters.iter().enumerate() {
            if !matches!(character, '.' | '!' | '?' | '؟' | '؛' | '\n') {
                continue;
            }
            let next = characters.get(at + 1);
            let ends = match next {
                None => true,
                Some((_, following)) => following.is_whitespace(),
            };
            if !ends {
                continue;
            }
            let end = offset + character.len_utf8();
            if slice[start..end].trim().is_empty() {
                start = end;
                continue;
            }
            cuts.push((start, end));
            start = end;
        }
        if !slice[start..].trim().is_empty() {
            cuts.push((start, slice.len()));
        }
    }

    let mut sentences = Vec::with_capacity(cuts.len());
    for (start, end) in cuts {
        let piece = &slice[start..end];
        let front = piece.len() - piece.trim_start().len();
        let back = piece.len() - piece.trim_end().len();
        if front + back >= piece.len() {
            continue;
        }
        sentences.push((block.start + start + front, block.start + end - back));
    }
    sentences
}

/// A ULID back out of a `BLOB(16)` column.
///
/// The columns this reads are written by this module and are always sixteen
/// bytes; the padding is here so that a truncated database is a wrong id rather
/// than a panic in the middle of a result set.
fn identifier(bytes: &[u8]) -> Ulid {
    let mut key = [0u8; 16];
    let take = bytes.len().min(16);
    key[..take].copy_from_slice(&bytes[..take]);
    Ulid::from_bytes(key)
}

/// A name is `a-z`, `0-9` and `_`. It is a directory name, it is what every
/// command takes to find the memory, and it goes into paths and log lines, so
/// anything that would need quoting or escaping is refused at the door.
/// A description is what a calling model reads to decide whether this memory
/// is the one to search, so a phrase shorter than a sentence is not enough
/// and a wall of text is not either.
fn check_description(text: &str) -> Result<(), Error> {
    if text.chars().count() > DESCRIPTION_LIMIT {
        return Err(Error::Description);
    }
    let mut words = 0;
    for _ in text.split_whitespace() {
        words += 1;
    }
    if words <= 10 {
        return Err(Error::DescriptionShort);
    }
    Ok(())
}

fn check_name(name: &str) -> Result<(), Error> {
    if name.is_empty() {
        return Err(Error::Empty);
    }
    if name.chars().count() > NAME_LIMIT {
        return Err(Error::Long {
            name: name.to_string(),
        });
    }
    for character in name.chars() {
        if !character.is_ascii_lowercase() && !character.is_ascii_digit() && character != '_' {
            return Err(Error::Charset {
                name: name.to_string(),
            });
        }
    }
    Ok(())
}
