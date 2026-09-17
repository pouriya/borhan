//! MCP over the HTTP server, at `/mcp`.
//!
//! A wrapper and nothing else. Every tool here is one of the `run_*` functions
//! in `api.rs` — the same ones the REST handlers call, deserializing into the
//! same body structs — so a tool and its endpoint cannot drift apart, and the
//! `refuse` list in `server.toml` governs both without being consulted twice.
//! What this file adds is the JSON-RPC envelope, the schemas that tell a model
//! what the arguments mean, and the catalogue of memories as resources.
//!
//! # Which revision, and why not the current one
//!
//! `2026-07-28` is the ratified specification and it is not what this speaks.
//! It removes the `initialize` handshake, sessions and the GET stream, and puts
//! the protocol version in each request's `_meta`. Of the clients this is for —
//! Claude Code, Cursor, Codex, OpenCode, Hermes — every one is on the handshake
//! era today; Claude Code's newer runtime probes for `2026-07-28` and falls
//! back when it does not find it. So a `2026-07-28`-only server would work with
//! one of the five, and this one works with all of them.
//!
//! What that costs is one deliberate-looking piece of rudeness in [`handle`]:
//! `server/discover` is answered with `404` and an **empty body**, never a
//! JSON-RPC error. The newer specification says a client falls back to
//! `initialize` only when the body is *not* a recognized modern error — so
//! answering `-32601` there would convince a modern client that this server
//! speaks its era and is merely missing a method, and it would give up instead
//! of falling back. Silence is the signal.
//!
//! # Transport
//!
//! Streamable HTTP, POST only, `application/json` out. There is no SSE stream
//! and no session: borhan never initiates a message, so a stream would have
//! nothing to carry, and there is no cross-call state to key. `GET` and
//! `DELETE` get `405` from the router, `Mcp-Session-Id` and `Last-Event-ID` are
//! ignored — which is what the specification asks a server of this shape to do.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header::ORIGIN};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde_json::{Value, json};

use crate::api::{self, App, Permission, RequestTrace};
use crate::storage::Storage;
use crate::ulid::Ulid;

/// What [`initialize`] answers a client that asked for something else.
///
/// The last revision of the handshake era, so it is the most a client can get
/// out of this server. A client asking for one of the older three gets its own
/// version back: nothing borhan exposes — tools and resources, no sampling, no
/// roots, no elicitation — changed shape across them.
const PROTOCOL: &str = "2025-11-25";

const PROTOCOLS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// Lemmas reported by `resources/read`.
///
/// Enough to show a caller what language the memory is in and which of its own
/// words are worth reaching for, few enough that reading the resource costs
/// less context than the search it is meant to improve.
const VOCABULARY: usize = 60;

/// The resource URI for one memory. `<scheme><name>`.
const SCHEME: &str = "borhan://memory/";

/// What one JSON-RPC message turns into.
enum Reply {
    /// A notification. `202 Accepted` with no body, which is the only thing the
    /// transport allows for a message that has no id to answer.
    Accepted,
    /// A result or an error, to go out inside the JSON-RPC envelope.
    Message(Value),
    /// A probe from a client speaking a revision this server does not. `404`
    /// and an empty body; see the note at the top of this file.
    Fallback,
}

/// A JSON-RPC error: a code from the specification and a sentence for a person.
struct Rejection {
    code: i64,
    message: String,
}

