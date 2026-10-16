//! kitae_server — Track B's tier B3: the headless HTTP API.
//!
//! One static binary that serves the agent verbs over HTTP, for clients that
//! speak HTTP rather than MCP (a shell script, a workflow engine, a language
//! with no MCP client). The verbs themselves are the same inventory the MCP
//! transport exposes — `kitae_mcp::verb_for` maps names to
//! `kitae_agent::Verb` for both — so the two transports cannot drift.
//!
//! Threading, honestly: `Browser` is **not** `Send` (its host, measurer and
//! UI state are single-threaded by construction). So there is exactly one
//! *browser thread*, which owns every session, and connection threads are
//! just parsers that hand a job over a channel and wait for the reply. That
//! is the same shape the app has — one thread drives the engine — rather
//! than a lie about parallelism.
//!
//! Sessions: clients get one session each (`?session=NAME`, created with
//! `POST /sessions`), so two agents do not fight over one page. Sessions are
//! capped and idle ones are evicted.
//!
//! ```text
//! GET    /health            → liveness + session count
//! GET    /verbs             → the verb inventory (name, description, schema)
//! GET    /sessions          → the open sessions
//! POST   /sessions          → create one ({"session":"name", "allow":["*"]})
//! DELETE /sessions/NAME     → drop one
//! POST   /act?session=NAME  → run one verb ({"verb":"navigate","url":"…"})
//! POST   /batch?session=NAME→ run a list ({"verbs":[{…},{…}]}) in order
//! ```
//!
//! `/batch` exists because a multi-step task is one intent: an agent that
//! has to make six round trips to fill and submit a form is paying for the
//! network rather than for browsing.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;

use kitae_agent::{Decision, Verb};
use kitae_core::agent::{AgentLimits, AgentSession, AllowList, PermissionPolicy};
use kitae_json::Value;
use kitae_mcp::{tools_json, verb_for};

/// How many sessions one server may hold open at once.
pub const MAX_SESSIONS: usize = 8;
/// How many verbs one `/batch` may run (a bound on a single request's work).
pub const MAX_BATCH: usize = 64;
/// Largest request body accepted, in bytes.
pub const MAX_BODY: usize = 1 << 20;

/// Server configuration.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub viewport: (f32, f32),
    pub scale: f32,
    /// The verbs a client may use when it does not name its own.
    pub allow: Vec<String>,
    pub limits: AgentLimits,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            viewport: (1024.0, 768.0),
            scale: 1.0,
            allow: Vec::new(),
            limits: AgentLimits::default(),
        }
    }
}

/// One job for the browser thread.
struct Job {
    kind: JobKind,
    reply: mpsc::Sender<Response>,
}

enum JobKind {
    Act {
        session: String,
        verbs: Vec<Verb>,
    },
    Create {
        name: String,
        allow: Option<Vec<String>>,
    },
    Drop {
        name: String,
    },
    List,
}

/// A finished HTTP response: status, content type, body.
pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: String,
}

/// The server: a channel to the browser thread, plus the shared config.
pub struct Server {
    jobs: Sender<Job>,
    config: ServerConfig,
}

impl Server {
    /// Start the browser thread and return the server handle.
    pub fn new(config: ServerConfig) -> Server {
        let (tx, rx) = mpsc::channel::<Job>();
        let worker_config = config.clone();
        std::thread::Builder::new()
            .name("kitae-browser".into())
            .spawn(move || browser_thread(worker_config, rx))
            .expect("spawn browser thread");
        Server { jobs: tx, config }
    }

    pub fn config(&self) -> &ServerConfig {
        &self.config
    }

    fn send(&self, kind: JobKind) -> Response {
        let (reply_tx, reply_rx) = mpsc::channel();
        if self
            .jobs
            .send(Job {
                kind,
                reply: reply_tx,
            })
            .is_err()
        {
            return Response::json(
                503,
                &Value::obj(vec![("error", Value::s("the browser thread is gone"))]),
            );
        }
        match reply_rx.recv() {
            Ok(response) => response,
            Err(_) => Response::json(
                500,
                &Value::obj(vec![(
                    "error",
                    Value::s("no reply from the browser thread"),
                )]),
            ),
        }
    }

