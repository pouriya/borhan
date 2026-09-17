//! Layer two: the inverted index, and the only thing in borhan that a rescan
//! is allowed to destroy.
//!
//! One tantivy index per memory, in `<home>/storage/<name>/index/`, holding one
//! document per unit. Document frequencies are therefore memory-local, which is
//! the whole reason for the per-memory layout: `error` is common in an
//! infrastructure room and rare in a scheduling one, and an IDF pooled across
//! both is wrong for each.
//!
//! # Why three fields over the same text
//!
//! The design document reaches this shape through a `term` table and a `lemma`
//! table; tantivy reaches it through fields, and the fields are better because
//! per-field document frequency comes out of the segment format for free.
//!
//! - `surface` holds the token as it was written. `JWT_SECRET`, `jwt_secret`
//!   and `Error` are separate terms with separate postings, which is what keeps
//!   an exact search for an identifier from being diluted by everything that
//!   folds onto it.
//! - `lemma` holds the normalized token. Its document frequency is incremented
//!   once per unit containing *any* spelling, so it is the correct denominator
//!   for IDF — scoring from surface frequency would give a rare misspelling an
//!   enormous IDF and let it outrank genuinely rare content.
//! - `context` holds terms propagated from the rest of the message, so a unit
//!   reading "he fixed it by rotating that" is findable at all. It carries no
//!   positions, is scored at a fraction of body weight, and — because it is a
//!   separate field — **cannot touch the document frequency of the other two**.
//!   Letting propagated terms inflate df would corrupt IDF across the index.
//!
//! Both body fields are fed the *same* text and differ only in their analyzer,
//! so token position 7 is the same word in both. That is what lets proximity
//! measure a span between a query clause that matched exactly and one that
//! matched after folding.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tantivy::schema::{
    INDEXED, IndexRecordOption, STORED, Schema, TextFieldIndexing, TextOptions, Value,
};
use tantivy::{IndexReader, IndexWriter, TantivyDocument, Term, doc};

use crate::normalize::{self, Analyzer};
use crate::storage::{Storage, Written};
use crate::ulid::Ulid;

/// The index directory inside a memory's directory, beside `borhan.db`.
pub const DIRECTORY: &str = "index";

/// `index_meta` key holding the [`normalize::VERSION`] the index was built
/// with. Compared on open; a mismatch is refused rather than served, because an
/// index built by older rules answers queries normalized by newer ones with
/// silence, and silence looks exactly like an empty memory.
pub const VERSION_KEY: &str = "normalization_version";

/// Name of the timestamp fast field. Looked up by name inside the collector,
/// where only a `SegmentReader` is in scope.
pub const TS: &str = "ts";

/// `index_meta` key holding when the index was last built or rebuilt, as unix
/// milliseconds.
pub const BUILT_KEY: &str = "built_at";

/// Bytes of heap the writer may use before it flushes a segment. Tantivy's
/// floor is 15 MB; this is the smallest round number well clear of it, and a
/// larger buffer would only matter to a bulk load that this binary does one
/// message at a time anyway.
const HEAP: usize = 50_000_000;

/// Terms propagated from the rest of a message into each of its units.
///
/// A handful, by design. Propagation exists to make an anaphoric unit reachable
/// at all, not to make every unit of a message match everything the message
/// discusses; past a few terms it stops being context and starts being noise
/// that dilutes the diversity cap.
const CONTEXT_TERMS: usize = 8;

/// Fraction of the index a term may appear in and still be worth propagating.
///
/// Above this it is a word the memory uses everywhere, so it says nothing about
/// which unit you want.
const CONTEXT_CEILING: f64 = 0.15;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not open the index at {path}")]
    Open {
        path: PathBuf,
        #[source]
        source: tantivy::TantivyError,
    },

    #[error("could not write to the index at {path}")]
    Write {
        path: PathBuf,
        #[source]
        source: tantivy::TantivyError,
    },

    #[error("could not read the index at {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: tantivy::TantivyError,
    },

    #[error(
        "the index at {path} was built with normalization rules v{found}, this borhan is v{wanted} — run `borhan memory rescan`"
    )]
    Stale {
        path: PathBuf,
        found: u32,
        wanted: u32,
    },
}

