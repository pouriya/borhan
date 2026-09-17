use std::sync::Mutex;

use borhan::api;
use borhan::index::{self, Index};
use borhan::normalize;
use borhan::storage::{Entry, Role, Storage};
use borhan::ulid::Ulid;

#[test]
fn an_index_from_older_rules_is_refused_until_rescanned() {
    let home = tempfile::tempdir().unwrap();
    let trace = Ulid::new().unwrap();
    let description = "Deployment notes: what broke during a release, why, and what changed so it would not again.";
    api::create(home.path(), "deploys", description, "en", &trace).unwrap();

    let store = Storage::open(home.path(), "deploys").unwrap();
    let built = Index::open(&store).unwrap();
    assert!(built.vocabulary(10).unwrap().is_empty());
    // A word in more than 15% of the units says nothing about which unit is
    // meant, so the vocabulary needs enough units to leave some words out.
    let mut paragraphs = vec![
        "The migration locked the orders table.",
        "Migrations now run before the deploy window.",
    ];
    paragraphs.extend(std::iter::repeat_n("Nothing else happened.", 12));
    let body = paragraphs.join("\n\n");
    let entry = Entry {
        session: "2026-09",
        message: None,
        author: "ops",
        role: Role::Tool,
        ts: Ulid::now(),
        body: &body,
    };
    let store = Mutex::new(store);
    let writer = Mutex::new(built.writer().unwrap());
    api::add(&store, &built, &writer, &entry, "deploys", &trace).unwrap();
    let vocabulary = built.vocabulary(2).unwrap();
    assert_eq!(
        vocabulary,
        [("migrat".to_string(), 2), ("the".to_string(), 2)]
    );
    drop(writer);
    drop(built);

    let store = store.into_inner().unwrap();
    store
        .set_meta(index::VERSION_KEY, &(normalize::VERSION - 1).to_string())
        .unwrap();
    let Err(error) = Index::open(&store) else {
        panic!("an index built by older rules was opened");
    };
    assert!(
        error.to_string().contains("run `borhan memory rescan`"),
        "{error}"
    );

    let attached = Index::attach(&store.directory.join(index::DIRECTORY)).unwrap();
    let writer = Mutex::new(attached.writer().unwrap());
    let store = Mutex::new(store);
    let (messages, units, _) = api::rescan(&store, &attached, &writer, "deploys", &trace).unwrap();
    assert_eq!((messages, units), (1, 14));
    drop(writer);
    drop(attached);
    Index::open(&store.into_inner().unwrap()).unwrap();
}
