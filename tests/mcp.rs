mod common;

use borhan::api::Permission;
use serde_json::{Value, json};

const DESCRIPTION: &str = "Notes on deployments: what broke, why it broke, and what we changed so it would not break again.";

/// The `result` of one JSON-RPC request, after checking it has no `error`.
fn result(origin: &str, method: &str, params: Value) -> Value {
    let (status, body) = common::call(
        origin,
        "POST",
        "/mcp",
        &[],
        Some(json!({"jsonrpc": "2.0", "id": 7, "method": method, "params": params})),
    );
    assert_eq!((status, &body["id"]), (200, &json!(7)), "{body}");
    assert!(body["error"].is_null(), "{method}: {body}");
    body["result"].clone()
}

/// A tool's answer, parsed out of its text content.
fn tool(origin: &str, name: &str, arguments: Value) -> Value {
    let called = result(
        origin,
        "tools/call",
        json!({"name": name, "arguments": arguments}),
    );
    assert_eq!(called["isError"], false, "{name}: {called}");
    serde_json::from_str(called["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[test]
fn every_tool_over_mcp() {
    let home = tempfile::tempdir().unwrap();
    let origin = common::serve(home.path().join("storage"), None, &[]);

    let initialized = result(
        &origin,
        "initialize",
        json!({"protocolVersion": "2024-11-05"}),
    );
    assert_eq!(initialized["protocolVersion"], "2024-11-05");
    assert_eq!(initialized["serverInfo"]["name"], "borhan");
    let initialized = result(
        &origin,
        "initialize",
        json!({"protocolVersion": "1999-01-01"}),
    );
    assert_eq!(initialized["protocolVersion"], "2025-11-25");
    assert_eq!(result(&origin, "ping", json!({})), json!({}));
    assert_eq!(
        result(&origin, "resources/templates/list", json!({}))["resourceTemplates"],
        json!([])
    );

    let listed = result(&origin, "tools/list", json!({}));
    let mut names = Vec::new();
    for tool in listed["tools"].as_array().unwrap() {
        names.push(tool["name"].as_str().unwrap().to_string());
    }
    assert_eq!(names.len(), 11, "{names:?}");

    tool(
        &origin,
        "memory_create",
        json!({"name": "deploys", "description": DESCRIPTION, "languages": "en"}),
    );
    let added = tool(
        &origin,
        "memory_add",
        json!({"memory": "deploys", "session": "2026-09", "message": "friday", "body": "The Friday deploy failed because the migration locked the orders table.\n\nWe now run migrations before the deploy window."}),
    );
    assert_eq!(added["units"], 2);
    assert_eq!(added["stats"].as_object().unwrap().len(), 1);
    tool(
        &origin,
        "memory_update",
        json!({"memory": "deploys", "description": format!("{DESCRIPTION} Also rollbacks.")}),
    );

    let memories = tool(&origin, "memory_list", json!({}));
    assert_eq!(memories["memory_list"][0]["units"], 2);
    let outline = tool(
        &origin,
        "memory_outline",
        json!({"memory": "deploys", "session": "2026-09"}),
    );
    assert_eq!(outline["message_list"][0]["message"], "friday");
    let lexicon = tool(
        &origin,
        "memory_lexicon",
        json!({"memory": "deploys", "word_list": ["migrations"]}),
    );
    assert_eq!(lexicon["word_list"][0]["lemma_units"], 2);

    let found = tool(
        &origin,
        "memory_search",
        json!({"memory": "deploys", "query": "+(migration migrations) (lock locked)"}),
    );
    let hit = &found["hit_list"][0];
    assert_eq!(hit["coverage"], json!([2, 2]));
    let read = tool(
        &origin,
        "memory_cursor",
        json!({"memory": "deploys", "cursor_list": [hit["cursor"]], "before": 0, "after": 0}),
    );
    assert_eq!(
        read["message_list"][0]["unit_list"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let replaced = tool(
        &origin,
        "memory_replace",
        json!({"memory": "deploys", "session": "2026-09", "message": "friday", "body": "Migrations run before the deploy window."}),
    );
    assert_eq!(replaced["units"], 1);
    let rescanned = tool(&origin, "memory_rescan", json!({"memory": "deploys"}));
    assert_eq!(rescanned["units"], 1);

    let resources = result(&origin, "resources/list", json!({}));
    let resource = &resources["resources"][0];
    assert_eq!(resource["uri"], "borhan://memory/deploys");
    assert_eq!(resource["_meta"]["borhan/units"], 1);
    let contents = result(
        &origin,
        "resources/read",
        json!({"uri": "borhan://memory/deploys"}),
    );
    let content: Value =
        serde_json::from_str(contents["contents"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(content["name"], "deploys");
    assert!(content["created_at"].as_str().unwrap().ends_with('Z'));

    // A failing operation is a tool result the model can read, not a protocol
    // error.
    let failed = result(
        &origin,
        "tools/call",
        json!({"name": "memory_search", "arguments": {"memory": "nobody", "query": "deploy"}}),
    );
    assert_eq!(failed["isError"], true);
    assert!(
        failed["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("nobody")
    );

    tool(&origin, "memory_delete", json!({"memory": "deploys"}));
    assert_eq!(
        result(&origin, "resources/list", json!({}))["resources"],
        json!([])
    );
}

#[test]
fn what_mcp_refuses() {
    let home = tempfile::tempdir().unwrap();
    let origin = common::serve(
        home.path().join("storage"),
        Some("secret"),
        &[Permission::Delete, Permission::Create],
    );
    let authorized = ("Authorization", "Bearer secret");
    let send = |headers: &[(&str, &str)], body: Value| {
        let mut all = vec![authorized];
        all.extend_from_slice(headers);
        common::call(&origin, "POST", "/mcp/", &all, Some(body))
    };
    let request = |method: &str, params: Value| {
        send(
            &[],
            json!({"jsonrpc": "2.0", "id": "x", "method": method, "params": params}),
        )
    };

    let (status, _) = common::call(
        &origin,
        "POST",
        "/mcp",
        &[],
        Some(json!({"jsonrpc": "2.0", "id": 1, "method": "ping"})),
    );
    assert_eq!(status, 401);

    let (_, listed) = request("tools/list", json!({}));
    let mut names = Vec::new();
    for tool in listed["result"]["tools"].as_array().unwrap() {
        names.push(tool["name"].as_str().unwrap().to_string());
    }
    assert!(!names.contains(&"memory_delete".to_string()));
    assert!(!names.contains(&"memory_create".to_string()));
    assert!(names.contains(&"memory_add".to_string()));

    let (_, refused) = request(
        "tools/call",
        json!({"name": "memory_create", "arguments": {"name": "notes", "description": DESCRIPTION}}),
    );
    assert_eq!(refused["result"]["isError"], true);

    for (params, code) in [
        (json!({"name": "memory_fly"}), -32602),
        (json!({"arguments": {}}), -32602),
        (
            json!({"name": "memory_search", "arguments": {"memory": "x", "limit": "ten"}}),
            -32602,
        ),
    ] {
        let (_, answer) = request("tools/call", params);
        assert_eq!(answer["error"]["code"], code, "{answer}");
    }
    for (params, code) in [
        (json!({}), -32602),
        (json!({"uri": "https://example.com"}), -32602),
        (json!({"uri": "borhan://memory/nobody"}), -32602),
    ] {
        let (_, answer) = request("resources/read", params);
        assert_eq!(answer["error"]["code"], code, "{answer}");
    }
    assert_eq!(request("tools/fly", json!({})).1["error"]["code"], -32601);
    assert_eq!(send(&[], json!({"id": 1})).1["error"]["code"], -32600);
    assert_eq!(request("server/discover", json!({})).0, 404);
    assert_eq!(
        send(
            &[],
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
        )
        .0,
        202
    );

    let (status, answer) = send(&[], json!("not an object"));
    assert_eq!((status, &answer["error"]["code"]), (200, &json!(-32600)));
    let (status, _) = common::call(
        &origin,
        "POST",
        "/mcp",
        &[authorized, ("Content-Type", "application/json")],
        None,
    );
    assert_eq!(status, 400);

    // Only a page served from this machine may call in from a browser.
    let ping = json!({"jsonrpc": "2.0", "id": 1, "method": "ping"});
    for (origin_header, wanted) in [
        ("http://localhost:3000", 200),
        ("http://127.0.0.1", 200),
        ("http://[::1]:8080", 200),
        ("https://evil.example", 403),
        ("null", 403),
    ] {
        assert_eq!(
            send(&[("Origin", origin_header)], ping.clone()).0,
            wanted,
            "{origin_header}"
        );
    }
}