pub(crate) async fn endpoint(
    State(app): State<Arc<App>>,
    Extension(RequestTrace(trace)): Extension<RequestTrace>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // The one protection the transport specification makes the server's
    // problem rather than the deployment's. A page on the open web can POST to
    // 127.0.0.1 from the visitor's own browser, and without this check any site
    // they visit can read every memory on their machine. An absent `Origin` is
    // a program rather than a page — the CLI, curl, an agent — and is allowed;
    // a present one has to be loopback, and `null` is not.
    if let Some(value) = headers.get(ORIGIN) {
        // Starts refused, so an `Origin` that is not even text — and `null`,
        // which is what a sandboxed frame sends — falls through as refused
        // rather than through a branch somebody has to remember to write.
        let mut allowed = false;
        let mut origin = String::new();
        if let Ok(text) = value.to_str() {
            origin = text.to_string();
            let authority = match text.split_once("://") {
                Some((_, authority)) => authority,
                None => text,
            };
            // A bracketed IPv6 literal keeps its brackets and its colons, so
            // the port is whatever follows the `]`. Everything else splits on
            // the first colon it has.
            let host = match authority.find(']') {
                Some(end) => &authority[..=end],
                None => match authority.split_once(':') {
                    Some((host, _)) => host,
                    None => authority,
                },
            };
            allowed = host == "localhost" || host == "127.0.0.1" || host == "[::1]";
        }
        if !allowed {
            tracing::warn!(
                trace_id = %trace,
                origin = origin,
                "refused a cross-origin MCP request",
            );
            return api::traced(
                StatusCode::FORBIDDEN,
                &trace.to_string(),
                envelope(
                    &Value::Null,
                    Err(Rejection {
                        code: -32600,
                        message: format!("{origin:?} is not a loopback origin"),
                    }),
                ),
            );
        }
    }

    let message: Value = match serde_json::from_slice(&body) {
        Ok(message) => message,
        Err(source) => {
            return api::traced(
                StatusCode::BAD_REQUEST,
                &trace.to_string(),
                envelope(
                    &Value::Null,
                    Err(Rejection {
                        code: -32700,
                        message: format!("the body is not JSON: {source}"),
                    }),
                ),
            );
        }
    };

    // Everything under here reads SQLite and mmapped segments, so it goes to
    // the blocking pool for the same reason every REST handler does.
    let reply = match api::spawn_op(move || handle(&app, &message, &trace)).await {
        Ok(reply) => reply,
        Err(source) => Reply::Message(envelope(
            &Value::Null,
            Err(Rejection {
                code: -32603,
                message: format!("the operation panicked: {source}"),
            }),
        )),
    };

    match reply {
        Reply::Accepted => StatusCode::ACCEPTED.into_response(),
        Reply::Message(body) => Json(body).into_response(),
        Reply::Fallback => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Route one message, and emit the one event that says which one it was.
///
/// Without this line the access log records every call as `POST /mcp` and
/// nothing else — the method and the tool are in the body, which the log does
/// not read. The operations themselves still log their own events underneath.
fn handle(app: &Arc<App>, message: &Value, trace: &Ulid) -> Reply {
    let _span = tracing::info_span!("mcp").entered();
    let started = std::time::Instant::now();

    let method = match message["method"].as_str() {
        Some(method) => method,
        None => {
            return Reply::Message(envelope(
                &Value::Null,
                Err(Rejection {
                    code: -32600,
                    message: "the message has no method".to_string(),
                }),
            ));
        }
    };

    // No id is a notification, and the only one this era defines is
    // `notifications/initialized`. There is nothing to do with any of them and
    // nothing to answer them with.
    let Some(id) = message.get("id") else {
        tracing::debug!(trace_id = %trace, method = method, "took an MCP notification");
        return Reply::Accepted;
    };

    if method == "server/discover" {
        tracing::debug!(
            trace_id = %trace,
            method = method,
            "declined an MCP probe from a newer revision",
        );
        return Reply::Fallback;
    }

    let params = &message["params"];
    let tool = match params["name"].as_str() {
        Some(name) if method == "tools/call" => name,
        _ => "",
    };
    let outcome = match method {
        "initialize" => Ok(initialize(params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools(app) })),
        "tools/call" => call(app, params, trace),
        "resources/list" => resources(app),
        "resources/read" => read(app, params, trace),
        // Advertised by nobody and asked for by several clients anyway, which
        // is cheaper to answer than to explain.
        "resources/templates/list" => Ok(json!({ "resourceTemplates": [] })),
        _ => Err(Rejection {
            code: -32601,
            message: format!("{method:?} is not a method this server implements"),
        }),
    };

    let failed = match &outcome {
        Ok(value) => value["isError"] == Value::Bool(true),
        Err(_) => true,
    };
    tracing::info!(
        trace_id = %trace,
        method = method,
        tool = tool,
        failed = failed,
        total_ms = started.elapsed().as_millis() as u64,
        "served an MCP call",
    );
    Reply::Message(envelope(id, outcome))
}

/// Wrap a result or a rejection in the JSON-RPC 2.0 envelope.
fn envelope(id: &Value, outcome: Result<Value, Rejection>) -> Value {
    match outcome {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(rejection) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": rejection.code, "message": rejection.message },
        }),
    }
}

