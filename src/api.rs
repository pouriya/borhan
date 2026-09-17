//! Shared memory operations, and the HTTP API that serves them.
//!
//! CLI and `serve` both call the functions in this file. The CLI prints; the
//! router serializes the same values as JSON. A ULID is minted by the caller —
//! the HTTP middleware for `serve`, the command for the CLI — logged on every
//! event as `trace_id`, returned in `stats.trace`, and copied to `X-Trace-Id`.
//! Every operation runs inside one span, named for what it does —
//! `memory.search`, `memory.rescan` — and under `http.server` when it arrived
//! over HTTP. Spans hold no fields: they are the trace path, and the subscriber
//! prints that path as `spans` on each line. Each operation emits exactly one
//! event, at the end, carrying `trace_id`, the identity of the work and every
//! duration it measured. One request is therefore two lines, the operation and
//! the response, and both are findable by the same `trace_id`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::body::HttpBody;
use axum::extract::{MatchedPath, Path as UrlPath, Request, State};
use axum::http::{
    HeaderMap, HeaderName, HeaderValue, StatusCode, Uri, header::AUTHORIZATION,
    header::CONTENT_LENGTH, header::CONTENT_TYPE, header::HOST,
};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get as get_route, patch, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use tantivy::IndexWriter;
use tracing::Instrument;

use crate::index::Index;
use crate::search::{Filter, Outcome};
use crate::storage::{
    Entry, Located, Memory, MessageRow, Revision, Role, SessionRow, Storage, Written,
};
use crate::ulid::Ulid;
use crate::{index, normalize, search};

const TRACE_HEADER: HeaderName = HeaderName::from_static("x-trace-id");
const SERVER_HEADER: HeaderName = HeaderName::from_static("server");
const VERSION_HEADER: HeaderName = HeaderName::from_static("x-borhan-version");
const VERSION: &str = env!("CARGO_PKG_VERSION");
const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");

#[derive(Debug, thiserror::Error)]
pub enum Error {
    // Deliberately says nothing about how to enable the operation.
    //
    // It used to name the key and the file: `add "replace" to permissions in
    // server.toml and restart it`. A model that read that took it as a repair
    // instruction and went looking for the file, and when it could not restart
    // the server it tried again — the error had turned a settled decision into
    // a task. An error is read by whoever is holding the failure, and here that
    // is the caller, who is precisely the party the refusal is aimed at. The
    // person who can change it is not in this conversation.
    #[error(
        "refused: this server does not do {permission}. This is how it is \
         configured, not a fault and not a problem to solve — do not retry, do \
         not go looking for the configuration, and do not restart anything. \
         Report it to the person you are working for and let them decide."
    )]
    Forbidden { permission: &'static str },

    #[error("pass at least one of description and languages")]
    NothingToUpdate,

    #[error("pass at least one unit ULID to read back")]
    EmptyIds,

    #[error("pass at least one word to look up")]
    EmptyWords,

    #[error("Role {role:?} is not one of user, assistant or tool")]
    Role { role: String },

    #[error(
        "`group_list` is gone: a search takes `query`, one string. Each group \
         becomes a clause in parentheses and a required one gets a leading + — \
         [{{\"word_list\": [\"error\", \"fault\"], \"required\": true}}, \
         {{\"word_list\": [\"token\"]}}] is \"+(error fault) token\""
    )]
    Groups,

    #[error("{value:?} is not a ULID")]
    NotUlid {
        value: String,
        #[source]
        source: crate::ulid::Error,
    },

    #[error(transparent)]
    Storage(#[from] crate::storage::Error),

    #[error(transparent)]
    Index(#[from] crate::index::Error),

    #[error(transparent)]
    Search(#[from] crate::search::Error),

    #[error(transparent)]
    Identifier(#[from] crate::ulid::Error),
}

/// Timings for one operation, in milliseconds. `None` fields were not run.
#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    pub trace: String,
    pub total_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store_lock_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub writer_lock_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fetch_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lexicon_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resplit_ms: Option<u64>,
}

impl Stats {
    fn new(trace: &Ulid, started: Instant) -> Self {
        Self {
            trace: trace.to_string(),
            total_ms: ms(started),
            open_ms: None,
            store_lock_ms: None,
            writer_lock_ms: None,
            fetch_ms: None,
            write_ms: None,
            search_ms: None,
            index_ms: None,
            commit_ms: None,
            cursor_ms: None,
            lexicon_ms: None,
            resplit_ms: None,
        }
    }
}

/// A way the HTTP server may change what is stored.
///
/// Reads are not on this list and never will be: what a caller may see is the
/// token's business, and a server that answers a search at all can answer any
/// search. These are the operations that leave the store different afterwards,
/// which is a different question with a different answer per deployment — a
/// read-mostly server behind an agent may want `add` and nothing else, and the
/// one place `delete` belongs is a workstation.
///
/// **Every one of them is allowed unless `server.toml` refuses it by name.**
/// The configuration is a list of what this server will not do, not a list of
/// what it will; see [`Permission::ALL`], which is what an absent list resolves
/// to.
///
/// The CLI does not consult these. It is running as whoever invoked it, on
/// files they can already remove with `rm`, and a permission list in a file
/// they own would be a pretence rather than a control. This is about what a
/// process reachable over a socket will do on behalf of somebody else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    Create,
    Update,
    Add,
    Replace,
    Rescan,
    Delete,
}

impl Permission {
    /// Every operation there is, and what a server with no `refuse` list does.
    ///
    /// The default is *everything*, including the two that destroy. That is a
    /// choice about who is being protected from what: the store belongs to
    /// whoever started the server, on files they can already remove with `rm`,
    /// and a default that withholds `delete` from them protects nobody — it
    /// only produces a refusal on the day they meant it, from a server they
    /// configured themselves.
    ///
    /// What the refusals are for is the other direction: a server that a model
    /// talks to, where `refuse = ["delete", "replace"]` says *this agent may
    /// write and may not destroy*. That is a sentence someone has to mean, so
    /// it has to be written down, and the file says what is withheld rather
    /// than what is granted — a granting list quietly withholds every operation
    /// added after it was written, which is how `replace` arrived switched off
    /// on servers whose owners had never heard of it.
    pub const ALL: [Permission; 6] = [
        Permission::Create,
        Permission::Update,
        Permission::Add,
        Permission::Replace,
        Permission::Rescan,
        Permission::Delete,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Permission::Create => "create",
            Permission::Update => "update",
            Permission::Add => "add",
            Permission::Replace => "replace",
            Permission::Rescan => "rescan",
            Permission::Delete => "delete",
        }
    }

    /// Parse one name from `server.toml`. An unknown name is refused rather
    /// than ignored: a typo in a `refuse` list is silently a *permission*, and
    /// the failure it produces is not an error at all — it is the destructive
    /// operation somebody meant to switch off, running.
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|permission| permission.as_str() == text)
    }
}

/// One memory's open handles, held for the life of the server.
pub struct OpenMemory {
    pub store: Mutex<Storage>,
    pub index: Index,
    pub writer: Mutex<IndexWriter>,
}

/// State for the HTTP router.
pub struct App {
    pub root: PathBuf,
    pub token: Option<String>,
    pub permissions: Vec<Permission>,
    pub memories: Mutex<HashMap<String, Arc<OpenMemory>>>,
}

