//! Search: one query string in, a short ranked list out.
//!
//! The caller — a language model, usually a weak one — writes a query:
//! `(error fault خطا) +(token jwt) -expired`. Parenthesised words are one idea
//! spelled several ways, `+` requires, `-` excludes, quotes make a phrase. The
//! parentheses are the only place cross-lingual unification happens, they exist
//! for the length of one query, and nothing about them is ever stored. `error`
//! and `خطا` have unrelated terms, unrelated document frequencies and unrelated
//! postings; what they share is a place in one clause, and whichever of them
//! scores higher for a given unit wins it.
//!
//! That has to be true. Synonymy is not transitive — `error ≈ fault`,
//! `fault ≈ عیب`, `عیب ≈ defect`, `defect ≈ نقص` — so taking its transitive
//! closure collapses half a vocabulary into one identifier, which is the
//! historical failure mode of thesaurus-based retrieval. Merging the two would
//! also destroy the fact that `error` occurs four hundred times here and `خطا`
//! six, and that difference is signal: it is what tells the caller this memory
//! is written in English.
//!
//! # What is tantivy's and what is not
//!
//! Tantivy parses the string, owns the postings, the term dictionary, the
//! fieldnorms and the BM25 arithmetic — for a phrase as well as for a word — and
//! none of that is reimplemented here. What the stock query tree cannot express
//! is the combination the design document specifies: **max within a clause**
//! (a paragraph that says the same thing three ways must not score triple),
//! **coverage across top-level clauses** as `(hits/total)^α`, **minimum-span
//! proximity** between positions belonging to *different* clauses, and the
//! **tiers** that make an exact spelling outrank a folded one and both outrank a
//! word borrowed from the rest of the message. So the parsed tree is compiled
//! twice: into a `BooleanQuery` that walks the segments, applies deletes,
//! enforces `+`, `-` and the filters and never scores, and into a tree of
//! tantivy weights that the collector below scores one unit at a time.
//!
//! Because the parser keeps no trace of outer parentheses — `(a b)` and `a b`
//! are the same tree — a query wrapped whole in one pair of parentheses is
//! recognised from the text and counted as one idea rather than several.

use std::collections::HashMap;
use std::ops::Bound;
use std::sync::Arc;

use tantivy::collector::{Collector, SegmentCollector};
use tantivy::columnar::Column;
use tantivy::postings::{Postings, SegmentPostings};
use tantivy::query::{
    BooleanQuery, EmptyQuery, EnableScoring, Occur, PhrasePrefixQuery, PhraseQuery, Query,
    RangeQuery, Scorer, TermQuery, Weight,
};
use tantivy::query_grammar::{self, Delimiter, UserInputAst, UserInputLeaf, UserInputLiteral};
use tantivy::schema::IndexRecordOption;
use tantivy::{DocAddress, DocId, DocSet, SegmentOrdinal, SegmentReader, Term};

use crate::index::{Fields, Index};
use crate::normalize;
use crate::storage::{Located, Role, Storage};
use crate::ulid::Ulid;

/// How hard coverage is weighted: the score is multiplied by
/// `(clauses_matched / clauses_total)^ALPHA`.
///
/// Coverage is the dominant relevance signal in this design — a paragraph that
/// touches all three of what you asked about should beat one that touches a
/// single clause by a wide margin, however emphatically it touches it. At 1.0
/// the factor is linear and a very high BM25 on one clause can still win;
/// above 1.0 it cannot, which is the intent.
const ALPHA: f32 = 1.5;

/// Largest multiplier a tight span between two clauses can earn.
///
/// `token` and `secret` three words apart is a far stronger signal than the two
/// sitting at opposite ends of a long paragraph, and in chat-sized paragraphs
/// this is a genuinely good precision signal. It is a bonus and not a gate: a
/// bonus can only reorder units that already matched.
const PROXIMITY: f32 = 0.5;

/// Half-life of the recency decay, in milliseconds. Ninety days.
///
/// Recent memories should surface more readily, all else equal. All else is
/// rarely equal, which is why there is a floor: without one, a perfect answer
/// from last year loses to a passing mention from this morning, and a memory
/// store that cannot recall anything old is not a memory store.
const HALF_LIFE: f32 = 90.0 * 24.0 * 60.0 * 60.0 * 1000.0;

/// Smallest multiplier the recency decay can reach, however old a unit is.
const FLOOR: f32 = 0.3;

/// Candidates pulled per requested hit, so that the diversity cap takes its
/// units out of the surplus rather than out of the page. Asking for exactly
/// `--limit` and then dropping half of them is how a full page becomes four
/// rows.
const OVERFETCH: usize = 4;

/// Terms returned as `vocab_hints`. Enough to redirect a second attempt, few
/// enough that the output stays terse.
const HINTS: usize = 6;

/// Units a term must appear in before it is offered as a hint. A term seen once
/// is a typo or a name, and neither helps a caller phrase a better query.
const HINT_FLOOR: u64 = 2;

/// Top-level clauses a query may score. Coverage is a bitmask per unit, one bit
/// per clause, and a query with more ideas than this is not a question anything
/// can cover.
const CLAUSES: usize = 32;

