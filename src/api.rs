//! Shared memory operations, and the HTTP API that serves them.
//!
//! CLI and `serve` both call the functions in this file. The CLI prints; the
//! router serializes the same values as JSON. A ULID is minted at the start of
//! every operation, carried on the tracing span as `trace`, returned in
//! `stats.trace`, and — on HTTP — copied to `X-Trace-Id`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::extract::{Path as UrlPath, Query, Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode, header::AUTHORIZATION};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get as get_route, patch, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tantivy::IndexWriter;

use crate::index::Index;
use crate::search::{Filter, Group, Outcome};
use crate::storage::{Entry, Located, Memory, Message, Role, Storage, Written};
use crate::ulid::Ulid;
use crate::{index, normalize, search};

const TRACE_HEADER: HeaderName = HeaderName::from_static("x-trace-id");
const SERVER_HEADER: HeaderName = HeaderName::from_static("server");
const VERSION_HEADER: HeaderName = HeaderName::from_static("x-borhan-version");
const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("pass at least one of description and languages")]
    NothingToUpdate,

    #[error("pass at least one unit ULID to read back")]
    EmptyIds,

    #[error("pass at least one word to look up")]
    EmptyWords,

    #[error("Role {role:?} is not one of user, assistant or tool")]
    Role { role: String },

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

    #[error("could not encode JSON")]
    Json {
        #[source]
        source: serde_json::Error,
    },
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
    pub memories: Mutex<HashMap<String, Arc<OpenMemory>>>,
}

