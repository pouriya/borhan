mod common;

use borhan::api::Permission;
use serde_json::{Value, json};

const DESCRIPTION: &str = "How our company works: who owns which service, why each policy exists, and what was tried before.";

const NOTE: &str = "# Password reset

Password reset links expire after 15 minutes, because longer-lived links kept showing up in forwarded emails.

- the link is single use
- the old password keeps working until the new one is set

| limit | value |
|---|---|
| requests per hour | 5 |

```
RESET_TTL=15m
```";

#[test]
fn a_memory_from_creation_to_deletion() {
    let home = tempfile::tempdir().unwrap();
    let origin = common::serve(home.path().join("storage"), None, &[]);
    let get = |path: &str| common::call(&origin, "GET", path, &[], None);
    let post = |path: &str, body: Value| common::call(&origin, "POST", path, &[], Some(body));

    let (status, health) = get("/api/v1/health");
    assert_eq!((status, &health["ok"]), (200, &json!(true)));

    let (status, guide) = common::call(
        &origin,
        "GET",
        "/",
        &[
            ("X-Forwarded-Proto", "https, http"),
            ("X-Forwarded-Host", "memory.example"),
        ],
        None,
    );
    assert_eq!(status, 200);
    assert!(
        guide
            .as_str()
            .unwrap()
            .contains("BORHAN=https://memory.example")
    );

    let response = ureq::get(&format!("{origin}/api/v1/health"))
        .call()
        .unwrap();
    assert_eq!(
        response.header("x-borhan-version"),
        Some(env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(response.header("x-trace-id").unwrap().len(), 26);

    // Creating.
    let created = json!({"name": "company", "description": DESCRIPTION});
    let (status, body) = post("/api/v1/memory", created.clone());
    assert_eq!(status, 200, "{body}");
    assert_eq!(post("/api/v1/memory", created).0, 409);
    for (name, description) in [
        ("Company", DESCRIPTION),
        ("", DESCRIPTION),
        ("a_name_well_over_the_forty_characters_allowed", DESCRIPTION),
        ("short", "Too few words."),
    ] {
        let (status, body) = post(
            "/api/v1/memory",
            json!({"name": name, "description": description}),
        );
        assert_eq!(status, 400, "{name}: {body}");
    }

    let (_, listed) = get("/api/v1/memory_list");
    assert_eq!(listed["memory_list"][0]["name"], "company");
    assert_eq!(listed["memory_list"][0]["languages"], "fa,en");

    // Adding.
    let (status, added) = post(
        "/api/v1/memory/company/message_list",
        json!({"session": "security-review", "message": "m1", "role": "user", "author": "pouriya", "ts": 1_000_000, "body": NOTE}),
    );
    assert_eq!(status, 200, "{added}");
    assert_eq!(added["units"], 6);
    let (status, _) = post(
        "/api/v1/memory/company/message_list",
        json!({"session": "security-review", "message": "m2", "role": "assistant", "ts": 2_000_000, "body": "لینک بازنشانی رمز عبور بعد از ۱۵ دقیقه منقضی می‌شود."}),
    );
    assert_eq!(status, 200);
    let (status, _) = post(
        "/api/v1/memory/company/message_list",
        json!({"session": "incidents", "role": "tool", "body": "We rate limit reset requests to 5 per hour per account."}),
    );
    assert_eq!(status, 200);
    for (body, wanted) in [
        (
            json!({"session": "security-review", "message": "m1", "body": "again"}),
            409,
        ),
        (
            json!({"session": "security-review", "role": "boss", "body": "who"}),
            400,
        ),
    ] {
        assert_eq!(post("/api/v1/memory/company/message_list", body).0, wanted);
    }
    assert_eq!(
        post(
            "/api/v1/memory/nobody/message_list",
            json!({"session": "s", "body": "text"})
        )
        .0,
        404
    );

    // What is on file.
    let (_, outline) = post("/api/v1/memory/company/outline", json!({}));
    assert_eq!(outline["session_list"].as_array().unwrap().len(), 2);
    let (_, outline) = post(
        "/api/v1/memory/company/outline",
        json!({"session": "security-review"}),
    );
    assert_eq!(outline["message_list"][1]["message"], "m2");
    assert_eq!(outline["message_list"][1]["author"], "assistant");
    assert_eq!(
        post("/api/v1/memory/company/outline", json!({"session": "nope"})).0,
        404
    );

    let (_, lexicon) = post(
        "/api/v1/memory/company/lexicon",
        json!({"word_list": ["Password", "links", "منقضی", "radiograph"]}),
    );
    let words = &lexicon["word_list"];
    assert_eq!(words[0]["surface_units"], 2);
    assert_eq!(words[1]["lemma"], "link");
    assert_eq!(words[2]["lemma_units"], 1);
    assert_eq!(words[3]["lemma_units"], 0);
    assert_eq!(
        post("/api/v1/memory/company/lexicon", json!({"word_list": []})).0,
        400
    );

    // Searching.
    let (status, found) = post(
        "/api/v1/memory/company/search",
        json!({"query": "(password passwords رمز) +(reset بازنشانی) radiograph"}),
    );
    assert_eq!(status, 200, "{found}");
    let hits = found["hit_list"].as_array().unwrap();
    assert_eq!(hits[0]["coverage"], json!([2, 2]));
    assert_eq!(found["unknown_list"][0]["word"], "radiograph");
    let session = hits[0]["session"].as_str().unwrap().to_string();
    let cursor = hits[0]["cursor"].as_str().unwrap().to_string();

    let (_, found) = post(
        "/api/v1/memory/company/search",
        json!({"query": "(reset بازنشانی)", "session": session, "role_list": ["assistant"], "after": 1_500_000, "before": 2_500_000, "limit": 5, "max_per_message": 1}),
    );
    let hits = found["hit_list"].as_array().unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["message_ref"], "m2");

    let (_, found) = post(
        "/api/v1/memory/company/search",
        json!({"query": "pasword", "fuzzy": true}),
    );
    assert_eq!(found["fuzzy_list"][0]["matched_list"], json!(["password"]));

    for body in [
        json!({"group_list": [{"word_list": ["reset"]}]}),
        json!({}),
        json!({"query": "reset", "session": "not-a-ulid"}),
        json!({"query": "reset", "role_list": ["boss"]}),
        json!({"query": "rot*"}),
    ] {
        let (status, answer) = post("/api/v1/memory/company/search", body);
        assert_eq!(status, 400, "{answer}");
        assert!(answer["error"].is_string());
    }

    // Reading back.
    let (_, read) = post(
        "/api/v1/memory/company/cursor",
        json!({"cursor_list": [cursor, "01ARZ3NDEKTSV4RRFFQ69G5FAV"], "before": 1, "after": 1}),
    );
    assert_eq!(read["missing_list"], json!(["01ARZ3NDEKTSV4RRFFQ69G5FAV"]));
    // One unit either side reaches across into the neighbouring messages.
    let messages = read["message_list"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    let mut anchors = 0;
    for message in messages {
        for unit in message["unit_list"].as_array().unwrap() {
            if unit["anchor"] == true {
                anchors += 1;
                assert_eq!(unit["unit"], cursor.as_str());
                assert_eq!(message["anchor"], true);
            }
        }
    }
    assert_eq!(anchors, 1);

    let (_, read) = post(
        "/api/v1/memory/company/cursor",
        json!({"cursor_list": [cursor], "messages": true}),
    );
    assert!(read["message_list"][0]["body"].is_string());
    for body in [json!({"cursor_list": []}), json!({"cursor_list": ["nope"]})] {
        assert_eq!(post("/api/v1/memory/company/cursor", body).0, 400);
    }

    // Changing.
    let (status, replaced) = post(
        "/api/v1/memory/company/message",
        json!({"session": "security-review", "message": "m1", "ts": 3_000_000, "body": "Reset links now expire after 10 minutes."}),
    );
    assert_eq!(status, 200, "{replaced}");
    assert_eq!(
        (&replaced["units"], &replaced["reindexed"]),
        (&json!(1), &json!(2))
    );
    for body in [
        json!({"session": "security-review", "message": "m9", "body": "text"}),
        json!({"session": "nope", "message": "m1", "body": "text"}),
    ] {
        assert_eq!(post("/api/v1/memory/company/message", body).0, 404);
    }

    let patch =
        |body: Value| common::call(&origin, "PATCH", "/api/v1/memory/company", &[], Some(body));
    assert_eq!(patch(json!({"languages": "en"})).0, 200);
    assert_eq!(patch(json!({})).0, 400);
    let (_, listed) = get("/api/v1/memory_list");
    assert_eq!(listed["memory_list"][0]["languages"], "en");

    let (_, rescanned) = post("/api/v1/memory/company/rescan", json!({}));
    assert_eq!(
        (&rescanned["messages"], &rescanned["units"]),
        (&json!(3), &json!(3))
    );

    let delete = || common::call(&origin, "DELETE", "/api/v1/memory/company", &[], None);
    let (status, deleted) = delete();
    assert_eq!(status, 200);
    assert_eq!(
        (&deleted["sessions"], &deleted["messages"]),
        (&json!(2), &json!(3))
    );
    assert_eq!(delete().0, 404);
    assert_eq!(
        post("/api/v1/memory/company/search", json!({"query": "reset"})).0,
        404
    );
}

#[test]
fn a_token_and_refusals_guard_the_server() {
    let home = tempfile::tempdir().unwrap();
    let origin = common::serve(
        home.path().join("storage"),
        Some("secret"),
        &[Permission::Delete, Permission::Replace],
    );
    let authorized = [("Authorization", "Bearer secret")];

    for headers in [&[][..], &[("Authorization", "Bearer wrong")][..]] {
        let (status, body) = common::call(&origin, "GET", "/api/v1/memory_list", headers, None);
        assert_eq!(status, 401);
        assert_eq!(body["error"], "missing or wrong token");
    }
    assert_eq!(
        common::call(&origin, "GET", "/no/such/route", &[], None).0,
        401
    );
    assert_eq!(common::call(&origin, "GET", "/", &[], None).0, 200);

    let (status, _) = common::call(
        &origin,
        "POST",
        "/api/v1/memory",
        &authorized,
        Some(json!({"name": "notes", "description": DESCRIPTION})),
    );
    assert_eq!(status, 200);
    let (status, refused) =
        common::call(&origin, "DELETE", "/api/v1/memory/notes", &authorized, None);
    assert_eq!(status, 403);
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("does not do delete")
    );
    let (status, _) = common::call(
        &origin,
        "POST",
        "/api/v1/memory/notes/message",
        &authorized,
        Some(json!({"session": "s", "message": "m", "body": "text"})),
    );
    assert_eq!(status, 403);
    assert_eq!(
        common::call(&origin, "GET", "/api/v1/no/such/route", &authorized, None).0,
        404
    );
}
