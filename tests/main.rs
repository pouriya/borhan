mod common;

use std::path::Path;
use std::process::{Command, Stdio};

use borhan::api::Permission;
use serde_json::Value;

const DESCRIPTION: &str = "How our company works: who owns which service, why each policy exists, and what was tried before.";

const NOTE: &str = "Password reset links expire after 15 minutes.

We rate limit reset requests to 5 per hour per account.";

/// Runs the binary with `home` as both `--home` and the user's home directory,
/// so nothing it writes lands outside the test's directory. Returns whether it
/// succeeded, its stdout and its stderr.
fn borhan(home: &Path, args: &[&str]) -> (bool, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_borhan"))
        .arg("--home")
        .arg(home)
        .args(args)
        .env_remove("BORHAN_HOME")
        .env("HOME", home)
        .env("USERPROFILE", home)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    (
        output.status.success(),
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

/// A command that has to succeed; its stdout.
fn ok(home: &Path, args: &[&str]) -> String {
    let (succeeded, stdout, stderr) = borhan(home, args);
    assert!(succeeded, "borhan {args:?}\n{stdout}\n{stderr}");
    stdout
}

/// A command that has to fail; its stderr.
fn fails(home: &Path, args: &[&str]) -> String {
    let (succeeded, stdout, stderr) = borhan(home, args);
    assert!(!succeeded, "borhan {args:?} succeeded\n{stdout}");
    stderr
}

fn json(home: &Path, args: &[&str]) -> Value {
    let mut args = args.to_vec();
    args.push("--json");
    serde_json::from_str(&ok(home, &args)).unwrap()
}

/// Everything a memory goes through, printed as text and as JSON. The same
/// test runs once on local storage and once through a server, because every
/// command has both paths.
fn every_command(home: &Path) {
    let id = ok(
        home,
        &["memory", "create", "company", "--description", DESCRIPTION],
    );
    assert_eq!(id.trim().len(), 26);
    let created = json(
        home,
        &[
            "memory",
            "create",
            "notes",
            "--description",
            DESCRIPTION,
            "--languages",
            "en",
        ],
    );
    assert_eq!(created["id"].as_str().unwrap().len(), 26);

    let listed = ok(home, &["memory", "list"]);
    assert!(
        listed.contains("company") && listed.contains("notes"),
        "{listed}"
    );
    assert_eq!(
        json(home, &["memory", "list"])["memory_list"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    assert!(fails(home, &["memory", "update", "notes"]).contains("--description"));
    ok(home, &["memory", "update", "notes", "--languages", "fa,en"]);
    json(
        home,
        &["memory", "update", "notes", "--description", DESCRIPTION],
    );

    let (succeeded, _, stderr) = borhan(
        home,
        &[
            "memory",
            "add",
            "company",
            NOTE,
            "--session",
            "security-review",
            "--message",
            "m1",
            "--author",
            "pouriya",
            "--ts",
            "1000",
        ],
    );
    assert!(succeeded, "{stderr}");
    assert!(stderr.contains("2 units"), "{stderr}");
    let added = json(
        home,
        &[
            "memory",
            "add",
            "company",
            "لینک بازنشانی رمز عبور منقضی می‌شود.",
            "--session",
            "security-review",
            "--role",
            "assistant",
            "--ts",
            "2000",
        ],
    );
    assert_eq!(added["units"], 1);
    fails(
        home,
        &[
            "memory",
            "add",
            "company",
            "text",
            "--session",
            "s",
            "--role",
            "boss",
        ],
    );
    fails(
        home,
        &[
            "memory",
            "add",
            "company",
            "again",
            "--session",
            "security-review",
            "--message",
            "m1",
        ],
    );

    assert!(ok(home, &["memory", "outline", "company"]).contains("security-review"));
    assert!(ok(home, &["memory", "outline", "company", "security-review"]).contains("pouriya"));
    assert_eq!(
        json(home, &["memory", "outline", "company"])["session_list"][0]["messages"],
        2
    );
    assert_eq!(
        json(home, &["memory", "outline", "company", "security-review"])["message_list"][1]["role"],
        "assistant"
    );

    let lexicon = ok(
        home,
        &[
            "memory",
            "lexicon",
            "company",
            "password",
            "منقضی",
            "radiograph",
        ],
    );
    assert!(lexicon.contains("radiograph"), "{lexicon}");
    assert_eq!(
        json(home, &["memory", "lexicon", "company", "reset"])["word_list"][0]["lemma_units"],
        2
    );
    assert!(fails(home, &["memory", "lexicon", "company"]).contains("at least one word"));

    let query = "(password رمز) +(reset بازنشانی) radiograph";
    let found = ok(home, &["memory", "search", "company", query]);
    // `~` marks an idea found only elsewhere in the hit's message.
    assert!(
        found.contains("[(reset OR بازنشانی),~(password OR رمز)]"),
        "{found}"
    );
    let found = json(home, &["memory", "search", "company", query]);
    let hits = found["hit_list"].as_array().unwrap();
    let cursor = hits[0]["cursor"].as_str().unwrap().to_string();
    let session = hits[0]["session"].as_str().unwrap().to_string();
    let filtered = json(
        home,
        &[
            "memory",
            "search",
            "company",
            "reset",
            "--session",
            &session,
            "--role",
            "user",
            "--after",
            "500",
            "--before",
            "1500",
            "--limit",
            "3",
            "--max-per-message",
            "1",
        ],
    );
    assert_eq!(filtered["hit_list"].as_array().unwrap().len(), 1);
    let (succeeded, fuzzy, stderr) =
        borhan(home, &["memory", "search", "company", "pasword", "--fuzzy"]);
    assert!(
        succeeded && fuzzy.contains("Password reset links"),
        "{fuzzy}"
    );
    assert!(
        stderr.contains("\"pasword\" matched nothing, taken as password"),
        "{stderr}"
    );
    let (_, _, stderr) = borhan(home, &["memory", "search", "company", "radiograph"]);
    assert!(
        stderr.contains("unknown: \"radiograph\"") && stderr.contains("No hits."),
        "{stderr}"
    );
    fails(
        home,
        &["memory", "search", "company", "reset", "--session", "nope"],
    );
    fails(
        home,
        &["memory", "search", "company", "reset", "--role", "boss"],
    );
    assert!(fails(home, &["memory", "search", "company", "rot*"]).contains("prefix"));
    fails(home, &["memory", "search", "nobody", "reset"]);

    let read = ok(
        home,
        &[
            "memory", "cursor", "company", &cursor, "--before", "0", "--after", "1",
        ],
    );
    assert!(read.contains('→'), "{read}");
    assert!(
        ok(
            home,
            &["memory", "cursor", "company", &cursor, "--messages"]
        )
        .contains("We rate limit")
    );
    let read = json(
        home,
        &[
            "memory",
            "cursor",
            "company",
            &cursor,
            "01ARZ3NDEKTSV4RRFFQ69G5FAV",
        ],
    );
    assert_eq!(read["missing_list"][0], "01ARZ3NDEKTSV4RRFFQ69G5FAV");
    // The top hit is the Persian message, the second of its session.
    let read = json(
        home,
        &["memory", "cursor", "company", &cursor, "--messages"],
    );
    assert_eq!(
        (
            &read["message_list"][0]["anchor"],
            &read["message_list"][1]["anchor"]
        ),
        (&Value::Bool(false), &Value::Bool(true))
    );
    fails(home, &["memory", "cursor", "company", "nope"]);

    let (succeeded, _, stderr) = borhan(
        home,
        &[
            "memory",
            "replace",
            "company",
            "Reset links expire after 10 minutes.",
            "--session",
            "security-review",
            "--message",
            "m1",
        ],
    );
    assert!(succeeded, "{stderr}");
    assert!(stderr.contains("1 units"), "{stderr}");
    let replaced = json(
        home,
        &[
            "memory",
            "replace",
            "company",
            "Reset links expire after 5 minutes.",
            "--session",
            "security-review",
            "--message",
            "m1",
            "--ts",
            "3000",
        ],
    );
    assert_eq!(replaced["reindexed"], 2);
    fails(
        home,
        &[
            "memory",
            "replace",
            "company",
            "text",
            "--session",
            "security-review",
            "--message",
            "m9",
        ],
    );

    let (succeeded, _, stderr) = borhan(home, &["memory", "rescan", "company"]);
    assert!(
        succeeded && stderr.contains("2 messages, 2 units"),
        "{stderr}"
    );
    assert_eq!(json(home, &["memory", "rescan", "company"])["units"], 2);

    assert!(fails(home, &["memory", "delete", "notes"]).contains("--yes"));
}

#[test]
fn commands_on_local_storage() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();

    assert!(ok(home, &[]).contains("Usage"));
    assert!(ok(home, &["memory"]).contains("borhan memory"));
    assert!(ok(home, &["skills"]).contains("borhan skills"));
    assert!(fails(home, &["memory", "list"]).contains("borhan never creates it"));
    assert!(fails(home, &["serve"]).contains("borhan never creates it"));

    assert!(ok(home, &["init"]).starts_with("Initialized"));
    ok(home, &["memory", "list"]);
    every_command(home);

    let again = ok(home, &["--info", "init"]);
    assert!(
        again.contains("Already initialized")
            && again.contains("Rescanned company: 2 messages, 2 units"),
        "{again}"
    );
    ok(home, &["--debug", "memory", "delete", "company", "--yes"]);
    assert_eq!(
        json(home, &["--trace", "memory", "delete", "notes", "--yes"])["name"],
        "notes"
    );

    // Skills print as they are, and install into every agent that is set up.
    assert!(ok(home, &["skills", "remember"]).starts_with("---"));
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    let installed = ok(home, &["skills", "survey", "--install"]);
    assert!(
        installed.contains("installed  agents") && installed.contains("installed  claude"),
        "{installed}"
    );
    assert!(installed.contains("skipped    codex"), "{installed}");
    assert!(home.join(".agents/skills/borhan-survey/SKILL.md").is_file());
    assert!(ok(home, &["skills", "survey", "--install"]).contains("replaced   claude"));
}

#[test]
fn commands_through_a_server() {
    let served = tempfile::tempdir().unwrap();
    let origin = common::serve(
        served.path().to_path_buf(),
        Some("secret"),
        &[Permission::Delete],
    );
    let listen = origin.strip_prefix("http://").unwrap();
    let home = tempfile::tempdir().unwrap();
    let home = home.path();

    assert!(
        ok(
            home,
            &[
                "init", "server", "--listen", listen, "--token", "secret", "--refuse", "delete"
            ]
        )
        .contains(listen)
    );
    assert!(fails(home, &["init", "server", "--listen", listen]).contains("already exists"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(home.join("server.toml"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // No `init`: every command below is answered by the server, which has its
    // own storage.
    every_command(home);
    let (_, listed) = common::call(
        &origin,
        "GET",
        "/api/v1/memory_list",
        &[("Authorization", "Bearer secret")],
        None,
    );
    assert_eq!(listed["memory_list"].as_array().unwrap().len(), 2);
    assert!(fails(home, &["memory", "delete", "company", "--yes"]).contains("refused"));

    // A server that does not answer, or does not accept the token, is not used.
    for token in ["wrong", ""] {
        let other = tempfile::tempdir().unwrap();
        let mut args = vec!["init", "server", "--listen", listen];
        if !token.is_empty() {
            args.extend(["--token", token]);
        }
        ok(other.path(), &args);
        assert!(fails(other.path(), &["memory", "list"]).contains("borhan never creates it"));
        // Its address is taken by the running server, on every platform.
        ok(other.path(), &["init"]);
        assert!(fails(other.path(), &["serve"]).contains("Could not listen"));
    }
    let closed = tempfile::tempdir().unwrap();
    ok(
        closed.path(),
        &["init", "server", "--listen", "127.0.0.1:1"],
    );
    ok(closed.path(), &["init"]);
    ok(closed.path(), &["memory", "list"]);

    let refusing = tempfile::tempdir().unwrap();
    assert!(
        fails(
            refusing.path(),
            &["init", "server", "--listen", listen, "--refuse", "fly"]
        )
        .contains("\"fly\" in refuse")
    );
    std::fs::write(
        refusing.path().join("server.toml"),
        "listen = \"127.0.0.1:1\"\nrefuse = [\"fly\"]\n",
    )
    .unwrap();
    ok(refusing.path(), &["init"]);
    assert!(fails(refusing.path(), &["serve"]).contains("\"fly\" in refuse"));
    std::fs::write(refusing.path().join("server.toml"), "listen = 5\n").unwrap();
    assert!(fails(refusing.path(), &["memory", "list"]).contains("Could not read"));
}