/// The handshake.
///
/// `instructions` is the one place a server gets to talk to the model before it
/// has called anything, so it says the two things that are not derivable from
/// any single tool schema: that parentheses hold one idea and separate parts are
/// separate ideas, and that coverage is the number to believe.
fn initialize(params: &Value) -> Value {
    let mut version = PROTOCOL;
    if let Some(asked) = params["protocolVersion"].as_str() {
        for known in PROTOCOLS {
            if known == asked {
                version = known;
            }
        }
    }
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {}, "resources": {} },
        "serverInfo": { "name": "borhan", "version": env!("CARGO_PKG_VERSION") },
        "instructions": "\
    borhan is a keyword memory over stored conversations, built for text that mixes \
    Persian and English in the same sentence. Read the resources first: each one is \
    a memory, and reading it gives you that memory's own vocabulary, which is what \
your query has to be written in.

Searching is by idea, not by sentence. `memory_search` takes one `query` string. \
    Words inside one pair of parentheses are one idea spelled every way the corpus \
    might spell it — (error خطا fail مشکل) is one idea, and a unit containing all four \
    scores once, not four times. Parts side by side are separate ideas: \
    (error خطا) (token توکن) asks for two things together. A + in front of a part \
    drops units without it, a - drops units with it: use + only when a result \
    without that idea would be useless. The tool's `query` description is the full \
    reference — read it before your first search.

Read `coverage` and not `score`. Coverage is [parts matched, parts asked] — a \
    fact. The score is BM25 squashed into 0..1 against the top hit of this one \
query; it is not a probability and it does not compare across queries.

Words that matched nothing come back in `unknown_list`, and words that the top \
    results share but you did not ask for come back in `hint_list`. Both are \
    directions for a second search, and a second search is usually the right move. \
    `memory_lexicon` answers the same question ahead of time: how often each word \
occurs, before you spend a query on it.

A hit is one paragraph or sentence, with a `cursor`. Pass those cursors — all \
    of them, in one call — to `memory_cursor` to read them back with as much or as \
    little around them as the question needs. It is the only reader here, so there is \
    nothing to choose between: widen the window rather than look for another tool. \
    Search for the gist, expand where it lands — do not raise `limit` to see more \
    context.",
    })
}