impl App {
    pub fn new(root: PathBuf, token: Option<String>, permissions: Vec<Permission>) -> Self {
        Self {
            root,
            token,
            permissions,
            memories: Mutex::new(HashMap::new()),
        }
    }

    /// The check every mutating handler makes first, before it opens anything.
    fn permit(&self, permission: Permission) -> Result<(), Error> {
        if self.permissions.contains(&permission) {
            return Ok(());
        }
        Err(Error::Forbidden {
            permission: permission.as_str(),
        })
    }
}

/// A word looked up in the lexicon.
#[derive(Debug, Clone, Serialize)]
pub struct Lexeme {
    pub word: String,
    pub surface: String,
    pub surface_units: u64,
    pub lemma: String,
    pub lemma_units: u64,
    pub context_units: u64,
}

pub fn list(root: &Path, trace: &Ulid) -> Result<(Vec<Memory>, Stats), Error> {
    let _span = tracing::info_span!("memory.list").entered();
    let started = Instant::now();
    let fetch = Instant::now();
    let memories = Storage::list(root)?;
    let fetch_ms = ms(fetch);
    let mut stats = Stats::new(trace, started);
    stats.fetch_ms = Some(fetch_ms);
    tracing::info!(
        trace_id = %trace,
        count = memories.len(),
        fetch_ms = fetch_ms,
        total_ms = stats.total_ms,
        "listed memories",
    );
    Ok((memories, stats))
}

pub fn create(
    root: &Path,
    name: &str,
    description: &str,
    languages: &str,
    trace: &Ulid,
) -> Result<(Ulid, Stats), Error> {
    let _span = tracing::info_span!("memory.create").entered();
    let started = Instant::now();
    let write = Instant::now();
    let (store, id) = Storage::create(root, name, description, languages)?;
    let write_ms = ms(write);
    let index = Instant::now();
    let built = Index::attach(&store.directory.join(index::DIRECTORY))?;
    store.set_meta(index::VERSION_KEY, &normalize::VERSION.to_string())?;
    store.set_meta(index::BUILT_KEY, &Ulid::now().to_string())?;
    drop(built);
    let index_ms = ms(index);
    let mut stats = Stats::new(trace, started);
    stats.write_ms = Some(write_ms);
    stats.index_ms = Some(index_ms);
    tracing::info!(
        trace_id = %trace,
        memory = name,
        ulid = %id,
        write_ms = write_ms,
        index_ms = index_ms,
        total_ms = stats.total_ms,
        "created memory",
    );
    Ok((id, stats))
}

/// Remove a memory and everything in it.
///
/// `evict` is the server's handle cache, so that the entry goes before the
/// files do and no request that arrives during the delete can be handed an open
/// database that is about to stop existing. The CLI passes `None`: it holds no
/// cache, and the process ends a moment later.
pub fn delete(
    root: &Path,
    name: &str,
    evict: Option<&Mutex<HashMap<String, Arc<OpenMemory>>>>,
    trace: &Ulid,
) -> Result<(Memory, Stats), Error> {
    let _span = tracing::info_span!("memory.delete").entered();
    let started = Instant::now();
    if let Some(memories) = evict {
        memories.lock().unwrap().remove(name);
    }
    let write = Instant::now();
    let memory = Storage::delete(root, name)?;
    let write_ms = ms(write);
    let mut stats = Stats::new(trace, started);
    stats.write_ms = Some(write_ms);
    // The counts are on this line because nothing else will ever carry them
    // again. Every other operation logs what it touched next to a store that
    // still holds it; this one logs an epitaph.
    tracing::info!(
        trace_id = %trace,
        memory = name,
        ulid = %memory.id,
        sessions = memory.sessions,
        messages = memory.messages,
        units = memory.units,
        write_ms = write_ms,
        total_ms = stats.total_ms,
        "deleted memory",
    );
    Ok((memory, stats))
}

pub fn update(
    store: &Storage,
    name: &str,
    description: Option<&str>,
    languages: Option<&str>,
    trace: &Ulid,
) -> Result<(Ulid, Stats), Error> {
    let _span = tracing::info_span!("memory.update").entered();
    if description.is_none() && languages.is_none() {
        return Err(Error::NothingToUpdate);
    }
    let started = Instant::now();
    let write = Instant::now();
    store.update(description, languages)?;
    let write_ms = ms(write);
    let memory = store.describe()?;
    let mut stats = Stats::new(trace, started);
    stats.write_ms = Some(write_ms);
    tracing::info!(
        trace_id = %trace,
        memory = name,
        description = description.is_some(),
        languages = languages.is_some(),
        write_ms = write_ms,
        total_ms = stats.total_ms,
        "updated memory",
    );
    Ok((memory.id, stats))
}

pub fn add(
    store: &Mutex<Storage>,
    index: &Index,
    writer: &Mutex<IndexWriter>,
    entry: &Entry<'_>,
    name: &str,
    trace: &Ulid,
) -> Result<(Written, Stats), Error> {
    let _span = tracing::info_span!("memory.add").entered();
    let started = Instant::now();
    let wait = Instant::now();
    let mut store = store.lock().unwrap();
    let store_lock_ms = ms(wait);
    let write = Instant::now();
    let written = store.add(entry)?;
    let write_ms = ms(write);
    drop(store);
    let wait = Instant::now();
    let mut writer = writer.lock().unwrap();
    let writer_lock_ms = ms(wait);
    let index_started = Instant::now();
    index.add(&writer, &written, entry.role.code(), entry.ts, entry.body)?;
    let index_ms = ms(index_started);
    let commit = Instant::now();
    index.commit(&mut writer)?;
    let commit_ms = ms(commit);
    let mut stats = Stats::new(trace, started);
    stats.store_lock_ms = Some(store_lock_ms);
    stats.writer_lock_ms = Some(writer_lock_ms);
    stats.write_ms = Some(write_ms);
    stats.index_ms = Some(index_ms);
    stats.commit_ms = Some(commit_ms);
    tracing::info!(
        trace_id = %trace,
        memory = name,
        session = entry.session,
        ulid = %written.message,
        seq = written.seq,
        units = written.units.len(),
        bytes = entry.body.len(),
        store_lock_ms = store_lock_ms,
        writer_lock_ms = writer_lock_ms,
        write_ms = write_ms,
        index_ms = index_ms,
        commit_ms = commit_ms,
        total_ms = stats.total_ms,
        "added message",
    );
    Ok((written, stats))
}

/// What an outline answers with: a memory's sessions, or one session's
/// messages.
///
/// One operation and two shapes rather than two operations, for the same reason
/// `cursor` is the only reader: a caller who has just been handed a list of
/// session refs should not have to discover a second tool name to open one.
pub enum Outline {
    Sessions(Vec<SessionRow>),
    Messages(Vec<MessageRow>),
}

/// What is on file, without reading any of it.
///
/// Without `session`, every session in the memory. With it, that session's
/// messages — refs, roles, sizes and unit counts, and no bodies. It is the
/// question `search` cannot answer, because search finds text and this asks
/// what exists: a feeder deciding whether it has already stored something has
/// no keyword to look for, only a name it is about to reuse.
pub fn outline(
    store: &Storage,
    name: &str,
    session: Option<&str>,
    trace: &Ulid,
) -> Result<(Outline, Stats), Error> {
    let _span = tracing::info_span!("memory.outline").entered();
    let started = Instant::now();
    let fetch = Instant::now();
    let outline = match session {
        Some(session) => Outline::Messages(store.messages(session)?),
        None => Outline::Sessions(store.sessions()?),
    };
    let fetch_ms = ms(fetch);
    let rows = match &outline {
        Outline::Sessions(sessions) => sessions.len(),
        Outline::Messages(messages) => messages.len(),
    };
    let mut stats = Stats::new(trace, started);
    stats.fetch_ms = Some(fetch_ms);
    tracing::info!(
        trace_id = %trace,
        memory = name,
        session = session.unwrap_or("*"),
        rows = rows,
        fetch_ms = fetch_ms,
        total_ms = stats.total_ms,
        "outlined memory",
    );
    Ok((outline, stats))
}