impl App {
    pub fn new(root: PathBuf, token: Option<String>) -> Self {
        Self {
            root,
            token,
            memories: Mutex::new(HashMap::new()),
        }
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

pub fn list(root: &Path) -> Result<(Vec<Memory>, Stats), Error> {
    let trace = Ulid::new()?;
    let span = tracing::info_span!("List", trace = %trace);
    let _entered = span.enter();
    let started = Instant::now();
    tracing::debug!(msg = "Listing memories");
    let fetch = Instant::now();
    let memories = Storage::list(root)?;
    let fetch_ms = ms(fetch);
    tracing::info!(msg = "Listed memories", count = memories.len());
    let mut stats = Stats::new(&trace, started);
    stats.fetch_ms = Some(fetch_ms);
    Ok((memories, stats))
}

pub fn create(
    root: &Path,
    name: &str,
    description: &str,
    languages: &str,
) -> Result<(Ulid, Stats), Error> {
    let trace = Ulid::new()?;
    let span = tracing::info_span!("Create", trace = %trace, memory = name);
    let _entered = span.enter();
    let started = Instant::now();
    tracing::debug!(msg = "Creating memory", name = name);
    let write = Instant::now();
    let (store, id) = Storage::create(root, name, description, languages)?;
    let write_ms = ms(write);
    let index = Instant::now();
    let built = Index::attach(&store.directory.join(index::DIRECTORY))?;
    store.set_meta(index::VERSION_KEY, &normalize::VERSION.to_string())?;
    store.set_meta(index::BUILT_KEY, &Ulid::now().to_string())?;
    drop(built);
    let index_ms = ms(index);
    tracing::info!(msg = "Created memory", ulid = %id, name = name);
    let mut stats = Stats::new(&trace, started);
    stats.write_ms = Some(write_ms);
    stats.index_ms = Some(index_ms);
    Ok((id, stats))
}

pub fn update(
    store: &Storage,
    name: &str,
    description: Option<&str>,
    languages: Option<&str>,
) -> Result<(Ulid, Stats), Error> {
    if description.is_none() && languages.is_none() {
        return Err(Error::NothingToUpdate);
    }
    let trace = Ulid::new()?;
    let span = tracing::info_span!("Update", trace = %trace, memory = name);
    let _entered = span.enter();
    let started = Instant::now();
    tracing::debug!(msg = "Updating memory", name = name);
    let write = Instant::now();
    store.update(description, languages)?;
    let write_ms = ms(write);
    let memory = store.describe()?;
    tracing::info!(msg = "Updated memory", name = name);
    let mut stats = Stats::new(&trace, started);
    stats.write_ms = Some(write_ms);
    Ok((memory.id, stats))
}

pub fn add(
    store: &Mutex<Storage>,
    index: &Index,
    writer: &Mutex<IndexWriter>,
    entry: &Entry<'_>,
    name: &str,
) -> Result<(Written, Stats), Error> {
    let trace = Ulid::new()?;
    let span = tracing::info_span!("Add", trace = %trace, memory = name);
    let _entered = span.enter();
    let started = Instant::now();
    tracing::debug!(
        msg = "Adding message",
        memory = name,
        session = entry.session,
        bytes = entry.body.len()
    );
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
    tracing::info!(
        msg = "Added message",
        memory = name,
        ulid = %written.message,
        seq = written.seq,
        units = written.units.len()
    );
    let mut stats = Stats::new(&trace, started);
    stats.store_lock_ms = Some(store_lock_ms);
    stats.writer_lock_ms = Some(writer_lock_ms);
    stats.write_ms = Some(write_ms);
    stats.index_ms = Some(index_ms);
    stats.commit_ms = Some(commit_ms);
    Ok((written, stats))
}

pub fn get(store: &Storage, ids: &[Ulid]) -> Result<(Vec<Located>, Stats), Error> {
    let trace = Ulid::new()?;
    let span = tracing::info_span!("Get", trace = %trace);
    let _entered = span.enter();
    let started = Instant::now();
    tracing::debug!(msg = "Reading units", count = ids.len());
    let fetch = Instant::now();
    let located = store.locate(ids)?;
    let fetch_ms = ms(fetch);
    tracing::info!(msg = "Read units", count = located.len());
    let mut stats = Stats::new(&trace, started);
    stats.fetch_ms = Some(fetch_ms);
    Ok((located, stats))
}

pub fn search(
    store: &Storage,
    index: &Index,
    name: &str,
    groups: &[Group],
    filter: &Filter,
    limit: usize,
    per_message: usize,
) -> Result<(Outcome, Stats), Error> {
    let trace = Ulid::new()?;
    let span = tracing::info_span!("Search", trace = %trace, memory = name);
    let _entered = span.enter();
    let started = Instant::now();
    tracing::debug!(
        msg = "Searching",
        memory = name,
        groups = groups.len(),
        limit = limit
    );
    let search_started = Instant::now();
    let outcome = search::search(store, index, groups, filter, limit, per_message)?;
    let search_ms = ms(search_started);

    let mut returned = Vec::new();
    for hit in &outcome.hits {
        returned.push(serde_json::json!({
            "unit": hit.unit.to_string(),
            "score": hit.score,
            "coverage": [hit.coverage.0, hit.coverage.1],
        }));
    }
    let mut asked = Vec::new();
    for group in groups {
        asked.push(serde_json::json!({
            "label": group.label,
            "word_list": group.words,
            "required": group.required,
        }));
    }
    let asked = match serde_json::to_string(&serde_json::Value::Array(asked)) {
        Ok(asked) => asked,
        Err(source) => return Err(Error::Json { source }),
    };
    let logged = match serde_json::to_string(&serde_json::Value::Array(returned)) {
        Ok(logged) => logged,
        Err(source) => return Err(Error::Json { source }),
    };
    let write = Instant::now();
    store.log_search(&asked, &logged)?;
    let write_ms = ms(write);

    tracing::info!(
        msg = "Searched",
        memory = name,
        hits = outcome.hits.len(),
        unknown = outcome.unknown.len()
    );
    let mut stats = Stats::new(&trace, started);
    stats.search_ms = Some(search_ms);
    stats.write_ms = Some(write_ms);
    Ok((outcome, stats))
}

pub fn cursor(
    store: &Storage,
    name: &str,
    unit: Ulid,
    before: i64,
    after: i64,
) -> Result<(Vec<Message>, Stats), Error> {
    let trace = Ulid::new()?;
    let span = tracing::info_span!("Cursor", trace = %trace, memory = name);
    let _entered = span.enter();
    let started = Instant::now();
    tracing::debug!(msg = "Expanding a cursor", memory = name, unit = %unit);
    let cursor_started = Instant::now();
    let messages = store.around(unit, before, after)?;
    let cursor_ms = ms(cursor_started);
    let write = Instant::now();
    store.log_expansion(unit)?;
    let write_ms = ms(write);
    tracing::info!(
        msg = "Expanded a cursor",
        memory = name,
        unit = %unit,
        messages = messages.len()
    );
    let mut stats = Stats::new(&trace, started);
    stats.cursor_ms = Some(cursor_ms);
    stats.write_ms = Some(write_ms);
    Ok((messages, stats))
}

pub fn lexicon(index: &Index, name: &str, words: &[String]) -> Result<(Vec<Lexeme>, Stats), Error> {
    if words.is_empty() {
        return Err(Error::EmptyWords);
    }
    let trace = Ulid::new()?;
    let span = tracing::info_span!("Lexicon", trace = %trace, memory = name);
    let _entered = span.enter();
    let started = Instant::now();
    tracing::debug!(msg = "Looking up words", memory = name, count = words.len());
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
    tracing::info!(msg = "Looked up words", memory = name, count = rows.len());
    let mut stats = Stats::new(&trace, started);
    stats.lexicon_ms = Some(lexicon_ms);
    Ok((rows, stats))
}

pub fn rescan(
    store: &Mutex<Storage>,
    index: &Index,
    writer: &Mutex<IndexWriter>,
    name: &str,
) -> Result<(usize, usize, Stats), Error> {
    let trace = Ulid::new()?;
    let span = tracing::info_span!("Rescan", trace = %trace, memory = name);
    let _entered = span.enter();
    let started = Instant::now();
    tracing::debug!(msg = "Rescanning memory", memory = name);
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
    tracing::info!(
        msg = "Rescanned memory",
        memory = name,
        messages = written.len(),
        units = units
    );
    let mut stats = Stats::new(&trace, started);
    stats.store_lock_ms = Some(store_lock_ms);
    stats.writer_lock_ms = Some(writer_lock_ms);
    stats.commit_ms = Some(commit_ms);
    stats.resplit_ms = Some(resplit_ms);
    stats.index_ms = Some(index_ms);
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

pub fn id_json(id: &Ulid, stats: &Stats) -> serde_json::Value {
    serde_json::json!({ "id": id.to_string(), "stats": stats })
}

pub fn get_json(located: &[Located], stats: &Stats) -> serde_json::Value {
    let mut unit_list = Vec::new();
    for row in located {
        unit_list.push(serde_json::json!({
            "unit": row.unit.to_string(),
            "message": row.message.to_string(),
            "message_ref": row.message_ref,
            "session": row.session.to_string(),
            "session_ref": row.session_ref,
            "seq": row.seq,
            "unit_seq": row.unit_seq,
            "author": row.author,
            "role": row.role.as_str(),
            "ts": row.ts,
            "text": row.text(),
        }));
    }
    serde_json::json!({ "unit_list": unit_list, "stats": stats })
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
            "session": hit.session,
            "message": hit.message,
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
            "group": unknown.group,
            "word": unknown.word,
        }));
    }
    let mut hint_list = Vec::new();
    for (term, units) in &outcome.hints {
        hint_list.push(serde_json::json!({ "term": term, "units": units }));
    }
    serde_json::json!({
        "hit_list": hit_list,
        "unknown_list": unknown_list,
        "hint_list": hint_list,
        "stats": stats,
    })
}