/// Shortest word, in letters, that a fuzzy search will look for near misses of.
///
/// One edit away from a four-letter Persian stem is a dozen unrelated words;
/// one edit away from `borow` is `borrow`.
const FUZZY_LETTERS: usize = 5;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(
        "the query is empty — pass `query`, one string of the words to find, \
         like (error fault) token"
    )]
    Empty,

    #[error(
        "the query {query:?} does not parse: {detail}. Quote a phrase with \"…\", \
         close every parenthesis you open, and put a backslash before any of \
         : ( ) [ ] {{ }} ^ \" ' that is part of a word"
    )]
    Syntax { query: String, detail: String },

    #[error(
        "`{field}:` is not something a query can name — the only fields are \
         surface: (exact spelling), lemma: (folded form) and context: (words \
         from the rest of the message)"
    )]
    Field { field: String },

    #[error(
        "`{field}:` is a filter, not a word — pass it as the search's session, \
         after/before or role filter instead of inside the query"
    )]
    Filter { field: String },

    #[error(
        "regular expressions (/…/) are not supported — write out the forms you \
         mean inside parentheses, like (rotate rotated rotation)"
    )]
    Regex,

    #[error(
        "ranges ([a TO b], >a, <=b) are not supported — for a time window, use \
         the search's after/before filter"
    )]
    Range,

    #[error(
        "`*` and `field:*` match every unit and rank none of them — search for \
         words"
    )]
    Everything,

    #[error(
        "{word}* is a one-word prefix, which is not supported — write out the \
         forms you mean inside parentheses, like (rotate rotated rotation), or \
         end a phrase of two or more words with *, like \"key rot\"*"
    )]
    Prefix { word: String },

    #[error(
        "every part of the query is excluded with - or NOT — say what to find, \
         then what to leave out, like token -expired"
    )]
    Unwanted,

    #[error(
        "the query has {count} top-level parts and at most 32 are scored — put \
         the words that mean the same thing inside one pair of parentheses"
    )]
    Clauses { count: usize },

    #[error("could not search the index")]
    Search {
        #[source]
        source: tantivy::TantivyError,
    },

    #[error(transparent)]
    Index(#[from] crate::index::Error),

    #[error(transparent)]
    Storage(#[from] crate::storage::Error),
}

/// How a query word reached a unit. Kept as an enum rather than folded into a
/// float so a result can say *why* it matched, and so a whole tier can be
/// turned off with one branch when measuring whether it earns its place.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Source {
    /// The word exists verbatim in the unit. The strongest evidence there is.
    Exact,
    /// Same lemma, a different written form — always the same language, because
    /// the normalizer is a pure function of the string and has no idea what any
    /// word means.
    Variant,
    /// One edit away from a word the memory has never seen, and only when the
    /// caller asked for that. A guess at a typo, so below every spelling the
    /// caller actually wrote.
    Fuzzy,
    /// Matched a term propagated from elsewhere in the message, not one the
    /// unit contains.
    Context,
}

impl Source {
    /// The weight tier. An expanded match should break ties, never win races.
    fn weight(&self) -> f32 {
        match self {
            Source::Exact => 1.0,
            Source::Variant => 0.9,
            Source::Fuzzy => 0.5,
            Source::Context => 0.35,
        }
    }
}

/// What narrows the search before anything is scored.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub session: Option<Ulid>,
    pub after: Option<i64>,
    pub before: Option<i64>,
    pub roles: Vec<Role>,
}

/// A word that resolved to nothing.
///
/// Returned to the caller rather than dropped. Telling a weak model that
/// `fault` matched nothing while `error` matched four hundred units is what
/// turns a blind guess into an informed second attempt.
#[derive(Debug, Clone)]
pub struct Unknown {
    /// The top-level clause the word was written in, as [`Hit::matched`]
    /// labels it.
    pub clause: String,
    pub word: String,
}

/// A word the memory has never seen that a fuzzy search matched to words it
/// has.
///
/// Reported rather than silently used, because the useful thing is not this
/// result set but the next query: the caller learns the spelling the corpus
/// actually uses and can stop relying on a guess.
#[derive(Debug, Clone)]
pub struct Fuzzy {
    pub word: String,
    /// The lemmas it was taken to mean.
    pub matched: Vec<String>,
}

/// One result.
#[derive(Debug, Clone)]
pub struct Hit {
    pub unit: Ulid,
    /// Opaque handle for the cursor tool. It is the unit's ULID today; callers
    /// are told only that it is a string to hand back.
    pub cursor: String,
    pub snippet: String,
    /// Squashed to 0..1 against the top hit, for ordering and nothing else.
    pub score: f32,
    /// The weighted BM25 sum before any of the multipliers, for debugging a
    /// ranking.
    pub raw: f32,
    /// Clauses matched over clauses asked for. Unlike the score this is a fact
    /// the caller can reason about correctly, and it is what the tool
    /// description tells a model to read. It counts [`Hit::nearby`] too, since
    /// that is what the score counted.
    pub coverage: (u8, u8),
    /// The top-level clauses whose words are written in this unit, spelled as
    /// the query wrote them.
    pub matched: Vec<String>,
    /// The top-level clauses that hit only through the context field — present
    /// somewhere else in the same message, absent from this unit.
    ///
    /// Kept apart from [`Hit::matched`] rather than merged into it because the
    /// two are different claims and only one of them can be quoted. A caller
    /// that reads `matched` and finds a clause there is entitled to expect the
    /// word in the snippet; one that finds it in `nearby` has been told where
    /// to look next, which is the cursor.
    pub nearby: Vec<String>,
    /// The session's ULID, spelled the way `cursor` spells it and the way the
    /// `session` filter of the next search wants it. `session_ref` beside it is
    /// the feeder's own name for the same thing — a thread id, a filename —
    /// which is what a person reads and what nothing accepts as input.
    ///
    /// Both are here, under the same names `cursor` uses, because a hit is the
    /// input to the next call and a caller that has to work out which of two
    /// spellings a field holds will eventually work it out wrong.
    pub session: Ulid,
    pub session_ref: String,
    pub message: Ulid,
    pub message_ref: Option<String>,
    pub author: String,
    pub role: Role,
    pub ts: i64,
    pub words: usize,
}

/// Everything one search returns.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub hits: Vec<Hit>,
    pub unknown: Vec<Unknown>,
    pub fuzzy: Vec<Fuzzy>,
    /// High-IDF terms that co-occur with the top results and were not asked
    /// for. The cheapest quality improvement available: it turns one shot in
    /// the dark into two informed ones.
    pub hints: Vec<(String, u64)>,
}

