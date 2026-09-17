use std::sync::Mutex;

use borhan::api;
use borhan::index::Index;
use borhan::search::{Error, Filter, Outcome};
use borhan::storage::{Entry, Role, Storage};
use borhan::ulid::Ulid;

const DESCRIPTION: &str = "Security decisions and incidents, with the reasons behind each of them, in English and Persian.";

/// A memory holding these messages, as `(session, role, ts, body)`.
fn memory(messages: &[(&str, Role, i64, &str)]) -> (tempfile::TempDir, Storage, Index) {
    let home = tempfile::tempdir().unwrap();
    let trace = Ulid::new().unwrap();
    api::create(home.path(), "security", DESCRIPTION, "fa,en", &trace).unwrap();
    let store = Storage::open(home.path(), "security").unwrap();
    let index = Index::open(&store).unwrap();
    let store = Mutex::new(store);
    let writer = Mutex::new(index.writer().unwrap());
    for (session, role, ts, body) in messages {
        let entry = Entry {
            session,
            message: None,
            author: role.as_str(),
            role: *role,
            ts: *ts,
            body,
        };
        api::add(&store, &index, &writer, &entry, "security", &trace).unwrap();
    }
    (home, store.into_inner().unwrap(), index)
}

fn search(
    store: &Storage,
    index: &Index,
    query: &str,
    fuzzy: bool,
    filter: &Filter,
    page: (usize, usize),
) -> Result<Outcome, api::Error> {
    let trace = Ulid::new().unwrap();
    match api::search(
        store,
        index,
        "security",
        (query, fuzzy),
        filter,
        page,
        &trace,
    ) {
        Ok((outcome, _)) => Ok(outcome),
        Err(error) => Err(error),
    }
}

#[test]
fn a_query_is_a_list_of_ideas() {
    let (_home, store, index) = memory(&[
        (
            "review",
            Role::User,
            1_000,
            "Password reset links expire after 15 minutes.\n\nThe JWT_SECRET rotates every month.",
        ),
        (
            "review",
            Role::Assistant,
            2_000,
            "لینک بازنشانی رمز عبور بعد از ۱۵ دقیقه منقضی می‌شود.",
        ),
        (
            "incidents",
            Role::Tool,
            3_000,
            "We rate limit reset requests to 5 per hour per account.\n\nRetry failed webhooks with exponential backoff.",
        ),
        (
            "incidents",
            Role::User,
            4_000,
            "The borrow checker rejected the mutable alias. It was fixed by cloning.",
        ),
    ]);
    let everything = Filter::default();
    let found = |query: &str| search(&store, &index, query, false, &everything, (10, 2)).unwrap();
    let snippets = |outcome: &Outcome| {
        let mut snippets = Vec::new();
        for hit in &outcome.hits {
            snippets.push(hit.snippet.clone());
        }
        snippets
    };

    // Two ideas, each spelled in both languages: both paragraphs about
    // resetting a password cover both, the rate limit covers one.
    let outcome = found("(password passwords رمز) +(reset بازنشانی)");
    let mut matched = Vec::new();
    let mut nearby = Vec::new();
    for hit in &outcome.hits {
        match hit.matched.is_empty() {
            false => matched.push(hit.coverage),
            true => nearby.push(hit.snippet.as_str()),
        }
    }
    assert_eq!(matched, [(2, 2), (2, 2), (1, 2)], "{:#?}", outcome.hits);
    // The other paragraph of the English message holds neither idea itself, and
    // is still returned because its message does.
    assert_eq!(nearby, ["The JWT_SECRET rotates every month."]);
    assert_eq!(outcome.hits[0].score, 1.0);
    assert!(outcome.unknown.is_empty());

    for query in ["reset -password", "reset NOT password"] {
        let outcome = found(query);
        assert_eq!(
            snippets(&outcome),
            ["We rate limit reset requests to 5 per hour per account."],
            "{query}"
        );
    }
    let outcome = found("password AND reset");
    assert_eq!(
        outcome.hits[0].snippet,
        "Password reset links expire after 15 minutes."
    );
    for hit in &outcome.hits {
        assert_eq!(hit.coverage, (2, 2));
    }

    assert_eq!(
        snippets(&found("\"exponential backoff\"")),
        ["Retry failed webhooks with exponential backoff."]
    );
    assert_eq!(found("\"backoff exponential\"").hits.len(), 0);
    assert_eq!(found("\"webhooks backoff\"~2").hits.len(), 1);
    assert_eq!(found("\"exponential back\"*").hits.len(), 1);

    assert_eq!(found("surface:JWT_SECRET").hits.len(), 1);
    assert_eq!(found("surface:jwt_secret").hits.len(), 0);
    assert_eq!(found("surface:(JWT_SECRET webhooks)").hits.len(), 2);
    assert_eq!(found("lemma:rotating").hits.len(), 1);

    // Weight reorders and never filters.
    let plain = found("(rotates limit)");
    let weighted = found("(rotates limit^4)");
    assert_eq!(plain.hits.len(), weighted.hits.len());
    assert!(weighted.hits[0].snippet.contains("limit"));

    // A word from elsewhere in the same message counts, and is reported apart.
    let outcome = found("surface:JWT_SECRET minutes");
    assert_eq!(outcome.hits[0].coverage, (2, 2));
    assert_eq!(outcome.hits[0].nearby, ["minutes"]);

    // The snippet is the sentence that matched, not the whole paragraph.
    assert_eq!(snippets(&found("cloning")), ["It was fixed by cloning."]);

    let outcome = found("(radiograph xray) reset");
    assert_eq!(outcome.unknown.len(), 2);
    assert_eq!(outcome.unknown[0].clause, "(radiograph OR xray)");

    // A typo is taken only when asked, and said to have been taken.
    assert_eq!(found("borow").hits.len(), 0);
    let outcome = search(&store, &index, "borow checker", true, &everything, (10, 2)).unwrap();
    assert_eq!(outcome.hits.len(), 1);
    assert_eq!(outcome.fuzzy[0].matched, ["borrow"]);

    let outcome = search(
        &store,
        &index,
        "(reset password jwt_secret minutes)",
        false,
        &everything,
        (10, 1),
    )
    .unwrap();
    let mut messages = Vec::new();
    for hit in &outcome.hits {
        assert!(
            !messages.contains(&hit.message),
            "two hits from one message"
        );
        messages.push(hit.message);
    }
    assert_eq!(
        search(&store, &index, "reset", false, &everything, (1, 2))
            .unwrap()
            .hits
            .len(),
        1
    );

    let session = found("backoff").hits[0].session;
    for (filter, wanted) in [
        (
            Filter {
                roles: vec![Role::Tool],
                ..Filter::default()
            },
            1,
        ),
        (
            Filter {
                roles: vec![Role::User, Role::Assistant],
                ..Filter::default()
            },
            3,
        ),
        (
            Filter {
                after: Some(1_500),
                before: Some(2_500),
                ..Filter::default()
            },
            1,
        ),
        (
            Filter {
                session: Some(session),
                ..Filter::default()
            },
            1,
        ),
    ] {
        let outcome = search(&store, &index, "(reset بازنشانی)", false, &filter, (10, 2)).unwrap();
        assert_eq!(outcome.hits.len(), wanted, "{filter:?}");
    }
}

