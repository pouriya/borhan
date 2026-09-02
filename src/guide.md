# borhan — HTTP API

You are reading `GET /` of a running borhan server. Everything below is
executable against this same host with `curl`. Nothing else needs to be
installed, and there is no client library.

borhan is a **keyword memory over stored conversations**, built for text that
mixes Persian and English in the same sentence. It stores messages, splits them
into paragraph-sized **units**, and searches those units by concept. It does not
summarize, does not answer questions, and does not embed anything: what comes
back is stored text and the address of where it sits.

Set the base URL once and paste the rest as-is. It is the address you reached
this page at, not one this server guessed about itself, so it already works from
wherever you are running `curl`:

    BORHAN={origin}

## The shape of what is stored

    memory      A named corpus, one subject each. Has a description saying
                what is in it and what is not — read that before you search
                it.
      session     One thread, channel or document set, with an ordinal per
                  message.
        message     One turn, one page, one file. Has an author, a role
                    (user | assistant | tool) and a timestamp.
          unit        A paragraph, heading, list item, table row or fenced
                      code block. Addressed by a ULID. **This is what search
                      scores and returns, and what you read back.**

A unit's ULID is called a `cursor` in every response that carries one. It is the
only address you ever need to keep.

## What a memory is for

A memory is a **subject**, not a source and not a bucket. A store usually holds
several, divided by what a thing is *about* rather than where it came from:

    company           How the organisation works: who owns which service, why
                      a policy exists, what was tried before you arrived.
    pouriya           One person: what they do, how they want to be worked
                      with, corrections that will still hold next month.
    project_borhan    One project: its constraints, its decisions and the
                      reasons behind them, its dead ends.
    rfcs              A document corpus, ingested rather than conversed.

Nothing in borhan knows those names; they are what a store tends to look like.
The consequence for a caller is that one conversation, one meeting or one
incident usually belongs in **more than one** memory, split by subject — and
that `memory_list` is not a formality, because a description is the only thing
that says which.

## The loop

Four calls, in this order. Skipping the second is the most common way to get a
result set that looks like answers and is not.

1. `GET  /api/v1/memory_list` — which memories exist, and what each holds.
2. `POST /api/v1/memory/{name}/lexicon` — do your words exist in this corpus?
3. `POST /api/v1/memory/{name}/search` — concept groups, not a sentence.
4. `POST /api/v1/memory/{name}/cursor` — read the hits back, as wide as needed.

## Authentication and headers

If the server was started with a token, every request under `/api/v1` and
`/mcp` needs it:

    curl -H "Authorization: Bearer $TOKEN" "$BORHAN/api/v1/memory_list"

Without it: `401` and `{"error":"missing or wrong token", ...}`. This page (`/`)
is the one thing served without a token — it is documentation, not data.

Every response carries `Server: borhan/<version>`, `X-Borhan-Version` and
`X-Trace-Id`. That trace id is also `stats.trace` in the body and the `trace_id`
on the server's log lines for that request; quote it when reporting a problem.

Every successful response is a JSON object with a `stats` member holding the
trace id and the milliseconds each phase took. It is diagnostic; ignore it
unless something is slow.

## Errors

    {"error": "human-readable sentence", "stats": {"trace": "01J8…", "total_ms": 0}}

| Status | Means |
|--------|-------|
| `400` | The request is malformed: a bad role, a string that is not a ULID, an empty list, a description under 10 words. The sentence says which. |
| `401` | Missing or wrong bearer token. The gate runs before routing, so on a server with a token an unknown path answers `401` rather than `404`. |
| `403` | The server is not permitted to do this. `permissions` in its `server.toml` governs `create`, `update`, `add`, `rescan` and `delete`; reads are never gated by it. Do not retry — retrying cannot change the answer. |
| `404` | No such memory, or no such route. |
| `409` | A name already taken, or a `message` id already used in that session. |
| `500` | Storage or index failure. One of these is fixable from here: *"the index … was built with normalization rules v2, this borhan is v3"* means the index is older than the binary — `POST /api/v1/memory/{name}/rescan` and try again. |

Read the sentence in `error` before deciding what to do; it is written to say
what to change.

---

# Reading

## `GET /api/v1/memory_list`

    curl -s "$BORHAN/api/v1/memory_list"

```json
{
  "memory_list": [
    {
      "id": "01M1BKGN46DJCQG5AC3NWA5Y37",
      "name": "rfcs",
      "description": "Rust RFCs … scanned by make seed as a local memory corpus",
      "languages": "en",
      "created_at": 1788169704000,
      "sessions": 1, "messages": 60, "units": 3087
    }
  ],
  "stats": { "trace": "01J8…", "total_ms": 3 }
}
```

`name` is what goes in every other URL. `description` says what the memory
holds and what it does not — it is the only thing that tells you whether a
question belongs here at all. `languages` is a hint from whoever created the
memory about which languages to expand a concept group into; it is not enforced.

