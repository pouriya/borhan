//! The server that the HTTP, MCP and command-line client tests talk to, and the
//! one call they make to it.

use std::path::PathBuf;

use borhan::api::{App, Permission};
use serde_json::Value;

/// Serves `storage` on a free loopback port from a thread of its own and
/// returns its origin. The listener is bound before this returns, so a first
/// request waits in the backlog instead of being refused.
pub fn serve(storage: PathBuf, token: Option<&str>, refused: &[Permission]) -> String {
    std::fs::create_dir_all(&storage).unwrap();
    let mut permissions = Vec::new();
    for permission in Permission::ALL {
        if !refused.contains(&permission) {
            permissions.push(permission);
        }
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let router = borhan::api::router(App::new(storage, token.map(str::to_string), permissions));
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            axum::serve(listener, router).await.unwrap();
        });
    });
    origin
}

/// Sends one request and returns its status and body. A body that is not JSON,
/// like the guide, comes back as a JSON string.
pub fn call(
    origin: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> (u16, Value) {
    // `http_status_as_error(false)`: a 404 here is the answer the test is
    // asserting on, and the body carrying it is not read by a client that
    // turns the status into an `Err` first.
    let agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .new_agent();
    let mut request = ureq::http::Request::builder()
        .method(method)
        .uri(format!("{origin}{path}"));
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = match body {
        Some(body) => {
            let request = request
                .header("Content-Type", "application/json")
                .body(body.to_string())
                .unwrap();
            agent.run(request)
        }
        None => agent.run(request.body(()).unwrap()),
    };
    let mut response = match response {
        Ok(response) => response,
        Err(error) => panic!("{method} {path}: {error}"),
    };
    let status = response.status().as_u16();
    let text = response.body_mut().read_to_string().unwrap();
    match serde_json::from_str(&text) {
        Ok(value) => (status, value),
        Err(_) => (status, Value::String(text)),
    }
}