/// One way a piece of the query can match a unit: a query on one field, at one
/// tier.
struct Probe {
    /// What tantivy scores this piece with — BM25 over one term, or over a whole
    /// phrase — before the tier weight is applied.
    weight: Box<dyn Weight>,
    source: Source,
    /// Terms whose positions feed proximity. Empty on the context field, which
    /// records none, and never the prefix end of a phrase, which is not a term.
    positions: Vec<Term>,
}

/// A piece of the query, shaped the way it is scored.
enum Node {
    /// A word, phrase or set member, on every field it resolved in. The best
    /// probe wins.
    Leaf(Vec<Probe>),
    /// Parts joined by `+`, `AND`, `OR` or plain spaces. Excluded parts are not
    /// kept: they only ever remove units, and the driver query does that.
    Clause(Vec<(Occur, Node)>),
    Boost(Box<Node>, f32),
}

/// One top-level part of the query: what coverage counts.
struct Concept {
    label: String,
    node: Node,
    /// Every surface and lemma the part's words resolved to, for choosing the
    /// snippet sentence and for keeping asked-for words out of the hints.
    texts: Vec<String>,
}

/// What compiling the query carries from one piece to the next.
struct Compiling<'a> {
    searcher: &'a tantivy::Searcher,
    fields: Fields,
    fuzzy: bool,
    /// The label of the top-level part being compiled, for [`Unknown::clause`].
    clause: String,
    unknown: Vec<Unknown>,
    typos: Vec<Fuzzy>,
    texts: Vec<String>,
    /// Probes compiled outside any exclusion, in the current top-level part. A
    /// part with none cannot match anything and is not counted in coverage.
    probes: usize,
}

