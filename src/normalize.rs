//! Turning text into terms, and the one place that is allowed to.
//!
//! Everything downstream of this module — the term dictionary, every posting,
//! every document frequency — is frozen around the rules below the moment a
//! message is scanned. That is why [`VERSION`] exists and why `memory rescan`
//! is a first-class command rather than a repair script: the way to change a
//! rule here is to change it, bump the version, and rebuild the index from the
//! messages, which are the only thing that was never derived.
//!
//! There are two analyzers over one segmenter, and the fact that it is *one*
//! segmenter is load-bearing. [`SURFACE`] keeps a token as it was written and
//! [`LEMMA`] folds it, but both cut the text at exactly the same byte offsets,
//! so token position 7 means the same word in both fields. Proximity scoring
//! reads positions from whichever field a group matched on, and if the two
//! disagreed about where tokens begin, a span measured across an exact match
//! and a folded one would be measuring nothing.

use std::sync::OnceLock;

use rust_stemmers::{Algorithm, Stemmer};
use tantivy::tokenizer::{Token, TokenStream, Tokenizer};
use unicode_normalization::UnicodeNormalization;

/// Name the [`SURFACE`] analyzer is registered under in tantivy's tokenizer
/// manager, and the name of the field it feeds.
pub const SURFACE: &str = "surface";

/// Name the [`LEMMA`] analyzer is registered under, and of the two fields it
/// feeds — `lemma` and `context`. This is the field whose document frequencies
/// are the IDF the scorer uses, which is the reason the folding is aggressive
/// here and absent on [`SURFACE`].
pub const LEMMA: &str = "lemma";

/// Bumped whenever anything in this module changes what a token becomes.
///
/// Written into `index_meta` at build time and compared on open. A mismatch is
/// refused rather than served: an index built by older rules answers queries
/// normalized by newer ones with silence, and silence is indistinguishable
/// from "nothing was ever stored about that".
pub const VERSION: u32 = 3;

/// Characters a stem must keep for a suffix to be stripped off it.
///
/// Persian suffixes are short and common enough to appear inside unrelated
/// words: `دفتر` ends in `تر` and `علی` ends in `ی`. A floor does not know
/// which is which, but it does keep the damage to words long enough to survive
/// losing two characters, and `بیشتر` → `بیش` is worth `ماهی` → `ماه`.
///
/// This is measured in characters, not bytes: every Persian letter is two
/// bytes and the floor is a statement about words.
const STEM: usize = 3;

/// The English stemmer, built once.
///
/// `Stemmer::create` only selects a function pointer, but [`lemma`] is called
/// on every token of every unit at scan time and on every query word at search
/// time, and there is no reason to do even that per token.
static ENGLISH: OnceLock<Stemmer> = OnceLock::new();

/// Longest a token can be and still be treated as a word.
///
/// Past this it is a base64 blob, a hash or a stack trace. It still gets a term
/// and still matches exactly; it just does not go through morphology, where the
/// rules are about words and would only corrupt it.
const LONG: usize = 25;

/// Which alphabet a token is written in. Cheap to compute while segmenting and
/// the thing that decides whether Persian morphology applies at all.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Script {
    /// ASCII letters and digits: `error`, `JWT_SECRET`, `404`.
    Latin,
    /// Arabic-script letters: `خطا`, `می‌رود`.
    Arabic,
    /// Both in one token, which in practice means an identifier somebody wrote
    /// half of in Persian.
    Mixed,
    /// Neither — CJK, Cyrillic, emoji.
    Other,
}

/// One token of the input, before either analyzer has touched it.
///
/// `start` and `end` are **byte** offsets into the text that was segmented, not
/// character offsets. Rust slices `&str` by byte index and Persian is multibyte
/// throughout, so character offsets would mean a scan of the string on every
/// snippet; and the snippet path runs on every hit of every search.
pub struct Word<'a> {
    pub text: &'a str,
    pub start: usize,
    pub end: usize,
    pub script: Script,
}

