//! Search: concept groups in, a short ranked list out.
//!
//! The caller — a language model, usually a weak one — supplies groups of words
//! that mean the same thing, like `["error","fault","خطا"]`. Those groups are
//! the only place cross-lingual unification happens, they exist for the length
//! of one query, and nothing about them is ever stored. `error` and `خطا` have
//! unrelated terms, unrelated document frequencies and unrelated postings; what
//! they share is an accumulator slot, and whichever of them scores higher for a
//! given unit wins it.
//!
//! That has to be true. Synonymy is not transitive — `error ≈ fault`,
//! `fault ≈ عیب`, `عیب ≈ defect`, `defect ≈ نقص` — so taking its transitive
//! closure collapses half a vocabulary into one identifier, which is the
//! historical failure mode of thesaurus-based retrieval. Merging the two would
//! also destroy the fact that `error` occurs four hundred times here and `خطا`
//! six, and that difference is signal: it is what tells the caller this memory
//! is written in English.
//!
//! # Why the scoring loop is written out rather than composed from queries
//!
//! Tantivy owns the postings, the term dictionary, the fieldnorms and the BM25
//! arithmetic, and none of that is reimplemented here. What is not expressible
//! in the stock queries is the combination the design document specifies:
//! **max within a group** (`DisjunctionMaxQuery` is close, but adds a tie-break
//! increment per extra matching clause, which double-counts a paragraph that
//! says the same thing three ways), **coverage across groups** as
//! `(hits/total)^α`, and **minimum-span proximity** between positions belonging
//! to *different* groups, which is not a phrase query with slop. So a
//! `BooleanQuery` drives the iteration and enforces required groups, and the
//! collector below does the scoring from raw postings.

use std::collections::HashMap;
use std::ops::Bound;
use std::sync::Arc;

use tantivy::collector::{Collector, SegmentCollector};
use tantivy::columnar::Column;
use tantivy::fieldnorm::FieldNormReader;
use tantivy::postings::{Postings, SegmentPostings};
use tantivy::query::{
    Bm25Weight, BooleanQuery, DisjunctionMaxQuery, Occur, Query, RangeQuery, TermQuery,
};
use tantivy::schema::{Field, IndexRecordOption};
use tantivy::{DocAddress, DocId, DocSet, SegmentOrdinal, SegmentReader, Term};

use crate::index::Index;
use crate::normalize;
use crate::storage::{Located, Role, Storage};
use crate::ulid::Ulid;

/// How hard coverage is weighted: the score is multiplied by
/// `(groups_matched / groups_total)^ALPHA`.
///
/// Coverage is the dominant relevance signal in this design — a paragraph that
/// touches all three of what you asked about should beat one that touches a
/// single group by a wide margin, however emphatically it touches it. At 1.0
/// the factor is linear and a very high BM25 on one group can still win;
/// above 1.0 it cannot, which is the intent.
const ALPHA: f32 = 1.5;

/// Largest multiplier a tight span between two groups can earn.
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

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the query has no groups — pass at least one")]
    Empty,

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
            Source::Context => 0.35,
        }
    }
}

/// A concept group as the caller sent it.
#[derive(Debug, Clone)]
pub struct Group {
    pub label: String,
    pub words: Vec<String>,
    /// Must-match rather than should-match. Enforced by the driver query, so a
    /// unit that cannot survive is never scored.
    pub required: bool,
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
    pub group: String,
    pub word: String,
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
    /// The BM25 sum before any of the multipliers, for debugging a ranking.
    pub raw: f32,
    /// Groups matched over groups asked for. Unlike the score this is a fact
    /// the caller can reason about correctly, and it is what the tool
    /// description tells a model to read.
    pub coverage: (u8, u8),
    /// The labels of the groups that hit.
    pub matched: Vec<String>,
    pub session: String,
    pub message: Option<String>,
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
    /// High-IDF terms that co-occur with the top results and were not asked
    /// for. The cheapest quality improvement available: it turns one shot in
    /// the dark into two informed ones.
    pub hints: Vec<(String, u64)>,
}

/// One term of one group, resolved against the dictionary and ready to score.
struct Plan {
    term: Term,
    field: Field,
    weight: f32,
    bm25: Bm25Weight,
    /// False for the context field, which records no positions.
    positions: bool,
}

/// A group's terms, after resolution.
struct Resolved {
    label: String,
    terms: Vec<Plan>,
    required: bool,
}