/// Run a search.
pub fn search(
    storage: &Storage,
    index: &Index,
    request: (&str, bool),
    filter: &Filter,
    limit: usize,
    per_message: usize,
) -> Result<Outcome, Error> {
    let (query, fuzzy) = request;
    let text = query.trim();
    if text.is_empty() {
        return Err(Error::Empty);
    }
    let ast = match query_grammar::parse_query(text) {
        Ok(ast) => ast,
        Err(_) => {
            // The strict parser says only that it failed. The lenient one, run
            // over the same text, says where and why, and that sentence is what
            // lets a model fix its query instead of rewriting it from scratch.
            let (_, errors) = query_grammar::parse_query_lenient(text);
            let mut detail = Vec::new();
            for error in &errors {
                let mut at = error.pos;
                if at <= text.len() && text.is_char_boundary(at) {
                    at = text[..at].chars().count();
                }
                detail.push(format!("{} at character {at}", error.message));
            }
            if detail.is_empty() {
                detail.push("it is not a query this search can read".to_string());
            }
            return Err(Error::Syntax {
                query: text.to_string(),
                detail: detail.join("; "),
            });
        }
    };

    // Whether one pair of parentheses holds the whole query, `+` or a trailing
    // boost aside. The parser drops outer parentheses, so `(error fault)` and
    // `error fault` arrive as the same tree, and only the text still says that
    // the first is one idea and the second two.
    let bare = match text.strip_prefix('+') {
        Some(rest) => rest.trim_start(),
        None => text,
    };
    let mut whole = false;
    if bare.starts_with('(') {
        let mut depth = 0usize;
        let mut quote: Option<char> = None;
        let mut escaped = false;
        let mut close = None;
        for (at, character) in bare.char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            if character == '\\' {
                escaped = true;
                continue;
            }
            if let Some(open) = quote {
                if character == open {
                    quote = None;
                }
                continue;
            }
            match character {
                '"' | '\'' => quote = Some(character),
                '(' => depth += 1,
                ')' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        close = Some(at);
                        break;
                    }
                }
                _ => {}
            }
        }
        if let Some(close) = close {
            let rest = bare[close + 1..].trim();
            whole = match rest.strip_prefix('^') {
                Some(boost) => boost.parse::<f64>().is_ok(),
                None => rest.is_empty(),
            };
        }
    }
    let mut parts: Vec<(Occur, &UserInputAst)> = Vec::new();
    match &ast {
        UserInputAst::Clause(children) if !whole => {
            for (occur, child) in children {
                let occur = match occur {
                    Some(occur) => *occur,
                    None => Occur::Should,
                };
                parts.push((occur, child));
            }
        }
        _ => parts.push((Occur::Should, &ast)),
    }

    let searcher = index.reader.searcher();
    let fields = index.fields;
    let mut state = Compiling {
        searcher: &searcher,
        fields,
        fuzzy,
        clause: String::new(),
        unknown: Vec::new(),
        typos: Vec::new(),
        texts: Vec::new(),
        probes: 0,
    };
    let mut top: Vec<(Occur, Box<dyn Query>)> = Vec::new();
    let mut concepts: Vec<Concept> = Vec::new();
    let mut wanted = false;
    for (occur, part) in parts {
        let excluded = occur == Occur::MustNot;
        state.clause = render(part);
        state.texts = Vec::new();
        state.probes = 0;
        let (query, node) = compile(part, excluded, &mut state)?;
        top.push((occur, query));
        if excluded {
            continue;
        }
        wanted = true;
        // A part none of whose words exist cannot be satisfied by any unit, so
        // it is left out of coverage — its words are in `unknown` instead. If
        // it was required, its empty query in the driver already means there
        // are no hits.
        if state.probes == 0 {
            continue;
        }
        concepts.push(Concept {
            label: state.clause.clone(),
            node,
            texts: std::mem::take(&mut state.texts),
        });
    }
    if !wanted {
        return Err(Error::Unwanted);
    }
    if concepts.len() > CLAUSES {
        return Err(Error::Clauses {
            count: concepts.len(),
        });
    }
    let unknown = state.unknown;
    let typos = state.typos;
    if concepts.is_empty() {
        return Ok(Outcome {
            hits: Vec::new(),
            unknown,
            fuzzy: typos,
            hints: Vec::new(),
        });
    }

    // The driver. Its scores are discarded — the collector recomputes
    // everything from the weights — but it is what walks the segments, applies
    // the deletes and enforces `+`, `-` and the filters, none of which is worth
    // reimplementing. The text half is required as a whole, so a filter can
    // never turn every unit of a session into a candidate.
    let mut clauses: Vec<(Occur, Box<dyn Query>)> =
        vec![(Occur::Must, Box::new(BooleanQuery::new(top)))];
    if let Some(session) = filter.session {
        clauses.push((
            Occur::Must,
            Box::new(TermQuery::new(
                Term::from_field_text(fields.session, &session.to_string()),
                IndexRecordOption::Basic,
            )),
        ));
    }
    if filter.after.is_some() || filter.before.is_some() {
        let lower = match filter.after {
            Some(after) => Bound::Included(Term::from_field_i64(fields.ts, after)),
            None => Bound::Unbounded,
        };
        let upper = match filter.before {
            Some(before) => Bound::Included(Term::from_field_i64(fields.ts, before)),
            None => Bound::Unbounded,
        };
        clauses.push((Occur::Must, Box::new(RangeQuery::new(lower, upper))));
    }
    if !filter.roles.is_empty() {
        let mut roles: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for role in &filter.roles {
            roles.push((
                Occur::Should,
                Box::new(TermQuery::new(
                    Term::from_field_u64(fields.role, role.code()),
                    IndexRecordOption::Basic,
                )),
            ));
        }
        clauses.push((Occur::Must, Box::new(BooleanQuery::new(roles))));
    }
    let query = BooleanQuery::new(clauses);

    let wanted = limit * OVERFETCH;
    let ranking = Ranking {
        concepts: Arc::new(concepts),
        now: Ulid::now(),
        limit: wanted,
    };
    let candidates = match searcher.search(&query, &ranking) {
        Ok(candidates) => candidates,
        Err(source) => return Err(Error::Search { source }),
    };

    // Diversity. Twenty hits from one message is a wasted result set, and the
    // caller has a cursor tool for reading around a hit anyway.
    let mut ranked = Vec::new();
    for candidate in &candidates {
        if let Some(unit) = index.unit(candidate.address)? {
            ranked.push((*candidate, unit));
        }
    }
    let units: Vec<Ulid> = ranked.iter().map(|(_, unit)| *unit).collect();
    let located = storage.locate(&units)?;
    let mut by_unit: HashMap<Ulid, &Located> = HashMap::new();
    for row in &located {
        by_unit.insert(row.unit, row);
    }

    let top = ranked.first().map(|(first, _)| first.score).unwrap_or(1.0);
    let mut seen: HashMap<Ulid, usize> = HashMap::new();
    let mut hits = Vec::new();
    for (candidate, unit) in &ranked {
        let Some(row) = by_unit.get(unit) else {
            continue;
        };
        let count = seen.entry(row.message).or_insert(0);
        if *count >= per_message {
            continue;
        }
        *count += 1;

        let mut matched = Vec::new();
        let mut nearby = Vec::new();
        for (at, concept) in ranking.concepts.iter().enumerate() {
            if candidate.mask & (1 << at) == 0 {
                continue;
            }
            if candidate.direct & (1 << at) != 0 {
                matched.push(concept.label.clone());
            } else {
                nearby.push(concept.label.clone());
            }
        }

        hits.push(Hit {
            unit: row.unit,
            cursor: row.unit.to_string(),
            snippet: snippet(storage, row, &ranking.concepts)?,
            score: if top > 0.0 {
                candidate.score / top
            } else {
                0.0
            },
            raw: candidate.raw,
            coverage: (candidate.hits, ranking.concepts.len() as u8),
            matched,
            nearby,
            session: row.session,
            session_ref: row.session_ref.clone(),
            message: row.message,
            message_ref: row.message_ref.clone(),
            author: row.author.clone(),
            role: row.role,
            ts: row.ts,
            words: row.text().split_whitespace().count(),
        });
        if hits.len() >= limit {
            break;
        }
    }

    let hints = vocabulary(&searcher, index, &hits, &by_unit, &ranking.concepts);
    Ok(Outcome {
        hits,
        unknown,
        fuzzy: typos,
        hints,
    })
}

/// A piece of the parsed query as the caller could have written it, for labels.
///
/// The parser keeps no source text, so this is rebuilt from the tree: `error OR
/// fault` and `(error fault)` both come back as `(error OR fault)`, and `a AND
/// b` as `(+a +b)`.
fn render(ast: &UserInputAst) -> String {
    match ast {
        UserInputAst::Clause(children) => {
            let mut plain = true;
            let mut parts = Vec::new();
            for (occur, child) in children {
                let sign = match occur {
                    Some(Occur::Must) => "+",
                    Some(Occur::MustNot) => "-",
                    _ => "",
                };
                if !sign.is_empty() {
                    plain = false;
                }
                parts.push(format!("{sign}{}", render(child)));
            }
            if plain {
                format!("({})", parts.join(" OR "))
            } else {
                format!("({})", parts.join(" "))
            }
        }
        UserInputAst::Boost(inner, boost) => format!("{}^{}", render(inner), boost.0),
        UserInputAst::Leaf(leaf) => match leaf.as_ref() {
            UserInputLeaf::Literal(literal) => {
                let mut text = String::new();
                if let Some(field) = &literal.field_name {
                    text.push_str(field);
                    text.push(':');
                }
                match literal.delimiter {
                    Delimiter::None => text.push_str(&literal.phrase),
                    Delimiter::DoubleQuotes => text.push_str(&format!("\"{}\"", literal.phrase)),
                    Delimiter::SingleQuotes => text.push_str(&format!("'{}'", literal.phrase)),
                }
                if literal.slop > 0 {
                    text.push_str(&format!("~{}", literal.slop));
                } else if literal.prefix {
                    text.push('*');
                }
                text
            }
            UserInputLeaf::Set { field, elements } => match field {
                Some(field) => format!("{field}: IN [{}]", elements.join(" ")),
                None => format!("IN [{}]", elements.join(" ")),
            },
            other => format!("{other:?}"),
        },
    }
}