/// Cut `text` into words, in order, with their byte offsets.
///
/// A word is a run of alphanumerics, and additionally of `_ - . / ' \u{200C}`,
/// which are the characters that hold an identifier or a Persian compound
/// together: splitting on them would turn `JWT_SECRET` into two very common
/// tokens, `v0.26.1` into three numbers, and `می‌رود` into `می` — a fragment
/// that occurs in every third sentence in the language. Those characters are
/// then trimmed from both ends, because the same rule that keeps `e.g.` whole
/// also picks up the full stop at the end of a sentence.
pub fn segment(text: &str) -> Vec<Word<'_>> {
    let mut words = Vec::new();
    let mut start = None;
    let mut latin = false;
    let mut arabic = false;
    let mut other = false;

    // One extra pass with a separator, so a word ending at the end of the text
    // is flushed by the same branch as every other word.
    let bytes = text.len();
    for (at, character) in text.char_indices().chain([(bytes, ' ')]) {
        let held = character.is_alphanumeric()
            || matches!(character, '_' | '-' | '.' | '/' | '\'' | '\u{200C}');
        if held {
            if start.is_none() {
                start = Some(at);
            }
            if character.is_ascii_alphanumeric() {
                latin = true;
            } else if arabic_script(character) {
                arabic = true;
            } else if character.is_alphanumeric() {
                other = true;
            }
            continue;
        }

        let Some(from) = start else {
            continue;
        };
        start = None;

        // Trim the glue characters back off the ends. `.` and `-` are word
        // characters in the middle of `file.rs` and punctuation everywhere
        // else, and there is no way to tell which until the word has ended.
        let whole = &text[from..at];
        let trimmed = whole.trim_matches(|c: char| !c.is_alphanumeric() && c != '\u{200C}');
        let taken = latin;
        let seen = arabic;
        let rest = other;
        latin = false;
        arabic = false;
        other = false;
        if trimmed.is_empty() {
            continue;
        }

        let script = match (taken, seen, rest) {
            (_, _, true) => Script::Other,
            (true, true, _) => Script::Mixed,
            (_, true, _) => Script::Arabic,
            (true, _, _) => Script::Latin,
            _ => Script::Other,
        };
        let offset = trimmed.as_ptr() as usize - whole.as_ptr() as usize;
        words.push(Word {
            text: trimmed,
            start: from + offset,
            end: from + offset + trimmed.len(),
            script,
        });
    }

    words
}

/// The [`SURFACE`] form: the token as it was written, with only the differences
/// that are invisible to the person who wrote it removed.
///
/// NFC, then the Arabic letters that Persian keyboards and Arabic keyboards
/// disagree about — `ك`/`ک` and `ي`/`ی` are *different codepoints that render
/// identically* — then diacritics, tatweel, and the two other families of
/// digits. Case is kept, ZWNJ is kept, morphology is not touched: this is the
/// field that has to be able to tell `JWT_SECRET` from `jwt_secret`, and every
/// fold applied here is a distinction it can no longer draw.
pub fn surface(word: &str) -> String {
    let mut folded = String::with_capacity(word.len());
    for character in word.nfc() {
        match character {
            // Arabic kaf and yeh, and alef maksura, to their Persian forms.
            '\u{0643}' => folded.push('\u{06A9}'),
            '\u{064A}' | '\u{0649}' => folded.push('\u{06CC}'),
            // Hamza carriers to their bare letters. `ۀ` is the one the design
            // document calls out: heh with hamza above, which is heh.
            //
            // Alef with madda, `آ`, is deliberately not in this list, and it is the
            // one letter here a Persian writer does type deliberately: it has
            // its own key, it is never omitted, and folding it away collapses
            // `آسم` (asthma) onto `اسم` (name), `آمار` (statistics) onto
            // `امار` (a commander) and `آب` (water) onto `اب`. The Arabic
            // carriers below are different: `أ` and `إ` are not on a Persian
            // keyboard and Persian spells both of them bare, so folding them is
            // recovering one spelling rather than destroying two words. The
            // madda fold still happens — on the lemma field, where an
            // over-eager fold is the point and the surface field is still
            // beside it holding the difference.
            '\u{0623}' | '\u{0625}' => folded.push('\u{0627}'),
            '\u{0624}' => folded.push('\u{0648}'),
            '\u{0626}' => folded.push('\u{06CC}'),
            '\u{06C0}' | '\u{0629}' => folded.push('\u{0647}'),
            // Harakat, tatweel, and the invisible direction marks. None of them
            // are ever typed twice the same way and none of them carry meaning
            // a search could use.
            '\u{064B}'..='\u{0652}'
            | '\u{0653}'..='\u{0655}'
            | '\u{0670}'
            | '\u{0640}'
            | '\u{200D}'
            | '\u{200E}'
            | '\u{200F}'
            | '\u{FEFF}' => {}
            // Arabic-Indic and extended Arabic-Indic digits to ASCII, so that
            // `۱۴۰۴` and `1404` are one term.
            '\u{0660}'..='\u{0669}' => {
                folded.push((b'0' + (character as u32 - 0x0660) as u8) as char)
            }
            '\u{06F0}'..='\u{06F9}' => {
                folded.push((b'0' + (character as u32 - 0x06F0) as u8) as char)
            }
            _ => folded.push(character),
        }
    }
    folded
}