/// Rewrite one message's body, keeping its place in the session.
///
/// The index repair is the expensive half and it is proportional to the
/// session, not to the message: a unit carries a session term and no message
/// term, so the narrowest thing tantivy can be told to forget is every unit of
/// the session. Both the forget and the units that replace it go into one
/// commit — a reader must never see the session half-present.
pub fn replace(
    store: &Mutex<Storage>,
    index: &Index,
    writer: &Mutex<IndexWriter>,
    revision: &Revision<'_>,
    name: &str,
    trace: &Ulid,
) -> Result<(Ulid, usize, usize, Stats), Error> {
    let _span = tracing::info_span!("memory.replace").entered();
    let started = Instant::now();
    let wait = Instant::now();
    let mut store = store.lock().unwrap();
    let store_lock_ms = ms(wait);
    let write = Instant::now();
    let (session_id, message_id, indexed) = store.replace(revision)?;
    let write_ms = ms(write);
    drop(store);

    let wait = Instant::now();
    let mut writer = writer.lock().unwrap();
    let writer_lock_ms = ms(wait);
    let index_started = Instant::now();
    index.forget(&writer, session_id)?;
    let mut units = 0;
    for held in &indexed {
        if held.written.message == message_id {
            units = held.written.units.len();
        }
        index.add(
            &writer,
            &held.written,
            held.role.code(),
            held.ts,
            &held.body,
        )?;
    }
    let index_ms = ms(index_started);
    let commit = Instant::now();
    index.commit(&mut writer)?;
    let commit_ms = ms(commit);

    let mut stats = Stats::new(trace, started);
    stats.store_lock_ms = Some(store_lock_ms);
    stats.writer_lock_ms = Some(writer_lock_ms);
    stats.write_ms = Some(write_ms);
    stats.index_ms = Some(index_ms);
    stats.commit_ms = Some(commit_ms);
    tracing::info!(
        trace_id = %trace,
        memory = name,
        session = revision.session,
        message = revision.message,
        ulid = %message_id,
        units = units,
        reindexed = indexed.len(),
        bytes = revision.body.len(),
        store_lock_ms = store_lock_ms,
        writer_lock_ms = writer_lock_ms,
        write_ms = write_ms,
        index_ms = index_ms,
        commit_ms = commit_ms,
        total_ms = stats.total_ms,
        "replaced message",
    );
    Ok((message_id, units, indexed.len(), stats))
}

pub fn search(
    store: &Storage,
    index: &Index,
    name: &str,
    request: (&str, bool),
    filter: &Filter,
    page: (usize, usize),
    trace: &Ulid,
) -> Result<(Outcome, Stats), Error> {
    let (limit, per_message) = page;
    let _span = tracing::info_span!("memory.search").entered();
    let started = Instant::now();
    let search_started = Instant::now();
    let outcome = search::search(store, index, request, filter, limit, per_message)?;
    let search_ms = ms(search_started);

    let mut stats = Stats::new(trace, started);
    stats.search_ms = Some(search_ms);
    tracing::info!(
        trace_id = %trace,
        memory = name,
        query_bytes = request.0.len(),
        fuzzy = request.1,
        limit = limit,
        hits = outcome.hits.len(),
        unknown = outcome.unknown.len(),
        typos = outcome.fuzzy.len(),
        search_ms = search_ms,
        total_ms = stats.total_ms,
        "searched memory",
    );
    Ok((outcome, stats))
}

/// Read units back around the ones a search returned.
///
/// The one reader: `before`/`after` of `0` is a unit read by id, wider is the
/// context around it, and `whole` is the messages those units came from. There
/// is deliberately no second operation for any of the three, because a caller
/// holding a cursor should never have to choose which one to ask.
///
/// It writes nothing. Whether the read was widened is worth a line on stderr and
/// nothing more; a reader that recorded which cursors were expanded would be a
/// read that leaves a trail in the store, which is what this is not.
pub fn cursor(
    store: &Storage,
    name: &str,
    units: &[Ulid],
    before: i64,
    after: i64,
    whole: bool,
    trace: &Ulid,
) -> Result<(Vec<Located>, Vec<Ulid>, Stats), Error> {
    let _span = tracing::info_span!("memory.cursor").entered();
    let started = Instant::now();
    let widened = whole || before > 0 || after > 0;
    let cursor_started = Instant::now();
    let mut rows: Vec<Located> = Vec::new();
    let mut seen: HashSet<Ulid> = HashSet::new();
    let mut missing = Vec::new();
    for unit in units {
        let window = store.around(*unit, before, after, whole)?;
        if window.is_empty() {
            missing.push(*unit);
            continue;
        }
        for row in window {
            if seen.insert(row.unit) {
                rows.push(row);
            }
        }
    }
    rows.sort_by_key(|row| (row.session, row.seq, row.unit_seq));
    let cursor_ms = ms(cursor_started);
    let mut stats = Stats::new(trace, started);
    stats.cursor_ms = Some(cursor_ms);
    tracing::info!(
        trace_id = %trace,
        memory = name,
        cursors = units.len(),
        before = before,
        after = after,
        whole = whole,
        units = rows.len(),
        missing = missing.len(),
        widened = widened,
        cursor_ms = cursor_ms,
        total_ms = stats.total_ms,
        "read around cursors",
    );
    Ok((rows, missing, stats))
}

pub fn lexicon(
    index: &Index,
    name: &str,
    words: &[String],
    trace: &Ulid,
) -> Result<(Vec<Lexeme>, Stats), Error> {
    let _span = tracing::info_span!("memory.lexicon").entered();
    if words.is_empty() {
        return Err(Error::EmptyWords);
    }
    let started = Instant::now();
    let lexicon_started = Instant::now();
    let searcher = index.reader.searcher();
    let mut rows = Vec::new();
    for word in words {
        let segmented = normalize::segment(word);
        let script = match segmented.first() {
            Some(word) => word.script,
            None => normalize::Script::Other,
        };
        let surface = normalize::surface(word);
        let lemma = normalize::lemma(word, script);
        let counts = [
            (index.fields.surface, surface.as_str()),
            (index.fields.lemma, lemma.as_str()),
            (index.fields.context, lemma.as_str()),
        ];
        let mut frequencies = [0u64; 3];
        for (at, (field, text)) in counts.iter().enumerate() {
            if text.is_empty() {
                continue;
            }
            let term = tantivy::Term::from_field_text(*field, text);
            frequencies[at] = searcher.doc_freq(&term)?;
        }
        rows.push(Lexeme {
            word: word.clone(),
            surface,
            surface_units: frequencies[0],
            lemma,
            lemma_units: frequencies[1],
            context_units: frequencies[2],
        });
    }
    let lexicon_ms = ms(lexicon_started);
    let mut stats = Stats::new(trace, started);
    stats.lexicon_ms = Some(lexicon_ms);
    tracing::info!(
        trace_id = %trace,
        memory = name,
        words = rows.len(),
        lexicon_ms = lexicon_ms,
        total_ms = stats.total_ms,
        "looked up words",
    );
    Ok((rows, stats))
}