#[test]
fn what_a_query_cannot_say() {
    let mut words = Vec::new();
    for at in 0..33 {
        words.push(format!("word{at}x"));
    }
    let words = words.join(" ");
    let (_home, store, index) = memory(&[
        ("s", Role::User, 1_000, "Rotate the token."),
        ("s", Role::User, 2_000, &words),
    ]);
    let refused =
        |query: &str| match search(&store, &index, query, false, &Filter::default(), (10, 2)) {
            Ok(outcome) => panic!("{query:?} was accepted with {} hits", outcome.hits.len()),
            Err(api::Error::Search(error)) => error,
            Err(error) => panic!("{query:?}: {error}"),
        };

    assert!(matches!(refused(""), Error::Empty));
    assert!(matches!(refused("\"rotate the"), Error::Syntax { .. }));
    assert!(matches!(refused("(rotate token"), Error::Syntax { .. }));
    assert!(matches!(refused("title:token"), Error::Field { .. }));
    for query in ["session:abc", "ts:1", "role:user"] {
        assert!(matches!(refused(query), Error::Filter { .. }), "{query}");
    }
    assert!(matches!(refused("/rot.te/"), Error::Regex));
    for query in ["[a TO b]", ">token", "ts:>5"] {
        assert!(
            matches!(refused(query), Error::Range | Error::Filter { .. }),
            "{query}"
        );
    }
    for query in ["*", "surface:*"] {
        assert!(matches!(refused(query), Error::Everything), "{query}");
    }
    assert!(matches!(refused("rot*"), Error::Prefix { .. }));
    for query in ["-token", "NOT token"] {
        assert!(matches!(refused(query), Error::Unwanted), "{query}");
    }
    assert!(matches!(refused(&words), Error::Clauses { count: 33 }));

    // Every refusal says what to write instead.
    assert!(
        refused("rot*")
            .to_string()
            .contains("(rotate rotated rotation)")
    );
}