## `POST /api/v1/memory/{name}/outline` — what is on file

    curl -s -X POST "$BORHAN/api/v1/memory/project_borhan/outline" \
      -H 'Content-Type: application/json' -d '{}'

```json
{"session_list": [
  {"id": "01M1H2…", "session": "2026-09-01-standup",
   "started_at": 1788172104000, "ended_at": null, "messages": 12, "units": 47}
]}
```

Pass `{"session": "2026-09-01-standup"}` for that session's messages instead —
their ids, ordinals, authors, lengths and unit counts, and no bodies, so the
answer stays readable when a session runs to a hundred messages.

This is the question search cannot answer. Search finds text; this asks **what
exists**. Before storing something under a session or message id, it is how you
find out whether that id is already taken and what is under it — a feeder about
to reuse a name has no keyword to search for, only the name. An unknown session
is a `404` rather than an empty list, because a session with no messages cannot
exist: the row is created by the first message added to it.

## `POST /api/v1/memory/{name}/lexicon`

Ask what a word looks like in this corpus **before** spending a search on it.

    curl -s -X POST "$BORHAN/api/v1/memory/rfcs/lexicon" \
      -H 'Content-Type: application/json' \
      -d '{"word_list": ["borrow", "borrowing", "radiograph", "خطا"]}'

```json
{
  "word_list": [
    {"word":"borrow","surface":"borrow","surface_units":14,
     "lemma":"borrow","lemma_units":47,"context_units":0},
    {"word":"borrowing","surface":"borrowing","surface_units":2,
     "lemma":"borrow","lemma_units":47,"context_units":0},
    {"word":"radiograph","surface":"radiograph","surface_units":0,
     "lemma":"radiograph","lemma_units":0,"context_units":0}
  ],
  "stats": {}
}
```

`surface_units` is that exact spelling. `lemma_units` is what the word folds to
— Snowball for English, affix stripping and letter folding for Persian — and
**it is the number that matters**, because it is what search ranks on.
`context_units` counts units the term was propagated into from a neighbour
rather than occurring in.

**`lemma_units` of 0 means this corpus has never seen the idea.** Searching for
it anyway returns *other things* rather than nothing, and afterwards there is no
way to tell those apart from an answer. When a word is 0, find the word the
corpus actually uses: if `radiograph` is 0, try `x-ray`; if `بریدگی` is 0, try
`زخم`.

## `POST /api/v1/memory/{name}/search`

    curl -s -X POST "$BORHAN/api/v1/memory/rfcs/search" \
      -H 'Content-Type: application/json' \
      -d '{
            "group_list": [
              {"label": "borrow",  "word_list": ["borrow","borrowed","borrowing"]},
              {"label": "mutable", "word_list": ["mutable","mutably","mut"]},
              {"label": "alias",   "word_list": ["alias","aliasing"], "required": true}
            ],
            "limit": 10
          }'

| Field | Default | Meaning |
|-------|---------|---------|
| `group_list` | required | One entry per **idea**. `label` names it, `word_list` is every spelling of it, `required: true` drops units that miss it instead of ranking them lower. |
| `limit` | `10` | Hits returned, at most. |
| `max_per_message` | `2` | Units returned from any one message. Twenty hits from one page is a wasted result set — use the cursor to read the rest of it. |
| `session` | — | Confine to one session, by its ULID. |
| `after` / `before` | — | Unix milliseconds, on the message timestamp. |
| `role_list` | — | Any of `user`, `assistant`, `tool`. |

```json
{
  "hit_list": [
    {
      "cursor": "01M1EVSPQ388GK59SQBM4A329M",
      "unit":   "01M1EVSPQ388GK59SQBM4A329M",
      "score": 1.0,
      "raw": 12.468404769897461,
      "coverage": [3, 3],
      "matched_list": ["borrow", "mutable", "alias"],
      "nearby_list": [],
      "session": "01M1BKGN5GWQA0AGWSJWQG6F1A", "session_ref": "rfcs",
      "message": "01M1BKGR8A2DSGVAWPE1H0GFY4", "message_ref": "0114-closures",
      "author": "rfcs", "role": "assistant", "ts": 1788169707786,
      "words": 56,
      "snippet": "This borrow does not permit aliasing (like `&mut`) but does\nnot require mutability (like `&`)."
    }
  ],
  "unknown_list": [],
  "hint_list": [ {"term": "violat", "units": 6}, {"term": "invalid", "units": 4} ],
  "stats": {}
}
```

### How to write the query

**A group is one idea spelled every way this corpus might spell it.** Words
inside a group are alternatives competing for a single slot, and only the best
of them scores — so a unit containing all four words of a group scores once, not
four times. One group must never hold two different ideas.