    /// Route one request. Pure: no sockets, so tests drive it directly.
    pub fn handle_request(&self, method: &str, target: &str, body: &str) -> Response {
        let (path, query) = split_target(target);
        let session = query
            .get("session")
            .cloned()
            .unwrap_or_else(|| "default".to_string());
        match (method, path.as_str()) {
            ("GET", "/health") => {
                let sessions = self.send(JobKind::List);
                Response::json(
                    200,
                    &Value::obj(vec![
                        ("ok", Value::Bool(true)),
                        ("name", Value::s("kitae-server")),
                        ("version", Value::s(env!("CARGO_PKG_VERSION"))),
                        ("sessions", sessions_value(&sessions)),
                    ]),
                )
            }
            ("GET", "/verbs") => Response::json(200, &tools_json()),
            ("GET", "/sessions") => self.send(JobKind::List),
            ("POST", "/sessions") => {
                let parsed = kitae_json::parse(body).unwrap_or(Value::Null);
                let name = parsed
                    .get("session")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("s{}", now_ms()));
                let allow = parsed.get("allow").and_then(Value::as_arr).map(|list| {
                    list.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                });
                self.send(JobKind::Create { name, allow })
            }
            ("DELETE", path) if path.starts_with("/sessions/") => {
                let name = path.trim_start_matches("/sessions/").to_string();
                self.send(JobKind::Drop { name })
            }
            ("POST", "/act") => match kitae_json::parse(body) {
                Ok(value) => {
                    let name = value
                        .get("verb")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    match verb_for(&name, &value) {
                        Ok(verb) => self.send(JobKind::Act {
                            session,
                            verbs: vec![verb],
                        }),
                        Err(message) => Response::error(400, &message),
                    }
                }
                Err(e) => Response::error(400, &format!("body is not JSON: {e}")),
            },
            ("POST", "/batch") => match kitae_json::parse(body) {
                Ok(value) => {
                    let Some(list) = value.get("verbs").and_then(Value::as_arr) else {
                        return Response::error(400, "`/batch` needs a `verbs` array");
                    };
                    if list.len() > MAX_BATCH {
                        return Response::error(
                            400,
                            &format!("`/batch` takes at most {MAX_BATCH} verbs"),
                        );
                    }
                    let mut verbs = Vec::with_capacity(list.len());
                    for entry in list {
                        let name = entry.get("verb").and_then(Value::as_str).unwrap_or("");
                        match verb_for(name, entry) {
                            Ok(verb) => verbs.push(verb),
                            Err(message) => return Response::error(400, &message),
                        }
                    }
                    self.send(JobKind::Act { session, verbs })
                }
                Err(e) => Response::error(400, &format!("body is not JSON: {e}")),
            },
            ("GET", _) | ("POST", _) | ("DELETE", _) => {
                Response::error(404, &format!("no route for {method} {path}"))
            }
            _ => Response::error(405, &format!("{method} is not supported")),
        }
    }

    /// Serve until the listener fails. One thread per connection; the
    /// browser work happens on its own thread (see the module note).
    pub fn serve(&self, listener: TcpListener) -> std::io::Result<()> {
        let server = Arc::new(SharedServer::from(self));
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let server = server.clone();
            std::thread::spawn(move || {
                let _ = handle_connection(&server, stream);
            });
        }
        Ok(())
    }
}

/// The server behind an `Arc` for connection threads (`Response` is not
/// cloneable, so the handle wraps the pieces that are).
pub struct SharedServer {
    jobs: Sender<Job>,
    config: ServerConfig,
}

impl From<&Server> for SharedServer {
    fn from(server: &Server) -> Self {
        Self {
            jobs: server.jobs.clone(),
            config: server.config.clone(),
        }
    }
}

impl SharedServer {
    fn send(&self, kind: JobKind) -> Response {
        Server {
            jobs: self.jobs.clone(),
            config: self.config.clone(),
        }
        .send(kind)
    }

    fn handle_request(&self, method: &str, target: &str, body: &str) -> Response {
        Server {
            jobs: self.jobs.clone(),
            config: self.config.clone(),
        }
        .handle_request(method, target, body)
    }
}

impl Response {
    pub fn json(status: u16, value: &Value) -> Response {
        Response {
            status,
            content_type: "application/json",
            body: value.to_json(),
        }
    }

    pub fn error(status: u16, message: &str) -> Response {
        Response::json(
            status,
            &Value::obj(vec![("error", Value::s(message.to_string()))]),
        )
    }
}

/// The browser thread: owns every session, runs one job at a time.
fn browser_thread(config: ServerConfig, rx: mpsc::Receiver<Job>) {
    let mut sessions: HashMap<String, AgentSession> = HashMap::new();
    while let Ok(job) = rx.recv() {
        let response = match job.kind {
            JobKind::List => {
                let names: Vec<Value> =
                    sessions.keys().map(|name| Value::s(name.clone())).collect();
                Response::json(
                    200,
                    &Value::obj(vec![
                        ("sessions", Value::arr(names)),
                        ("count", Value::Num(sessions.len() as f64)),
                    ]),
                )
            }
            JobKind::Create { name, allow } => {
                if sessions.len() >= MAX_SESSIONS {
                    Response::error(
                        429,
                        &format!("at most {MAX_SESSIONS} sessions are open at once"),
                    )
                } else {
                    let allow = allow.unwrap_or_else(|| config.allow.clone());
                    sessions.insert(name.clone(), new_session(&config, &allow));
                    Response::json(
                        201,
                        &Value::obj(vec![
                            ("session", Value::s(name)),
                            ("created", Value::Bool(true)),
                        ]),
                    )
                }
            }
            JobKind::Drop { name } => match sessions.remove(&name) {
                Some(_) => Response::json(
                    200,
                    &Value::obj(vec![
                        ("session", Value::s(name)),
                        ("closed", Value::Bool(true)),
                    ]),
                ),
                None => Response::error(404, &format!("no session named `{name}`")),
            },
            JobKind::Act { session, verbs } => {
                if !sessions.contains_key(&session) {
                    sessions.insert(session.clone(), new_session(&config, &config.allow));
                }
                let active = sessions.get_mut(&session).expect("inserted above");
                run_verbs(active, &session, verbs)
            }
        };
        let _ = job.reply.send(response);
    }
}