/// Turn one piece of the parsed query into the query that finds candidates and
/// the node that scores them.
///
/// `negated` is true under `-` or `NOT`. Such a piece still has to exclude, so
/// it still gets a query, but its words are not what the caller is looking for:
/// they are not reported as unknown, not expanded by fuzzy matching and not
/// counted.
fn compile(
    ast: &UserInputAst,
    negated: bool,
    state: &mut Compiling,
) -> Result<(Box<dyn Query>, Node), Error> {
    let literal = match ast {
        UserInputAst::Clause(children) => {
            let mut queries: Vec<(Occur, Box<dyn Query>)> = Vec::new();
            let mut nodes = Vec::new();
            for (occur, child) in children {
                let occur = match occur {
                    Some(occur) => *occur,
                    None => Occur::Should,
                };
                let excluded = occur == Occur::MustNot;
                let (query, node) = compile(child, negated || excluded, state)?;
                queries.push((occur, query));
                if !excluded {
                    nodes.push((occur, node));
                }
            }
            return Ok((Box::new(BooleanQuery::new(queries)), Node::Clause(nodes)));
        }
        UserInputAst::Boost(inner, boost) => {
            let (query, node) = compile(inner, negated, state)?;
            return Ok((query, Node::Boost(Box::new(node), boost.0 as f32)));
        }
        UserInputAst::Leaf(leaf) => match leaf.as_ref() {
            UserInputLeaf::Literal(literal) => literal,
            UserInputLeaf::Regex { .. } => return Err(Error::Regex),
            UserInputLeaf::Range { .. } => return Err(Error::Range),
            UserInputLeaf::All | UserInputLeaf::Exists { .. } => return Err(Error::Everything),
            // `IN [a b c]` is `(a OR b OR c)` with the field applied to each,
            // and is compiled as exactly that.
            UserInputLeaf::Set { field, elements } => {
                let mut children = Vec::new();
                for element in elements {
                    let member = UserInputLiteral {
                        field_name: field.clone(),
                        phrase: element.clone(),
                        delimiter: Delimiter::None,
                        slop: 0,
                        prefix: false,
                    };
                    children.push((
                        Some(Occur::Should),
                        UserInputAst::from(UserInputLeaf::Literal(member)),
                    ));
                }
                return compile(&UserInputAst::Clause(children), negated, state);
            }
        },
    };

    let all = state.fields;
    let targets = match literal.field_name.as_deref() {
        None => vec![
            (all.surface, Source::Exact),
            (all.lemma, Source::Variant),
            (all.context, Source::Context),
        ],
        Some(normalize::SURFACE) => vec![(all.surface, Source::Exact)],
        Some(normalize::LEMMA) => vec![(all.lemma, Source::Variant)],
        Some("context") => vec![(all.context, Source::Context)],
        Some(field @ ("session" | "ts" | "role")) => {
            return Err(Error::Filter {
                field: field.to_string(),
            });
        }
        Some(field) => {
            return Err(Error::Field {
                field: field.to_string(),
            });
        }
    };

    // Cut exactly the way both analyzers cut stored text, positions included,
    // so a phrase here lines up token for token with the one in the index.
    let mut words: Vec<(usize, String, String, String)> = Vec::new();
    for (position, word) in normalize::segment(&literal.phrase).into_iter().enumerate() {
        words.push((
            position,
            word.text.to_string(),
            normalize::surface(word.text),
            normalize::lemma(word.text, word.script),
        ));
    }
    // `rot*` never reaches the parser's prefix flag: `*` is a word character
    // to it, and the segmenter then drops the `*` and searches for `rot` — a
    // silent answer to a question nobody asked. It is refused like the flagged
    // form instead.
    let starred = literal.delimiter == Delimiter::None && literal.phrase.ends_with('*');
    if starred || (literal.prefix && words.len() < 2) {
        return Err(Error::Prefix {
            word: literal.phrase.trim_end_matches('*').to_string(),
        });
    }

    let mut found = vec![false; words.len()];
    let mut queries: Vec<(Occur, Box<dyn Query>)> = Vec::new();
    let mut probes = Vec::new();
    for (field, source) in targets {
        let mut terms: Vec<(usize, Term)> = Vec::new();
        let mut absent = false;
        for (at, (position, _, surface, lemma)) in words.iter().enumerate() {
            let text = if field == all.surface { surface } else { lemma };
            if text.is_empty() {
                continue;
            }
            let term = Term::from_field_text(field, text);
            // The end of a prefix phrase is a fragment, not a term: it has no
            // frequency of its own and is not a word to call unknown.
            if !(literal.prefix && at + 1 == words.len()) {
                let frequency = match state.searcher.doc_freq(&term) {
                    Ok(frequency) => frequency,
                    Err(source) => return Err(Error::Search { source }),
                };
                if frequency == 0 {
                    absent = true;
                } else {
                    found[at] = true;
                }
            }
            terms.push((*position, term));
        }
        // A phrase with a word missing from this field cannot occur in it, and
        // neither can a phrase on the context field, which records no positions.
        if terms.is_empty() || absent || (terms.len() > 1 && field == all.context) {
            continue;
        }
        let mut positions = Vec::new();
        let query: Box<dyn Query> = if terms.len() == 1 {
            if literal.prefix {
                continue;
            }
            if field != all.context {
                positions.push(terms[0].1.clone());
            }
            Box::new(TermQuery::new(
                terms[0].1.clone(),
                IndexRecordOption::WithFreqs,
            ))
        } else if literal.prefix {
            for (_, term) in &terms[..terms.len() - 1] {
                positions.push(term.clone());
            }
            Box::new(PhrasePrefixQuery::new_with_offset(terms))
        } else {
            for (_, term) in &terms {
                positions.push(term.clone());
            }
            Box::new(PhraseQuery::new_with_offset_and_slop(terms, literal.slop))
        };
        let weight = match query.weight(EnableScoring::enabled_from_searcher(state.searcher)) {
            Ok(weight) => weight,
            Err(source) => return Err(Error::Search { source }),
        };
        queries.push((Occur::Should, query));
        probes.push(Probe {
            weight,
            source,
            positions,
        });
    }

    if !negated {
        let lemmas = matches!(literal.field_name.as_deref(), None | Some(normalize::LEMMA));
        for (at, (_, text, surface, lemma)) in words.iter().enumerate() {
            for held in [surface, lemma] {
                if !held.is_empty() && !state.texts.contains(held) {
                    state.texts.push(held.clone());
                }
            }
            if found[at] || (literal.prefix && at + 1 == words.len()) {
                continue;
            }

            // A word nothing in the memory spells. With fuzzy matching asked
            // for, and a word long enough that one edit is still a typo rather
            // than a different word, look for the lemmas one edit away: a
            // letter changed, added or dropped, or two neighbours swapped. The
            // dictionary is walked by hand rather than through a Levenshtein
            // automaton because the match has to come back as the terms it hit
            // — they are scored one by one and reported — and tantivy's fuzzy
            // query scores every expansion the same and names none of them.
            let mut near: Vec<String> = Vec::new();
            if state.fuzzy
                && lemmas
                && words.len() == 1
                && !literal.prefix
                && !lemma.is_empty()
                && text.chars().count() >= FUZZY_LETTERS
            {
                let wanted: Vec<char> = lemma.chars().collect();
                for reader in state.searcher.segment_readers() {
                    let inverted = match reader.inverted_index(all.lemma) {
                        Ok(inverted) => inverted,
                        Err(source) => return Err(Error::Search { source }),
                    };
                    let mut stream = match inverted.terms().stream() {
                        Ok(stream) => stream,
                        Err(source) => {
                            return Err(Error::Search {
                                source: tantivy::TantivyError::from(source),
                            });
                        }
                    };
                    while stream.advance() {
                        let Ok(candidate) = std::str::from_utf8(stream.key()) else {
                            continue;
                        };
                        // Four bytes is two Persian letters, the most one edit
                        // can change a length by in any script this sees.
                        if candidate.len().abs_diff(lemma.len()) > 4
                            || near.iter().any(|held| held == candidate)
                        {
                            continue;
                        }
                        let other: Vec<char> = candidate.chars().collect();
                        let close = if wanted.len() == other.len() {
                            let mut differ = Vec::new();
                            for (index, letter) in wanted.iter().enumerate() {
                                if *letter != other[index] {
                                    differ.push(index);
                                }
                            }
                            differ.len() == 1
                                || (differ.len() == 2
                                    && differ[1] == differ[0] + 1
                                    && wanted[differ[0]] == other[differ[1]]
                                    && wanted[differ[1]] == other[differ[0]])
                        } else if wanted.len().abs_diff(other.len()) == 1 {
                            let (short, long) = if wanted.len() < other.len() {
                                (&wanted, &other)
                            } else {
                                (&other, &wanted)
                            };
                            let mut index = 0;
                            while index < short.len() && short[index] == long[index] {
                                index += 1;
                            }
                            short[index..] == long[index + 1..]
                        } else {
                            false
                        };
                        if close {
                            near.push(candidate.to_string());
                        }
                    }
                }
                near.sort();
            }
            for candidate in &near {
                let term = Term::from_field_text(all.lemma, candidate);
                let query: Box<dyn Query> =
                    Box::new(TermQuery::new(term.clone(), IndexRecordOption::WithFreqs));
                let weight =
                    match query.weight(EnableScoring::enabled_from_searcher(state.searcher)) {
                        Ok(weight) => weight,
                        Err(source) => return Err(Error::Search { source }),
                    };
                queries.push((Occur::Should, query));
                probes.push(Probe {
                    weight,
                    source: Source::Fuzzy,
                    positions: vec![term],
                });
                if !state.texts.contains(candidate) {
                    state.texts.push(candidate.clone());
                }
            }
            if near.is_empty() {
                state.unknown.push(Unknown {
                    clause: state.clause.clone(),
                    word: text.clone(),
                });
            } else {
                state.typos.push(Fuzzy {
                    word: text.clone(),
                    matched: near,
                });
            }
        }
        state.probes += probes.len();
    }

    let query: Box<dyn Query> = if queries.is_empty() {
        Box::new(EmptyQuery)
    } else {
        Box::new(BooleanQuery::new(queries))
    };
    Ok((query, Node::Leaf(probes)))
}

