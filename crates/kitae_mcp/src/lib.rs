//! kitae_mcp — Track B's tier B2: the Model Context Protocol server.
//!
//! An MCP client (Claude Desktop, an editor, an agent framework) speaks
//! JSON-RPC 2.0 over stdio; behind each `tools/call` is a real Kitae
//! `Browser` driven through the shared verb vocabulary. Nothing here
//! reimplements browsing — the protocol layer maps names to
//! `kitae_agent::Verb` and shapes results, and
//! `kitae_core::agent::AgentSession` does the work. One implementation of
//! every verb, so `click` cannot mean something different over MCP than it
//! does in the app.
//!
//! Transport: one JSON-RPC message per line on stdin/stdout (the MCP stdio
//! convention). **stdout carries protocol frames only** — logs and progress
//! go to stderr, because a stray print would desync the client.
//!
//! Methods: `initialize`, `notifications/initialized`, `ping`,
//! `tools/list`, `tools/call`. Unknown methods get a JSON-RPC
//! `-32601` (method not found); malformed frames get `-32700` (parse
//! error). A tool that fails returns a *result* with `isError: true` and
//! the message as text — that is what MCP clients show the model, and an
//! agent needs to see "nothing matches `.checkout`" as data, not as a
//! dropped connection.
//!
//! Permissions: the server is the policy owner. `--allow` names the verbs a
//! client may use (default: read-only), and refusals come back as
//! `isError: true` text naming the flag that would allow them — an agent
//! that hits a refusal learns how to ask for the capability.

use std::io::{BufRead, Write};

use kitae_agent::{Target, Verb};
use kitae_core::agent::{AgentSession, AllowList, PermissionPolicy};
use kitae_json::Value;

/// The protocol revision this server implements.
pub const PROTOCOL_VERSION: &str = "2025-06-18";
pub const SERVER_NAME: &str = "kitae";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// One MCP tool: the verb it maps to, plus the schema the client sees.
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    /// JSON Schema (hand-written; these are the contract an agent reads).
    pub schema: &'static str,
}