**Separate groups are separate things being asked about**, and how many of them
a unit matched — its `coverage` — is the largest term in the score. Three groups
of two words each ask a far better question than one group of six.

**Do not send a sentence.** Reduce the question to the two to four things that
have to co-occur, then expand each into its synonyms and translations. "Why does
the borrow checker reject this mutable alias" is three groups, as in the call
above — not one group of nine words.

### How to read the result

**`coverage` before `score`.** Coverage is `[groups matched, groups asked]` — a
fact. The score is BM25 squashed into `0..1` against the top hit *of this one
query*; it orders these hits and means nothing next to the score of a different
query. Prefer `[3,3]` at a middling score over `[1,3]` at a high one.

**`matched_list` with a `~` is a warning.** A bare label means the unit itself
contains one of that group's words, and you will find it in `snippet`. A `~`
prefix means the group was reached only through the *surrounding units of the
same message*: the concept is somewhere on that page but not on this line, and
quoting `snippet` for it would be wrong. It still counts toward coverage,
because it is still evidence — treat it as a pointer to read wider with the
cursor, never as an answer.

**`unknown_list` is the difference between two very different failures.** A word
listed there matched nothing in the corpus. "This memory disagrees with you" and
"this memory has never heard that word" look identical in a result set, and only
the second is a reason to search again with different wording.

**`hint_list` is the cheapest second query you will get.** These are frequent
terms shared across the top results that you did not ask for. It is how you
learn that the corpus says `x-ray` where you said `radiograph`. They come back
as **lemmas**, not as written — `bacterem`, `inappropri` — so read them as stems
and put the whole word in your next `word_list`.

**Two ids, two purposes.** `session` and `message` are ULIDs: they are what
another call accepts, and `session` is exactly what the `session` filter of the
next search wants. `session_ref` and `message_ref` are the feeder's own names —
a thread id, a page number, a filename — and are for reading, not for passing
back. `cursor` is spelled the same way in both endpoints. `message_ref` is
`null` when the feeder supplied no id.

**Do not raise `limit` to get more context.** Search for the gist, then expand
where it lands.

## `POST /api/v1/memory/{name}/cursor`

The only reader. Every width of read is this one call, so there is nothing to
choose between: widen the window rather than look for another endpoint.

    curl -s -X POST "$BORHAN/api/v1/memory/rfcs/cursor" \
      -H 'Content-Type: application/json' \
      -d '{"cursor_list": ["01M1EVSPQ388GK59SQBM4A329M"], "before": 1, "after": 1}'

| Field | Default | Meaning |
|-------|---------|---------|
| `cursor_list` | required | The `cursor` values from `hit_list`. **Pass every one you want in a single call** — the windows are merged and de-duplicated. |
| `before` / `after` | `2` | Units either side of each anchor. `0` and `0` returns only the anchor units themselves. |
| `messages` | `false` | Count the window in whole messages instead, and return each message's `body`. |

```json
{
  "message_list": [
    {
      "message": "01M1BKGR8A2DSGVAWPE1H0GFY4", "message_ref": "0114-closures",
      "session": "01M1BKGN5GWQA0AGWSJWQG6F1A", "session_ref": "rfcs",
      "seq": 33, "author": "rfcs", "role": "assistant", "ts": 1788169707786,
      "anchor": true,
      "unit_list": [
        {"unit":"01M1EVSPQ2…","unit_seq":66,"anchor":false,"text":"In the body of a `ref` closure, …"},
        {"unit":"01M1EVSPQ3…","unit_seq":67,"anchor":true, "text":"Note that there are some cases …"},
        {"unit":"01M1EVSPQ4…","unit_seq":68,"anchor":false,"text":"**Evolutionary note:** …"}
      ]
    }
  ],
  "missing_list": [],
  "stats": {}
}
```

Results are **grouped by message** — the fields saying where a unit sits are
identical for every unit of a message, and repeating them per unit is nine
tenths of the payload. `anchor` marks the units you asked for, and the messages
holding one. `unit_list` ids are what you pass back to move again, and `seq` and
`unit_seq` are the message's position in its session and the unit's in its
message. `session`, `session_ref`, `message` and `message_ref` mean exactly what
they mean in a search hit.

**The window counts units, not messages.** A unit is a thirtieth of a message in
a corpus fed from PDFs, so `"messages": true` to re-read one paragraph costs
eleven to forty times the context for text you did not ask for. Use it when the
paragraph does not say who was talking; otherwise widen `before` and `after`.

With `"messages": true` a message carries `body` — the whole text — instead of
`unit_list`, because those ids would buy nothing there.

A cursor this memory no longer holds comes back in `missing_list` rather than
failing the read, so a batch carried over from an older result set still returns
the ones that resolve. A string that is not a ULID at all is a `400`.

---

# Writing