/// Run one or more verbs on a session, shaping HTTP statuses: 403 for a
/// permission denial (the client learns the verb was refused, not broken),
/// 422 for an act that failed. A batch stops at its first failure and
/// reports how far it got.
fn run_verbs(session: &mut AgentSession, name: &str, verbs: Vec<Verb>) -> Response {
    let batch = verbs.len() > 1;
    let mut results: Vec<Value> = Vec::with_capacity(verbs.len());
    for verb in verbs {
        let verb_name = verb.name().to_string();
        match session.permit_and_run(&verb) {
            Ok(result) => {
                let parsed = kitae_json::parse(&result).unwrap_or(Value::Str(result));
                if batch {
                    results.push(Value::obj(vec![
                        ("verb", Value::s(verb_name)),
                        ("ok", Value::Bool(true)),
                        ("result", parsed),
                    ]));
                } else {
                    return Response::json(
                        200,
                        &Value::obj(vec![
                            ("session", Value::s(name.to_string())),
                            ("verb", Value::s(verb_name)),
                            ("result", parsed),
                        ]),
                    );
                }
            }
            Err(message) => {
                let denied = message.contains("permission denied");
                if batch {
                    results.push(Value::obj(vec![
                        ("verb", Value::s(verb_name)),
                        ("ok", Value::Bool(false)),
                        ("error", Value::s(message.clone())),
                    ]));
                    return Response::json(
                        if denied { 403 } else { 422 },
                        &Value::obj(vec![
                            ("session", Value::s(name.to_string())),
                            ("ok", Value::Bool(false)),
                            ("error", Value::s(message)),
                            ("completed", Value::Num((results.len() - 1) as f64)),
                            ("results", Value::arr(results)),
                        ]),
                    );
                }
                return Response::error(if denied { 403 } else { 422 }, &message);
            }
        }
    }
    Response::json(
        200,
        &Value::obj(vec![
            ("session", Value::s(name.to_string())),
            ("ok", Value::Bool(true)),
            ("count", Value::Num(results.len() as f64)),
            ("results", Value::arr(results)),
        ]),
    )
}

fn new_session(config: &ServerConfig, allow: &[String]) -> AgentSession {
    let mut session = kitae_headless::session(config.viewport)
        .unwrap_or_else(|| AgentSession::headless(config.viewport))
        .with_limits(config.limits);
    let policy = allow_list(allow);
    session.set_policy(Box::new(policy));
    session
}

/// `--allow` / `{"allow":[…]}` → a policy. As everywhere in this plane, the
/// default is read-only.
pub fn allow_list(allow: &[String]) -> AllowList {
    if allow.iter().any(|a| a == "*" || a == "all") {
        AllowList::all()
    } else if allow.is_empty() {
        AllowList::read_only()
    } else {
        let names: Vec<&str> = allow.iter().map(String::as_str).collect();
        AllowList::new(&names)
    }
}

/// Is this policy allowing the verb? (Startup diagnostics and tests.)
pub fn policy_allows(policy: &dyn PermissionPolicy, verb: &Verb) -> bool {
    matches!(policy.check(verb), Decision::Allow)
}

fn sessions_value(response: &Response) -> Value {
    kitae_json::parse(&response.body)
        .ok()
        .and_then(|value| value.get("count").cloned())
        .unwrap_or(Value::Num(0.0))
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Split `path?query` into its parts, with the query decoded into pairs.
fn split_target(target: &str) -> (String, HashMap<String, String>) {
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, query),
        None => (target, ""),
    };
    let mut map = HashMap::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        map.insert(percent_decode(key), percent_decode(value));
    }
    (path.to_string(), map)
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// One HTTP/1.1 request: request line, headers, body.
pub struct Request {
    pub method: String,
    pub target: String,
    pub body: String,
}

/// Read one request from a stream. `None` on EOF or a malformed request.
pub fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).ok()? == 0 {
        return None;
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            if key.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    let content_length = content_length.min(MAX_BODY);
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).ok()?;
    }
    Some(Request {
        method,
        target,
        body: String::from_utf8_lossy(&body).to_string(),
    })
}

/// Read, route, answer, close.
fn handle_connection(server: &SharedServer, mut stream: TcpStream) -> std::io::Result<()> {
    let Some(request) = read_request(&mut stream) else {
        return Ok(());
    };
    let response = server.handle_request(&request.method, &request.target, &request.body);
    let status_text = match response.status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "OK",
    };
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}; charset=utf-8\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n",
        response.status,
        status_text,
        response.content_type,
        response.body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(response.body.as_bytes())?;
    stream.flush()
}