pub fn rescan(
    store: &Mutex<Storage>,
    index: &Index,
    writer: &Mutex<IndexWriter>,
    name: &str,
    trace: &Ulid,
) -> Result<(usize, usize, Stats), Error> {
    // The one operation slow enough that its start is worth a line of its own:
    // a rescan of a large memory runs for minutes, and without this the only
    // evidence it is running rather than wedged is the absence of a log. Debug,
    // so the default level still sees exactly one event for the operation.
    let _span = tracing::info_span!("memory.rescan").entered();
    tracing::debug!(trace_id = %trace, memory = name, "rescanning memory");
    let started = Instant::now();
    let wait = Instant::now();
    let mut store_guard = store.lock().unwrap();
    let store_lock_ms = ms(wait);
    let wait = Instant::now();
    let mut writer_guard = writer.lock().unwrap();
    let writer_lock_ms = ms(wait);
    let index_started = Instant::now();
    index.clear(&mut writer_guard)?;
    index.commit(&mut writer_guard)?;
    let commit_ms = ms(index_started);
    drop(writer_guard);
    let resplit = Instant::now();
    let written = store_guard.resplit()?;
    let resplit_ms = ms(resplit);
    drop(store_guard);
    let index_started = Instant::now();
    let mut units = 0;
    for (message, written) in &written {
        let (body, role, ts) = {
            let store = store.lock().unwrap();
            store.message(*message)?
        };
        let writer = writer.lock().unwrap();
        index.add(&writer, written, role, ts, &body)?;
        units += written.units.len();
    }
    {
        let mut writer = writer.lock().unwrap();
        index.commit(&mut writer)?;
    }
    {
        let store = store.lock().unwrap();
        store.set_meta(index::VERSION_KEY, &normalize::VERSION.to_string())?;
        store.set_meta(index::BUILT_KEY, &Ulid::now().to_string())?;
    }
    let index_ms = ms(index_started);
    let mut stats = Stats::new(trace, started);
    stats.store_lock_ms = Some(store_lock_ms);
    stats.writer_lock_ms = Some(writer_lock_ms);
    stats.commit_ms = Some(commit_ms);
    stats.resplit_ms = Some(resplit_ms);
    stats.index_ms = Some(index_ms);
    tracing::info!(
        trace_id = %trace,
        memory = name,
        messages = written.len(),
        units = units,
        store_lock_ms = store_lock_ms,
        writer_lock_ms = writer_lock_ms,
        commit_ms = commit_ms,
        resplit_ms = resplit_ms,
        index_ms = index_ms,
        total_ms = stats.total_ms,
        "rescanned memory",
    );
    Ok((written.len(), units, stats))
}

pub fn list_json(memories: &[Memory], stats: &Stats) -> serde_json::Value {
    let mut memory_list = Vec::new();
    for memory in memories {
        memory_list.push(serde_json::json!({
            "id": memory.id.to_string(),
            "name": memory.name,
            "description": memory.description,
            "languages": memory.languages,
            "created_at": memory.created_at,
            "sessions": memory.sessions,
            "messages": memory.messages,
            "units": memory.units,
        }));
    }
    serde_json::json!({ "memory_list": memory_list, "stats": stats })
}

pub fn outline_json(outline: &Outline, stats: &Stats) -> serde_json::Value {
    match outline {
        Outline::Sessions(sessions) => {
            let mut session_list = Vec::new();
            for session in sessions {
                session_list.push(serde_json::json!({
                    "id": session.id.to_string(),
                    "session": session.reference,
                    "started_at": session.started_at,
                    "ended_at": session.ended_at,
                    "messages": session.messages,
                    "units": session.units,
                }));
            }
            serde_json::json!({ "session_list": session_list, "stats": stats })
        }
        Outline::Messages(messages) => {
            let mut message_list = Vec::new();
            for message in messages {
                message_list.push(serde_json::json!({
                    "id": message.id.to_string(),
                    "message": message.reference,
                    "seq": message.seq,
                    "author": message.author,
                    "role": message.role.as_str(),
                    "ts": message.ts,
                    "characters": message.characters,
                    "units": message.units,
                }));
            }
            serde_json::json!({ "message_list": message_list, "stats": stats })
        }
    }
}

pub fn id_json(id: &Ulid, stats: &Stats) -> serde_json::Value {
    serde_json::json!({ "id": id.to_string(), "stats": stats })
}

pub fn search_json(outcome: &Outcome, stats: &Stats) -> serde_json::Value {
    let mut hit_list = Vec::new();
    for hit in &outcome.hits {
        hit_list.push(serde_json::json!({
            "cursor": hit.cursor,
            "unit": hit.unit.to_string(),
            "score": hit.score,
            "raw": hit.raw,
            "coverage": [hit.coverage.0, hit.coverage.1],
            "matched_list": hit.matched,
            "nearby_list": hit.nearby,
            "session": hit.session.to_string(),
            "session_ref": hit.session_ref,
            "message": hit.message.to_string(),
            "message_ref": hit.message_ref,
            "author": hit.author,
            "role": hit.role.as_str(),
            "ts": hit.ts,
            "words": hit.words,
            "snippet": hit.snippet,
        }));
    }
    let mut unknown_list = Vec::new();
    for unknown in &outcome.unknown {
        unknown_list.push(serde_json::json!({
            "clause": unknown.clause,
            "word": unknown.word,
        }));
    }
    let mut fuzzy_list = Vec::new();
    for fuzzy in &outcome.fuzzy {
        fuzzy_list.push(serde_json::json!({
            "word": fuzzy.word,
            "matched_list": fuzzy.matched,
        }));
    }
    let mut hint_list = Vec::new();
    for (term, units) in &outcome.hints {
        hint_list.push(serde_json::json!({ "term": term, "units": units }));
    }
    serde_json::json!({
        "hit_list": hit_list,
        "unknown_list": unknown_list,
        "fuzzy_list": fuzzy_list,
        "hint_list": hint_list,
        "stats": stats,
    })
}

/// The window, grouped by the message each unit came from.
///
/// Grouped rather than flat because the fields that say *where* a unit sits —
/// its message, its session, who wrote it and when — are identical for every
/// unit of a message, and a whole-message window is a hundred and twenty units
/// in a corpus fed from PDFs. Flat, that is nine tenths repetition; the caller
/// paying for it is a context window.
///
/// A message carries `unit_list` when the window was counted in units, and
/// `body` when it was counted in messages. The unit ids are the addressable
/// part — they are what a caller passes back to move again — and there is no
/// point spending them on a read that already returned the whole message.
pub fn cursor_json(
    rows: &[Located],
    asked: &[Ulid],
    missing: &[Ulid],
    whole: bool,
    stats: &Stats,
) -> serde_json::Value {
    let mut message_list: Vec<serde_json::Value> = Vec::new();
    let mut at: Option<Ulid> = None;
    for row in rows {
        if at != Some(row.message) {
            at = Some(row.message);
            let mut message = serde_json::json!({
                "message": row.message.to_string(),
                "message_ref": row.message_ref,
                "session": row.session.to_string(),
                "session_ref": row.session_ref,
                "seq": row.seq,
                "author": row.author,
                "role": row.role.as_str(),
                "ts": row.ts,
                "anchor": false,
            });
            if whole {
                message["body"] = serde_json::json!(row.body);
            } else {
                message["unit_list"] = serde_json::json!([]);
            }
            message_list.push(message);
        }
        let Some(message) = message_list.last_mut() else {
            continue;
        };
        if asked.contains(&row.unit) {
            message["anchor"] = serde_json::json!(true);
        }
        if whole {
            continue;
        }
        if let Some(unit_list) = message["unit_list"].as_array_mut() {
            unit_list.push(serde_json::json!({
                "unit": row.unit.to_string(),
                "unit_seq": row.unit_seq,
                "anchor": asked.contains(&row.unit),
                "text": row.text(),
            }));
        }
    }
    let mut missing_list = Vec::new();
    for unit in missing {
        missing_list.push(serde_json::json!(unit.to_string()));
    }
    serde_json::json!({
        "message_list": message_list,
        "missing_list": missing_list,
        "stats": stats,
    })
}