/// The tool list, which is where `server.toml`'s refusals become visible.
///
/// A server that may not write does not describe writing tools, so a model
/// never plans around an operation it will be refused. `tools/call` checks
/// again anyway, through the same `App::permit` the REST handlers use, because
/// a client is entitled to cache this list and a server is not entitled to
/// trust it.
///
/// Absence is the whole of the message. There is no disabled tool carrying a
/// note about how to enable it, because a model that reads such a note treats
/// it as the next step — the refusal has to look like the world being a
/// certain way, not like a door with the key taped to it.
fn tools(app: &App) -> Vec<Value> {
    let memory = json!({
        "type": "string",
        "description": "Name of the memory, as listed by memory_list.",
    });
    let mut tools = vec![
        json!({
            "name": "memory_list",
            "title": "List memories",
            "description": "Every memory on this server, with its description, \
        its languages and how much is in it. Start here when you do not already know \
        which memory holds what you want.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": { "readOnlyHint": true, "openWorldHint": false },
        }),
        json!({
            "name": "memory_outline",
            "title": "What is on file",
            "description": "What a memory holds, without reading any of it. \
        Without `session`, every session in the memory: its name, when it started, how \
        many messages and units. With `session`, that session's messages: their names, \
        who wrote them, how long they are. This is the question search cannot answer, \
        because search finds text and this asks what exists — before storing something \
        under a name, this is how you find out whether that name is already taken and \
        what is under it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "memory": memory,
                    "session": {
                        "type": "string",
                        "description": "A session's own name, as `session_list` \
        spells it. Omit to list the sessions instead.",
                    },
                },
                "required": ["memory"],
            },
            "annotations": { "readOnlyHint": true, "openWorldHint": false },
        }),
        json!({
            "name": "memory_search",
            "title": "Search a memory",
            "description": "Search one memory with a query string. Parentheses \
        hold one idea written every way the corpus might write it, across languages; a \
        unit matching several words of one idea counts once for it, and matching several \
        ideas is what makes it rank. Returns paragraphs and sentences with a snippet, a \
        coverage pair, the parts that matched, a cursor for reading around the hit, and — \
        separately — the words that matched nothing and the words the top results share \
        that you did not ask for. Ids are spelled the same way memory_cursor spells them: \
        `session` and `message` are ULIDs and take you somewhere, `session_ref` and \
        `message_ref` are the feeder's own names and are for reading.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "memory": memory,
                    "query": {
                        "type": "string",
                        "description": "What to find, in this syntax. Not a \
        sentence: reduce the question to the two to four ideas that must appear \
        together, then spell each one every way the corpus might.

WORDS. `borrow` matches a unit containing borrow, borrowed or borrowing — every \
        inflection. Case does not decide a match, but the exact spelling ranks above \
        another form of the word.

ONE IDEA: PARENTHESES. `(error fault خطا)` is one idea: synonyms, both languages, \
        the abbreviation, the misspelling the room uses. Only the best word in it \
        scores, so a unit with all three counts once. Never put two ideas in one pair.

SEVERAL IDEAS: PARTS SIDE BY SIDE. Every top-level part — a word, a phrase or a \
        parenthesised idea — is one idea, and `coverage` counts how many a unit \
        matched. `(error خطا) (token jwt) (expired انقضا)` is three ideas. `error \
        token` is two ideas; `(error token)` is one. A query wrapped whole in one \
        pair of parentheses is one idea.

REQUIRED AND EXCLUDED. `+` directly before a part drops units that miss it; `-` \
        drops units that contain it; no space after the sign. `+(token jwt) (error \
        خطا) -test`. `AND`, `OR`, `NOT` in capitals also work: `a AND b` is `+a +b`, \
        `NOT a` is `-a`, `a OR b` is `a b`. Lowercase and, or, not are plain words.

PHRASES. `\"borrow checker\"` needs both words together, in order. \
        `\"rotate token\"~2` allows up to 2 words between them. `\"borrow check\"*` \
        reads the last word as a beginning: borrow checker, borrow checking. A phrase \
        is two words or more.

WEIGHT. `^2` after a word, phrase or parenthesised idea doubles what it adds; \
        `^0.5` halves it. It reorders hits and never changes which units match.

FIELDS. A bare word is looked up as written, folded to its root, and in the rest \
        of its message, and the best counts. `surface:JWT_SECRET` is the exact \
        spelling only, case included — use it for identifiers. `lemma:borrowing` is \
        the folded form only. `context:rotation` is only the rest of the message. A \
        field before parentheses covers every word inside: `surface:(JWT_SECRET \
        API_KEY)`, the same as `surface: IN [JWT_SECRET API_KEY]`.

ESCAPING. `: ( ) [ ] { } ^ \" '` and backslash have meaning. Inside a word put a \
        backslash before them, `http\\://host`, or quote the phrase. A word cannot \
        start with + or -.

NOT SUPPORTED, each refused with an error saying what to write instead: regular \
        expressions (/…/); `*` on one word (`rot*` — write (rotate rotated \
        rotation)); ranges ([a TO b], >a); `*` alone; and session:, ts: or role: \
        inside the query — use the `session`, `after`/`before` and `role_list` \
        arguments.

EXAMPLE. \"why does the borrow checker reject this mutable alias\" becomes \
        `(borrow borrowck borrowing) (mutable mut) +(alias aliasing)`.",
                    },
                    "fuzzy": {
                        "type": "boolean",
                        "description": "Also match words one letter away from a \
        word this memory has never seen, for words of five letters or more: borow finds \
        borrow. Default false. Such matches count for half, words the memory does have \
        are never expanded, and `fuzzy_list` says which word was taken for which — \
        write that spelling next time rather than leaving this on.",
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Hits to return. Default 10. Prefer \
        expanding a good hit with memory_cursor over asking for more of them.",
                    },
                    "max_per_message": {
                        "type": "integer",
                        "description": "Most hits from any one message, so a \
        single long document cannot fill the page. Default 2.",
                    },
                    "session": {
                        "type": "string",
                        "description": "Confine the search to one session, by \
        its ULID — which is the `session` field of any hit from that session. (`session_ref` \
        beside it is the feeder's own name for the session and is not accepted here.)",
                    },
                    "after": {
                        "type": "integer",
                        "description": "Only units at or after this unix \
        timestamp in milliseconds.",
                    },
                    "before": {
                        "type": "integer",
                        "description": "Only units at or before this unix \
        timestamp in milliseconds.",
                    },
                    "role_list": {
                        "type": "array",
                        "items": { "type": "string", "enum": ["user", "assistant", "tool"] },
                        "description": "Only units written by these roles.",
                    },
                },
                "required": ["memory", "query"],
            },
            "annotations": { "readOnlyHint": true, "openWorldHint": false },
        }),
        json!({
            "name": "memory_cursor",
            "title": "Read a hit back",
            "description": "Take the `cursor` values from search hits and read \
        them back with their surroundings, in order, anchors marked. This is the \
        second half of every search, and the only way to read stored text: a hit is one \
        paragraph, and a paragraph rarely says who was talking or what they decided. \
        The window is counted in units, not messages — `before` and `after` of 0 \
        returns just the units asked for, the default of 2 adds the paragraphs either \
        side, and `messages: true` widens to the whole messages they came from, which \
        can be thirty times as much text. Results come back grouped by message: each \
        one carries a `unit_list` you can cursor from again, or its `body` when you \
        asked for whole messages. Pass every cursor worth reading in one call rather \
        than one call each.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "memory": memory,
                    "cursor_list": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "The `cursor` field of one or more hits. \
        Opaque — pass them back as they came. Overlapping windows are merged, and a \
        cursor this memory no longer holds is named in `missing_list` rather than \
        failing the read.",
                    },
                    "before": {
                        "type": "integer",
                        "description": "Units before each anchor, or messages \
        before it when `messages` is set. Default 2.",
                    },
                    "after": {
                        "type": "integer",
                        "description": "Units after each anchor, or messages \
        after it when `messages` is set. Default 2.",
                    },
                    "messages": {
                        "type": "boolean",
                        "description": "Count the window in whole messages and \
        return each one's `body` instead of its units. Ask for this when the paragraph \
        is not enough to tell what was being discussed; it is much more text.",
                    },
                },
                "required": ["memory", "cursor_list"],
            },
            "annotations": { "readOnlyHint": true, "openWorldHint": false },
        }),
        json!({
            "name": "memory_lexicon",
            "title": "Look words up before searching",
            "description": "How often each word occurs in this memory: as \
        written, as normalized, and propagated from surrounding messages. A word with a \
        count of zero will match nothing, so this is how to test a guess without \
        spending a search on it — and how to find out that the corpus says خطا where \
        you were about to say error.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "memory": memory,
                    "word_list": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Words to look up.",
                    },
                },
                "required": ["memory", "word_list"],
            },
            "annotations": { "readOnlyHint": true, "openWorldHint": false },
        }),
    ];

    if app.permissions.contains(&Permission::Create) {
        tools.push(json!({
            "name": "memory_create",
            "title": "Create a memory",
            "description": "Make a new, empty memory. The description is \
required and has to be a real sentence — it is what every later caller reads to \
decide whether to search here.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Name of the new memory." },
                    "description": {
                        "type": "string",
                        "description": "What this memory holds. More than ten words.",
                    },
                    "languages": {
                        "type": "string",
                        "description": "Comma-separated language codes. Default \"fa,en\".",
                    },
                },
                "required": ["name", "description"],
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "openWorldHint": false },
        }));
    }

    if app.permissions.contains(&Permission::Update) {
        tools.push(json!({
            "name": "memory_update",
            "title": "Update a memory's description",
            "description": "Change a memory's description or its languages. \
Nothing stored is touched.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "memory": memory,
                    "description": { "type": "string", "description": "The new description." },
                    "languages": { "type": "string", "description": "The new language codes." },
                },
                "required": ["memory"],
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false },
        }));
    }

    if app.permissions.contains(&Permission::Add) {
        tools.push(json!({
            "name": "memory_add",
            "title": "Store a message",
            "description": "Store one message, read as Markdown, split into \
paragraphs and sentences and indexed. Both `session` and `message` are the \
feeder's own identifiers and come back on every hit, so pass the ids the source \
system uses rather than inventing new ones.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "memory": memory,
                    "session": {
                        "type": "string",
                        "description": "The conversation this message belongs \
to, in the source system's own terms — a thread id, a channel, a filename.",
                    },
                    "message": {
                        "type": "string",
                        "description": "The source system's id for this \
message, if it has one. Sending the same one twice is refused, which is what \
makes a re-feed safe.",
                    },
                    "body": { "type": "string", "description": "The message text, as Markdown." },
                    "role": {
                        "type": "string",
                        "enum": ["user", "assistant", "tool"],
                        "description": "Default \"user\".",
                    },
                    "author": {
                        "type": "string",
                        "description": "Who wrote it. Defaults to the role.",
                    },
                    "ts": {
                        "type": "integer",
                        "description": "When it was written, unix milliseconds. \
Defaults to now — pass the original time when feeding history, or recency \
ranking will believe the whole corpus arrived today.",
                    },
                },
                "required": ["memory", "session", "body"],
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "openWorldHint": false },
        }));
    }

    if app.permissions.contains(&Permission::Rescan) {
        tools.push(json!({
            "name": "memory_rescan",
            "title": "Rebuild a memory's index",
            "description": "Throw the index away and rebuild it from the \
stored messages. Slow — minutes on a large memory — and destroys nothing that \
was not derived. For an index refused as stale after the normalizer changed.",
            "inputSchema": {
                "type": "object",
                "properties": { "memory": memory },
                "required": ["memory"],
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false },
        }));
    }

    if app.permissions.contains(&Permission::Replace) {
        tools.push(json!({
            "name": "memory_replace",
            "title": "Rewrite a stored message",
            "description": "Replace the body of a message that is already \
stored, keeping its place in the session. For a description that has gone out \
of date — the thing it describes changed, so the text is now wrong and leaving \
it beside a corrected copy would mean a search returns both. The old body is \
not kept anywhere. Both `session` and `message` must already exist: this never \
creates either, and memory_outline is how you find out what they are called.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "memory": memory,
                    "session": {
                        "type": "string",
                        "description": "The session holding the message.",
                    },
                    "message": {
                        "type": "string",
                        "description": "The message to rewrite, by the id it \
was stored under.",
                    },
                    "body": { "type": "string", "description": "The new text, as Markdown." },
                    "ts": {
                        "type": "integer",
                        "description": "When the new text was written, unix \
milliseconds. Defaults to now, which is usually right: the body is new, and \
keeping the old time would have recency rank a correction as though it were as \
old as the thing it corrects.",
                    },
                },
                "required": ["memory", "session", "message", "body"],
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": true, "idempotentHint": true, "openWorldHint": false },
        }));
    }

    if app.permissions.contains(&Permission::Delete) {
        tools.push(json!({
            "name": "memory_delete",
            "title": "Delete a memory",
            "description": "Remove a memory and everything in it. There is no \
undo and no copy: every message, session and unit goes. Ask before calling it.",
            "inputSchema": {
                "type": "object",
                "properties": { "memory": memory },
                "required": ["memory"],
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": false },
        }));
    }

    tools
}

