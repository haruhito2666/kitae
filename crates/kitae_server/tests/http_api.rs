//! B3's tests: the HTTP API driven over real sockets against a real local
//! site, including the multi-step task the tier is judged on.
//!
//! The site and the request helper are deliberately small and duplicated
//! from the MCP tests: a test fixture that reaches into production code to
//! make itself convenient stops testing the wire.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use kitae_json::Value;
use kitae_server::{Server, ServerConfig};

/// A local site: `/` is a form, `/results` echoes the query.
fn start_site() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind site");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                let Ok(n) = stream.read(&mut buf) else { return };
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                let body = if path.starts_with("/results") {
                    let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
                    format!(
                        "<html><head><title>Results</title></head><body>\
                         <h1>Results</h1><p>query={query}</p>\
                         <p>Submitted successfully.</p></body></html>"
                    )
                } else {
                    "<html><head><title>Search</title></head><body>\
                     <h1>Search the catalogue</h1>\
                     <p>Use the <a href=\"/help\">help page</a> if unsure.</p>\
                     <form action=\"/results\" method=\"get\">\
                     <label for=\"q\">Query</label>\
                     <input id=\"q\" name=\"q\" type=\"text\" required placeholder=\"what to find\">\
                     <input type=\"submit\" value=\"Search\">\
                     </form></body></html>"
                        .to_string()
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            });
        }
    });
    format!("http://{addr}")
}

/// One raw HTTP request against the server, returning (status, JSON body).
fn http(addr: &str, method: &str, target: &str, body: &str) -> (u16, Value) {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("timeout");
    let request = format!(
        "{method} {target} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).expect("write");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read");
    let text = String::from_utf8_lossy(&response).to_string();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text.as_str(), ""));
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .expect("status code");
    let value =
        kitae_json::parse(body.trim()).unwrap_or_else(|e| panic!("body not JSON ({e}): {body:?}"));
    (status, value)
}

/// Start a server on an ephemeral port with everything allowed.
fn start_server(allow: &[&str]) -> String {
    let config = ServerConfig {
        allow: allow.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    };
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind server");
    let addr = listener.local_addr().expect("addr").to_string();
    let server = Server::new(config);
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    addr
}

#[test]
fn health_verbs_and_sessions_are_served() {
    let addr = start_server(&["*"]);
    let (status, health) = http(&addr, "GET", "/health", "");
    assert_eq!(status, 200);
    assert_eq!(health.get("ok").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(
        health.get("name").and_then(|v| v.as_str()),
        Some("kitae-server")
    );

    // The inventory is the MCP inventory: same names, same schemas.
    let (status, verbs) = http(&addr, "GET", "/verbs", "");
    assert_eq!(status, 200);
    let tools = verbs.get("tools").and_then(|v| v.as_arr()).expect("tools");
    assert_eq!(tools.len(), kitae_mcp::TOOLS.len());
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(|v| v.as_str()))
        .collect();
    assert!(names.contains(&"extract") && names.contains(&"batch") == false);
    assert!(tools.iter().all(|t| t.get("inputSchema").is_some()));

    // Sessions are created, listed and dropped by name.
    let (status, created) = http(
        &addr,
        "POST",
        "/sessions",
        r#"{"session":"reader","allow":["navigate","read","extract"]}"#,
    );
    assert_eq!(status, 201, "{created:?}");
    let (status, list) = http(&addr, "GET", "/sessions", "");
    assert_eq!(status, 200);
    let sessions: Vec<&str> = list
        .get("sessions")
        .and_then(|v| v.as_arr())
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(sessions.contains(&"reader"), "{list:?}");
    let (status, dropped) = http(&addr, "DELETE", "/sessions/reader", "");
    assert_eq!(status, 200, "{dropped:?}");
    let (status, missing) = http(&addr, "DELETE", "/sessions/reader", "");
    assert_eq!(status, 404);

    // Routing errors are legible.
    let (status, _) = http(&addr, "GET", "/nope", "");
    assert_eq!(status, 404);
    let (status, body) = http(&addr, "POST", "/act", "{not json");
    assert_eq!(status, 400);
    assert!(body
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .contains("not JSON"));
}