These five are refused with `403` unless the server's `server.toml` permits
them. A read-only server is the normal deployment, and `replace` is off even on
a server that permits everything else — see the note under it.

## `POST /api/v1/memory` — create a memory

    curl -s -X POST "$BORHAN/api/v1/memory" \
      -H 'Content-Type: application/json' \
      -d '{"name": "project_borhan",
           "description": "The borhan project: decisions and the reasons behind them, constraints agreed in conversations that outlived them, and dead ends worth not repeating. Not the code, which git holds, and not the API, which is this page.",
           "languages": "fa,en"}'

`name` is 1–40 characters of `a-z`, `0-9` and `_`, and becomes a directory name.
`description` must be more than 10 words and at most 2000 characters — it is
shown to whoever picks a memory later, so write what is in it *and what is not*.
`languages` defaults to `fa,en`. Returns `{"id": "01J8…", "stats": {}}`.

## `POST /api/v1/memory/{name}/message_list` — add a message

    curl -s -X POST "$BORHAN/api/v1/memory/project_borhan/message_list" \
      -H 'Content-Type: application/json' \
      -d '{"session": "2026-09-01-standup",
           "message": "page-014",
           "role": "user",
           "author": "Sara",
           "ts": 1788172104000,
           "body": "# Heading\n\nA paragraph.\n\n- a list item"}'

`body` is read as **Markdown**: a paragraph, a heading, a list item, a table row
and a fenced code block each become one unit. `session` is required and is the
feeder's own identifier — the first message of a session creates it. `message`
is the feeder's own id, optional, and unique within the session, so a replay
that overlaps what is stored fails with `409` instead of duplicating. `role`
defaults to `user`, `author` to the role, `ts` to now — supply `ts` when
replaying a transcript rather than watching one.

Returns `{"id": "<message ULID>", "units": 3, "stats": {}}`.

## `POST /api/v1/memory/{name}/message` — rewrite a stored message

    curl -s -X POST "$BORHAN/api/v1/memory/project_borhan/message" \
      -H 'Content-Type: application/json' \
      -d '{"session": "2026-09-01-standup",
           "message": "page-014",
           "body": "# Heading\n\nThe paragraph, corrected."}'

For a message whose text has gone out of date — the thing it described changed,
so what is stored is now wrong, and adding a corrected copy beside it would only
mean a search returns both with nothing to tell them apart.

The message keeps its ordinal, so `cursor` windows spanning it stay correct, and
keeps its author and role: a replacement corrects what a message *says*, never
who said it. `ts` defaults to now rather than being carried over, because the
body is new text — a correction holding the old time would be ranked by recency
as though it were as old as the thing it corrects.

Both ids must already exist; this never creates either, and `404` says which one
is missing. **The old body is not kept anywhere.** Note the singular path:
`/message` rewrites one, `/message_list` adds one.

Returns `{"id": "<message ULID>", "units": 2, "reindexed": 12, "stats": {}}`.
`reindexed` is the whole session, which is the cost of this operation: a unit
carries a session term and no message term, so the narrowest thing the index can
be told to forget is every unit of the session, and it is written back in the
same commit.

`replace` is **not** in the default permission set, unlike `create`, `update`,
`add` and `rescan`. It is the second operation that destroys something with no
copy kept, and a `server.toml` written before it existed cannot have consented
to it.

## `PATCH /api/v1/memory/{name}` — change description or languages

    curl -s -X PATCH "$BORHAN/api/v1/memory/project_borhan" \
      -H 'Content-Type: application/json' \
      -d '{"description": "…more than ten words…"}'

At least one of `description`, `languages`. Returns `{"id": …}`.

## `POST /api/v1/memory/{name}/rescan` — rebuild the index

    curl -s -X POST "$BORHAN/api/v1/memory/project_borhan/rescan"

Splits every stored message again and rebuilds the index from scratch. Nothing
is lost: everything it destroys was derived from the messages, which are not
touched. Returns `{"messages": 60, "units": 3087, "stats": {}}`.

## `DELETE /api/v1/memory/{name}` — destroy a memory

    curl -s -X DELETE "$BORHAN/api/v1/memory/project_borhan"

**Irreversible, and there is nothing to fall back on.** `rescan` can rebuild an
index because an index is derived; nothing can rebuild the messages. Requires
the `delete` permission, which is the one permission a server without an
explicit list does *not* have. Returns the counts of what was destroyed, which
is the last record of it.

---

# Other endpoints

`GET /api/v1/health` → `{"ok": true, "stats": {}}`. Cheap; use it to tell a
server that is down from a route that is wrong.

`POST /mcp` speaks the Model Context Protocol (revision `2025-11-25`, back to
`2024-11-05`) behind the same token, over the same operations. If your client
can be configured with an MCP server, prefer it to this API: the tool schemas
carry the same guidance and you do not have to shape the JSON yourself.