/// The fields of a unit document, resolved once so nothing looks them up by
/// name in a loop.
#[derive(Debug, Clone, Copy)]
pub struct Fields {
    /// The unit's ULID, stored so a hit can be resolved back to layer one.
    pub unit: tantivy::schema::Field,
    /// The session's ULID as text, indexed so a search can be confined to one
    /// conversation.
    pub session: tantivy::schema::Field,
    /// Unix milliseconds, a fast field because recency decay reads it for every
    /// candidate unit of every query.
    pub ts: tantivy::schema::Field,
    /// [`crate::storage::Role::code`], for the role filter.
    pub role: tantivy::schema::Field,
    pub surface: tantivy::schema::Field,
    pub lemma: tantivy::schema::Field,
    pub context: tantivy::schema::Field,
}

/// One memory's tantivy index, open for reading and writing.
pub struct Index {
    pub index: tantivy::Index,
    pub fields: Fields,
    pub reader: IndexReader,
    path: PathBuf,
}

impl Index {
    /// Open the index beside a memory's database, creating it if it is not
    /// there, and refuse it if it was built by a different normalizer.
    pub fn open(storage: &Storage) -> Result<Self, Error> {
        let index = Self::attach(&storage.directory.join(DIRECTORY))?;
        let found = match storage.meta(VERSION_KEY) {
            Ok(Some(value)) => value.parse::<u32>().unwrap_or(0),
            // No row means an index that has never been built. An empty index
            // is not stale, it is empty, and every search over it correctly
            // returns nothing.
            _ => normalize::VERSION,
        };
        if found != normalize::VERSION {
            return Err(Error::Stale {
                path: index.path.clone(),
                found,
                wanted: normalize::VERSION,
            });
        }
        Ok(index)
    }