/// The tool inventory. One row per verb — the same inventory the JS surface
/// and the HTTP API use, described for a model instead of a script.
pub const TOOLS: &[ToolSpec] = &[
    ToolSpec {
        name: "navigate",
        description: "Load a URL and let the page settle (scripts, fetches, timers). \
                      Returns the loaded URL, title, word count and any fetch error.",
        schema: r#"{"type":"object","properties":{"url":{"type":"string","description":"Absolute http(s) URL to load"}},"required":["url"],"additionalProperties":false}"#,
    },
    ToolSpec {
        name: "read",
        description: "The current page as plain text, with url, title and word count. \
                      The cheapest way to see what is on the page.",
        schema: r#"{"type":"object","properties":{},"additionalProperties":false}"#,
    },
    ToolSpec {
        name: "links",
        description: "Every link the layout painted, one entry per anchor: resolved href, \
                      the text it shows, and the box an agent should click.",
        schema: r#"{"type":"object","properties":{},"additionalProperties":false}"#,
    },
    ToolSpec {
        name: "find",
        description: "In-page text search: one match per painted text run, with the rects it \
                      covers. Use it to check whether text exists before clicking.",
        schema: r#"{"type":"object","properties":{"query":{"type":"string"}},"required":["query"],"additionalProperties":false}"#,
    },
    ToolSpec {
        name: "extract",
        description: "The structured page-as-data document: headings, paragraph/list/table/\
                      form/image blocks in document order, each with its DOM node and box, plus \
                      sections and telemetry (third-party hosts, refusals). This is the input \
                      layer for multi-step work.",
        schema: r#"{"type":"object","properties":{},"additionalProperties":false}"#,
    },
    ToolSpec {
        name: "click",
        description: "Click an element (by CSS selector) or a page point. Returns which node \
                      was clicked and whether the click navigated.",
        schema: r#"{"type":"object","properties":{"selector":{"type":"string","description":"CSS selector of the element to click"},"x":{"type":"number"},"y":{"type":"number","description":"Page coordinates; give both x and y instead of a selector"}},"additionalProperties":false}"#,
    },
    ToolSpec {
        name: "fill",
        description: "Set a form field's value through the engine's own input path (focus, \
                      value write, input/change events). Address it with a CSS selector; the \
                      element does not need an id.",
        schema: r#"{"type":"object","properties":{"selector":{"type":"string"},"text":{"type":"string"}},"required":["selector","text"],"additionalProperties":false}"#,
    },
    ToolSpec {
        name: "submit",
        description: "Submit a form. Give a selector for the <form>, or omit it to submit the \
                      form owning the focused control. Constraint validation runs first: \
                      invalid controls are reported instead of navigating.",
        schema: r#"{"type":"object","properties":{"form":{"type":"string","description":"CSS selector of the <form> element"}},"additionalProperties":false}"#,
    },
    ToolSpec {
        name: "back",
        description: "Go back one step in history and settle the previous document.",
        schema: r#"{"type":"object","properties":{},"additionalProperties":false}"#,
    },
    ToolSpec {
        name: "watch",
        description: "Wait until a CSS selector appears, polling the page's own machinery \
                      (scripts, fetches, timers). Returns matched, elapsedMs and the box when \
                      it matched. Use it after an action that loads content asynchronously.",
        schema: r#"{"type":"object","properties":{"selector":{"type":"string"},"timeoutMs":{"type":"number","description":"How long to wait (default 5000, capped at 10000)"}},"required":["selector"],"additionalProperties":false}"#,
    },
    ToolSpec {
        name: "dispatch",
        description: "Dispatch a DOM event (click, input, change, or a custom type) at an \
                      element. Needs page JavaScript; fails clearly when the page has none.",
        schema: r#"{"type":"object","properties":{"selector":{"type":"string"},"event":{"type":"string"},"detail":{"type":"string","description":"Optional event detail payload"}},"required":["selector","event"],"additionalProperties":false}"#,
    },
    ToolSpec {
        name: "screenshot",
        description: "PNG of the painted page (whole document, device pixels). With a path the \
                      PNG is written there; without one the image is returned inline.",
        schema: r#"{"type":"object","properties":{"path":{"type":"string","description":"Where to write the PNG"}},"additionalProperties":false}"#,
    },
    ToolSpec {
        name: "pdf",
        description: "Print-ready PDF of the page (US Letter, one image per page — the text in \
                      it is not selectable). With a path it is written there, else returned as \
                      a base64 blob.",
        schema: r#"{"type":"object","properties":{"path":{"type":"string"}},"additionalProperties":false}"#,
    },
    ToolSpec {
        name: "archive",
        description: "Archive the page as signed QTSQ evidence plus a manifest of digests \
                      (content, replay extraction, live extraction). Returns both paths.",
        schema: r#"{"type":"object","properties":{"dir":{"type":"string","description":"Directory for the archive (default: $KITAE_EVIDENCE_DIR or a temp dir)"}},"additionalProperties":false}"#,
    },
    ToolSpec {
        name: "verify",
        description: "Verify an evidence archive or manifest: signature, content digest, and a \
                      replay that re-extracts the archived bytes. Reports every check \
                      individually.",
        schema: r#"{"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}"#,
    },
];

/// The server: one session, one client, one line at a time.
pub struct McpServer {
    session: AgentSession,
    initialized: bool,
    /// Set by `initialize`; the client's protocol revision.
    client_protocol: Option<String>,
}

impl McpServer {
    pub fn new(session: AgentSession) -> Self {
        Self {
            session,
            initialized: false,
            client_protocol: None,
        }
    }

    pub fn session(&self) -> &AgentSession {
        &self.session
    }

    pub fn session_mut(&mut self) -> &mut AgentSession {
        &mut self.session
    }

    /// Handle one JSON-RPC message. `None` for notifications (nothing to
    /// reply to) and for unparseable input that already got its error.
    pub fn handle(&mut self, message: &str) -> Option<String> {
        let request = match kitae_json::parse(message) {
            Ok(value) => value,
            Err(e) => {
                return Some(error_response(
                    &Value::Null,
                    -32700,
                    &format!("parse error: {e}"),
                ))
            }
        };
        let id = request.get("id").cloned();
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        if method.is_empty() {
            // A response or garbage addressed to us: nothing to do.
            return match id {
                Some(id) => Some(error_response(&id, -32600, "not a request")),
                None => None,
            };
        }
        let params = request.get("params").cloned().unwrap_or(Value::Null);
        // Notifications carry no id: no response, whatever the method.
        let is_notification = id.is_none();
        let result = match method {
            "initialize" => Ok(self.initialize(&params)),
            "notifications/initialized" => {
                self.initialized = true;
                return None;
            }
            "ping" => Ok(Value::obj(vec![])),
            "tools/list" => Ok(self.tools_list()),
            "tools/call" => Ok(self.tools_call(&params)),
            other => Err((-32601, format!("method not found: {other}"))),
        };
        if is_notification {
            return None;
        }
        let id = id.unwrap_or(Value::Null);
        match result {
            Ok(result) => Some(success_response(&id, &result)),
            Err((code, message)) => Some(error_response(&id, code, &message)),
        }
    }