/// Run one tool.
///
/// The two failure modes are kept apart on purpose. Arguments that do not fit
/// the schema are a JSON-RPC `-32602` — the client built the call wrong, and
/// the model cannot fix that. Everything the store itself refuses — an unknown
/// memory, a malformed ULID, a permission this server does not have — comes
/// back as a *successful* call carrying `isError`, because that is the form the
/// model gets to read, and reading "this server is not allowed to delete" is
/// what stops it trying again.
fn call(app: &Arc<App>, params: &Value, trace: &Ulid) -> Result<Value, Rejection> {
    let Some(name) = params["name"].as_str() else {
        return Err(Rejection {
            code: -32602,
            message: "tools/call needs a tool name".to_string(),
        });
    };
    let arguments = match params.get("arguments") {
        Some(arguments) => arguments.clone(),
        None => json!({}),
    };
    // Every per-memory tool takes the memory here rather than in the body
    // struct, so that `memory_create`'s `name` stays the name of the memory
    // being made and does not collide with the name of the tool making it.
    let memory = match arguments["memory"].as_str() {
        Some(memory) => memory.to_string(),
        None => String::new(),
    };

    macro_rules! body {
        () => {
            match serde_json::from_value(arguments) {
                Ok(body) => body,
                Err(source) => {
                    return Err(Rejection {
                        code: -32602,
                        message: format!(
                            "{name} was called with arguments it cannot use: {source}"
                        ),
                    });
                }
            }
        };
    }

    let outcome = match name {
        "memory_list" => api::run_list(app, trace),
        "memory_search" => api::run_search(app, &memory, body!(), trace),
        "memory_cursor" => api::run_cursor(app, &memory, body!(), trace),
        "memory_lexicon" => api::run_lexicon(app, &memory, body!(), trace),
        "memory_create" => api::run_create(app, body!(), trace),
        "memory_update" => api::run_update(app, &memory, body!(), trace),
        "memory_outline" => api::run_outline(app, &memory, body!(), trace),
        "memory_add" => api::run_add(app, &memory, body!(), trace),
        "memory_replace" => api::run_replace(app, &memory, body!(), trace),
        "memory_rescan" => api::run_rescan(app, &memory, trace),
        "memory_delete" => api::run_delete(app, &memory, trace),
        _ => {
            return Err(Rejection {
                code: -32602,
                message: format!("{name:?} is not a tool this server offers"),
            });
        }
    };

    match outcome {
        Ok((mut value, stats)) => {
            // The timings are for the operator's dashboards, not for the
            // model's context window, and there are twelve of them. The trace
            // stays, so a bad answer quoted back at you is one grep away.
            value["stats"] = json!({ "trace": stats.trace });
            Ok(json!({
                "content": [{ "type": "text", "text": value.to_string() }],
                "isError": false,
            }))
        }
        // `{:#}` flattens the `#[source]` chain onto one line. Without it the
        // model is told "could not open the index" and not which index or why.
        Err(error) => Ok(json!({
            "content": [{
                "type": "text",
                "text": format!("{:#}", anyhow::Error::from(error)),
            }],
            "isError": true,
        })),
    }
}