/// Run a search.
pub fn search(
    storage: &Storage,
    index: &Index,
    groups: &[Group],
    filter: &Filter,
    limit: usize,
    per_message: usize,
) -> Result<Outcome, Error> {
    if groups.is_empty() {
        return Err(Error::Empty);
    }
    let searcher = index.reader.searcher();
    let fields = index.fields;

    // Resolve every word on its own. `error` and `خطا` land on unrelated terms
    // and produce unrelated postings lists; the group is not consulted here and
    // has no effect on what a word resolves to.
    let mut resolved: Vec<Resolved> = Vec::new();
    let mut unknown = Vec::new();
    for group in groups {
        let mut terms = Vec::new();
        for word in &group.words {
            let mut found = false;
            let segmented = normalize::segment(word);
            let script = match segmented.first() {
                Some(word) => word.script,
                None => normalize::Script::Other,
            };
            let surface = normalize::surface(word);
            let lemma = normalize::lemma(word, script);

            let candidates = [
                (fields.surface, surface.clone(), Source::Exact, true),
                (fields.lemma, lemma.clone(), Source::Variant, true),
                (fields.context, lemma.clone(), Source::Context, false),
            ];
            for (field, text, source, positions) in candidates {
                if text.is_empty() {
                    continue;
                }
                let term = Term::from_field_text(field, &text);
                let frequency = match searcher.doc_freq(&term) {
                    Ok(frequency) => frequency,
                    Err(source) => return Err(Error::Search { source }),
                };
                if frequency == 0 {
                    continue;
                }
                found = true;
                let bm25 = match Bm25Weight::for_terms(&searcher, std::slice::from_ref(&term)) {
                    Ok(bm25) => bm25,
                    Err(source) => return Err(Error::Search { source }),
                };
                terms.push(Plan {
                    term,
                    field,
                    weight: source.weight(),
                    bm25,
                    positions,
                });
            }
            if !found {
                unknown.push(Unknown {
                    group: group.label.clone(),
                    word: word.clone(),
                });
            }
        }

        // A required group nothing resolved into cannot be satisfied by any
        // unit, so there is no search to run. Saying so — with the words that
        // failed — is more use than an empty list.
        if terms.is_empty() {
            if group.required {
                return Ok(Outcome {
                    hits: Vec::new(),
                    unknown,
                    hints: Vec::new(),
                });
            }
            continue;
        }
        resolved.push(Resolved {
            label: group.label.clone(),
            terms,
            required: group.required,
        });
    }

    if resolved.is_empty() {
        return Ok(Outcome {
            hits: Vec::new(),
            unknown,
            hints: Vec::new(),
        });
    }

    // The driver. Its scores are discarded — the collector recomputes
    // everything from postings — but it is what walks the segments, applies the
    // deletes and enforces the required groups and the filters, none of which
    // is worth reimplementing.
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
    for group in &resolved {
        let mut alternatives: Vec<Box<dyn Query>> = Vec::new();
        for plan in &group.terms {
            alternatives.push(Box::new(TermQuery::new(
                plan.term.clone(),
                IndexRecordOption::WithFreqs,
            )));
        }
        let occur = if group.required {
            Occur::Must
        } else {
            Occur::Should
        };
        clauses.push((occur, Box::new(DisjunctionMaxQuery::new(alternatives))));
    }
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
        let mut roles: Vec<Box<dyn Query>> = Vec::new();
        for role in &filter.roles {
            roles.push(Box::new(TermQuery::new(
                Term::from_field_u64(fields.role, role.code()),
                IndexRecordOption::Basic,
            )));
        }
        clauses.push((Occur::Must, Box::new(DisjunctionMaxQuery::new(roles))));
    }
    let query = BooleanQuery::new(clauses);

    let wanted = limit * OVERFETCH;
    let ranking = Ranking {
        groups: Arc::new(resolved),
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
        for (at, group) in ranking.groups.iter().enumerate() {
            if candidate.mask & (1 << at) != 0 {
                matched.push(group.label.clone());
            }
        }

        hits.push(Hit {
            unit: row.unit,
            cursor: row.unit.to_string(),
            snippet: snippet(storage, row, &ranking.groups)?,
            score: if top > 0.0 {
                candidate.score / top
            } else {
                0.0
            },
            raw: candidate.raw,
            coverage: (candidate.hits, ranking.groups.len() as u8),
            matched,
            session: row.session_ref.clone(),
            message: row.message_ref.clone(),
            author: row.author.clone(),
            role: row.role,
            ts: row.ts,
            words: row.text().split_whitespace().count(),
        });
        if hits.len() >= limit {
            break;
        }
    }

    let hints = vocabulary(&searcher, index, &hits, &by_unit, groups);
    Ok(Outcome {
        hits,
        unknown,
        hints,
    })
}