    fn initialize(&mut self, params: &Value) -> Value {
        self.client_protocol = params
            .get("protocolVersion")
            .and_then(Value::as_str)
            .map(str::to_string);
        Value::obj(vec![
            ("protocolVersion", Value::s(PROTOCOL_VERSION)),
            (
                "capabilities",
                Value::obj(vec![(
                    "tools",
                    Value::obj(vec![("listChanged", Value::Bool(false))]),
                )]),
            ),
            (
                "serverInfo",
                Value::obj(vec![
                    ("name", Value::s(SERVER_NAME)),
                    ("version", Value::s(SERVER_VERSION)),
                ]),
            ),
            (
                "instructions",
                Value::s(
                    "Drives a real Kitae browser. `extract` returns the page as structured \
                     data with provenance; `read` returns its text. Verbs outside this \
                     session's allow-list return isError with the flag that would permit \
                     them.",
                ),
            ),
        ])
    }

    fn tools_list(&self) -> Value {
        tools_json()
    }

    /// `tools/call`: name → verb → session → MCP content. Failures are
    /// results with `isError`, not JSON-RPC errors: the model must see them.
    fn tools_call(&mut self, params: &Value) -> Value {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return error_result("tools/call needs a `name`");
        };
        let args = params.get("arguments").cloned().unwrap_or(Value::Null);
        let verb = match verb_for(name, &args) {
            Ok(verb) => verb,
            Err(message) => return error_result(&message),
        };
        match self.session.permit_and_run(&verb) {
            Ok(result) => content_for(name, &result),
            Err(message) => error_result(&message),
        }
    }

    /// The stdio loop: one JSON-RPC frame per line in, one per line out.
    /// Blank lines are ignored; EOF ends the server.
    pub fn serve<R: BufRead, W: Write>(&mut self, input: R, mut output: W) -> std::io::Result<()> {
        for line in input.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let response = self.handle(&line);
            if let Some(response) = response {
                writeln!(output, "{response}")?;
                output.flush()?;
            }
        }
        Ok(())
    }
}

/// The JSON-RPC request → verb mapping. Argument validation errors are the
/// kind an agent can fix from the message alone.
pub fn verb_for(name: &str, args: &Value) -> Result<Verb, String> {
    let string_arg = |key: &str| -> Result<String, String> {
        args.get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("`{name}` needs a non-empty string `{key}`"))
    };
    let optional_path = |key: &str| -> Option<String> {
        args.get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.is_empty())
    };
    Ok(match name {
        "navigate" => Verb::Navigate {
            url: string_arg("url")?,
        },
        "read" => Verb::Read,
        "links" => Verb::Links,
        "find" => Verb::Find {
            query: string_arg("query")?,
        },
        "extract" => Verb::Extract,
        "click" => {
            let selector = optional_path("selector");
            let point = match (
                args.get("x").and_then(Value::as_f64),
                args.get("y").and_then(Value::as_f64),
            ) {
                (Some(x), Some(y)) => Some(Target::Point {
                    x: x as f32,
                    y: y as f32,
                }),
                _ => None,
            };
            match (selector, point) {
                (Some(selector), _) => Verb::Click {
                    target: Target::Selector(selector),
                },
                (None, Some(point)) => Verb::Click { target: point },
                (None, None) => {
                    return Err("`click` needs a `selector`, or both `x` and `y`".to_string())
                }
            }
        }
        "fill" => Verb::Fill {
            target: Target::Selector(string_arg("selector")?),
            text: args
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        },
        "submit" => Verb::Submit {
            form: optional_path("form"),
        },
        "back" => Verb::Back,
        "watch" => Verb::Watch {
            selector: string_arg("selector")?,
            timeout_ms: args
                .get("timeoutMs")
                .and_then(Value::as_f64)
                .unwrap_or(5_000.0)
                .clamp(0.0, 10_000.0) as u64,
        },
        "dispatch" => Verb::Dispatch {
            target: Target::Selector(string_arg("selector")?),
            event: string_arg("event")?,
            detail: optional_path("detail"),
        },
        "screenshot" => Verb::Screenshot {
            path: optional_path("path"),
        },
        "pdf" => Verb::Pdf {
            path: optional_path("path"),
        },
        "archive" => Verb::Archive {
            dir: optional_path("dir"),
        },
        "verify" => Verb::Verify {
            path: string_arg("path")?,
        },
        other => return Err(format!("unknown tool `{other}`")),
    })
}