/// The catalogue: one resource per memory.
///
/// There is no `size`. The specification defines it as bytes of the resource's
/// content, and what a caller wants to know about a memory is how many units it
/// holds — a number that predicts what a search will do and that no byte count
/// stands in for. Lying to a client budgeting its context is worse than telling
/// it nothing, so the counts go where they are true: in the description a model
/// reads, and in `_meta` for anything reading structurally.
fn resources(app: &Arc<App>) -> Result<Value, Rejection> {
    let memories = match Storage::list(&app.root) {
        Ok(memories) => memories,
        Err(source) => {
            return Err(Rejection {
                code: -32603,
                message: format!("{:#}", anyhow::Error::from(source)),
            });
        }
    };

    let mut resources = Vec::new();
    for memory in &memories {
        let said = match &memory.description {
            Some(description) => description.as_str(),
            None => "No description.",
        };
        resources.push(json!({
            "uri": format!("{SCHEME}{}", memory.name),
            "name": memory.name,
            "title": memory.name,
            "description": format!(
                "{said}\n{} sessions, {} messages, {} units. Languages: {}. \
        Read this resource for the memory's own vocabulary before searching it.",
                memory.sessions, memory.messages, memory.units, memory.languages,
            ),
            "mimeType": "application/json",
            "_meta": {
                "borhan/sessions": memory.sessions,
                "borhan/messages": memory.messages,
                "borhan/units": memory.units,
            },
        }));
    }
    Ok(json!({ "resources": resources }))
}