pub fn lexicon_json(rows: &[Lexeme], stats: &Stats) -> serde_json::Value {
    serde_json::json!({ "word_list": rows, "stats": stats })
}

pub fn router(app: App) -> Router {
    let app = Arc::new(app);
    Router::new()
        .route("/api/v1/health", get_route(health))
        .route("/api/v1/memory_list", get_route(memory_list))
        .route("/api/v1/memory", post(create_memory))
        .route(
            "/api/v1/memory/{name}",
            patch(update_memory).delete(delete_memory),
        )
        .route("/api/v1/memory/{name}/message_list", post(add_message))
        .route("/api/v1/memory/{name}/message", post(replace_message))
        .route("/api/v1/memory/{name}/outline", post(outline_memory))
        .route("/api/v1/memory/{name}/search", post(search_memory))
        .route("/api/v1/memory/{name}/cursor", post(cursor_memory))
        .route("/api/v1/memory/{name}/lexicon", post(lexicon_memory))
        .route("/api/v1/memory/{name}/rescan", post(rescan_memory))
        // Both spellings, because a client is configured with a URL a person
        // typed and axum matches neither for the other. Only POST is
        // registered: the MCP endpoint answers GET and DELETE with `405`, and
        // that is exactly what a `MethodRouter` does with a method it has no
        // handler for.
        .route("/mcp", post(crate::mcp::endpoint))
        .route("/mcp/", post(crate::mcp::endpoint))
        .layer(middleware::from_fn_with_state(app.clone(), token_gate))
        // Registered *after* the token layer, so it is the one thing served
        // without one. axum applies a layer to the routes added before it, and
        // that is the whole reason this line sits down here rather than at the
        // top with the others: the guide is documentation, not data, and an
        // agent handed nothing but a URL has to be able to read it before it
        // can know a token is wanted. Everything it describes is still gated.
        .route("/", get_route(guide))
        .layer(middleware::from_fn(identify))
        .layer(middleware::from_fn(http_layer))
        .with_state(app)
}

#[derive(Clone, Copy)]
pub(crate) struct RequestTrace(pub Ulid);

fn content_length(headers: &axum::http::HeaderMap) -> u64 {
    let Some(value) = headers.get(CONTENT_LENGTH) else {
        return 0;
    };
    let Ok(text) = value.to_str() else {
        return 0;
    };
    text.parse().unwrap_or_default()
}

pub(crate) fn spawn_op<F, T>(f: F) -> tokio::task::JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let span = tracing::Span::current();
    tokio::task::spawn_blocking(move || span.in_scope(f))
}

async fn http_layer(mut request: Request, next: Next) -> Response {
    let trace = match Ulid::new() {
        Ok(trace) => trace,
        Err(error) => return fail(StatusCode::INTERNAL_SERVER_ERROR, error, ""),
    };
    let method = request.method().as_str().to_string();
    let path = request.uri().path().to_string();
    let query = match request.uri().query() {
        Some(query) => query.to_string(),
        None => String::new(),
    };
    let route = match request.extensions().get::<MatchedPath>() {
        Some(matched) => matched.as_str().to_string(),
        None => path.clone(),
    };
    let request_bytes = content_length(request.headers());
    request.extensions_mut().insert(RequestTrace(trace));
    let started = Instant::now();
    // `http.server`, and the span carries nothing: it is the root of the trace
    // path that every line under it prints as `spans`, and the request's
    // details belong on the one event that reports the outcome, where the
    // status is known too.
    let span = tracing::info_span!("http.server");
    async move {
        let mut response = next.run(request).await;
        let status = response.status().as_u16();
        let response_bytes = content_length(response.headers());
        let total_ms = ms(started);
        if let Ok(value) = HeaderValue::from_str(&trace.to_string()) {
            response.headers_mut().insert(TRACE_HEADER, value);
        }
        // One event, and its level and its wording both follow the status, so
        // that `handled` and `rejected` are different things to search for
        // rather than one `msg` repeated at three levels. Field names are the
        // OpenTelemetry HTTP ones, so a Grafana dashboard written against any
        // other service reads these without a translation table.
        macro_rules! report {
            ($level:ident, $said:literal) => {
                tracing::$level!(
                    trace_id = %trace,
                    http.request.method = method.as_str(),
                    http.route = route.as_str(),
                    url.path = path.as_str(),
                    url.query = query.as_str(),
                    http.request.body.size = request_bytes,
                    http.response.status_code = status,
                    http.response.body.size = response_bytes,
                    total_ms = total_ms,
                    $said,
                )
            };
        }
        match status {
            500.. => report!(error, "failed request"),
            400..500 => report!(warn, "rejected request"),
            _ => report!(info, "handled request"),
        }
        response
    }
    .instrument(span)
    .await
}

async fn identify(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let server = format!("borhan/{VERSION} ({REPOSITORY})");
    if let Ok(value) = HeaderValue::from_str(&server) {
        response.headers_mut().insert(SERVER_HEADER, value);
    }
    if let Ok(value) = HeaderValue::from_str(VERSION) {
        response.headers_mut().insert(VERSION_HEADER, value);
    }
    response
}

fn ms(started: Instant) -> u64 {
    started.elapsed().as_millis() as u64
}

pub(crate) fn traced(status: StatusCode, trace: &str, body: serde_json::Value) -> Response {
    let mut response = (status, Json(body)).into_response();
    if let Ok(value) = HeaderValue::from_str(trace) {
        response.headers_mut().insert(TRACE_HEADER, value);
    }
    // Hyper writes `Content-Length` when it serializes the response, which is
    // after every middleware has looked at it — so the logging middleware read
    // the header and always found nothing, and reported every response as zero
    // bytes while the client, reading the same header off the wire, logged the
    // real size. Setting it here makes the two sides agree and costs nothing:
    // the body is a fully buffered `Json`, so its length is already known.
    let size = response.body().size_hint().exact().unwrap_or(0);
    if let Ok(value) = HeaderValue::from_str(&size.to_string()) {
        response.headers_mut().insert(CONTENT_LENGTH, value);
    }
    response
}

pub(crate) fn fail(status: StatusCode, error: impl std::fmt::Display, trace: &str) -> Response {
    traced(
        status,
        trace,
        serde_json::json!({
            "error": error.to_string(),
            "stats": { "trace": trace, "total_ms": 0 },
        }),
    )
}