/// The verb's result document → MCP content blocks. Images come back as
/// image blocks (clients render them); PDFs go inline as a base64 resource
/// or as a path; everything else is text.
fn content_for(tool: &str, result: &str) -> Value {
    let parsed = kitae_json::parse(result).unwrap_or(Value::Str(result.to_string()));
    let mut blocks: Vec<Value> = Vec::new();
    if tool == "screenshot" {
        if let Some(data) = parsed.get("png").and_then(Value::as_str) {
            blocks.push(Value::obj(vec![
                ("type", Value::s("image")),
                ("data", Value::s(data)),
                ("mimeType", Value::s("image/png")),
            ]));
        }
        if let Some(path) = parsed.get("path").and_then(Value::as_str) {
            blocks.push(text_block(&format!(
                "PNG written to {path} ({} bytes)",
                parsed.get("bytes").and_then(Value::as_f64).unwrap_or(0.0) as u64
            )));
        }
    }
    if tool == "pdf" {
        if let Some(data) = parsed.get("pdf").and_then(Value::as_str) {
            blocks.push(Value::obj(vec![
                ("type", Value::s("resource")),
                (
                    "resource",
                    Value::obj(vec![
                        ("uri", Value::s("kitae://page.pdf")),
                        ("mimeType", Value::s("application/pdf")),
                        ("blob", Value::s(data)),
                    ]),
                ),
            ]));
        }
        if let Some(path) = parsed.get("path").and_then(Value::as_str) {
            blocks.push(text_block(&format!(
                "PDF written to {path} ({} bytes)",
                parsed.get("bytes").and_then(Value::as_f64).unwrap_or(0.0) as u64
            )));
        }
    }
    if blocks.is_empty() {
        blocks.push(text_block(&parsed.to_json()));
    }
    Value::obj(vec![("content", Value::arr(blocks))])
}

fn text_block(text: &str) -> Value {
    Value::obj(vec![
        ("type", Value::s("text")),
        ("text", Value::s(text.to_string())),
    ])
}

fn error_result(message: &str) -> Value {
    Value::obj(vec![
        ("content", Value::arr(vec![text_block(message)])),
        ("isError", Value::Bool(true)),
    ])
}

fn success_response(id: &Value, result: &Value) -> String {
    Value::obj(vec![
        ("jsonrpc", Value::s("2.0")),
        ("id", id.clone()),
        ("result", result.clone()),
    ])
    .to_json()
}

fn error_response(id: &Value, code: i64, message: &str) -> String {
    Value::obj(vec![
        ("jsonrpc", Value::s("2.0")),
        ("id", id.clone()),
        (
            "error",
            Value::obj(vec![
                ("code", Value::Num(code as f64)),
                ("message", Value::s(message.to_string())),
            ]),
        ),
    ])
    .to_json()
}

/// The tool inventory as a JSON value: `{ "tools": [ {name, description,
/// inputSchema} … ] }`. Shared with the HTTP transport (`GET /verbs`), so
/// the two transports publish one inventory.
pub fn tools_json() -> Value {
    let tools: Vec<Value> = TOOLS
        .iter()
        .map(|tool| {
            let schema = kitae_json::parse(tool.schema).unwrap_or(Value::Null);
            Value::obj(vec![
                ("name", Value::s(tool.name)),
                ("description", Value::s(tool.description)),
                ("inputSchema", schema),
            ])
        })
        .collect();
    Value::obj(vec![("tools", Value::arr(tools))])
}

/// The policy a `--allow` list describes, with the server's own wording for
/// a refusal.
pub fn policy_from_args(allow: &[String]) -> AllowList {
    if allow.iter().any(|a| a == "*" || a == "all") {
        AllowList::all()
    } else if allow.is_empty() {
        AllowList::read_only()
    } else {
        let names: Vec<&str> = allow.iter().map(String::as_str).collect();
        AllowList::new(&names)
    }
}

/// Does this policy allow the verb? (Used by `--print-tools` style startup
/// diagnostics and by tests.)
pub fn allowed(policy: &dyn PermissionPolicy, verb: &Verb) -> bool {
    matches!(policy.check(verb), kitae_agent::Decision::Allow)
}