/// The [`LEMMA`] form: [`surface`], lowercased, then stemmed — Snowball English
/// for Latin words, and for Persian ones the affix rules below with ZWNJ closed
/// up afterwards.
///
/// This is the field whose document frequency drives IDF, so it is where every
/// spelling of one word has to land on one term. Being wrong here is survivable
/// in a way it would not be without a surface field beside it: an over-eager
/// strip costs precision on the lemma field only, and the exact form is still
/// indexed, still searchable and still scores higher when it matches.
///
/// The one thing this must be is **deterministic and total**. It is called on
/// the query words by the same code path that called it on the stored ones, and
/// a rule that fired at scan time but not at search time is an index that
/// cannot be searched at all.
pub fn lemma(word: &str, script: Script) -> String {
    let folded = surface(word).to_lowercase();

    // Identifiers do not inflect. `user_id`, `v0.26.1`, `getUserByID` and a
    // 40-character sha are not words, and the morphology below is a set of
    // rules about words; running it on them mangles the one kind of token
    // where exact matching is the entire value.
    if !embeddable(&folded, script) {
        return folded.replace('\u{200C}', "");
    }
    // Latin words go through Snowball English (Porter2). This is the whole of
    // what makes `errors` and `error`, or `deprecated` and `deprecation`, one
    // term on this field. It is applied here and not as a tantivy `TokenFilter`
    // because search.rs calls this same function on the words of a query, and a
    // fold that happened on only one of those two sides is a fold that never
    // matches anything.
    //
    // Being wrong costs less here than anywhere else in this module: Snowball
    // over-stems (`generic` and `generous` both reach `gener`), but it does so
    // on the recall field, next to a `surface` field that still holds the exact
    // spelling and still outscores it.
    if script != Script::Arabic && script != Script::Mixed {
        let stemmer = ENGLISH.get_or_init(|| Stemmer::create(Algorithm::English));
        return stemmer.stem(&folded).into_owned();
    }

    // Alef with madda to bare alef. [`surface`] leaves this alone so that
    // `آسم` and `اسم` stay two words there; here, on the field whose whole
    // job is recall, they become one — which is what rescues a corpus whose
    // producer dropped the madda, as the extracted triage deck did on every
    // one of its pages, from being unsearchable by anybody typing the word
    // correctly. The exact spelling still outscores it when the corpus has it.
    let folded = match folded.contains('\u{0622}') {
        true => folded.replace('\u{0622}', "\u{0627}"),
        false => folded,
    };

    // `می‌رود` → `رود`, but only across a ZWNJ. Stripping `می` from a glued
    // token would take it off `میلاد` and `میدان` too, and there is nothing
    // left in the string at that point to tell them apart. The ZWNJ is the
    // writer saying this is a prefix, and it is the only reliable signal in
    // Persian orthography that anything here can use.
    let mut stem = folded.as_str();
    for prefix in ["نمی\u{200C}", "می\u{200C}"] {
        if let Some(rest) = stem.strip_prefix(prefix) {
            stem = rest;
            break;
        }
    }

    // Suffixes, longest first so `هایمان` is never read as `مان` and `هایی`
    // never as `ها` plus noise. A ZWNJ in front of one is the same explicit
    // signal the prefix rule uses, so those strip unconditionally; a glued
    // suffix has to clear `STEM`.
    //
    // The table is taken from `parsitext` (Apache-2.0, obsernetics/rust-lib),
    // the one Rust crate carrying a Persian light stemmer, which arrived at the
    // same ZWNJ-join policy and the same three-character floor independently.
    // Its verb endings — `یم`, `ید`, `ند`, `ست` — are deliberately not here:
    // they are two characters, they collide with the ends of ordinary nouns
    // (`کلید`, `بلند`, `درست`), and they would not buy the unification they
    // look like they buy, because Persian verbs alternate their stems too and
    // `رود`/`روند` do not meet whatever is stripped off the end.
    let suffixes = [
        // Plural plus possessive: `کتاب‌هایمان`.
        "هایمان",
        "هایتان",
        "هایشان",
        // The older `ان` plural plus possessive: `درختانمان`.
        "انمان",
        "انتان",
        "انشان",
        "هایی",
        "هایم",
        "هایت",
        "هایش",
        "ترین",
        "های",
        "انم",
        "انت",
        "انش",
        // Possessives on their own: `کتابمان`.
        "مان",
        "تان",
        "شان",
        "ها",
        "تر",
        "ام",
        "ات",
        "اش",
        "ان",
        "ای",
        "ی",
    ];
    for suffix in suffixes {
        let attached = format!("\u{200C}{suffix}");
        if let Some(rest) = stem.strip_suffix(attached.as_str())
            && !rest.is_empty()
        {
            stem = rest;
            break;
        }
        if let Some(rest) = stem.strip_suffix(suffix)
            && rest.chars().count() >= STEM
        {
            stem = rest;
            break;
        }
    }

    // Whatever ZWNJ is left is inside a compound the rules above had no opinion
    // about — `کتاب‌خانه`. Closing it up is what makes that one term with the
    // glued spelling somebody else used.
    stem.replace('\u{200C}', "")
}