fn status_of(error: &Error) -> StatusCode {
    match error {
        Error::NothingToUpdate
        | Error::EmptyIds
        | Error::EmptyWords
        | Error::Role { .. }
        | Error::NotUlid { .. }
        | Error::Groups
        | Error::Search(
            search::Error::Empty
            | search::Error::Syntax { .. }
            | search::Error::Field { .. }
            | search::Error::Filter { .. }
            | search::Error::Regex
            | search::Error::Range
            | search::Error::Everything
            | search::Error::Prefix { .. }
            | search::Error::Unwanted
            | search::Error::Clauses { .. },
        )
        | Error::Storage(
            crate::storage::Error::Empty
            | crate::storage::Error::Long { .. }
            | crate::storage::Error::Charset { .. }
            | crate::storage::Error::Description
            | crate::storage::Error::DescriptionShort
            | crate::storage::Error::Languages
            | crate::storage::Error::Reference { .. },
        ) => StatusCode::BAD_REQUEST,
        Error::Storage(
            crate::storage::Error::Missing { .. }
            | crate::storage::Error::UnknownSession { .. }
            | crate::storage::Error::UnknownMessage { .. },
        ) => StatusCode::NOT_FOUND,
        Error::Forbidden { .. } => StatusCode::FORBIDDEN,
        Error::Storage(
            crate::storage::Error::Exists { .. } | crate::storage::Error::Duplicate { .. },
        ) => StatusCode::CONFLICT,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// The tail all ten handlers share: a task that panicked, an operation the
/// store refused, or a body to send.
fn answer(
    result: Result<Result<(serde_json::Value, Stats), Error>, tokio::task::JoinError>,
    trace: &Ulid,
) -> Response {
    match result {
        Ok(Ok((body, stats))) => traced(StatusCode::OK, &stats.trace, body),
        Ok(Err(error)) => fail(status_of(&error), error, &trace.to_string()),
        Err(error) => fail(StatusCode::INTERNAL_SERVER_ERROR, error, &trace.to_string()),
    }
}

pub(crate) fn open(app: &App, name: &str) -> Result<(Arc<OpenMemory>, Option<u64>), Error> {
    {
        let map = app.memories.lock().unwrap();
        if let Some(memory) = map.get(name) {
            return Ok((memory.clone(), None));
        }
    }
    let started = Instant::now();
    let store = Storage::open(&app.root, name)?;
    let index = Index::open(&store)?;
    let writer = index.writer()?;
    let open_ms = ms(started);
    // Debug, not info. Opening is not an operation a caller asked for, it is
    // the first request for a memory paying for the handles every later
    // request reuses, and the cost is already reported as `stats.open_ms` on
    // the operation that triggered it.
    tracing::debug!(memory = name, open_ms = open_ms, "opened memory");
    let memory = Arc::new(OpenMemory {
        store: Mutex::new(store),
        index,
        writer: Mutex::new(writer),
    });
    let mut map = app.memories.lock().unwrap();
    if let Some(existing) = map.get(name) {
        return Ok((existing.clone(), None));
    }
    map.insert(name.to_string(), memory.clone());
    Ok((memory, Some(open_ms)))
}

fn open_attached(app: &App, name: &str) -> Result<(Arc<OpenMemory>, Option<u64>), Error> {
    {
        let map = app.memories.lock().unwrap();
        if let Some(memory) = map.get(name) {
            return Ok((memory.clone(), None));
        }
    }
    let started = Instant::now();
    let store = Storage::open(&app.root, name)?;
    let index = Index::attach(&store.directory.join(index::DIRECTORY))?;
    let writer = index.writer()?;
    let open_ms = ms(started);
    tracing::debug!(memory = name, open_ms = open_ms, "attached memory");
    let memory = Arc::new(OpenMemory {
        store: Mutex::new(store),
        index,
        writer: Mutex::new(writer),
    });
    let mut map = app.memories.lock().unwrap();
    if let Some(existing) = map.get(name) {
        return Ok((existing.clone(), None));
    }
    map.insert(name.to_string(), memory.clone());
    Ok((memory, Some(open_ms)))
}

async fn token_gate(State(app): State<Arc<App>>, request: Request, next: Next) -> Response {
    if let Some(wanted) = &app.token {
        let presented = match request.headers().get(AUTHORIZATION) {
            Some(value) => match value.to_str() {
                Ok(value) => value.strip_prefix("Bearer "),
                Err(_) => None,
            },
            None => None,
        };
        if presented != Some(wanted.as_str()) {
            let trace = match request.extensions().get::<RequestTrace>() {
                Some(RequestTrace(trace)) => trace.to_string(),
                None => String::new(),
            };
            return fail(StatusCode::UNAUTHORIZED, "missing or wrong token", &trace);
        }
    }
    next.run(request).await
}

/// `GET /` — the whole API, as Markdown, for whoever arrived with only a URL.
///
/// The third way into borhan, beside the MCP endpoint and the command line: a
/// model that has been given an address and nothing else fetches this, and the
/// document is written to be the only thing it needs. It is `include_str!` of
/// `src/guide.md` rather than a string in this file so that it stays editable
/// as prose — and it is served as `text/markdown` because that is what it is,
/// and a client that renders it is doing the right thing with it.
///
/// **Add a route, edit `src/guide.md`.** Nothing checks that the two agree, and
/// a documented endpoint that does not exist is worse than an undocumented one.
///
/// The one substitution is `{origin}`, which becomes the address this request
/// arrived on so that the `curl` lines below it are runnable as printed. It has
/// to be the caller's address and not the one this process bound: those differ
/// behind a proxy, across a container port mapping, and any time the listen
/// address is a wildcard — `0.0.0.0:1995` is a thing to bind and not a thing to
/// paste into a shell. `Host` is what the caller just used successfully, which
/// is the only address known to work.
///
/// Done with `str::replace` rather than `format!` on purpose: the document is
/// full of JSON, so every `{` and `}` in it would have to be doubled, and the
/// file is meant to stay readable as prose.
async fn guide(headers: HeaderMap, uri: Uri) -> Response {
    let _span = tracing::info_span!("guide").entered();

    // A proxy may send a list — `X-Forwarded-Proto: https, http` — and the
    // first element is the hop nearest the client, which is the one whose
    // scheme and authority the client actually typed.
    let first = |value: &HeaderValue| -> Option<String> {
        let Ok(text) = value.to_str() else {
            return None;
        };
        let text = match text.split_once(',') {
            Some((head, _)) => head.trim(),
            None => text.trim(),
        };
        match text.is_empty() {
            true => None,
            false => Some(text.to_string()),
        }
    };

    let mut scheme = None;
    if let Some(value) = headers.get("x-forwarded-proto") {
        scheme = first(value);
    }
    let mut host = None;
    if let Some(value) = headers.get("x-forwarded-host") {
        host = first(value);
    }
    if host.is_none()
        && let Some(value) = headers.get(HOST)
    {
        host = first(value);
    }
    // HTTP/2 carries no `Host` header; the `:authority` pseudo-header lands in
    // the URI instead. Between the two there is always one, and the constant is
    // only reached by a client that sent neither.
    if host.is_none()
        && let Some(authority) = uri.authority()
    {
        host = Some(authority.to_string());
    }

    let scheme = match scheme {
        Some(scheme) => scheme,
        None => "http".to_string(),
    };
    let host = match host {
        Some(host) => host,
        None => crate::DEFAULT_LISTEN_ADDRESS.to_string(),
    };
    let document = include_str!("guide.md").replace("{origin}", &format!("{scheme}://{host}"));

    (
        StatusCode::OK,
        [(CONTENT_TYPE, "text/markdown; charset=utf-8")],
        document,
    )
        .into_response()
}

async fn health(Extension(RequestTrace(trace)): Extension<RequestTrace>) -> Response {
    let _span = tracing::info_span!("health").entered();
    let started = Instant::now();
    let stats = Stats::new(&trace, started);
    // Debug: `probe_server` calls this before every CLI command that might go
    // remote, so at info it would be the most frequent line in the log and say
    // the least. The `http.server` event still reports the request.
    tracing::debug!(trace_id = %trace, total_ms = stats.total_ms, "checked health");
    traced(
        StatusCode::OK,
        &stats.trace,
        serde_json::json!({ "ok": true, "stats": stats }),
    )
}

async fn memory_list(
    State(app): State<Arc<App>>,
    Extension(RequestTrace(trace)): Extension<RequestTrace>,
) -> Response {
    let result = spawn_op(move || run_list(&app, &trace)).await;
    answer(result, &trace)
}

pub(crate) fn run_list(app: &App, trace: &Ulid) -> Result<(serde_json::Value, Stats), Error> {
    let (memories, stats) = list(&app.root, trace)?;
    Ok((list_json(&memories, &stats), stats))
}

#[derive(Debug, Deserialize)]
pub(crate) struct OutlineBody {
    /// Absent asks for the memory's sessions; present asks for that session's
    /// messages.
    session: Option<String>,
}

async fn outline_memory(
    State(app): State<Arc<App>>,
    Extension(RequestTrace(trace)): Extension<RequestTrace>,
    UrlPath(name): UrlPath<String>,
    Json(body): Json<OutlineBody>,
) -> Response {
    let result = spawn_op(move || run_outline(&app, &name, body, &trace)).await;
    answer(result, &trace)
}

pub(crate) fn run_outline(
    app: &App,
    name: &str,
    body: OutlineBody,
    trace: &Ulid,
) -> Result<(serde_json::Value, Stats), Error> {
    let (opened, open_ms) = open(app, name)?;
    let store = opened.store.lock().unwrap();
    let (outline, mut stats) = outline(&store, name, body.session.as_deref(), trace)?;
    stats.open_ms = open_ms;
    Ok((outline_json(&outline, &stats), stats))
}

#[derive(Debug, Deserialize)]
pub(crate) struct ReplaceBody {
    session: String,
    message: String,
    ts: Option<i64>,
    body: String,
}

async fn replace_message(
    State(app): State<Arc<App>>,
    Extension(RequestTrace(trace)): Extension<RequestTrace>,
    UrlPath(name): UrlPath<String>,
    Json(body): Json<ReplaceBody>,
) -> Response {
    let result = spawn_op(move || run_replace(&app, &name, body, &trace)).await;
    answer(result, &trace)
}

pub(crate) fn run_replace(
    app: &App,
    name: &str,
    body: ReplaceBody,
    trace: &Ulid,
) -> Result<(serde_json::Value, Stats), Error> {
    app.permit(Permission::Replace)?;
    // Now, like `add`, and not the time the message being corrected was
    // written: the text is new, and recency should treat it that way.
    let ts = match body.ts {
        Some(ts) => ts,
        None => Ulid::now(),
    };
    let (opened, open_ms) = open(app, name)?;
    let revision = Revision {
        session: &body.session,
        message: &body.message,
        ts,
        body: &body.body,
    };
    let (id, units, reindexed, mut stats) = replace(
        &opened.store,
        &opened.index,
        &opened.writer,
        &revision,
        name,
        trace,
    )?;
    stats.open_ms = open_ms;
    let value = serde_json::json!({
        "id": id.to_string(),
        "units": units,
        "reindexed": reindexed,
        "stats": stats,
    });
    Ok((value, stats))
}

#[derive(Debug, Deserialize)]
pub(crate) struct CreateBody {
    name: String,
    description: String,
    languages: Option<String>,
}

async fn create_memory(
    State(app): State<Arc<App>>,
    Extension(RequestTrace(trace)): Extension<RequestTrace>,
    Json(body): Json<CreateBody>,
) -> Response {
    let result = spawn_op(move || run_create(&app, body, &trace)).await;
    answer(result, &trace)
}

pub(crate) fn run_create(
    app: &App,
    body: CreateBody,
    trace: &Ulid,
) -> Result<(serde_json::Value, Stats), Error> {
    app.permit(Permission::Create)?;
    let languages = match &body.languages {
        Some(text) if !text.is_empty() => text.as_str(),
        _ => "fa,en",
    };
    let (id, mut stats) = create(&app.root, &body.name, &body.description, languages, trace)?;
    let (_, open_ms) = open(app, &body.name)?;
    stats.open_ms = open_ms;
    Ok((id_json(&id, &stats), stats))
}

#[derive(Debug, Deserialize)]
pub(crate) struct UpdateBody {
    description: Option<String>,
    languages: Option<String>,
}

async fn update_memory(
    State(app): State<Arc<App>>,
    Extension(RequestTrace(trace)): Extension<RequestTrace>,
    UrlPath(name): UrlPath<String>,
    Json(body): Json<UpdateBody>,
) -> Response {
    let result = spawn_op(move || run_update(&app, &name, body, &trace)).await;
    answer(result, &trace)
}

pub(crate) fn run_update(
    app: &App,
    name: &str,
    body: UpdateBody,
    trace: &Ulid,
) -> Result<(serde_json::Value, Stats), Error> {
    app.permit(Permission::Update)?;
    let (opened, open_ms) = open(app, name)?;
    let wait = Instant::now();
    let store = opened.store.lock().unwrap();
    let store_lock_ms = ms(wait);
    let (id, mut stats) = update(
        &store,
        name,
        body.description.as_deref(),
        body.languages.as_deref(),
        trace,
    )?;
    stats.open_ms = open_ms;
    stats.store_lock_ms = Some(store_lock_ms);
    Ok((id_json(&id, &stats), stats))
}

#[derive(Debug, Deserialize)]
pub(crate) struct AddBody {
    session: String,
    message: Option<String>,
    role: Option<String>,
    author: Option<String>,
    ts: Option<i64>,
    body: String,
}

async fn add_message(
    State(app): State<Arc<App>>,
    Extension(RequestTrace(trace)): Extension<RequestTrace>,
    UrlPath(name): UrlPath<String>,
    Json(body): Json<AddBody>,
) -> Response {
    let result = spawn_op(move || run_add(&app, &name, body, &trace)).await;
    answer(result, &trace)
}

pub(crate) fn run_add(
    app: &App,
    name: &str,
    body: AddBody,
    trace: &Ulid,
) -> Result<(serde_json::Value, Stats), Error> {
    app.permit(Permission::Add)?;
    let role_text = match &body.role {
        Some(role) => role.as_str(),
        None => "user",
    };
    let Some(role) = Role::parse(role_text) else {
        return Err(Error::Role {
            role: role_text.to_string(),
        });
    };
    let ts = match body.ts {
        Some(ts) => ts,
        None => Ulid::now(),
    };
    let author = match &body.author {
        Some(author) => author.as_str(),
        None => role.as_str(),
    };
    let (opened, open_ms) = open(app, name)?;
    let entry = Entry {
        session: &body.session,
        message: body.message.as_deref(),
        author,
        role,
        ts,
        body: &body.body,
    };
    let (written, mut stats) = add(
        &opened.store,
        &opened.index,
        &opened.writer,
        &entry,
        name,
        trace,
    )?;
    stats.open_ms = open_ms;
    let value = serde_json::json!({
        "id": written.message.to_string(),
        "units": written.units.len(),
        "stats": stats,
    });
    Ok((value, stats))
}

#[derive(Debug, Deserialize)]
pub(crate) struct SearchBody {
    query: Option<String>,
    #[serde(default)]
    fuzzy: bool,
    /// The input this endpoint took before `query`. Accepted only so that a
    /// caller still sending it is told what replaced it, rather than that
    /// `query` is missing.
    group_list: Option<serde_json::Value>,
    limit: Option<usize>,
    max_per_message: Option<usize>,
    session: Option<String>,
    after: Option<i64>,
    before: Option<i64>,
    role_list: Option<Vec<String>>,
}

async fn search_memory(
    State(app): State<Arc<App>>,
    Extension(RequestTrace(trace)): Extension<RequestTrace>,
    UrlPath(name): UrlPath<String>,
    Json(body): Json<SearchBody>,
) -> Response {
    let result = spawn_op(move || run_search(&app, &name, body, &trace)).await;
    answer(result, &trace)
}

pub(crate) fn run_search(
    app: &App,
    name: &str,
    body: SearchBody,
    trace: &Ulid,
) -> Result<(serde_json::Value, Stats), Error> {
    if body.group_list.is_some() {
        return Err(Error::Groups);
    }
    let query = match body.query {
        Some(query) => query,
        None => return Err(Error::Search(search::Error::Empty)),
    };
    let mut filter = Filter {
        after: body.after,
        before: body.before,
        ..Filter::default()
    };
    if let Some(session) = &body.session {
        match Ulid::parse(session) {
            Ok(session) => filter.session = Some(session),
            Err(source) => {
                return Err(Error::NotUlid {
                    value: session.clone(),
                    source,
                });
            }
        }
    }
    if let Some(roles) = &body.role_list {
        for role in roles {
            let Some(role) = Role::parse(role) else {
                return Err(Error::Role { role: role.clone() });
            };
            filter.roles.push(role);
        }
    }
    let limit = body.limit.unwrap_or(10);
    let per_message = body.max_per_message.unwrap_or(2);
    let (opened, open_ms) = open(app, name)?;
    let wait = Instant::now();
    let store = opened.store.lock().unwrap();
    let store_lock_ms = ms(wait);
    let (outcome, mut stats) = search(
        &store,
        &opened.index,
        name,
        (&query, body.fuzzy),
        &filter,
        (limit, per_message),
        trace,
    )?;
    stats.open_ms = open_ms;
    stats.store_lock_ms = Some(store_lock_ms);
    Ok((search_json(&outcome, &stats), stats))
}

#[derive(Debug, Deserialize)]
pub(crate) struct CursorBody {
    cursor_list: Vec<String>,
    before: Option<i64>,
    after: Option<i64>,
    messages: Option<bool>,
}

async fn cursor_memory(
    State(app): State<Arc<App>>,
    Extension(RequestTrace(trace)): Extension<RequestTrace>,
    UrlPath(name): UrlPath<String>,
    Json(body): Json<CursorBody>,
) -> Response {
    let result = spawn_op(move || run_cursor(&app, &name, body, &trace)).await;
    answer(result, &trace)
}

pub(crate) fn run_cursor(
    app: &App,
    name: &str,
    body: CursorBody,
    trace: &Ulid,
) -> Result<(serde_json::Value, Stats), Error> {
    if body.cursor_list.is_empty() {
        return Err(Error::EmptyIds);
    }
    let mut units = Vec::new();
    for id in &body.cursor_list {
        match Ulid::parse(id) {
            Ok(unit) => units.push(unit),
            Err(source) => {
                return Err(Error::NotUlid {
                    value: id.clone(),
                    source,
                });
            }
        }
    }
    let before = body.before.unwrap_or(2);
    let after = body.after.unwrap_or(2);
    let whole = body.messages.unwrap_or(false);
    let (opened, open_ms) = open(app, name)?;
    let wait = Instant::now();
    let store = opened.store.lock().unwrap();
    let store_lock_ms = ms(wait);
    let (rows, missing, mut stats) = cursor(&store, name, &units, before, after, whole, trace)?;
    stats.open_ms = open_ms;
    stats.store_lock_ms = Some(store_lock_ms);
    Ok((cursor_json(&rows, &units, &missing, whole, &stats), stats))
}

#[derive(Debug, Deserialize)]
pub(crate) struct LexiconBody {
    word_list: Vec<String>,
}

async fn lexicon_memory(
    State(app): State<Arc<App>>,
    Extension(RequestTrace(trace)): Extension<RequestTrace>,
    UrlPath(name): UrlPath<String>,
    Json(body): Json<LexiconBody>,
) -> Response {
    let result = spawn_op(move || run_lexicon(&app, &name, body, &trace)).await;
    answer(result, &trace)
}

pub(crate) fn run_lexicon(
    app: &App,
    name: &str,
    body: LexiconBody,
    trace: &Ulid,
) -> Result<(serde_json::Value, Stats), Error> {
    let (opened, open_ms) = open(app, name)?;
    let (rows, mut stats) = lexicon(&opened.index, name, &body.word_list, trace)?;
    stats.open_ms = open_ms;
    Ok((lexicon_json(&rows, &stats), stats))
}

async fn delete_memory(
    State(app): State<Arc<App>>,
    Extension(RequestTrace(trace)): Extension<RequestTrace>,
    UrlPath(name): UrlPath<String>,
) -> Response {
    let result = spawn_op(move || run_delete(&app, &name, &trace)).await;
    answer(result, &trace)
}

pub(crate) fn run_delete(
    app: &App,
    name: &str,
    trace: &Ulid,
) -> Result<(serde_json::Value, Stats), Error> {
    app.permit(Permission::Delete)?;
    let (memory, stats) = delete(&app.root, name, Some(&app.memories), trace)?;
    let value = serde_json::json!({
        "id": memory.id.to_string(),
        "name": memory.name,
        "sessions": memory.sessions,
        "messages": memory.messages,
        "units": memory.units,
        "stats": stats,
    });
    Ok((value, stats))
}

async fn rescan_memory(
    State(app): State<Arc<App>>,
    Extension(RequestTrace(trace)): Extension<RequestTrace>,
    UrlPath(name): UrlPath<String>,
) -> Response {
    let result = spawn_op(move || run_rescan(&app, &name, &trace)).await;
    answer(result, &trace)
}

pub(crate) fn run_rescan(
    app: &App,
    name: &str,
    trace: &Ulid,
) -> Result<(serde_json::Value, Stats), Error> {
    app.permit(Permission::Rescan)?;
    let (opened, open_ms) = open_attached(app, name)?;
    let (messages, units, mut stats) =
        rescan(&opened.store, &opened.index, &opened.writer, name, trace)?;
    stats.open_ms = open_ms;
    let value = serde_json::json!({
        "messages": messages,
        "units": units,
        "stats": stats,
    });
    Ok((value, stats))
}

impl From<tantivy::TantivyError> for Error {
    fn from(source: tantivy::TantivyError) -> Self {
        Error::Search(search::Error::Search { source })
    }
}