    /// Open without the version check, for `rescan`, which is the command whose
    /// entire job is to make a stale index current.
    pub fn attach(path: &Path) -> Result<Self, Error> {
        let mut builder = Schema::builder();

        // Positions on both body fields: proximity is a strong precision signal
        // in short chat paragraphs, and positions cannot be added later without
        // a full rebuild, so the decision is made once, here, in favour.
        let body = |tokenizer: &str| {
            TextOptions::default().set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer(tokenizer)
                    .set_index_option(IndexRecordOption::WithFreqsAndPositions),
            )
        };
        // No positions on the context field. Propagated terms did not occur
        // anywhere in particular, so a position for them would be a fiction
        // that proximity scoring would then measure.
        let context = TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(normalize::LEMMA)
                .set_index_option(IndexRecordOption::WithFreqs),
        );
        let raw = TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("raw")
                .set_index_option(IndexRecordOption::Basic),
        );

        let fields = Fields {
            unit: builder.add_bytes_field("unit", STORED),
            session: builder.add_text_field("session", raw),
            ts: builder.add_i64_field(TS, INDEXED | tantivy::schema::FAST),
            role: builder.add_u64_field("role", INDEXED),
            surface: builder.add_text_field(normalize::SURFACE, body(normalize::SURFACE)),
            lemma: builder.add_text_field(normalize::LEMMA, body(normalize::LEMMA)),
            context: builder.add_text_field("context", context),
        };

        if let Err(source) = std::fs::create_dir_all(path) {
            return Err(Error::Open {
                path: path.to_path_buf(),
                source: tantivy::TantivyError::IoError(std::sync::Arc::new(source)),
            });
        }
        let directory = match tantivy::directory::MmapDirectory::open(path) {
            Ok(directory) => directory,
            Err(source) => {
                return Err(Error::Open {
                    path: path.to_path_buf(),
                    source: source.into(),
                });
            }
        };
        let index = match tantivy::Index::open_or_create(directory, builder.build()) {
            Ok(index) => index,
            Err(source) => {
                return Err(Error::Open {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };

        // The two analyzers, over one segmenter. Registered on the index rather
        // than passed around, because tantivy resolves a field's tokenizer by
        // the name recorded in the schema when the segment is written.
        index
            .tokenizers()
            .register(normalize::SURFACE, Analyzer::new(false));
        index
            .tokenizers()
            .register(normalize::LEMMA, Analyzer::new(true));

        let reader = match index.reader() {
            Ok(reader) => reader,
            Err(source) => {
                return Err(Error::Open {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };

        Ok(Self {
            index,
            fields,
            reader,
            path: path.to_path_buf(),
        })
    }

    /// A writer. One per process at a time — tantivy holds a lock file — so
    /// this is called once per command and not once per message.
    pub fn writer(&self) -> Result<IndexWriter, Error> {
        match self.index.writer::<TantivyDocument>(HEAP) {
            Ok(writer) => Ok(writer),
            Err(source) => Err(Error::Write {
                path: self.path.clone(),
                source,
            }),
        }
    }

    /// Index every unit of one message.
    ///
    /// The message is already durable in SQLite by the time this runs, so a
    /// crash here costs an index that `rescan` rebuilds rather than a message
    /// that is gone.
    pub fn add(
        &self,
        writer: &IndexWriter,
        written: &Written,
        role: u64,
        ts: i64,
        body: &str,
    ) -> Result<(), Error> {
        // Every term of the message, so each unit can be given the ones it does
        // not contain itself. Computed once per message rather than once per
        // unit, and from the message alone: a window over the session would be
        // a second read of rows this transaction has not written yet.
        let mut shared: Vec<(u32, String)> = Vec::new();
        let searcher = self.reader.searcher();
        let total = searcher.num_docs().max(1);
        let ceiling = (total as f64 * CONTEXT_CEILING) as u32;
        for unit in &written.units {
            for word in normalize::segment(&body[unit.start..unit.end]) {
                let text = normalize::lemma(word.text, word.script);
                if text.is_empty() || shared.iter().any(|(_, held)| *held == text) {
                    continue;
                }
                let term = Term::from_field_text(self.fields.lemma, &text);
                let df = searcher.doc_freq(&term).unwrap_or(0) as u32;
                if df > ceiling && ceiling > 0 {
                    continue;
                }
                shared.push((df, text));
            }
        }
        // Rarest first, ties broken by the term itself so that a rescan of the
        // same messages produces the same index.
        shared.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

        for unit in &written.units {
            let text = &body[unit.start..unit.end];

            let mut own = Vec::new();
            for word in normalize::segment(text) {
                own.push(normalize::lemma(word.text, word.script));
            }
            let mut context = String::new();
            for (_, term) in &shared {
                if own.iter().any(|held| held == term) {
                    continue;
                }
                if !context.is_empty() {
                    context.push(' ');
                }
                context.push_str(term);
                if context.split(' ').count() >= CONTEXT_TERMS {
                    break;
                }
            }

            let document = doc!(
                self.fields.unit => unit.id.bytes().to_vec(),
                self.fields.session => written.session.to_string(),
                self.fields.ts => ts,
                self.fields.role => role,
                // The same text twice. The analyzers differ, the tokens do not.
                self.fields.surface => text,
                self.fields.lemma => text,
                self.fields.context => context,
            );
            if let Err(source) = writer.add_document(document) {
                return Err(Error::Write {
                    path: self.path.clone(),
                    source,
                });
            }
        }
        Ok(())
    }

    /// Throw the whole index away, for `rescan`.
    /// Drop every unit of one session.
    ///
    /// By term, which is why `session` is an indexed field at all beyond the
    /// search filter it was added for. A unit carries no message term, so this
    /// is the narrowest thing the index can be asked to forget — narrower than
    /// [`Index::clear`], and the reason `replace` rewrites a session rather
    /// than rebuilding a memory.
    ///
    /// Deletion in tantivy is not visible until a commit, and the caller has
    /// the writer, so this does not commit: the delete and the units that
    /// replace it belong in one commit, or a crash between them leaves the
    /// session missing from the index rather than merely out of date.
    pub fn forget(&self, writer: &IndexWriter, session: Ulid) -> Result<(), Error> {
        let term = Term::from_field_text(self.fields.session, &session.to_string());
        writer.delete_term(term);
        Ok(())
    }

    pub fn clear(&self, writer: &mut IndexWriter) -> Result<(), Error> {
        if let Err(source) = writer.delete_all_documents() {
            return Err(Error::Write {
                path: self.path.clone(),
                source,
            });
        }
        Ok(())
    }

    /// Commit, and make the new segments visible to the reader this struct
    /// holds. Without the reload a search in the same process would still be
    /// looking at the searcher it opened before the write.
    pub fn commit(&self, writer: &mut IndexWriter) -> Result<(), Error> {
        if let Err(source) = writer.commit() {
            return Err(Error::Write {
                path: self.path.clone(),
                source,
            });
        }
        if let Err(source) = self.reader.reload() {
            return Err(Error::Read {
                path: self.path.clone(),
                source,
            });
        }
        Ok(())
    }

    /// The unit ULID stored on a document.
    pub fn unit(&self, address: tantivy::DocAddress) -> Result<Option<Ulid>, Error> {
        let searcher = self.reader.searcher();
        let document: TantivyDocument = match searcher.doc(address) {
            Ok(document) => document,
            Err(source) => {
                return Err(Error::Read {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        for (field, value) in document.field_values() {
            if field != self.fields.unit {
                continue;
            }
            if let Some(bytes) = value.as_bytes()
                && bytes.len() == 16
            {
                let mut key = [0u8; 16];
                key.copy_from_slice(bytes);
                return Ok(Some(Ulid::from_bytes(key)));
            }
        }
        Ok(None)
    }

    /// The memory's own vocabulary: the lemmas it uses most, minus the ones it
    /// uses everywhere.
    ///
    /// What a caller about to search is missing is not the ranking rules but
    /// the words. A memory of Persian tele-triage transcripts answers to `تب`
    /// and not to `fever`, and nothing in its description says so — the
    /// description was written by whoever created the memory, the vocabulary
    /// was written by whoever filled it. Reading this before writing a query is
    /// the difference between one shot in the dark and an informed one, which
    /// is the same argument the lexicon and the hints already make.
    ///
    /// The ceiling is [`CONTEXT_CEILING`], reused rather than picked again
    /// because the question is identical: a lemma present in a seventh of the
    /// units names the corpus and not anything inside it, and a list headed by
    /// `و` and `the` tells a caller nothing it could search for.
    pub fn vocabulary(&self, limit: usize) -> Result<Vec<(String, u64)>, Error> {
        let searcher = self.reader.searcher();
        // Zero units is an empty memory, and an empty memory has an empty
        // vocabulary — the ceiling below would be zero and exclude every term
        // there is, which is the right answer arrived at the wrong way.
        let ceiling = ((searcher.num_docs() as f64) * CONTEXT_CEILING) as u64;
        if ceiling == 0 {
            return Ok(Vec::new());
        }

        let mut counts: HashMap<String, u64> = HashMap::new();
        for reader in searcher.segment_readers() {
            let inverted = match reader.inverted_index(self.fields.lemma) {
                Ok(inverted) => inverted,
                Err(source) => {
                    return Err(Error::Read {
                        path: self.path.clone(),
                        source,
                    });
                }
            };
            let mut stream = match inverted.terms().stream() {
                Ok(stream) => stream,
                Err(source) => {
                    return Err(Error::Read {
                        path: self.path.clone(),
                        source: tantivy::TantivyError::from(source),
                    });
                }
            };
            // Summed across segments rather than read off one: a unit lives in
            // exactly one segment, so the sum over segments *is* the index-wide
            // document frequency. It counts units deleted but not yet merged
            // away, which for an ordering of the hundred commonest words is
            // noise rather than error.
            while stream.advance() {
                let Ok(word) = std::str::from_utf8(stream.key()) else {
                    continue;
                };
                let frequency = counts.entry(word.to_string()).or_insert(0);
                *frequency += stream.value().doc_freq as u64;
            }
        }

        let mut words: Vec<(String, u64)> = Vec::new();
        for (word, frequency) in counts {
            if frequency > ceiling {
                continue;
            }
            words.push((word, frequency));
        }
        // Frequency descending, then the word itself, so that two lemmas with
        // the same count come out in the same order on every call. A resource a
        // client is entitled to cache must not change because a `HashMap`
        // iterated differently this time.
        words.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
        words.truncate(limit);
        Ok(words)
    }
}