/// Whether a token is word-shaped enough that morphology and, later, any
/// expansion may touch it.
///
/// False for digits, underscores, dots, slashes, internal capitals and
/// excessive length — which is to say, for identifiers. They keep their term
/// and match exactly; they just have no path by which a search for something
/// else can reach them, and that is the correct behaviour for a token whose
/// whole meaning is that it is spelled that way.
pub fn embeddable(word: &str, script: Script) -> bool {
    if word.chars().count() > LONG || script == Script::Other {
        return false;
    }
    let mut letters = false;
    let mut digits = false;
    for character in word.chars() {
        if matches!(character, '_' | '.' | '/' | '-') {
            return false;
        }
        if character.is_numeric() {
            digits = true;
        } else if character.is_alphabetic() {
            letters = true;
        }
    }
    // A bare number is not a word; a number welded to letters is an identifier.
    // Either way there is nothing to inflect.
    letters && !digits
}

/// True for the Arabic script block and the Persian additions to it.
fn arabic_script(character: char) -> bool {
    matches!(character, '\u{0600}'..='\u{06FF}' | '\u{0750}'..='\u{077F}' | '\u{FB50}'..='\u{FDFF}' | '\u{FE70}'..='\u{FEFF}')
}

/// The tantivy side: one analyzer, told at construction which form to emit.
///
/// Registered twice under [`SURFACE`] and [`LEMMA`], over the same [`segment`],
/// so the two fields agree token for token and position for position.
#[derive(Clone)]
pub struct Analyzer {
    lemma: bool,
}

impl Analyzer {
    pub fn new(lemma: bool) -> Self {
        Self { lemma }
    }
}

impl Tokenizer for Analyzer {
    type TokenStream<'a> = Stream;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Stream {
        let mut tokens = Vec::new();
        for (position, word) in segment(text).into_iter().enumerate() {
            let text = if self.lemma {
                lemma(word.text, word.script)
            } else {
                surface(word.text)
            };
            // Morphology can empty a token — a bare `ها` clears no stem floor
            // and a token of only diacritics folds away entirely. Dropping it
            // rather than indexing an empty term costs a position, and a gap
            // in positions is exactly what a dropped word should look like to
            // proximity scoring.
            if text.is_empty() {
                continue;
            }
            tokens.push(Token {
                offset_from: word.start,
                offset_to: word.end,
                position,
                text,
                position_length: 1,
            });
        }
        Stream { tokens, at: 0 }
    }
}

/// The token stream [`Analyzer`] hands back. Built eagerly: a unit is a
/// paragraph, the whole of it is already in memory, and a lazy stream over it
/// would buy nothing but a lifetime parameter.
pub struct Stream {
    tokens: Vec<Token>,
    at: usize,
}

impl TokenStream for Stream {
    fn advance(&mut self) -> bool {
        self.at += 1;
        self.at <= self.tokens.len()
    }

    fn token(&self) -> &Token {
        &self.tokens[self.at - 1]
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.tokens[self.at - 1]
    }
}