/// The tier's headline test: one HTTP client completes a multi-step task —
/// navigate, read, extract, fill, submit, confirm — over real sockets.
#[test]
fn a_multi_step_task_completes_over_http() {
    let site = start_site();
    let addr = start_server(&["*"]);

    let (status, nav) = http(
        &addr,
        "POST",
        "/act?session=task",
        &format!(r#"{{"verb":"navigate","url":"{site}/"}}"#),
    );
    assert_eq!(status, 200, "{nav:?}");
    assert_eq!(
        nav.get("result")
            .and_then(|r| r.get("title"))
            .and_then(|v| v.as_str()),
        Some("Search")
    );

    // The batch endpoint: a form fill + submit + read in one round trip.
    let body = format!(
        r#"{{"verbs":[
            {{"verb":"fill","selector":"input[name=q]","text":"engine oil"}},
            {{"verb":"submit","form":"form"}},
            {{"verb":"read"}}
        ]}}"#
    );
    let (status, batch) = http(&addr, "POST", "/batch?session=task", &body);
    assert_eq!(status, 200, "{batch:?}");
    assert_eq!(batch.get("ok").and_then(|v| v.as_bool()), Some(true));
    let results = batch
        .get("results")
        .and_then(|v| v.as_arr())
        .expect("results");
    assert_eq!(results.len(), 3);
    assert_eq!(
        results[0].get("verb").and_then(|v| v.as_str()),
        Some("fill")
    );
    assert_eq!(
        results[0]
            .get("result")
            .and_then(|r| r.get("value"))
            .and_then(|v| v.as_str()),
        Some("engine oil")
    );
    let submitted = results[1].get("result").expect("submit result");
    assert_eq!(
        submitted.get("navigated").and_then(|v| v.as_bool()),
        Some(true),
        "{submitted:?}"
    );
    // The read at step three must show the results page with the query.
    let text = results[2]
        .get("result")
        .and_then(|r| r.get("text"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(text.contains("Results"), "{text}");
    assert!(
        text.contains("query=q=engine"),
        "the submitted value round-tripped: {text}"
    );

    // A batch stops at its first failure and says how far it got.
    let body = r#"{"verbs":[
        {"verb":"click","selector":".checkout"},
        {"verb":"read"}
    ]}"#;
    let (status, batch) = http(&addr, "POST", "/batch?session=task", body);
    assert_eq!(status, 422, "{batch:?}");
    assert_eq!(batch.get("ok").and_then(|v| v.as_bool()), Some(false));
    assert_eq!(batch.get("completed").and_then(|v| v.as_f64()), Some(0.0));

    // Session isolation: another session is still on its own page (none).
    let (status, read) = http(&addr, "POST", "/act?session=other", r#"{"verb":"read"}"#);
    assert_eq!(status, 200, "{read:?}");
    assert_eq!(
        read.get("result")
            .and_then(|r| r.get("text"))
            .and_then(|v| v.as_str()),
        Some("")
    );
}

#[test]
fn the_policy_is_enforced_per_request_and_per_session() {
    let site = start_site();
    // Default posture: read-only.
    let addr = start_server(&[]);
    let (status, denied) = http(
        &addr,
        "POST",
        "/act",
        &format!(r#"{{"verb":"navigate","url":"{site}/"}}"#),
    );
    assert_eq!(status, 403, "{denied:?}");
    let message = denied.get("error").and_then(|v| v.as_str()).unwrap_or("");
    assert!(message.contains("permission denied"), "{message}");
    let (status, _) = http(&addr, "POST", "/act", r#"{"verb":"extract"}"#);
    assert_eq!(status, 200, "read-only verbs still work");

    // A session created with its own allow-list is bounded by it.
    http(
        &addr,
        "POST",
        "/sessions",
        r#"{"session":"browsing","allow":["navigate","read"]}"#,
    );
    let (status, _) = http(
        &addr,
        "POST",
        "/act?session=browsing",
        &format!(r#"{{"verb":"navigate","url":"{site}/"}}"#),
    );
    assert_eq!(status, 200);
    let (status, denied) = http(
        &addr,
        "POST",
        "/act?session=browsing",
        r#"{"verb":"submit","form":"form"}"#,
    );
    assert_eq!(status, 403);
    assert!(denied
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .contains("--allow"));

    // An unknown verb name is a client error, not a server error.
    let (status, body) = http(&addr, "POST", "/act", r#"{"verb":"teleport"}"#);
    assert_eq!(status, 400);
    assert!(body
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .contains("unknown tool"));
}

#[test]
fn screenshots_come_back_as_base64_over_http() {
    let site = start_site();
    let addr = start_server(&["*"]);
    http(
        &addr,
        "POST",
        "/act",
        &format!(r#"{{"verb":"navigate","url":"{site}/"}}"#),
    );
    let (status, shot) = http(&addr, "POST", "/act", r#"{"verb":"screenshot"}"#);
    assert_eq!(status, 200, "{shot:?}");
    let result = shot.get("result").expect("result");
    assert_eq!(
        result.get("mime").and_then(|v| v.as_str()),
        Some("image/png")
    );
    let data = result.get("png").and_then(|v| v.as_str()).unwrap_or("");
    assert!(data.starts_with("iVBORw0KGgo"), "PNG bytes");
    assert!(
        result.get("bytes").and_then(|v| v.as_f64()).unwrap_or(0.0) > 1000.0,
        "a real image: {result:?}"
    );
}