/// The best sentence of a unit, or the whole unit when it is short.
///
/// Sliced out of `message.body` with the offsets stored at scan time. The best
/// sentence is the one holding the most distinct query groups — the same
/// coverage idea as the ranking, applied inside one paragraph.
fn snippet(storage: &Storage, row: &Located, groups: &[Resolved]) -> Result<String, Error> {
    let sentences = storage.sentences(row.unit)?;
    if sentences.len() < 2 {
        return Ok(row.text().to_string());
    }

    let mut wanted: Vec<Vec<String>> = Vec::new();
    for group in groups {
        let mut texts = Vec::new();
        for plan in &group.terms {
            if let Some(text) = plan.term.value().as_str() {
                texts.push(text.to_string());
            }
        }
        wanted.push(texts);
    }

    let mut best = (0usize, row.start, row.end);
    for (start, end) in sentences {
        let text = &row.body[start..end];
        let mut hits = 0;
        for group in &wanted {
            let mut found = false;
            for word in normalize::segment(text) {
                let surface = normalize::surface(word.text);
                let lemma = normalize::lemma(word.text, word.script);
                if group.iter().any(|held| *held == surface || *held == lemma) {
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
    groups: &[Group],
) -> Vec<(String, u64)> {
    let mut asked: Vec<String> = Vec::new();
    for group in groups {
        for word in &group.words {
            let segmented = normalize::segment(word);
            let script = match segmented.first() {
                Some(word) => word.script,
                None => normalize::Script::Other,
            };
            asked.push(normalize::lemma(word, script));
        }
    }

    let mut frequencies: HashMap<String, u64> = HashMap::new();
    for hit in hits {
        let Some(row) = located.get(&hit.unit) else {
            continue;
        };
        for word in normalize::segment(row.text()) {
            let lemma = normalize::lemma(word.text, word.script);
            if lemma.is_empty() || asked.contains(&lemma) || frequencies.contains_key(&lemma) {
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
    /// One bit per group, so coverage is a `count_ones` and the matched labels
    /// are readable afterwards.
    mask: u32,
}

/// The collector that does the actual scoring.
struct Ranking {
    groups: Arc<Vec<Resolved>>,
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
        let mut groups = Vec::with_capacity(self.groups.len());
        for group in self.groups.iter() {
            let mut terms = Vec::new();
            for plan in &group.terms {
                let inverted = reader.inverted_index(plan.field)?;
                let option = if plan.positions {
                    IndexRecordOption::WithFreqsAndPositions
                } else {
                    IndexRecordOption::WithFreqs
                };
                let Some(postings) = inverted.read_postings(&plan.term, option)? else {
                    continue;
                };
                terms.push(Cursor {
                    postings,
                    fieldnorms: reader.get_fieldnorms_reader(plan.field)?,
                    bm25: plan.bm25.clone(),
                    weight: plan.weight,
                    positions: plan.positions,
                });
            }
            groups.push(terms);
        }

        Ok(Scoring {
            ordinal,
            groups,
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

/// One term's postings within one segment, plus what is needed to score them.
struct Cursor {
    postings: SegmentPostings,
    fieldnorms: FieldNormReader,
    bm25: Bm25Weight,
    weight: f32,
    positions: bool,
}

/// The per-segment half of [`Ranking`].
struct Scoring {
    ordinal: SegmentOrdinal,
    groups: Vec<Vec<Cursor>>,
    ts: Column<i64>,
    now: i64,
    limit: usize,
    best: Vec<Candidate>,
    buffer: Vec<u32>,
}

impl SegmentCollector for Scoring {
    type Fruit = Vec<Candidate>;

    fn collect(&mut self, doc: DocId, _score: tantivy::Score) {
        let total = self.groups.len();
        let mut sum = 0.0;
        let mut hits = 0u8;
        let mut mask = 0u32;
        let mut spans: Vec<Vec<u32>> = Vec::new();

        for (at, group) in self.groups.iter_mut().enumerate() {
            // One slot per group, holding the best contribution rather than the
            // sum of them. A paragraph containing `error`, `fault` *and* `خطا`
            // describes one concept three ways and must not score triple.
            let mut best = 0.0f32;
            let mut positions: Vec<u32> = Vec::new();
            for cursor in group.iter_mut() {
                // Documents arrive in increasing order within a segment, so
                // every cursor only ever moves forward.
                if cursor.postings.doc() < doc {
                    cursor.postings.seek(doc);
                }
                if cursor.postings.doc() != doc {
                    continue;
                }
                let frequency = cursor.postings.term_freq();
                let score = cursor
                    .bm25
                    .score(cursor.fieldnorms.fieldnorm_id(doc), frequency)
                    * cursor.weight;
                if score > best {
                    best = score;
                }
                if cursor.positions {
                    self.buffer.clear();
                    cursor.postings.positions(&mut self.buffer);
                    positions.extend_from_slice(&self.buffer);
                }
            }
            if best <= 0.0 {
                continue;
            }
            sum += best;
            hits += 1;
            mask |= 1 << at;
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
/// a position from every matched group, the larger it is.
///
/// A single group has no span to measure, so it earns nothing — which is
/// correct, since there is nothing for it to have been near.
fn proximity(spans: &[Vec<u32>]) -> f32 {
    if spans.len() < 2 {
        return 1.0;
    }

    // The classic sweep: hold one cursor per group, measure the window between
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

/// Turn a span into a multiplier. A span equal to the number of groups minus
/// one is adjacent words and earns the whole bonus; it falls off from there.
fn bonus(span: u32, groups: usize) -> f32 {
    if span == u32::MAX {
        return 1.0;
    }
    let tightest = (groups - 1) as f32;
    let excess = (span as f32 - tightest).max(0.0);
    1.0 + PROXIMITY / (1.0 + excess)
}