/// The best sentence of a unit, or the whole unit when it is short.
///
/// Sliced out of `message.body` with the offsets stored at scan time. The best
/// sentence is the one holding the most distinct query clauses — the same
/// coverage idea as the ranking, applied inside one paragraph.
fn snippet(storage: &Storage, row: &Located, concepts: &[Concept]) -> Result<String, Error> {
    let sentences = storage.sentences(row.unit)?;
    if sentences.len() < 2 {
        return Ok(row.text().to_string());
    }

    let mut best = (0usize, row.start, row.end);
    for (start, end) in sentences {
        let text = &row.body[start..end];
        let mut hits = 0;
        for concept in concepts {
            let mut found = false;
            for word in normalize::segment(text) {
                let surface = normalize::surface(word.text);
                let lemma = normalize::lemma(word.text, word.script);
                if concept
                    .texts
                    .iter()
                    .any(|held| *held == surface || *held == lemma)
                {
                    found = true;
                    break;
                }
            }
            if found {
                hits += 1;
            }
        }
        if hits > best.0 {
            best = (hits, start, end);
        }
    }
    Ok(row.body[best.1..best.2].to_string())
}

/// The highest-IDF terms across the top results that the caller did not ask
/// for.
fn vocabulary(
    searcher: &tantivy::Searcher,
    index: &Index,
    hits: &[Hit],
    located: &HashMap<Ulid, &Located>,
    concepts: &[Concept],
) -> Vec<(String, u64)> {
    let mut frequencies: HashMap<String, u64> = HashMap::new();
    for hit in hits {
        let Some(row) = located.get(&hit.unit) else {
            continue;
        };
        for word in normalize::segment(row.text()) {
            let lemma = normalize::lemma(word.text, word.script);
            if lemma.is_empty() || frequencies.contains_key(&lemma) {
                continue;
            }
            let mut asked = false;
            for concept in concepts {
                if concept.texts.contains(&lemma) {
                    asked = true;
                    break;
                }
            }
            if asked {
                continue;
            }
            let term = Term::from_field_text(index.fields.lemma, &lemma);
            let frequency = searcher.doc_freq(&term).unwrap_or(0);
            if frequency < HINT_FLOOR {
                continue;
            }
            frequencies.insert(lemma, frequency);
        }
    }

    let mut hints: Vec<(String, u64)> = frequencies.into_iter().collect();
    hints.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    hints.truncate(HINTS);
    hints
}