/// One memory's metadata, and the words it is actually written in.
///
/// The vocabulary is the reason this returns anything beyond what
/// `resources/list` already said. A description tells a caller what a memory is
/// *about*, which was written by whoever created it; the lemmas tell it what
/// the memory *says*, which was written by whoever filled it. Only the second
/// one can be searched for.
fn read(app: &Arc<App>, params: &Value, trace: &Ulid) -> Result<Value, Rejection> {
    let Some(uri) = params["uri"].as_str() else {
        return Err(Rejection {
            code: -32602,
            message: "resources/read needs a uri".to_string(),
        });
    };
    let Some(name) = uri.strip_prefix(SCHEME) else {
        return Err(Rejection {
            code: -32602,
            message: format!("{uri:?} is not a borhan memory — expected {SCHEME}<name>"),
        });
    };

    let started = std::time::Instant::now();
    let outcome = match api::open(app, name) {
        Ok((opened, _)) => {
            let described = opened.store.lock().unwrap().describe();
            match described {
                Ok(memory) => match opened.index.vocabulary(VOCABULARY) {
                    Ok(words) => Ok((memory, words)),
                    Err(source) => Err(anyhow::Error::from(source)),
                },
                Err(source) => Err(anyhow::Error::from(source)),
            }
        }
        Err(source) => Err(anyhow::Error::from(source)),
    };
    let (memory, words) = match outcome {
        Ok(read) => read,
        Err(error) => {
            // `-32602`, not `-32002`: the resource-not-found code was renumbered
            // onto Invalid Params, and an unreadable memory is a bad `uri`
            // whichever way it failed.
            return Err(Rejection {
                code: -32602,
                message: format!("{error:#}"),
            });
        }
    };

    let created = match chrono::DateTime::from_timestamp_millis(memory.created_at) {
        Some(created) => created.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        None => memory.created_at.to_string(),
    };
    let mut vocabulary = Vec::new();
    for (word, units) in &words {
        vocabulary.push(json!({ "word": word, "units": units }));
    }
    tracing::info!(
        trace_id = %trace,
        memory = name,
        words = vocabulary.len(),
        total_ms = started.elapsed().as_millis() as u64,
        "read a memory resource",
    );

    let content = json!({
        "id": memory.id.to_string(),
        "name": memory.name,
        "description": memory.description,
        "languages": memory.languages,
        "created_at": created,
        "sessions": memory.sessions,
        "messages": memory.messages,
        "units": memory.units,
        "vocabulary_note": "The lemmas this memory uses most, with the number \
    of units each appears in, excluding the ones it uses everywhere. Search with \
    these words; a word that is not here and not a name will match nothing.",
        "vocabulary": vocabulary,
        "stats": { "trace": trace.to_string() },
    });
    Ok(json!({
        "contents": [{
            "uri": uri,
            "name": memory.name,
            "mimeType": "application/json",
            "text": content.to_string(),
        }],
    }))
}
