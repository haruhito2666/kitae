//! B2's protocol tests and its self-tested multi-step task.
//!
//! Everything here goes through real MCP frames on a real (in-memory) stdio
//! pair against a real local HTTP site: the point of tier B2 is that an
//! external client can complete a task, so the test is an external client.

use std::io::{BufReader, Cursor};
use std::net::TcpListener;
use std::sync::Arc;

use kitae_core::agent::{AgentSession, AllowList};
use kitae_headless::HeadlessRaster;
use kitae_json::Value;
use kitae_mcp::{McpServer, TOOLS};

/// A local site: `/` is a form, `/results` echoes what was submitted. The
/// engine fetches it over real HTTP on the network pool.
struct Site {
    addr: String,
}

impl Site {
    fn start() -> Site {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                std::thread::spawn(move || {
                    use std::io::{Read, Write};
                    let mut buf = [0u8; 4096];
                    let Ok(n) = stream.read(&mut buf) else { return };
                    let request = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let body = if path.starts_with("/results") {
                        let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
                        format!(
                            "<html><head><title>Results</title></head><body>\
                             <h1>Results</h1><p id=\"echo\">query={query}</p>\
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
                         </form>\
                         <p id=\"status\">Idle</p></body></html>"
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
        Site {
            addr: format!("http://{addr}"),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.addr)
    }
}

/// A server with everything allowed, over a font-backed session (so the
/// screenshot tool is exercised for real).
fn server(allow: &[&str]) -> McpServer {
    let session = match HeadlessRaster::load() {
        Ok(raster) => AgentSession::new(kitae_core::Browser::new(
            Box::new(kitae_core::agent::HeadlessHost::default()),
            Box::new(raster.measurer()),
            (800.0, 600.0),
        ))
        .with_raster(Arc::new(raster)),
        Err(_) => AgentSession::headless((800.0, 600.0)),
    };
    let policy = AllowList::new(allow);
    McpServer::new(session.with_policy(Box::new(policy)))
}

/// Drive one frame through the *stdio loop* and parse the reply.
fn call(server: &mut McpServer, frame: &str) -> Value {
    let input = Cursor::new(format!("{frame}\n"));
    let mut output: Vec<u8> = Vec::new();
    server
        .serve(BufReader::new(input), &mut output)
        .expect("serve");
    let text = String::from_utf8(output).expect("utf-8 frames");
    let line = text.lines().next().unwrap_or("");
    kitae_json::parse(line).unwrap_or_else(|e| panic!("reply not JSON ({e}): {line:?}"))
}

fn tool_call(server: &mut McpServer, id: u32, name: &str, args: &str) -> Value {
    let frame = format!(
        r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{name}","arguments":{args}}}}}"#
    );
    call(server, &frame)
}

/// The text of the first content block, parsed as JSON when it is one.
fn content_text(reply: &Value) -> String {
    reply
        .get("result")
        .and_then(|r| r.get("content"))
        .and_then(|c| c.as_arr())
        .and_then(|blocks| blocks.first())
        .and_then(|b| b.get("text"))
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string()
}

fn content_json(reply: &Value) -> Value {
    kitae_json::parse(&content_text(reply)).expect("content is JSON")
}

fn is_error(reply: &Value) -> bool {
    reply
        .get("result")
        .and_then(|r| r.get("isError"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

#[test]
fn initialize_and_tools_list_describe_the_whole_inventory() {
    let mut srv = server(&["*"]);
    let reply = call(
        &mut srv,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#,
    );
    assert_eq!(
        reply
            .get("result")
            .and_then(|r| r.get("protocolVersion"))
            .and_then(|v| v.as_str()),
        Some(kitae_mcp::PROTOCOL_VERSION)
    );
    assert_eq!(
        reply
            .get("result")
            .and_then(|r| r.get("serverInfo"))
            .and_then(|s| s.get("name"))
            .and_then(|v| v.as_str()),
        Some("kitae")
    );
    // The initialized notification produces no reply at all.
    let input = Cursor::new("{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n");
    let mut output: Vec<u8> = Vec::new();
    srv.serve(BufReader::new(input), &mut output).unwrap();
    assert!(output.is_empty(), "notifications are not answered");

    let reply = call(
        &mut srv,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
    );
    let tools = reply
        .get("result")
        .and_then(|r| r.get("tools"))
        .and_then(|t| t.as_arr())
        .expect("tools array");
    assert_eq!(tools.len(), TOOLS.len());
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(|v| v.as_str()))
        .collect();
    for expected in [
        "navigate",
        "read",
        "links",
        "find",
        "extract",
        "click",
        "fill",
        "submit",
        "back",
        "watch",
        "dispatch",
        "screenshot",
        "pdf",
        "archive",
        "verify",
    ] {
        assert!(names.contains(&expected), "missing tool `{expected}`");
    }
    for tool in tools {
        let schema = tool.get("inputSchema").expect("inputSchema");
        assert_eq!(
            schema.get("type").and_then(|v| v.as_str()),
            Some("object"),
            "schema for {:?} is an object",
            tool.get("name")
        );
        assert!(
            tool.get("description")
                .and_then(|v| v.as_str())
                .map(|d| d.len() > 20)
                .unwrap_or(false),
            "a description a model can act on"
        );
    }

    // Unknown methods are JSON-RPC errors, not silence.
    let reply = call(
        &mut srv,
        r#"{"jsonrpc":"2.0","id":3,"method":"resources/list"}"#,
    );
    assert_eq!(
        reply
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|v| v.as_f64()),
        Some(-32601.0)
    );
    // Garbage is a parse error with a null id.
    let reply = call(&mut srv, "{not json");
    assert_eq!(
        reply
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|v| v.as_f64()),
        Some(-32700.0)
    );
}

/// The tier's headline test: an external client completes a real multi-step
/// task — navigate, read, extract, fill, submit, verify the result — through
/// MCP frames and a real HTTP fetch.
#[test]
fn a_multi_step_task_completes_through_mcp() {
    let site = Site::start();
    let mut srv = server(&["*"]);

    // 1. navigate
    let reply = tool_call(
        &mut srv,
        1,
        "navigate",
        &format!(r#"{{"url":"{}"}}"#, site.url("/")),
    );
    assert!(!is_error(&reply), "{reply:?}");
    let page = content_json(&reply);
    assert_eq!(
        page.get("title").and_then(|v| v.as_str()),
        Some("Search"),
        "{page:?}"
    );

    // 2. extract: the form's fields are visible to the agent, with the
    //    constraint that matters.
    let reply = tool_call(&mut srv, 2, "extract", "{}");
    let doc = content_json(&reply);
    let form = doc
        .get("blocks")
        .and_then(|v| v.as_arr())
        .unwrap()
        .iter()
        .find(|b| b.get("kind").and_then(|v| v.as_str()) == Some("form"))
        .expect("the form is in the extraction");
    let fields = form.get("fields").and_then(|v| v.as_arr()).unwrap();
    let q = fields
        .iter()
        .find(|f| f.get("name").and_then(|v| v.as_str()) == Some("q"))
        .expect("the query field");
    assert_eq!(q.get("required").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(q.get("label").and_then(|v| v.as_str()), Some("Query"));

    // 3. links: the help page is reachable and named.
    let reply = tool_call(&mut srv, 3, "links", "{}");
    let links = content_json(&reply);
    let href = links
        .get("links")
        .and_then(|v| v.as_arr())
        .and_then(|l| l.first())
        .and_then(|l| l.get("href"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    assert_eq!(href, site.url("/help"));

    // 4. fill by selector — the control's value lands.
    let reply = tool_call(
        &mut srv,
        4,
        "fill",
        r#"{"selector":"input[name=q]","text":"tea"}"#,
    );
    assert!(!is_error(&reply), "{reply:?}");
    assert_eq!(
        content_json(&reply).get("value").and_then(|v| v.as_str()),
        Some("tea")
    );

    // 5. submit: a real GET to /results?q=tea.
    let reply = tool_call(&mut srv, 5, "submit", r#"{"form":"form"}"#);
    assert!(!is_error(&reply), "{reply:?}");
    let submitted = content_json(&reply);
    assert_eq!(
        submitted.get("navigated").and_then(|v| v.as_bool()),
        Some(true),
        "{submitted:?}"
    );

    // 6. read: the results page really shows the submitted value.
    let reply = tool_call(&mut srv, 6, "read", "{}");
    let text = content_text(&reply);
    let read: Value = kitae_json::parse(&text).expect("read result");
    let body = read.get("text").and_then(|v| v.as_str()).unwrap_or("");
    assert!(body.contains("Results"), "{body}");
    assert!(body.contains("query=q=tea"), "the query echoed: {body}");

    // 7. find: prove a claim about the page mechanically.
    let reply = tool_call(&mut srv, 7, "find", r#"{"query":"Successfully"}"#);
    assert_eq!(
        content_json(&reply).get("count").and_then(|v| v.as_f64()),
        Some(1.0)
    );

    // 8. back: history works over the real protocol.
    let reply = tool_call(&mut srv, 8, "back", "{}");
    let back = content_json(&reply);
    assert_eq!(back.get("moved").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(
        back.get("url").and_then(|v| v.as_str()),
        Some(site.url("/").as_str())
    );
}

#[test]
fn validation_failures_come_back_as_data_not_disconnects() {
    let site = Site::start();
    let mut srv = server(&["*"]);
    tool_call(
        &mut srv,
        1,
        "navigate",
        &format!(r#"{{"url":"{}"}}"#, site.url("/")),
    );
    // Submitting with the required field empty: the engine refuses, and the
    // agent is told which control blocked it.
    let reply = tool_call(&mut srv, 2, "submit", r#"{"form":"form"}"#);
    assert!(
        !is_error(&reply),
        "a validation stop is a result: {reply:?}"
    );
    let submitted = content_json(&reply);
    assert_eq!(
        submitted.get("validationFailed").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        submitted.get("navigated").and_then(|v| v.as_bool()),
        Some(false)
    );
    // A selector that matches nothing is an *error result* naming the miss.
    let reply = tool_call(&mut srv, 3, "click", r#"{"selector":".checkout"}"#);
    assert!(is_error(&reply));
    assert!(
        content_text(&reply).contains("matches"),
        "the miss explains itself: {:?}",
        content_text(&reply)
    );
}

#[test]
fn the_policy_decides_what_a_client_may_do() {
    let site = Site::start();
    // The default posture: read-only. Navigation itself is not in it, so the
    // client sees a refusal that teaches the flag.
    let mut srv = server(&[]);
    let reply = tool_call(
        &mut srv,
        1,
        "navigate",
        &format!(r#"{{"url":"{}"}}"#, site.url("/")),
    );
    assert!(is_error(&reply));
    let message = content_text(&reply);
    assert!(
        message.contains("permission denied") && message.contains("--allow navigate"),
        "{message}"
    );
    // Read-only verbs still work (there is no page, so `read` reports that
    // honestly rather than pretending).
    let reply = tool_call(&mut srv, 2, "read", "{}");
    assert!(!is_error(&reply));

    // A session that allows navigation but not submission can browse and
    // read, and is stopped at the mutation.
    let mut srv = server(&["navigate", "back"]);
    let reply = tool_call(
        &mut srv,
        3,
        "navigate",
        &format!(r#"{{"url":"{}"}}"#, site.url("/")),
    );
    assert!(!is_error(&reply), "{reply:?}");
    let reply = tool_call(&mut srv, 4, "submit", r#"{"form":"form"}"#);
    assert!(is_error(&reply));
    assert!(content_text(&reply).contains("--allow submit"));
}

#[test]
fn screenshot_and_pdf_travel_as_image_and_resource_blocks() {
    let site = Site::start();
    let mut srv = server(&["*"]);
    tool_call(
        &mut srv,
        1,
        "navigate",
        &format!(r#"{{"url":"{}"}}"#, site.url("/")),
    );

    let reply = tool_call(&mut srv, 2, "screenshot", "{}");
    let blocks = reply
        .get("result")
        .and_then(|r| r.get("content"))
        .and_then(|c| c.as_arr())
        .expect("content");
    let image = blocks
        .iter()
        .find(|b| b.get("type").and_then(|v| v.as_str()) == Some("image"))
        .expect("an image block");
    assert_eq!(
        image.get("mimeType").and_then(|v| v.as_str()),
        Some("image/png")
    );
    let data = image.get("data").and_then(|v| v.as_str()).unwrap_or("");
    assert!(data.starts_with("iVBORw0KGgo"), "PNG bytes in the block");

    let reply = tool_call(&mut srv, 3, "pdf", "{}");
    let blocks = reply
        .get("result")
        .and_then(|r| r.get("content"))
        .and_then(|c| c.as_arr())
        .expect("content");
    let resource = blocks
        .iter()
        .find(|b| b.get("type").and_then(|v| v.as_str()) == Some("resource"))
        .expect("a resource block");
    assert_eq!(
        resource
            .get("resource")
            .and_then(|r| r.get("mimeType"))
            .and_then(|v| v.as_str()),
        Some("application/pdf")
    );
    assert!(resource
        .get("resource")
        .and_then(|r| r.get("blob"))
        .and_then(|v| v.as_str())
        .map(|b| b.starts_with("JVBERi0xLjQ"))
        .unwrap_or(false));

    // Writing to a path instead of returning bytes.
    let dir = std::env::temp_dir().join(format!("kitae_mcp_art_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("page.png");
    let reply = tool_call(
        &mut srv,
        4,
        "screenshot",
        &format!(r#"{{"path":"{}"}}"#, path.to_string_lossy()),
    );
    assert!(!is_error(&reply), "{reply:?}");
    assert!(path.exists(), "the file was written");
    assert!(content_text(&reply).contains("PNG written to"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn evidence_is_archived_and_verified_through_mcp() {
    let site = Site::start();
    let mut srv = server(&["*"]);
    tool_call(
        &mut srv,
        1,
        "navigate",
        &format!(r#"{{"url":"{}"}}"#, site.url("/")),
    );
    let dir = std::env::temp_dir().join(format!("kitae_mcp_ev_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let reply = tool_call(
        &mut srv,
        2,
        "archive",
        &format!(r#"{{"dir":"{}"}}"#, dir.to_string_lossy()),
    );
    assert!(!is_error(&reply), "{reply:?}");
    let manifest = content_json(&reply);
    assert_eq!(
        manifest.get("signature").and_then(|v| v.as_str()),
        Some("valid"),
        "{manifest:?}"
    );
    let manifest_path = manifest
        .get("manifestPath")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();

    let reply = tool_call(
        &mut srv,
        3,
        "verify",
        &format!(r#"{{"path":"{manifest_path}"}}"#),
    );
    let verified = content_json(&reply);
    assert_eq!(
        verified.get("ok").and_then(|v| v.as_bool()),
        Some(true),
        "{verified:?}"
    );
    assert_eq!(
        verified.get("replay").and_then(|v| v.as_str()),
        Some("match")
    );
    let _ = std::fs::remove_dir_all(&dir);
}