/// One unit that survived scoring.
#[derive(Debug, Clone, Copy)]
struct Candidate {
    address: DocAddress,
    score: f32,
    raw: f32,
    hits: u8,
    /// One bit per top-level clause, so coverage is a `count_ones` and the
    /// matched labels are readable afterwards.
    mask: u32,
    /// The subset of `mask` whose words are actually written in the unit.
    ///
    /// The difference between the two is what a caller cannot otherwise see: a
    /// clause can be satisfied entirely by the context field, which holds terms
    /// propagated from the neighbouring units of the same message, and a unit
    /// reached that way does not contain the word. Reporting it under the same
    /// heading as a real match is how a reader ends up quoting an acuity level
    /// off a line that never mentioned one.
    direct: u32,
}

/// The collector that does the actual scoring.
struct Ranking {
    concepts: Arc<Vec<Concept>>,
    now: i64,
    limit: usize,
}

impl Collector for Ranking {
    type Fruit = Vec<Candidate>;
    type Child = Scoring;

    fn for_segment(
        &self,
        ordinal: SegmentOrdinal,
        reader: &SegmentReader,
    ) -> tantivy::Result<Scoring> {
        let mut concepts = Vec::with_capacity(self.concepts.len());
        for concept in self.concepts.iter() {
            concepts.push(open(&concept.node, reader)?);
        }

        Ok(Scoring {
            ordinal,
            concepts,
            ts: reader.fast_fields().i64(crate::index::TS)?,
            now: self.now,
            limit: self.limit,
            best: Vec::with_capacity(self.limit * 2),
            buffer: Vec::new(),
        })
    }

    /// False: the driver query's score is never read, and saying so lets
    /// tantivy iterate without computing one.
    fn requires_scoring(&self) -> bool {
        false
    }

    fn merge_fruits(&self, fruits: Vec<Vec<Candidate>>) -> tantivy::Result<Vec<Candidate>> {
        let mut merged = Vec::new();
        for fruit in fruits {
            merged.extend(fruit);
        }
        merged.sort_by(|a, b| b.score.total_cmp(&a.score));
        merged.truncate(self.limit);
        Ok(merged)
    }
}

/// A [`Node`] opened on one segment: every probe's scorer, and the postings its
/// positions are read from.
enum Live {
    Leaf(Vec<Reading>),
    Clause(Vec<(Occur, Live)>),
    Boost(Box<Live>, f32),
}

/// One [`Probe`] opened on one segment.
struct Reading {
    scorer: Box<dyn Scorer>,
    source: Source,
    positions: Vec<SegmentPostings>,
}

/// Open a node's weights and position postings on one segment.
fn open(node: &Node, reader: &SegmentReader) -> tantivy::Result<Live> {
    match node {
        Node::Leaf(probes) => {
            let mut readings = Vec::with_capacity(probes.len());
            for probe in probes {
                let mut positions = Vec::new();
                for term in &probe.positions {
                    let inverted = reader.inverted_index(term.field())?;
                    if let Some(postings) =
                        inverted.read_postings(term, IndexRecordOption::WithFreqsAndPositions)?
                    {
                        positions.push(postings);
                    }
                }
                readings.push(Reading {
                    scorer: probe.weight.scorer(reader, 1.0)?,
                    source: probe.source,
                    positions,
                });
            }
            Ok(Live::Leaf(readings))
        }
        Node::Clause(children) => {
            let mut opened = Vec::with_capacity(children.len());
            for (occur, child) in children {
                opened.push((*occur, open(child, reader)?));
            }
            Ok(Live::Clause(opened))
        }
        Node::Boost(inner, boost) => Ok(Live::Boost(Box::new(open(inner, reader)?), *boost)),
    }
}