pub fn cursor_json(messages: &[Message], stats: &Stats) -> serde_json::Value {
    let mut message_list = Vec::new();
    for message in messages {
        message_list.push(serde_json::json!({
            "message": message.id.to_string(),
            "message_ref": message.reference,
            "seq": message.seq,
            "author": message.author,
            "role": message.role.as_str(),
            "ts": message.ts,
            "anchor": message.anchor,
            "body": message.body,
        }));
    }
    serde_json::json!({ "message_list": message_list, "stats": stats })
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
        .route("/api/v1/memory/{name}", patch(update_memory))
        .route("/api/v1/memory/{name}/message_list", post(add_message))
        .route("/api/v1/memory/{name}/unit_list", post(get_units))
        .route("/api/v1/memory/{name}/search", post(search_memory))
        .route(
            "/api/v1/memory/{name}/cursor/{id}",
            get_route(cursor_memory),
        )
        .route("/api/v1/memory/{name}/lexicon", post(lexicon_memory))
        .route("/api/v1/memory/{name}/rescan", post(rescan_memory))
        .layer(middleware::from_fn_with_state(app.clone(), token_gate))
        .layer(middleware::from_fn(identify))
        .with_state(app)
}

async fn identify(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let server = format!("borhan/{VERSION}");
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

fn traced(status: StatusCode, trace: &str, body: serde_json::Value) -> Response {
    let mut response = (status, Json(body)).into_response();
    if let Ok(value) = HeaderValue::from_str(trace) {
        response.headers_mut().insert(TRACE_HEADER, value);
    }
    response
}

fn fail(status: StatusCode, error: impl std::fmt::Display) -> Response {
    let trace = match Ulid::new() {
        Ok(id) => id.to_string(),
        Err(_) => String::new(),
    };
    traced(
        status,
        &trace,
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
        | Error::Search(search::Error::Empty)
        | Error::Storage(
            crate::storage::Error::Empty
            | crate::storage::Error::Long { .. }
            | crate::storage::Error::Charset { .. }
            | crate::storage::Error::Description
            | crate::storage::Error::DescriptionShort
            | crate::storage::Error::Languages
            | crate::storage::Error::Reference { .. },
        ) => StatusCode::BAD_REQUEST,
        Error::Storage(crate::storage::Error::Missing { .. }) => StatusCode::NOT_FOUND,
        Error::Storage(
            crate::storage::Error::Exists { .. } | crate::storage::Error::Duplicate { .. },
        ) => StatusCode::CONFLICT,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn reject(error: Error) -> Response {
    fail(status_of(&error), error)
}

fn open(app: &App, name: &str) -> Result<(Arc<OpenMemory>, Option<u64>), Error> {
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
            return fail(StatusCode::UNAUTHORIZED, "missing or wrong token");
        }
    }
    next.run(request).await
}

async fn health() -> Response {
    let started = Instant::now();
    let trace = match Ulid::new() {
        Ok(id) => id,
        Err(error) => return fail(StatusCode::INTERNAL_SERVER_ERROR, error),
    };
    let span = tracing::info_span!("Health", trace = %trace);
    let _entered = span.enter();
    let stats = Stats::new(&trace, started);
    tracing::debug!(msg = "Health check");
    traced(
        StatusCode::OK,
        &stats.trace,
        serde_json::json!({ "ok": true, "stats": stats }),
    )
}

async fn memory_list(State(app): State<Arc<App>>) -> Response {
    let root = app.root.clone();
    let result = tokio::task::spawn_blocking(move || list(&root)).await;
    match result {
        Ok(Ok((memories, stats))) => {
            traced(StatusCode::OK, &stats.trace, list_json(&memories, &stats))
        }
        Ok(Err(error)) => reject(error),
        Err(error) => fail(StatusCode::INTERNAL_SERVER_ERROR, error),
    }
}

#[derive(Debug, Deserialize)]
struct CreateBody {
    name: String,
    description: String,
    languages: Option<String>,
}

async fn create_memory(State(app): State<Arc<App>>, Json(body): Json<CreateBody>) -> Response {
    let result = tokio::task::spawn_blocking(move || {
        let languages = match &body.languages {
            Some(text) if !text.is_empty() => text.as_str(),
            _ => "fa,en",
        };
        let (id, mut stats) = create(&app.root, &body.name, &body.description, languages)?;
        match open(&app, &body.name) {
            Ok((_, open_ms)) => stats.open_ms = open_ms,
            Err(error) => return Err(error),
        }
        Ok::<_, Error>((id, stats))
    })
    .await;
    match result {
        Ok(Ok((id, stats))) => traced(StatusCode::OK, &stats.trace, id_json(&id, &stats)),
        Ok(Err(error)) => reject(error),
        Err(error) => fail(StatusCode::INTERNAL_SERVER_ERROR, error),
    }
}

#[derive(Debug, Deserialize)]
struct UpdateBody {
    description: Option<String>,
    languages: Option<String>,
}

async fn update_memory(
    State(app): State<Arc<App>>,
    UrlPath(name): UrlPath<String>,
    Json(body): Json<UpdateBody>,
) -> Response {
    let result = tokio::task::spawn_blocking(move || {
        let (opened, open_ms) = open(&app, &name)?;
        let wait = Instant::now();
        let store = opened.store.lock().unwrap();
        let store_lock_ms = ms(wait);
        let (id, mut stats) = update(
            &store,
            &name,
            body.description.as_deref(),
            body.languages.as_deref(),
        )?;
        stats.open_ms = open_ms;
        stats.store_lock_ms = Some(store_lock_ms);
        Ok::<_, Error>((id, stats))
    })
    .await;
    match result {
        Ok(Ok((id, stats))) => traced(StatusCode::OK, &stats.trace, id_json(&id, &stats)),
        Ok(Err(error)) => reject(error),
        Err(error) => fail(StatusCode::INTERNAL_SERVER_ERROR, error),
    }
}

#[derive(Debug, Deserialize)]
struct AddBody {
    session: String,
    message: Option<String>,
    role: Option<String>,
    author: Option<String>,
    ts: Option<i64>,
    body: String,
}

async fn add_message(
    State(app): State<Arc<App>>,
    UrlPath(name): UrlPath<String>,
    Json(body): Json<AddBody>,
) -> Response {
    let result = tokio::task::spawn_blocking(move || {
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
        let (opened, open_ms) = open(&app, &name)?;
        let entry = Entry {
            session: &body.session,
            message: body.message.as_deref(),
            author,
            role,
            ts,
            body: &body.body,
        };
        let (written, mut stats) =
            add(&opened.store, &opened.index, &opened.writer, &entry, &name)?;
        stats.open_ms = open_ms;
        Ok((written, stats))
    })
    .await;
    match result {
        Ok(Ok((written, stats))) => traced(
            StatusCode::OK,
            &stats.trace,
            serde_json::json!({
                "id": written.message.to_string(),
                "units": written.units.len(),
                "stats": stats,
            }),
        ),
        Ok(Err(error)) => reject(error),
        Err(error) => fail(StatusCode::INTERNAL_SERVER_ERROR, error),
    }
}

#[derive(Debug, Deserialize)]
struct GetBody {
    id_list: Vec<String>,
}

async fn get_units(
    State(app): State<Arc<App>>,
    UrlPath(name): UrlPath<String>,
    Json(body): Json<GetBody>,
) -> Response {
    let result = tokio::task::spawn_blocking(move || {
        if body.id_list.is_empty() {
            return Err(Error::EmptyIds);
        }
        let mut ids = Vec::new();
        for id in &body.id_list {
            match Ulid::parse(id) {
                Ok(id) => ids.push(id),
                Err(source) => {
                    return Err(Error::NotUlid {
                        value: id.clone(),
                        source,
                    });
                }
            }
        }
        let (opened, open_ms) = open(&app, &name)?;
        let wait = Instant::now();
        let store = opened.store.lock().unwrap();
        let store_lock_ms = ms(wait);
        let (located, mut stats) = get(&store, &ids)?;
        stats.open_ms = open_ms;
        stats.store_lock_ms = Some(store_lock_ms);
        Ok((located, stats))
    })
    .await;
    match result {
        Ok(Ok((located, stats))) => {
            traced(StatusCode::OK, &stats.trace, get_json(&located, &stats))
        }
        Ok(Err(error)) => reject(error),
        Err(error) => fail(StatusCode::INTERNAL_SERVER_ERROR, error),
    }
}

#[derive(Debug, Deserialize)]
struct SearchBody {
    group_list: Vec<GroupBody>,
    limit: Option<usize>,
    max_per_message: Option<usize>,
    session: Option<String>,
    after: Option<i64>,
    before: Option<i64>,
    role_list: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct GroupBody {
    label: String,
    word_list: Vec<String>,
    #[serde(default)]
    required: bool,
}

async fn search_memory(
    State(app): State<Arc<App>>,
    UrlPath(name): UrlPath<String>,
    Json(body): Json<SearchBody>,
) -> Response {
    let result = tokio::task::spawn_blocking(move || {
        let mut groups = Vec::new();
        for group in body.group_list {
            groups.push(Group {
                label: group.label,
                words: group.word_list,
                required: group.required,
            });
        }
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
        let (opened, open_ms) = open(&app, &name)?;
        let wait = Instant::now();
        let store = opened.store.lock().unwrap();
        let store_lock_ms = ms(wait);
        let (outcome, mut stats) = search(
            &store,
            &opened.index,
            &name,
            &groups,
            &filter,
            limit,
            per_message,
        )?;
        stats.open_ms = open_ms;
        stats.store_lock_ms = Some(store_lock_ms);
        Ok((outcome, stats))
    })
    .await;
    match result {
        Ok(Ok((outcome, stats))) => {
            traced(StatusCode::OK, &stats.trace, search_json(&outcome, &stats))
        }
        Ok(Err(error)) => reject(error),
        Err(error) => fail(StatusCode::INTERNAL_SERVER_ERROR, error),
    }
}

#[derive(Debug, Deserialize)]
struct CursorQuery {
    before: Option<i64>,
    after: Option<i64>,
}

async fn cursor_memory(
    State(app): State<Arc<App>>,
    UrlPath((name, id)): UrlPath<(String, String)>,
    Query(query): Query<CursorQuery>,
) -> Response {
    let result = tokio::task::spawn_blocking(move || {
        let unit = match Ulid::parse(&id) {
            Ok(unit) => unit,
            Err(source) => {
                return Err(Error::NotUlid { value: id, source });
            }
        };
        let before = query.before.unwrap_or(2);
        let after = query.after.unwrap_or(2);
        let (opened, open_ms) = open(&app, &name)?;
        let wait = Instant::now();
        let store = opened.store.lock().unwrap();
        let store_lock_ms = ms(wait);
        let (messages, mut stats) = cursor(&store, &name, unit, before, after)?;
        stats.open_ms = open_ms;
        stats.store_lock_ms = Some(store_lock_ms);
        Ok((messages, stats))
    })
    .await;
    match result {
        Ok(Ok((messages, stats))) => {
            traced(StatusCode::OK, &stats.trace, cursor_json(&messages, &stats))
        }
        Ok(Err(error)) => reject(error),
        Err(error) => fail(StatusCode::INTERNAL_SERVER_ERROR, error),
    }
}

#[derive(Debug, Deserialize)]
struct LexiconBody {
    word_list: Vec<String>,
}

async fn lexicon_memory(
    State(app): State<Arc<App>>,
    UrlPath(name): UrlPath<String>,
    Json(body): Json<LexiconBody>,
) -> Response {
    let result = tokio::task::spawn_blocking(move || {
        let (opened, open_ms) = open(&app, &name)?;
        let (rows, mut stats) = lexicon(&opened.index, &name, &body.word_list)?;
        stats.open_ms = open_ms;
        Ok((rows, stats))
    })
    .await;
    match result {
        Ok(Ok((rows, stats))) => traced(StatusCode::OK, &stats.trace, lexicon_json(&rows, &stats)),
        Ok(Err(error)) => reject(error),
        Err(error) => fail(StatusCode::INTERNAL_SERVER_ERROR, error),
    }
}

async fn rescan_memory(State(app): State<Arc<App>>, UrlPath(name): UrlPath<String>) -> Response {
    let result = tokio::task::spawn_blocking(move || {
        let (opened, open_ms) = open_attached(&app, &name)?;
        let (messages, units, mut stats) =
            rescan(&opened.store, &opened.index, &opened.writer, &name)?;
        stats.open_ms = open_ms;
        Ok((messages, units, stats))
    })
    .await;
    match result {
        Ok(Ok((messages, units, stats))) => traced(
            StatusCode::OK,
            &stats.trace,
            serde_json::json!({
                "messages": messages,
                "units": units,
                "stats": stats,
            }),
        ),
        Ok(Err(error)) => reject(error),
        Err(error) => fail(StatusCode::INTERNAL_SERVER_ERROR, error),
    }
}

impl From<tantivy::TantivyError> for Error {
    fn from(source: tantivy::TantivyError) -> Self {
        Error::Search(search::Error::Search { source })
    }
}