/// Score one unit against one piece of the query.
///
/// A leaf is its best probe: a paragraph containing `error`, `fault` *and*
/// `خطا` describes one idea three ways and must not score triple, and the same
/// holds for a word matching both its exact and its folded form. A clause is
/// the sum of what it requires plus the best of what it merely allows — `+a +b`
/// really did match twice, `a OR b` did not. A boost multiplies.
///
/// Returns the score and whether any probe that matched is written in the unit
/// itself rather than reached through context. Positions of every matching
/// probe are appended to `positions`, for proximity.
fn evaluate(
    live: &mut Live,
    doc: DocId,
    buffer: &mut Vec<u32>,
    positions: &mut Vec<u32>,
) -> (f32, bool) {
    match live {
        Live::Leaf(readings) => {
            let mut best = 0.0f32;
            let mut written = false;
            for reading in readings.iter_mut() {
                // Documents arrive in increasing order within a segment, so
                // every scorer only ever moves forward.
                if reading.scorer.doc() < doc {
                    reading.scorer.seek(doc);
                }
                if reading.scorer.doc() != doc {
                    continue;
                }
                let score = reading.scorer.score() * reading.source.weight();
                if score > best {
                    best = score;
                }
                // Independent of which probe scored highest: one exact or
                // variant match on this unit is enough to say the word is here,
                // even when a context match happened to outscore it.
                if reading.source != Source::Context {
                    written = true;
                }
                for postings in reading.positions.iter_mut() {
                    if postings.doc() < doc {
                        postings.seek(doc);
                    }
                    if postings.doc() != doc {
                        continue;
                    }
                    buffer.clear();
                    postings.positions(buffer);
                    positions.extend_from_slice(buffer);
                }
            }
            (best, written)
        }
        Live::Clause(children) => {
            let mut required = 0.0f32;
            let mut allowed = 0.0f32;
            let mut written = false;
            for (occur, child) in children.iter_mut() {
                let (score, direct) = evaluate(child, doc, buffer, positions);
                if score <= 0.0 {
                    continue;
                }
                written |= direct;
                if *occur == Occur::Must {
                    required += score;
                } else if score > allowed {
                    allowed = score;
                }
            }
            (required + allowed, written)
        }
        Live::Boost(inner, boost) => {
            let (score, written) = evaluate(inner, doc, buffer, positions);
            (score * *boost, written)
        }
    }
}

/// The per-segment half of [`Ranking`].
struct Scoring {
    ordinal: SegmentOrdinal,
    concepts: Vec<Live>,
    ts: Column<i64>,
    now: i64,
    limit: usize,
    best: Vec<Candidate>,
    buffer: Vec<u32>,
}

impl SegmentCollector for Scoring {
    type Fruit = Vec<Candidate>;

    fn collect(&mut self, doc: DocId, _score: tantivy::Score) {
        let total = self.concepts.len();
        let mut sum = 0.0;
        let mut hits = 0u8;
        let mut mask = 0u32;
        let mut direct = 0u32;
        let mut spans: Vec<Vec<u32>> = Vec::new();

        for (at, concept) in self.concepts.iter_mut().enumerate() {
            let mut positions: Vec<u32> = Vec::new();
            let (score, written) = evaluate(concept, doc, &mut self.buffer, &mut positions);
            if score <= 0.0 {
                continue;
            }
            sum += score;
            hits += 1;
            mask |= 1 << at;
            if written {
                direct |= 1 << at;
            }
            if !positions.is_empty() {
                positions.sort_unstable();
                spans.push(positions);
            }
        }

        if hits == 0 {
            return;
        }

        let coverage = (hits as f32 / total as f32).powf(ALPHA);
        let score = sum * coverage * proximity(&spans) * self.recency(doc);
        self.best.push(Candidate {
            address: DocAddress::new(self.ordinal, doc),
            score,
            raw: sum,
            hits,
            mask,
            direct,
        });

        // Kept bounded rather than sorted on every push: a common term can
        // match a large fraction of the index, and holding all of it to sort
        // once at the end is the one way this loop runs out of memory.
        if self.best.len() >= self.limit * 2 {
            self.best.sort_by(|a, b| b.score.total_cmp(&a.score));
            self.best.truncate(self.limit);
        }
    }

    fn harvest(mut self) -> Vec<Candidate> {
        self.best.sort_by(|a, b| b.score.total_cmp(&a.score));
        self.best.truncate(self.limit);
        self.best
    }
}

impl Scoring {
    /// Exponential decay on age with a floor, so that recency reorders results
    /// without ever deciding them.
    fn recency(&self, doc: DocId) -> f32 {
        let ts = self.ts.first(doc).unwrap_or(self.now);
        let age = (self.now - ts).max(0) as f32;
        FLOOR + (1.0 - FLOOR) * 0.5f32.powf(age / HALF_LIFE)
    }
}

/// The multiplicative proximity bonus: the tighter the smallest window holding
/// a position from every matched clause, the larger it is.
///
/// A single clause has no span to measure, so it earns nothing — which is
/// correct, since there is nothing for it to have been near.
fn proximity(spans: &[Vec<u32>]) -> f32 {
    if spans.len() < 2 {
        return 1.0;
    }

    // The classic sweep: hold one cursor per clause, measure the window between
    // the smallest and largest head, then advance the smallest. Every candidate
    // window is seen exactly once.
    let mut heads = vec![0usize; spans.len()];
    let mut narrowest = u32::MAX;
    loop {
        let mut low = u32::MAX;
        let mut high = 0u32;
        let mut at = 0;
        for (group, head) in heads.iter().enumerate() {
            let Some(position) = spans[group].get(*head) else {
                return bonus(narrowest, spans.len());
            };
            if *position < low {
                low = *position;
                at = group;
            }
            if *position > high {
                high = *position;
            }
        }
        narrowest = narrowest.min(high - low);
        heads[at] += 1;
    }
}

/// Turn a span into a multiplier. A span equal to the number of clauses minus
/// one is adjacent words and earns the whole bonus; it falls off from there.
fn bonus(span: u32, clauses: usize) -> f32 {
    if span == u32::MAX {
        return 1.0;
    }
    let tightest = (clauses - 1) as f32;
    let excess = (span as f32 - tightest).max(0.0);
    1.0 + PROXIMITY / (1.0 + excess)
}
