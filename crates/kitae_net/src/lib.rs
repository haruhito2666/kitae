//! kitae_net — fetching pages over HTTP(S) and file:// with a single shared
//! blocking client (rustls — no OpenSSL, per the security brief).
//!
//! Transport surface beyond the shared client (parity bucket G): HTTP/2
//! prior knowledge (`fetch_h2c`), HTTP/3 over QUIC (`fetch_h3`), WebSocket
//! (`ws_connect`), Server-Sent Events (`sse_connect`), resumable downloads
//! (`download_to_file`), Basic/Digest auth challenges (`set_auth_provider`),
//! preconnect/prefetch warmup, and RFC 9218 priority hints.
//!
//! Responsibilities kept deliberately narrow: scheme dispatch, typed errors,
//! charset-aware decoding of bodies, an in-memory response cache for history
//! and reloads, and process-wide timeout configuration. Rendering decisions
//! (what to do with a PDF, whether a page is "plain text enough") belong to
//! callers.

use std::collections::HashMap;
use std::fmt;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use url::Url;

mod auth;
mod download;
mod h3;
pub mod pool;
mod privacy;
mod sse;
#[cfg(test)]
pub(crate) mod support;
mod warm;
mod ws;

pub use privacy::*;

pub use auth::{
    clear_session_credentials, describe_challenge, install_session_provider, session_credentials,
    set_auth_provider, store_session_credentials, AuthChallenge, AuthProvider, AuthScheme,
    Credentials,
};
pub use download::{
    begin_download, download_dir, download_to_file, downloads_snapshot, fetch_range,
    filename_from_disposition, set_download_dir, suggested_filename, DownloadHook, DownloadInfo,
    DownloadState,
};
pub use h3::{fetch_h3, fetch_h3_with, H3Options};
pub use sse::{sse_connect, SseEvent, SseStream};
pub use warm::{apply_priority, preconnect, prefetch, prefetch_many, RequestPriority};
pub use ws::{ws_connect, ws_connect_with, WsConfig, WsMessage, WsStream};

const USER_AGENT: &str = "Kitae/0.1 (JavaScript-free reader browser)";

/// Process-wide client configuration. Read once, when the shared HTTP client
/// is first built; `configure` must run before the first `fetch`.
/// (`cookies` is read on every request — it gates jar use, not the client.)
#[derive(Debug, Clone)]
pub struct FetchConfig {
    pub user_agent: String,
    /// Whole-request budget (send + await headers + read body).
    pub total_timeout: Duration,
    /// TCP+TLS handshake budget.
    pub connect_timeout: Duration,
    /// Send stored cookies and honor `Set-Cookie` (Phase O2). The jar is
    /// in-memory and dies with the process; set false to opt out entirely.
    pub cookies: bool,
}

impl Default for FetchConfig {
    fn default() -> Self {
        FetchConfig {
            user_agent: USER_AGENT.to_string(),
            total_timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(10),
            cookies: true,
        }
    }
}

/// Install the client configuration before the first fetch. Returns
/// `Err(config)` if the client already exists (a fetch ran first) or another
/// `configure` call won the race — the default config is the documented
/// contract, so callers should treat failure as harmless.
pub fn configure(config: FetchConfig) -> Result<(), FetchConfig> {
    if CLIENT.get().is_some() {
        return Err(config);
    }
    CONFIG.set(config)
}

static CONFIG: OnceLock<FetchConfig> = OnceLock::new();
static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();

fn client() -> &'static reqwest::blocking::Client {
    CLIENT.get_or_init(|| {
        let cfg = CONFIG.get().cloned().unwrap_or_default();
        reqwest::blocking::Client::builder()
            .user_agent(cfg.user_agent)
            .connect_timeout(cfg.connect_timeout)
            .timeout(cfg.total_timeout)
            .build()
            .expect("failed to build HTTP client")
    })
}

/// Why a fetch failed. This is the engine's typed error surface for the
/// embedder: `Display` renders the user-facing message (the same string the
/// error page shows), `Error` makes it composable with `?` pipelines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// The URL could not be parsed.
    InvalidUrl(String),
    HttpsUpgrade(String),
    InsecureRedirect(String),
    /// The scheme is recognized but not fetchable by the engine.
    UnsupportedScheme(String),
    /// DNS, TLS, connection, timeout, or body-read failure.
    Transport(String),
    /// A `file://` read failed.
    FileIo(String),
    /// A `file://` request was refused by the local-origin gate (S-1): the
    /// current page is a network document, so it may not read local files.
    LocalOrigin(ForbiddenLocalOrigin),
    /// The server answered with this error status (4xx/5xx).
    Http {
        status: u16,
    },
    /// G5: a download was cancelled through its progress hook; the `.part`
    /// file is left in place for a later resume.
    Cancelled,
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FetchError::InvalidUrl(what) => write!(f, "invalid URL {what:?}: unparseable"),
            FetchError::HttpsUpgrade(what) => write!(f, "HTTPS upgrade failed: {what}"),
            FetchError::InsecureRedirect(url) => {
                write!(f, "insecure redirect requires confirmation: {url}")
            }
            FetchError::UnsupportedScheme(s) => write!(f, "unsupported scheme: {s}"),
            FetchError::Transport(what) => write!(f, "{what}"),
            FetchError::FileIo(what) => write!(f, "{what}"),
            FetchError::LocalOrigin(e) => write!(f, "{e}"),
            FetchError::Http { status } => write!(f, "HTTP {status}"),
            FetchError::Cancelled => write!(f, "download cancelled"),
        }
    }
}

impl std::error::Error for FetchError {}

fn cookies_enabled() -> bool {
    CONFIG.get().map(|c| c.cookies).unwrap_or(true)
}

#[derive(Debug, Clone)]
pub struct Response {
    /// URL after redirects — the base for resolving subresources and links.
    pub final_url: String,
    pub status: u16,
    pub body: Vec<u8>,
    pub content_type: String,
    /// Raw `Cache-Control` header ("" when the transport has no headers).
    pub cache_control: String,
    /// Raw `ETag` header for conditional revalidation (Phase O3; "" when
    /// absent). Stored with the cache entry, sent back as `If-None-Match`.
    pub etag: String,
    /// Raw `Last-Modified` header (Phase O3; "" when absent). Stored with
    /// the cache entry, sent back as `If-Modified-Since`.
    pub last_modified: String,
    /// G1: the negotiated HTTP protocol version ("HTTP/1.1", "HTTP/2",
    /// "HTTP/3"; "file" for local reads).
    pub protocol: String,
    /// H1: raw `Content-Disposition` header ("" when absent). The engine's
    /// download decision (`attachment`) and suggested filename read this.
    pub content_disposition: String,
    /// H6: raw `WWW-Authenticate` header ("" when absent). A 401 carrying a
    /// Basic/Digest challenge renders the engine's sign-in page and asks
    /// the host for credentials instead of the generic error page.
    pub www_authenticate: String,
}

impl Response {
    pub fn is_html(&self) -> bool {
        let ct = self.content_type.to_ascii_lowercase();
        ct.contains("text/html") || ct.contains("application/xhtml")
    }

    pub fn is_plain_text(&self) -> bool {
        let ct = self.content_type.to_ascii_lowercase();
        ct.starts_with("text/plain")
    }

    pub fn is_image(&self) -> bool {
        self.content_type.to_ascii_lowercase().starts_with("image/")
    }

    /// QTSQ native-format payload, either declared (`application/x-qtsq`) or
    /// sniffed from the `QTS\x01` magic. A *known* binary — callers route it
    /// to the viewer pipeline instead of treating it as a rejection.
    pub fn is_qtsq(&self) -> bool {
        if self
            .content_type
            .to_ascii_lowercase()
            .starts_with("application/x-qtsq")
        {
            return true;
        }
        self.body.len() >= 4 && self.body.starts_with(b"QTS\x01")
    }

    /// Body decoded to text. Charset precedence: BOM in the body, then the
    /// `charset=` parameter of Content-Type, then UTF-8 (lossy — pages that
    /// lie about their charset get replacement marks, not a rejected load).
    pub fn text(&self) -> String {
        decode_body(&self.body, &self.content_type)
    }
}

/// G1: the negotiated protocol version as the user-facing string kept in
/// [`Response::protocol`].
fn version_str(v: reqwest::Version) -> String {
    match v {
        reqwest::Version::HTTP_09 => "HTTP/0.9",
        reqwest::Version::HTTP_10 => "HTTP/1.0",
        reqwest::Version::HTTP_11 => "HTTP/1.1",
        reqwest::Version::HTTP_2 => "HTTP/2",
        reqwest::Version::HTTP_3 => "HTTP/3",
        _ => "unknown",
    }
    .to_string()
}

// ---------------------------------------------------------------------------
// Charset decoding (hand-rolled; encoding_rs is deliberately not a dep)
// ---------------------------------------------------------------------------

/// UTF-8, UTF-16 (BOM-detected), ISO-8859-1 and windows-1252 — the charsets
/// real-world pages declare. Anything else (Shift-JIS, GBK, …) falls back to
/// UTF-8 lossy until a dedicated table crate earns its place here.
fn decode_body(body: &[u8], content_type: &str) -> String {
    // A BOM overrides whatever the header claims.
    if body.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(&body[3..]).into_owned();
    }
    if body.starts_with(&[0xFF, 0xFE]) {
        return decode_utf16(&body[2..], true);
    }
    if body.starts_with(&[0xFE, 0xFF]) {
        return decode_utf16(&body[2..], false);
    }

    match declared_charset(content_type) {
        None => String::from_utf8_lossy(body).into_owned(),
        Some(cs) => match cs.as_str() {
            "utf-8" | "utf8" | "us-ascii" | "ascii" => String::from_utf8_lossy(body).into_owned(),
            "utf-16le" => decode_utf16(body, true),
            "utf-16be" => decode_utf16(body, false),
            "utf-16" => decode_utf16(body, true), // WHATWG: bare utf-16 means LE
            "iso-8859-1" | "latin1" | "latin-1" | "windows-1252" | "cp1252" => {
                body.iter().map(|&b| windows1252_byte(b)).collect()
            }
            // Unknown declaration: UTF-8 lossy, never a failed load.
            _ => String::from_utf8_lossy(body).into_owned(),
        },
    }
}

/// The `charset` parameter of a Content-Type header, lowercased, quotes
/// stripped. `None` when absent.
fn declared_charset(content_type: &str) -> Option<String> {
    for part in content_type.split(';').skip(1) {
        let part = part.trim();
        if let Some(value) = part.strip_prefix("charset=").or_else(|| {
            part.split_once('=')
                .filter(|(k, _)| k.eq_ignore_ascii_case("charset"))
                .map(|(_, v)| v)
        }) {
            let value = value.trim().trim_matches('"').trim();
            if !value.is_empty() {
                return Some(value.to_ascii_lowercase());
            }
        }
    }
    None
}

fn decode_utf16(bytes: &[u8], little: bool) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| {
            if little {
                u16::from_le_bytes([c[0], c[1]])
            } else {
                u16::from_be_bytes([c[0], c[1]])
            }
        })
        .collect();
    String::from_utf16_lossy(&units)
}

/// One windows-1252 byte → char. Bytes 0xA0..=0xFF are identical to
/// ISO-8859-1 (the byte value as a code point); 0x80..=0x9F hold the
/// punctuation remap; the five unassigned slots map to their control code
/// points, as the WHATWG encoding standard does.
fn windows1252_byte(b: u8) -> char {
    if b >= 0xA0 {
        return b as char;
    }
    match b {
        0x80 => '\u{20AC}', // €
        0x82 => '\u{201A}', // ‚
        0x83 => '\u{0192}', // ƒ
        0x84 => '\u{201E}', // „
        0x85 => '\u{2026}', // …
        0x86 => '\u{2020}', // †
        0x87 => '\u{2021}', // ‡
        0x88 => '\u{02C6}', // ˆ
        0x89 => '\u{2030}', // ‰
        0x8A => '\u{0160}', // Š
        0x8B => '\u{2039}', // ‹
        0x8C => '\u{0152}', // Œ
        0x8E => '\u{017D}', // Ž
        0x91 => '\u{2018}', // '
        0x92 => '\u{2019}', // '
        0x93 => '\u{201C}', // "
        0x94 => '\u{201D}', // "
        0x95 => '\u{2022}', // •
        0x96 => '\u{2013}', // –
        0x97 => '\u{2014}', // —
        0x98 => '\u{02DC}', // ˜
        0x99 => '\u{2122}', // ™
        0x9A => '\u{0161}', // š
        0x9B => '\u{203A}', // ›
        0x9C => '\u{0153}', // œ
        0x9E => '\u{017E}', // ž
        0x9F => '\u{0178}', // Ÿ
        _ => b as char,     // controls + the unassigned 0x81/8D/8F/90/9D
    }
}

// ---------------------------------------------------------------------------
// Local-origin gate (SECURITY_AUDIT.md S-1)
// ---------------------------------------------------------------------------

/// Whether requests to `file://` URLs are allowed from a page at
/// `page_url`. The rule mirrors browser behavior: only a page that is
/// *itself* local (a `file://` document, or the engine's built-in
/// `about:`/`kitae:`/`data:` pages) may reach the local filesystem. A page
/// fetched over the network can never read local files — no matter what
/// its HTML references (`<a href>`, `<link>`, `@import`, `<img src>`).
///
/// An empty page URL means embedder-supplied HTML with no network context
/// (`load_html("", …)` / tests): treated as trusted-local, since the host
/// chose the content.
pub fn local_origin_allowed(page_url: &str) -> bool {
    let u = page_url.trim();
    u.is_empty()
        || u.starts_with("file://")
        || u.starts_with("about:")
        || u.starts_with("kitae:")
        || u.starts_with("data:")
}

/// Why the local-origin gate refused a fetch. `Display` renders the
/// user-facing message (the same string the error page shows).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForbiddenLocalOrigin {
    /// The `file://` URL the page tried to reach.
    pub target: String,
    /// The page that attempted it.
    pub page_url: String,
}

impl fmt::Display for ForbiddenLocalOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "local file access blocked: a network page may not read {}",
            self.target
        )
    }
}

// The engine's current page origin — set by `kitae_core::Browser` on every
// load and read by the fetch entry points to enforce the local-origin
// gate. Empty means "no page context" (embedder-supplied HTML, tests),
// which the gate treats as trusted-local.
//
// This is deliberately a *thread-local*: the engine performs every fetch
// on its own engine thread, and per-thread contexts keep parallel test
// threads (and multi-engine hosts) from overwriting each other's origin.
thread_local! {
    static CURRENT_PAGE_ORIGIN: std::cell::RefCell<String> =
        const { std::cell::RefCell::new(String::new()) };
}

/// Record the page whose subresources are about to be fetched (S-1,
/// SECURITY_AUDIT.md). Called by the browser on every load/render.
pub fn set_page_origin(url: &str) {
    CURRENT_PAGE_ORIGIN.with(|c| *c.borrow_mut() = url.trim().to_string());
}

/// The recorded page origin (inspection / tests).
pub fn page_origin() -> String {
    CURRENT_PAGE_ORIGIN.with(|c| c.borrow().clone())
}

fn current_origin() -> String {
    page_origin()
}

// ---------------------------------------------------------------------------
// Cookie jar (Phase O2): in-memory, per-host, dies with the process
// ---------------------------------------------------------------------------

// Caps (SECURITY_AUDIT.md M-2): a hostile `Set-Cookie` flood must not grow
// engine memory without bound. Matches the RFC 6265 §6.3 shape — per-domain
// and global limits, evicting expired-then-oldest first — plus the RFC's
// 4 KiB per-cookie budget ("silently ignore" oversized values).
const COOKIE_MAX_PER_DOMAIN: usize = 50;
const COOKIE_MAX_TOTAL: usize = 3000;
const COOKIE_MAX_BYTES: usize = 4096;

/// One stored cookie: name=value plus the scoping attributes from
/// `Set-Cookie` (Path, Domain, Secure, HttpOnly, Expires/Max-Age).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cookie {
    name: String,
    value: String,
    /// Lowercased domain scope. `host_only` means "exactly this host" (no
    /// Domain attribute); otherwise a suffix match (leading dot stripped).
    domain: String,
    host_only: bool,
    path: String,
    secure: bool,
    /// `None` = session cookie (lives until process exit).
    expires: Option<Instant>,
    /// Last (re)store time — the LRU key for cap eviction (S-2/M-2 work,
    /// SECURITY_AUDIT.md). `None` predates the cap change.
    last_access: Option<Instant>,
}

impl Cookie {
    /// Whether this cookie is sent with a request to `host`/`path` over
    /// `secure_conn` at `now`. Expired cookies are treated as absent (the
    /// jar prunes them lazily on read).
    fn matches(&self, host: &str, path: &str, secure_conn: bool, now: Instant) -> bool {
        if let Some(exp) = self.expires {
            if now >= exp {
                return false;
            }
        }
        if self.secure && !secure_conn {
            return false;
        }
        let host = host.to_ascii_lowercase();
        if self.host_only {
            if host != self.domain {
                return false;
            }
        } else if host != self.domain && !host.ends_with(&format!(".{}", self.domain)) {
            return false;
        }
        path_match(path, &self.path)
    }
}

/// RFC 6265 §5.1.4 path-match: identical, or cookie-path is a prefix ending
/// in `/`, or the next request-path char after the prefix is `/`.
fn path_match(request_path: &str, cookie_path: &str) -> bool {
    if request_path == cookie_path {
        return true;
    }
    if request_path.starts_with(cookie_path) {
        if cookie_path.ends_with('/') {
            return true;
        }
        if let Some(next) = request_path.as_bytes().get(cookie_path.len()) {
            return *next == b'/';
        }
    }
    false
}

/// Default cookie path (RFC 6265 §5.1.4): the request path up to but not
/// including the last `/` ("/" when there is none, or the path is "/").
fn default_cookie_path(request_path: &str) -> String {
    if !request_path.starts_with('/') {
        return "/".to_string();
    }
    match request_path.rfind('/') {
        None | Some(0) => "/".to_string(),
        Some(i) => request_path[..i].to_string(),
    }
}

/// Parse one `Set-Cookie` header value into a [`Cookie`]. Returns `None`
/// for an empty name=value pair. `request_host`/`request_path` supply the
/// defaults; `now` anchors Max-Age. Unknown attributes are ignored; a
/// `Domain` that is not a suffix of the request host is rejected (the
/// response is trying to set cookies for someone else's domain).
fn parse_set_cookie(
    header: &str,
    request_host: &str,
    request_path: &str,
    now: Instant,
) -> Option<Cookie> {
    let mut parts = header.split(';');
    let pair = parts.next()?.trim();
    let (name, value) = pair.split_once('=')?;
    let name = name.trim().to_string();
    let value = value.trim().to_string();
    if name.is_empty() {
        return None;
    }
    let request_host = request_host.to_ascii_lowercase();
    let mut domain = request_host.clone();
    let mut host_only = true;
    let mut path = default_cookie_path(request_path);
    let mut secure = false;
    let mut expires: Option<Instant> = None;

    for attr in parts {
        let attr = attr.trim();
        if attr.is_empty() {
            continue;
        }
        let (k, v) = match attr.split_once('=') {
            Some((k, v)) => (k.trim().to_ascii_lowercase(), v.trim().to_string()),
            None => (attr.to_ascii_lowercase(), String::new()),
        };
        match k.as_str() {
            "domain" => {
                let d = v.trim().trim_start_matches('.').to_ascii_lowercase();
                if d.is_empty() {
                    continue;
                }
                // Must domain-match the request host, or it is a forgery.
                if request_host != d && !request_host.ends_with(&format!(".{d}")) {
                    return None;
                }
                domain = d;
                host_only = false;
            }
            "path" => {
                if v.starts_with('/') {
                    path = v;
                }
            }
            "secure" => secure = true,
            // HttpOnly is stored (no script can read the jar anyway — there
            // is no script — but the flag round-trips honestly).
            "httponly" => {}
            "max-age" => {
                if let Ok(secs) = v.trim().parse::<i64>() {
                    if secs <= 0 {
                        // Immediate expiry: the jar deletes on insert.
                        expires = Some(now);
                    } else {
                        expires = now.checked_add(Duration::from_secs(secs as u64));
                    }
                }
            }
            // Expires dates need an HTTP-date parser; honor the shape
            // `Expires=<anything>` by keeping the session default (documented:
            // Max-Age wins per RFC 6265, Expires alone means session scope).
            "expires" => {}
            _ => {}
        }
    }
    Some(Cookie {
        name,
        value,
        domain,
        host_only,
        path,
        secure,
        expires,
        last_access: Some(now),
    })
}

/// The process-wide jar: insertion-ordered vec, small by reader-browser
/// standards (no eviction cap — sites that stuff hundreds of cookies just
/// grow it until process exit).
#[derive(Debug, Default)]
struct CookieJar {
    cookies: Vec<Cookie>,
}

impl CookieJar {
    /// Store the `Set-Cookie` values of one response, replacing any cookie
    /// with the same (name, domain, path). Immediately-expired values
    /// delete instead of storing.
    ///
    /// Caps (SECURITY_AUDIT.md M-2, RFC 6265 §6.3 style): per-domain limit,
    /// global limit (evicting the oldest first), and a per-cookie value
    /// budget — a hostile `Set-Cookie` flood must not grow engine memory
    /// without bound.
    fn store(&mut self, headers: &[String], request_host: &str, request_path: &str, now: Instant) {
        for h in headers {
            let Some(mut c) = parse_set_cookie(h, request_host, request_path, now) else {
                continue;
            };
            // 4 KiB budget per cookie (name + value), matching the RFC's
            // "silently ignore" posture for oversized values.
            if c.name.len() + c.value.len() > COOKIE_MAX_BYTES {
                continue;
            }
            self.cookies
                .retain(|e| !(e.name == c.name && e.domain == c.domain && e.path == c.path));
            let expired = c.expires.is_some_and(|exp| now >= exp);
            if !expired {
                c.last_access = Some(now);
                self.cookies.push(c);
            }
        }
        self.enforce_caps();
    }

    /// Evict to the caps: expired entries first (they are dropped outright
    /// by `header_for` anyway), then least-recently-stored — globally and
    /// per domain. Deterministic, O(n) per eviction.
    fn enforce_caps(&mut self) {
        let now = Instant::now();
        while self.cookies.len() > COOKIE_MAX_TOTAL {
            let expired_idx = self
                .cookies
                .iter()
                .position(|c| c.expires.is_some_and(|exp| now >= exp));
            let oldest_idx = expired_idx.or_else(|| {
                self.cookies
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, c)| (c.last_access, c.name.len()))
                    .map(|(i, _)| i)
            });
            let Some(idx) = oldest_idx else {
                break;
            };
            self.cookies.remove(idx);
        }
        // Per-domain cap (50/domain, the RFC's suggested shape): one site
        // must not crowd the jar even when the global total is under way.
        loop {
            let mut counts: HashMap<&str, usize> = HashMap::new();
            for c in &self.cookies {
                *counts.entry(c.domain.as_str()).or_default() += 1;
            }
            let Some(offender) = counts
                .into_iter()
                .find(|(_, n)| *n > COOKIE_MAX_PER_DOMAIN)
                .map(|(d, _)| d)
            else {
                break;
            };
            let Some(idx) = self
                .cookies
                .iter()
                .enumerate()
                .filter(|(_, c)| c.domain == offender)
                .min_by_key(|(_, c)| (c.last_access, c.name.len()))
                .map(|(i, _)| i)
            else {
                break;
            };
            self.cookies.remove(idx);
        }
    }

    /// The `Cookie` header value for an outgoing request, or `None` when
    /// nothing matches. Expired entries are pruned as a side effect.
    fn header_for(
        &mut self,
        host: &str,
        path: &str,
        secure_conn: bool,
        now: Instant,
    ) -> Option<String> {
        self.cookies
            .retain(|c| c.expires.is_none_or(|exp| now < exp));
        let pairs: Vec<String> = self
            .cookies
            .iter()
            .filter(|c| c.matches(host, path, secure_conn, now))
            .map(|c| format!("{}={}", c.name, c.value))
            .collect();
        if pairs.is_empty() {
            None
        } else {
            Some(pairs.join("; "))
        }
    }

    fn len(&self) -> usize {
        self.cookies.len()
    }

    fn clear(&mut self) {
        self.cookies.clear();
    }
}

fn cookie_jar() -> &'static std::sync::Mutex<CookieJar> {
    static JAR: OnceLock<std::sync::Mutex<CookieJar>> = OnceLock::new();
    JAR.get_or_init(|| std::sync::Mutex::new(CookieJar::default()))
}

/// Number of cookies currently stored (inspection / tests).
pub fn cookie_len() -> usize {
    cookie_jar().lock().unwrap().len()
}

/// Drop every stored cookie.
pub fn clear_cookies() {
    cookie_jar().lock().unwrap().clear();
}

/// The `Cookie`-header half of the Phase O2 jar flow, shared by every
/// transport entry point: attach matching cookies when cookies are enabled.
pub(crate) fn attach_cookies_for_origin(
    req: reqwest::blocking::RequestBuilder,
    host: &str,
    path: &str,
    https: bool,
    origin: &str,
    explicit_cookie: bool,
) -> reqwest::blocking::RequestBuilder {
    if !cookies_enabled() || !cookies_allowed_for_host(origin, https, host) {
        return req;
    }
    if explicit_cookie {
        return req;
    }
    match cookie_jar()
        .lock()
        .unwrap()
        .header_for(host, path, https, Instant::now())
    {
        Some(value) => req.header(reqwest::header::COOKIE, value),
        None => req,
    }
}

pub(crate) fn cookie_header_for_origin(
    host: &str,
    path: &str,
    https: bool,
    origin: &str,
) -> Option<String> {
    if !cookies_enabled() || !cookies_allowed_for_host(origin, https, host) {
        return None;
    }
    cookie_jar()
        .lock()
        .unwrap()
        .header_for(host, path, https, Instant::now())
}

/// The `Set-Cookie` half of the Phase O2 jar flow, shared by every transport
/// entry point: store a response's cookies against its final URL.
pub(crate) fn store_cookies_from_for_origin(resp: &reqwest::blocking::Response, origin: &str) {
    let set_cookies: Vec<String> = resp
        .headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok().map(|s| s.to_string()))
        .collect();
    if set_cookies.is_empty() || !cookies_enabled() {
        return;
    }
    let url = resp.url();
    if !cookies_allowed_for_request(origin, url.as_str()) {
        return;
    }
    let host = url.host_str().unwrap_or("").to_string();
    let path = if url.path().is_empty() {
        "/".to_string()
    } else {
        url.path().to_string()
    };
    cookie_jar()
        .lock()
        .unwrap()
        .store(&set_cookies, &host, &path, Instant::now());
}

pub(crate) fn store_cookie_values_for_origin(
    values: &[String],
    host: &str,
    path: &str,
    https: bool,
    origin: &str,
) {
    if values.is_empty() || !cookies_enabled() || !cookies_allowed_for_host(origin, https, host) {
        return;
    }
    cookie_jar()
        .lock()
        .unwrap()
        .store(values, host, path, Instant::now());
}

// ---------------------------------------------------------------------------
// Response cache (history + reload should not re-hit the network)
// ---------------------------------------------------------------------------

const CACHE_MAX_ENTRIES: usize = 64;
const CACHE_DEFAULT_TTL: Duration = Duration::from_secs(300);

struct CacheInner {
    entries: HashMap<String, (Response, Instant, Duration)>,
    /// LRU order, oldest first.
    order: Vec<String>,
    max_entries: usize,
    default_ttl: Duration,
}

impl CacheInner {
    fn new(max_entries: usize, default_ttl: Duration) -> CacheInner {
        CacheInner {
            entries: HashMap::new(),
            order: Vec::new(),
            max_entries,
            default_ttl,
        }
    }

    fn insert(&mut self, url: String, resp: Response, now: Instant) {
        let ttl = cache_ttl(&resp.cache_control).unwrap_or(self.default_ttl);
        if self.entries.insert(url.clone(), (resp, now, ttl)).is_none() {
            self.order.push(url.clone());
        }
        while self.entries.len() > self.max_entries {
            let Some(oldest) = self.order.first().cloned() else {
                break;
            };
            self.entries.remove(&oldest);
            self.order.remove(0);
        }
    }

    fn touch(&mut self, url: &str) {
        if let Some(i) = self.order.iter().position(|k| k == url) {
            let key = self.order.remove(i);
            self.order.push(key);
        }
    }

    /// One locked probe: fresh entries serve (LRU-touched); stale entries
    /// are removed and handed back for conditional revalidation (Phase O3);
    /// anything else is a miss.
    fn lookup(&mut self, url: &str, now: Instant) -> CacheLookup {
        let fresh = match self.entries.get(url) {
            None => return CacheLookup::Miss,
            Some((_, stored, ttl)) => now.duration_since(*stored) < *ttl,
        };
        if fresh {
            let resp = self
                .entries
                .get(url)
                .map(|(r, _, _)| r.clone())
                .expect("present");
            self.touch(url);
            CacheLookup::Fresh(resp)
        } else {
            let (resp, _, _) = self.entries.remove(url).expect("present");
            self.order.retain(|k| k != url);
            CacheLookup::Stale(resp)
        }
    }
}

/// Freshness outcome of one cache probe (Phase O3): a fresh entry serves
/// immediately, a stale entry is returned for conditional revalidation, a
/// miss goes to the network.
enum CacheLookup {
    Fresh(Response),
    Stale(Response),
    Miss,
}

/// Per-response TTL from `Cache-Control`; `None` → use the default. `no-store`
/// is handled by the caller (don't store at all).
fn cache_ttl(cache_control: &str) -> Option<Duration> {
    let cc = cache_control.to_ascii_lowercase();
    if cc.split(',').any(|d| d.trim() == "no-store") {
        return None;
    }
    cc.split(',').find_map(|d| {
        let d = d.trim();
        d.strip_prefix("max-age=")
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(Duration::from_secs)
    })
}

fn cache() -> &'static std::sync::Mutex<CacheInner> {
    static CACHE: OnceLock<std::sync::Mutex<CacheInner>> = OnceLock::new();
    CACHE.get_or_init(|| {
        std::sync::Mutex::new(CacheInner::new(CACHE_MAX_ENTRIES, CACHE_DEFAULT_TTL))
    })
}

/// Number of responses currently cached (inspection / tests).
pub fn cached_len() -> usize {
    cache().lock().unwrap().entries.len()
}

/// Drop every cached response.
pub fn clear_cache() {
    let mut c = cache().lock().unwrap();
    c.entries.clear();
    c.order.clear();
}

/// [`fetch`] with the in-memory cache in front: a fresh cached copy (within
/// its TTL, LRU-tracked) is returned without touching the network. Only
/// successful, `no-store`-free GET responses are cached. A stale entry that
/// carries validators (`ETag`/`Last-Modified`) is revalidated with
/// conditional headers (Phase O3): a 304 answer refreshes the timestamp and
/// serves the cached body without re-downloading it.
pub fn fetch_cached(raw_url: &str) -> Result<Response, FetchError> {
    let origin = current_origin();
    fetch_cached_kind(raw_url, &origin, top_level_request(raw_url, &origin))
}

pub(crate) fn fetch_cached_for_origin(raw_url: &str, origin: &str) -> Result<Response, FetchError> {
    fetch_cached_kind(raw_url, origin, false)
}

pub(crate) fn fetch_cached_kind(
    raw_url: &str,
    origin: &str,
    top_level: bool,
) -> Result<Response, FetchError> {
    if raw_url.starts_with("file://") && !local_origin_allowed(origin) {
        return Err(FetchError::LocalOrigin(ForbiddenLocalOrigin {
            target: raw_url.to_string(),
            page_url: origin.to_string(),
        }));
    }
    let cache_url = cached_policy_url(raw_url, top_level);
    let stale = match cache().lock().unwrap().lookup(&cache_url, Instant::now()) {
        CacheLookup::Fresh(hit) => {
            privacy::validate_final_url(&hit.final_url, top_level)?;
            return Ok(hit);
        }
        CacheLookup::Miss => None,
        CacheLookup::Stale(s) => Some(s),
    };
    let mut cond: Vec<(String, String)> = Vec::new();
    if let Some(ref s) = stale {
        if !s.etag.is_empty() {
            cond.push(("If-None-Match".into(), s.etag.clone()));
        }
        if !s.last_modified.is_empty() {
            cond.push(("If-Modified-Since".into(), s.last_modified.clone()));
        }
    }
    let resp = if cond.is_empty() {
        fetch_for_origin_kind("GET", raw_url, &cond, None, origin, top_level)?
    } else {
        match fetch_for_origin_kind("GET", raw_url, &cond, None, origin, top_level)? {
            r if r.status == 304 => {
                let mut c = cache().lock().unwrap();
                if let Some(s) = stale {
                    privacy::validate_final_url(&s.final_url, top_level)?;
                    c.insert(cache_url.clone(), s.clone(), Instant::now());
                    return Ok(s);
                }
                return Ok(r);
            }
            r => r,
        }
    };
    maybe_store(&cache_url, &resp);
    Ok(resp)
}

fn cached_policy_url(raw_url: &str, top_level: bool) -> String {
    let Ok(mut url) = Url::parse(raw_url) else {
        return raw_url.to_string();
    };
    let scheme = url.scheme().to_string();
    if matches!(url.scheme(), "http" | "https")
        && privacy::apply_request_url_policy(&mut url, top_level).is_ok()
        && url.scheme() != scheme
    {
        return url.to_string();
    }
    raw_url.to_string()
}

pub(crate) fn top_level_request(raw_url: &str, origin: &str) -> bool {
    let origin = origin.trim();
    if origin.is_empty() {
        return true;
    }
    match (Url::parse(raw_url.trim()), Url::parse(origin)) {
        (Ok(request), Ok(page)) => request == page,
        _ => raw_url.trim() == origin,
    }
}

/// Store a fetched response when it is cacheable: status 200, no `no-store`,
/// GET semantics (POST responses never enter the cache — Phase O1).
fn maybe_store(url: &str, resp: &Response) {
    let cc = resp.cache_control.to_ascii_lowercase();
    let storable = resp.status == 200 && !cc.split(',').any(|d| d.trim() == "no-store");
    if storable {
        cache()
            .lock()
            .unwrap()
            .insert(url.to_string(), resp.clone(), Instant::now());
    }
}

// ---------------------------------------------------------------------------
// URL handling + fetch
// ---------------------------------------------------------------------------

fn is_local_or_ip_input(input: &str) -> bool {
    if input.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    Url::parse(&format!("http://{input}"))
        .ok()
        .and_then(|url| url.host_str().map(is_https_exempt_host))
        .unwrap_or(false)
}

/// Turns whatever the user typed into a URL: bare hosts get https://,
/// plain text is sent to DuckDuckGo's JavaScript-free HTML endpoint.
pub fn normalize_input(input: &str) -> String {
    let input = input.trim();
    if input.is_empty() {
        return "about:start".to_string();
    }
    if input.starts_with("about:")
        || input.starts_with("file:")
        || input.starts_with("data:")
        || input.starts_with("mailto:")
        || input.starts_with("javascript:")
        || input.starts_with("blob:")
        || input.starts_with("kitae:")
    {
        return input.to_string();
    }
    if input.contains("://") {
        return input.to_string();
    }
    if is_local_or_ip_input(input) {
        return format!("http://{input}");
    }
    // Heuristic: a dot and no spaces looks like a host, otherwise treat as a search.
    let host_like = input.contains('.') && !input.contains(' ');
    if host_like {
        format!("https://{input}")
    } else {
        search_provider().search_url(input)
    }
}

/// Fetch a URL. Handles https/http (reqwest) and file:// (local disk).
pub fn fetch(raw_url: &str) -> Result<Response, FetchError> {
    fetch_for_origin_kind("GET", raw_url, &[], None, &current_origin(), true)
}

pub fn fetch_top_level(raw_url: &str) -> Result<Response, FetchError> {
    fetch(raw_url)
}

pub fn fetch_top_level_with(
    method: &str,
    raw_url: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
) -> Result<Response, FetchError> {
    fetch_for_origin_kind(method, raw_url, headers, body, &current_origin(), true)
}

/// Fetch a URL with an explicit method, extra headers and an optional body
/// (Phase O1). `method` is `"GET"`/`"POST"` (anything else is sent verbatim
/// and the server decides); `headers` are `(name, value)` pairs sent as-is;
/// `body` is the raw request payload. Only GET responses are cacheable —
/// POST (and friends) never enter the cache, and this function never reads
/// it either (use [`fetch_cached`] for the caching GET path).
///
/// `file://` URLs ignore method/headers/body and serve local bytes (forms
/// filed against local pages keep working; there is nothing to POST to).
///
/// Reads the engine's current page origin and enforces the local-origin
/// gate exactly like [`fetch_for_origin`].
pub fn fetch_with(
    method: &str,
    raw_url: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
) -> Result<Response, FetchError> {
    fetch_for_origin_kind(method, raw_url, headers, body, &current_origin(), true)
}

/// Fetch on behalf of the page at `origin`. The last gate before the disk:
/// a `file://` request from a network page is refused (S-1,
/// SECURITY_AUDIT.md) — local bytes are only ever served to a page that is
/// itself local, or to an embedder with no page context at all (empty
/// origin).
pub fn fetch_for_origin(
    method: &str,
    raw_url: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    origin: &str,
) -> Result<Response, FetchError> {
    fetch_for_origin_kind(method, raw_url, headers, body, origin, false)
}

pub(crate) fn fetch_for_origin_kind(
    method: &str,
    raw_url: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    origin: &str,
    top_level: bool,
) -> Result<Response, FetchError> {
    if raw_url.starts_with("file://") && !local_origin_allowed(origin) {
        return Err(FetchError::LocalOrigin(ForbiddenLocalOrigin {
            target: raw_url.to_string(),
            page_url: origin.to_string(),
        }));
    }
    fetch_inner(client(), method, raw_url, headers, body, origin, top_level)
}

/// The scheme-dispatch + transport body shared by every fetch entry point.
/// `client` picks the transport profile: the shared client (default), the
/// h2c prior-knowledge client (G1), or the download client (G5, no total
/// timeout).
fn fetch_inner(
    client: &reqwest::blocking::Client,
    method: &str,
    raw_url: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    origin: &str,
    top_level: bool,
) -> Result<Response, FetchError> {
    if let Some(path) = raw_url.strip_prefix("file://") {
        let path = path.strip_prefix("localhost").unwrap_or(path); // file://localhost/...
        let bytes = std::fs::read(path).map_err(|e| FetchError::FileIo(format!("{path}: {e}")))?;
        let content_type = match path.rsplit('.').next() {
            Some("html") | Some("htm") => "text/html".to_string(),
            Some("css") => "text/css".to_string(),
            Some("txt") | Some("md") => "text/plain".to_string(),
            Some("png") => "image/png".to_string(),
            Some("jpg") | Some("jpeg") => "image/jpeg".to_string(),
            Some("gif") => "image/gif".to_string(),
            Some("webp") => "image/webp".to_string(),
            Some("svg") => "image/svg+xml".to_string(),
            Some("qtsq") => "application/x-qtsq".to_string(),
            _ => "application/octet-stream".to_string(),
        };
        return Ok(Response {
            final_url: raw_url.to_string(),
            status: 200,
            body: bytes,
            content_type,
            cache_control: String::new(),
            etag: String::new(),
            last_modified: String::new(),
            protocol: "file".to_string(),
            content_disposition: String::new(),
            www_authenticate: String::new(),
        });
    }

    let mut parsed =
        Url::parse(raw_url).map_err(|e| FetchError::InvalidUrl(format!("{raw_url}: {e}")))?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(FetchError::UnsupportedScheme(parsed.scheme().to_string()));
    }
    privacy::apply_request_url_policy(&mut parsed, top_level)?;
    let request_url = parsed.to_string();
    let is_https = parsed.scheme() == "https";
    let host = parsed.host_str().unwrap_or("").to_string();
    let path = if parsed.path().is_empty() {
        "/"
    } else {
        parsed.path()
    };

    // G4: Basic credentials embedded in the URL (`https://user:pass@host`)
    // — used only when the caller passed no explicit Authorization header.
    let url_credentials = if parsed.username().is_empty() {
        None
    } else {
        let user = percent_encoding::percent_decode_str(parsed.username())
            .decode_utf8_lossy()
            .into_owned();
        let pass = percent_encoding::percent_decode_str(parsed.password().unwrap_or(""))
            .decode_utf8_lossy()
            .into_owned();
        Some(auth::basic_credentials(&user, &pass))
    };

    // Effective header set — a 401 retry (G4) swaps the Authorization entry.
    let mut effective: Vec<(String, String)> = headers.to_vec();
    if let Some(basic) = url_credentials {
        if !effective
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("authorization"))
        {
            effective.push(("Authorization".to_string(), basic));
        }
    }
    if !cookies_allowed_for_request(origin, &request_url) {
        effective.retain(|(name, _)| !name.eq_ignore_ascii_case("cookie"));
    }
    let explicit_cookie = effective
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("cookie"));
    add_dnt_header(&mut effective);

    // One automatic retry on a 401 challenge when an auth provider is
    // installed (G4, auth.rs); anything else answers as-is.
    let mut attempt = 0usize;
    let resp = loop {
        let mut req = client.request(
            reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET),
            parsed.clone(),
        );
        for (k, v) in &effective {
            req = req.header(k.as_str(), v.as_str());
        }
        // Cookies (Phase O2): attach matches, unless opted out.
        req = attach_cookies_for_origin(req, &host, path, is_https, origin, explicit_cookie);
        if let Some(payload) = body {
            req = req.body(payload.to_vec());
        }
        let resp = req
            .send()
            .map_err(|e| FetchError::Transport(format!("request failed: {e}")))?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED && attempt == 0 {
            let challenge = resp
                .headers()
                .get(reqwest::header::WWW_AUTHENTICATE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            if let Some(authorization) =
                auth::authorization_for_challenge(&request_url, method, &challenge)
            {
                attempt += 1;
                match effective
                    .iter_mut()
                    .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
                {
                    Some(slot) => slot.1 = authorization,
                    None => effective.push(("Authorization".to_string(), authorization)),
                }
                continue;
            }
        }
        break resp;
    };

    let status = resp.status().as_u16();
    let final_url = resp.url().to_string();
    let headers_map = resp.headers().clone();
    privacy::validate_final_url(&final_url, top_level)?;
    for value in headers_map
        .get_all("strict-transport-security")
        .iter()
        .filter_map(|value| value.to_str().ok())
    {
        observe_hsts(&final_url, value);
    }
    let header_str = |name: reqwest::header::HeaderName| {
        headers_map
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    let content_type = header_str(reqwest::header::CONTENT_TYPE);
    let cache_control = header_str(reqwest::header::CACHE_CONTROL);
    let etag = header_str(reqwest::header::ETAG);
    let last_modified = header_str(reqwest::header::LAST_MODIFIED);
    // H1: the download decision (`attachment`) and suggested filename read this.
    let content_disposition = header_str(reqwest::header::CONTENT_DISPOSITION);
    // H6: the 401 sign-in flow reads the challenge (scheme + realm) from this.
    let www_authenticate = header_str(reqwest::header::WWW_AUTHENTICATE);
    let protocol = version_str(resp.version());
    // Store cookies before reading the body (Phase O2 — shared helper).
    store_cookies_from_for_origin(&resp, origin);
    // A 304 has no body by definition; surface the status and let the cache
    // layer (`fetch_cached`) serve the stored bytes.
    if status == 304 {
        return Ok(Response {
            final_url,
            status,
            body: Vec::new(),
            content_type,
            cache_control,
            etag,
            last_modified,
            protocol,
            content_disposition: String::new(),
            www_authenticate: String::new(),
        });
    }
    let body = resp
        .bytes()
        .map_err(|e| FetchError::Transport(format!("reading body: {e}")))?
        .to_vec();

    Ok(Response {
        final_url,
        status,
        body,
        content_type,
        cache_control,
        etag,
        last_modified,
        protocol,
        content_disposition,
        www_authenticate,
    })
}

/// G1: the shared client pinned to HTTP/2 with prior knowledge. HTTPS
/// already negotiates h2 through ALPN on the normal client; this exists for
/// plain-text h2c (`http://` origins that speak h2 directly), which no
/// browser turns on by default.
fn h2_client() -> &'static reqwest::blocking::Client {
    static H2: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    H2.get_or_init(|| {
        let cfg = CONFIG.get().cloned().unwrap_or_default();
        reqwest::blocking::Client::builder()
            .user_agent(cfg.user_agent)
            .connect_timeout(cfg.connect_timeout)
            .timeout(cfg.total_timeout)
            .http2_prior_knowledge()
            .build()
            .expect("failed to build HTTP/2 client")
    })
}

/// Fetch over plain-text HTTP/2 with prior knowledge (G1). HTTPS origins
/// upgrade to h2 automatically via ALPN — use [`fetch`] for those.
pub fn fetch_h2c(raw_url: &str) -> Result<Response, FetchError> {
    fetch_inner(
        h2_client(),
        "GET",
        raw_url,
        &[],
        None,
        &current_origin(),
        false,
    )
}

/// Resolve `href` against the page's base URL.
pub fn resolve(base: &str, href: &str) -> Option<String> {
    let href = href.trim();
    if href.is_empty() {
        return None;
    }
    if href.starts_with("about:") || href.starts_with("kitae:") {
        return Some(href.to_string());
    }
    let base = Url::parse(base).ok()?;
    base.join(href).ok().map(|u| u.to_string())
}

/// I6 fetch policy — mixed-content blocking: a page loaded over `https` must
/// not pull subresources (images, stylesheets, fonts, imports) over
/// plaintext `http`, where a network attacker could rewrite content the
/// secure page will trust. Loopback hosts are exempt: the secure-contexts
/// spec treats them as potentially trustworthy, and they never leave the
/// machine. Unparseable bases (`file://` pages, empty strings) and
/// non-http(s) subresources (`data:`, `about:`) allow — the policy only
/// tightens the https→http edge.
pub fn mixed_content_allowed(page_base: &str, sub_url: &str) -> bool {
    let (Ok(page), Ok(sub)) = (Url::parse(page_base.trim()), Url::parse(sub_url.trim())) else {
        return true;
    };
    if page.scheme() != "https" || sub.scheme() != "http" {
        return true;
    }
    if hsts_covers(sub_url) {
        return true;
    }
    is_loopback_host(sub.host_str().unwrap_or(""))
}

/// Whether `host` is a loopback origin: the loopback IP literals (v4 and v6),
/// `localhost`, and its subdomains. IPv6 hosts arrive bracketed from
/// [`Url::host_str`].
fn is_loopback_host(host: &str) -> bool {
    let host = host.trim();
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    if bare.eq_ignore_ascii_case("localhost") || bare.ends_with(".localhost") {
        return true;
    }
    bare.parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::support::*;

    fn response(body: Vec<u8>, content_type: &str) -> Response {
        Response {
            final_url: "https://example.com/".to_string(),
            status: 200,
            body,
            content_type: content_type.to_string(),
            cache_control: String::new(),
            etag: String::new(),
            last_modified: String::new(),
            protocol: "HTTP/1.1".to_string(),
            content_disposition: String::new(),
            www_authenticate: String::new(),
        }
    }

    #[test]
    fn fetch_errors_are_typed_and_displayable() {
        let e = fetch("not a url at all").unwrap_err();
        assert!(matches!(e, FetchError::InvalidUrl(_)));
        assert!(!e.to_string().is_empty());

        let e = fetch("ftp://example.com/x").unwrap_err();
        assert_eq!(e, FetchError::UnsupportedScheme("ftp".into()));

        let e = fetch("file:///definitely/not/here.txt").unwrap_err();
        assert!(matches!(e, FetchError::FileIo(_)));
    }

    #[test]
    fn normalizes_bare_hosts() {
        let _lock = privacy_test_lock();
        reset_privacy_config();
        assert_eq!(normalize_input("example.com"), "https://example.com");
        assert_eq!(normalize_input("localhost:8080"), "http://localhost:8080");
        assert_eq!(normalize_input("127.0.0.1:8080"), "http://127.0.0.1:8080");
        assert_eq!(normalize_input("http://example.com"), "http://example.com");
        assert_eq!(normalize_input("about:blank"), "about:blank");
        assert_eq!(normalize_input("file:///tmp/page"), "file:///tmp/page");
        assert_eq!(
            normalize_input("rust blog posts"),
            "https://html.duckduckgo.com/html/?q=rust%20blog%20posts"
        );
    }

    #[test]
    fn resolves_relative_links() {
        assert_eq!(
            resolve("https://example.com/a/b/page.html", "./other.html").unwrap(),
            "https://example.com/a/b/other.html"
        );
        assert_eq!(
            resolve("https://example.com/a/b/", "/root.html").unwrap(),
            "https://example.com/root.html"
        );
        assert_eq!(
            resolve("https://example.com/a/", "https://other.org/x").unwrap(),
            "https://other.org/x"
        );
    }

    #[test]
    fn mixed_content_blocks_https_to_http() {
        let _lock = privacy_test_lock();
        clear_hsts();
        // The core rule: secure page, plaintext subresource → blocked.
        assert!(!mixed_content_allowed(
            "https://example.com/",
            "http://other.example/pic.png"
        ));
        assert!(!mixed_content_allowed(
            "https://example.com/",
            "http://example.com/pic.png"
        ));
        store_hsts_for_test("other.example", Duration::from_secs(60), false);
        assert!(mixed_content_allowed(
            "https://example.com/",
            "http://other.example/pic.png"
        ));
        clear_hsts();
        // Same-origin or not, the transport is what matters.
        assert!(!mixed_content_allowed(
            "https://example.com/a",
            "http://example.com:8080/x.png"
        ));
        // Everything except the https→http edge allows.
        assert!(mixed_content_allowed(
            "https://example.com/",
            "https://other.example/pic.png"
        ));
        assert!(mixed_content_allowed(
            "http://example.com/",
            "http://other.example/pic.png"
        ));
        assert!(mixed_content_allowed(
            "http://example.com/",
            "https://secure.example/pic.png"
        ));
        // Non-https pages have no secure context to undermine (file, blank).
        assert!(mixed_content_allowed(
            "file:///tmp/page.html",
            "http://other.example/pic.png"
        ));
        assert!(mixed_content_allowed("", "http://other.example/pic.png"));
        // Non-http subresources carry no plaintext transport.
        assert!(mixed_content_allowed(
            "https://example.com/",
            "data:image/png;base64,aGk="
        ));
        // Unparseable subresource: allow, the fetch itself fails closed.
        assert!(mixed_content_allowed(
            "https://example.com/",
            "not a url at all"
        ));
    }

    #[test]
    fn mixed_content_exempts_loopback_origins() {
        let _lock = privacy_test_lock();
        clear_hsts();
        // Potentially-trustworthy origins per the secure-contexts spec.
        assert!(mixed_content_allowed(
            "https://example.com/",
            "http://localhost:8080/x"
        ));
        assert!(mixed_content_allowed(
            "https://example.com/",
            "http://LOCALHOST/x"
        ));
        assert!(mixed_content_allowed(
            "https://example.com/",
            "http://api.localhost/x"
        ));
        assert!(mixed_content_allowed(
            "https://example.com/",
            "http://127.0.0.1:9000/x"
        ));
        assert!(mixed_content_allowed(
            "https://example.com/",
            "http://127.13.0.9/x"
        ));
        assert!(mixed_content_allowed(
            "https://example.com/",
            "http://[::1]:9000/x"
        ));
        // ...but only loopback — other private ranges are still plaintext.
        assert!(!mixed_content_allowed(
            "https://example.com/",
            "http://192.168.1.4/x"
        ));
        assert!(!mixed_content_allowed(
            "https://example.com/",
            "http://10.0.0.2/x"
        ));
        assert!(!mixed_content_allowed(
            "https://example.com/",
            "http://[fe80::1]/x"
        ));
        // Hostnames that merely contain the words do not pass.
        assert!(!mixed_content_allowed(
            "https://example.com/",
            "http://localhost.evil.example/x"
        ));
        assert!(!mixed_content_allowed(
            "https://example.com/",
            "http://127.0.0.1.evil.example/x"
        ));
    }

    #[test]
    fn decodes_declared_charsets() {
        // windows-1252: "Café" with é as 0xE9, plus the curly-quote remap.
        let r = response(
            vec![0x43, 0x61, 0x66, 0xE9],
            "text/html; charset=windows-1252",
        );
        assert_eq!(r.text(), "Café");
        let r = response(vec![0x93, 0x71, 0x94], "text/html; charset=ISO-8859-1");
        assert_eq!(r.text(), "\u{201C}q\u{201D}");
        // Header parsing is case- and quote-insensitive.
        let r = response(vec![0xE9], "Text/HTML; CHARSET=\"iso-8859-1\"");
        assert_eq!(r.text(), "é");
    }

    #[test]
    fn bom_overrides_the_declared_charset() {
        // UTF-16LE BOM + "hi", while the header claims UTF-8.
        let mut body = vec![0xFF, 0xFE, b'h', 0, b'i', 0];
        body.extend_from_slice("é".as_bytes()); // trailing junk after units — lossy is fine
        let r = response(body, "text/html; charset=utf-8");
        assert!(r.text().starts_with("hi"));
    }

    #[test]
    fn undecodable_bytes_degrade_lossily_notfatally() {
        // 0xFF alone is invalid UTF-8 and no BOM sequence (FF FE would be);
        // expect replacement marks, never an error.
        let r = response(vec![0xFF, 0xFD], "text/html; charset=x-user-defined");
        assert_eq!(r.text(), "\u{FFFD}\u{FFFD}");
    }

    #[test]
    fn cache_hit_eviction_and_ttl() {
        let mut c = CacheInner::new(2, Duration::from_secs(300));
        let t0 = Instant::now();
        let dummy = |u: &str| response(u.as_bytes().to_vec(), "text/html");

        c.insert("a".into(), dummy("a"), t0);
        c.insert("b".into(), dummy("b"), t0);
        c.insert("c".into(), dummy("c"), t0); // evicts "a" (LRU cap 2)
        assert_eq!(cached_keys(&c), vec!["b", "c"]);
        assert!(matches!(c.lookup("a", t0), CacheLookup::Miss));

        // A hit bumps recency: "b" survives, "c" is evicted next.
        assert!(matches!(c.lookup("b", t0), CacheLookup::Fresh(_)));
        c.insert("d".into(), dummy("d"), t0);
        assert_eq!(cached_keys(&c), vec!["b", "d"]);

        // TTL expiry: past the entry's lifetime it probes stale (for
        // conditional revalidation), never fresh.
        assert!(matches!(
            c.lookup("b", t0 + Duration::from_secs(301)),
            CacheLookup::Stale(_)
        ));
    }

    #[test]
    fn cache_control_sets_ttl_and_no_store() {
        assert_eq!(cache_ttl("max-age=60"), Some(Duration::from_secs(60)));
        assert_eq!(
            cache_ttl("public, MAX-AGE=3600"),
            Some(Duration::from_secs(3600))
        );
        assert_eq!(cache_ttl("no-store"), None);
        assert_eq!(cache_ttl(""), None);

        let mut c = CacheInner::new(8, Duration::from_secs(300));
        let t0 = Instant::now();
        let mut r = response(b"x".to_vec(), "text/html");
        r.cache_control = "max-age=1".into();
        c.insert("k".into(), r, t0);
        assert!(
            matches!(
                c.lookup("k", t0 + Duration::from_secs(2)),
                CacheLookup::Stale(_)
            ),
            "max-age=1 must expire before the 300s default"
        );
    }

    #[test]
    fn fetch_cached_roundtrips_a_local_file() {
        let _lock = cache_test_lock();
        clear_cache();
        let url = concat!(
            "file://",
            env!("CARGO_MANIFEST_DIR"),
            "/../kitae_html/src/lib.rs"
        );
        let first = fetch_cached(url).unwrap();
        assert_eq!(cached_len(), 1);
        let second = fetch_cached(url).unwrap();
        assert_eq!(first.body, second.body);
        assert_eq!(cached_len(), 1, "same key must not grow the cache");
        clear_cache();
        assert_eq!(cached_len(), 0);
    }

    #[test]
    fn image_content_types_detected() {
        assert!(response(vec![], "image/png").is_image());
        assert!(response(vec![], "Image/JPEG; charset=binary").is_image());
        assert!(!response(vec![], "text/html").is_image());
        assert!(!response(vec![], "text/plain").is_image());
    }

    #[test]
    fn qtsq_detected_by_type_or_magic() {
        // Declared content type wins, empty body or not.
        assert!(response(vec![], "application/x-qtsq").is_qtsq());
        assert!(response(vec![], "Application/X-QTSQ").is_qtsq());
        // Magic sniffing catches servers that mislabel the file.
        assert!(response(b"QTS\x01payload".to_vec(), "application/octet-stream").is_qtsq());
        // Other binaries stay rejected.
        assert!(!response(vec![], "application/octet-stream").is_qtsq());
        assert!(!response(b"QTS\x02wrong version".to_vec(), "").is_qtsq());
        assert!(!response(b"QT".to_vec(), "").is_qtsq());
        assert!(!response(vec![], "text/html").is_qtsq());
    }

    fn cached_keys(c: &CacheInner) -> Vec<&str> {
        c.order.iter().map(|s| s.as_str()).collect()
    }

    #[test]
    fn compressed_responses_are_transparently_decoded() {
        let _lock = cache_test_lock();
        clear_cache();
        let original = "<html><body><p>compressed world — kitae net G3</p></body></html>";
        let gz = {
            use std::io::Write;
            let mut out = Vec::new();
            let mut e = flate2::write::GzEncoder::new(&mut out, flate2::Compression::default());
            e.write_all(original.as_bytes()).unwrap();
            e.finish().unwrap();
            out
        };
        let zf = {
            use std::io::Write;
            let mut out = Vec::new();
            let mut e = flate2::write::ZlibEncoder::new(&mut out, flate2::Compression::default());
            e.write_all(original.as_bytes()).unwrap();
            e.finish().unwrap();
            out
        };
        let br = {
            use std::io::Write;
            let mut out = Vec::new();
            let mut c = brotli::CompressorWriter::new(&mut out, 4096, 9, 22);
            c.write_all(original.as_bytes()).unwrap();
            drop(c);
            out
        };
        let (gz2, br2, zf2) = (gz.clone(), br.clone(), zf.clone());
        let (base, log, handle) = start_server_raw(4, move |req| {
            if req.starts_with("GET /gz") {
                http_ok_bytes(&gz2, "gzip")
            } else if req.starts_with("GET /br") {
                http_ok_bytes(&br2, "br")
            } else if req.starts_with("GET /deflate") {
                http_ok_bytes(&zf2, "deflate")
            } else {
                http_ok_bytes(original.as_bytes(), "")
            }
        });
        for (path, enc) in [
            ("/gz", "gzip"),
            ("/br", "br"),
            ("/deflate", "deflate"),
            ("/plain", "identity"),
        ] {
            let resp = fetch_with("GET", &format!("{base}{path}"), &[], None)
                .unwrap_or_else(|e| panic!("fetch {path} ({enc}) failed: {e:?}"));
            assert_eq!(resp.status, 200, "{path} ({enc})");
            assert!(
                resp.text().contains("compressed world"),
                "{path} ({enc}) body not decoded: {:?}",
                resp.text()
            );
        }
        handle.join().expect("server thread");
        let requests = log.lock().unwrap();
        let all = requests.join("\n").to_ascii_lowercase();
        assert!(
            all.contains("accept-encoding"),
            "client must advertise Accept-Encoding"
        );
        assert!(
            all.contains("br") || all.contains("brotli"),
            "brotli must be advertised: {all:?}"
        );
        clear_cache();
    }

    #[test]
    fn post_sends_method_headers_and_body_and_is_never_cached() {
        let _lock = cache_test_lock();
        clear_cache();
        let before = cached_len();
        let (base, log, handle) = start_server(1, |_| http_ok("<p>posted</p>", ""));
        let url = format!("{base}/submit");
        let resp = fetch_with(
            "POST",
            &url,
            &[(
                "Content-Type".to_string(),
                "application/x-www-form-urlencoded".to_string(),
            )],
            Some(b"name=ada&city=leeds"),
        )
        .expect("POST to loopback");
        assert_eq!(resp.status, 200);
        assert!(resp.text().contains("posted"));
        assert_eq!(
            cached_len(),
            before,
            "POST responses must never enter the cache"
        );
        handle.join().expect("server thread");
        let requests = log.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let req = &requests[0];
        assert!(req.starts_with("POST /submit "), "request line: {req:?}");
        let req_lower = req.to_ascii_lowercase();
        assert!(
            req_lower.contains("content-type: application/x-www-form-urlencoded"),
            "headers: {req:?}"
        );
        assert!(req.ends_with("name=ada&city=leeds"), "body: {req:?}");
        clear_cache();
    }

    #[test]
    fn fetch_with_get_matches_fetch_on_files() {
        let url = concat!(
            "file://",
            env!("CARGO_MANIFEST_DIR"),
            "/../kitae_html/src/lib.rs"
        );
        let a = fetch(url).unwrap();
        let b = fetch_with("GET", url, &[], None).unwrap();
        assert_eq!(a.body, b.body);
        assert_eq!(a.content_type, b.content_type);
    }

    #[test]
    fn cookie_jar_scopes_by_domain_path_and_secure() {
        use std::time::Instant;
        let now = Instant::now();
        let mut jar = CookieJar::default();
        jar.store(
            &[
                "sid=abc; Path=/; Max-Age=100".to_string(),
                "prefs=dark; Path=/settings; Max-Age=100".to_string(),
                "tok=secret; Path=/; Secure; Max-Age=100".to_string(),
                "wide=1; Domain=example.com; Path=/; Max-Age=100".to_string(),
                "dead=x; Path=/; Max-Age=0".to_string(),
            ],
            "example.com",
            "/settings/account",
            now,
        );
        assert_eq!(jar.len(), 4, "Max-Age=0 deletes instead of storing");

        // Plain http to the exact host: session + path-scoped, no Secure.
        let h = jar
            .header_for("example.com", "/settings/account", false, now)
            .unwrap();
        assert!(h.contains("sid=abc"), "{h}");
        assert!(h.contains("prefs=dark"), "{h}");
        assert!(
            !h.contains("tok=secret"),
            "Secure must not go over http: {h}"
        );

        // Path scoping: /settings/cookie is outside /settings/account.
        let h = jar.header_for("example.com", "/other", false, now).unwrap();
        assert!(h.contains("sid=abc"), "{h}");
        assert!(!h.contains("prefs=dark"), "{h}");

        // Subdomain: host-only sid stays home, Domain cookie travels.
        let h = jar.header_for("sub.example.com", "/", false, now).unwrap();
        assert!(!h.contains("sid=abc"), "host-only: {h}");
        assert!(h.contains("wide=1"), "{h}");

        // Unrelated host gets nothing.
        assert!(jar.header_for("other.org", "/", false, now).is_none());

        // https unlocks the Secure cookie.
        let h = jar.header_for("example.com", "/", true, now).unwrap();
        assert!(h.contains("tok=secret"), "{h}");

        // Replacement: same (name, domain, path) overwrites.
        jar.store(
            &["sid=def; Path=/; Max-Age=100".to_string()],
            "example.com",
            "/",
            now,
        );
        let h = jar.header_for("example.com", "/", false, now).unwrap();
        assert!(h.contains("sid=def") && !h.contains("sid=abc"), "{h}");
    }

    #[test]
    fn cookie_domain_forgery_is_rejected() {
        use std::time::Instant;
        let now = Instant::now();
        let mut jar = CookieJar::default();
        jar.store(
            &["evil=1; Domain=victim.com; Path=/".to_string()],
            "attacker.com",
            "/",
            now,
        );
        assert_eq!(jar.len(), 0, "cross-domain Set-Cookie must be dropped");
    }

    #[test]
    fn conditional_revalidation_serves_cached_body_on_304() {
        let _lock = cache_test_lock();
        clear_cache();
        let body_v1 = "<html><body><p>version one</p></body></html>";
        let (base, log, handle) = start_server(2, |req: &str| {
            if req.contains("If-None-Match: \"v1\"") {
                "HTTP/1.1 304 Not Modified\r\nConnection: close\r\n\r\n".to_string()
            } else {
                http_ok(body_v1, "ETag: \"v1\"\r\nCache-Control: max-age=0\r\n")
            }
        });
        let url = format!("{base}/page");
        let first = fetch_cached(&url).expect("first fetch");
        assert!(first.text().contains("version one"));
        assert_eq!(first.etag, "\"v1\"");
        // max-age=0 → immediately stale; the second call must revalidate.
        let second = fetch_cached(&url).expect("revalidation");
        assert!(
            second.text().contains("version one"),
            "304 serves the cached body"
        );
        handle.join().expect("server thread");
        let requests = log.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            !requests[0].to_ascii_lowercase().contains("if-none-match"),
            "first request is unconditional"
        );
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("if-none-match: \"v1\""),
            "stale entry revalidates: {:?}",
            requests[1]
        );
        clear_cache();
    }

    #[test]
    fn h2c_prior_knowledge_roundtrip() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept().expect("accept");
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut preface = [0u8; 24];
            stream.read_exact(&mut preface).expect("client preface");
            assert_eq!(
                &preface, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n",
                "h2c prior knowledge sends the magic preface"
            );
            // Frame loop: answer SETTINGS, then on the request HEADERS
            // respond with a literal-HPACK ":status 200" block and a body.
            let sid;
            loop {
                let mut fh = [0u8; 9];
                stream.read_exact(&mut fh).expect("frame header");
                let len = ((fh[0] as usize) << 16) | ((fh[1] as usize) << 8) | fh[2] as usize;
                let mut payload = vec![0u8; len];
                if len > 0 {
                    stream.read_exact(&mut payload).expect("frame payload");
                }
                match fh[3] {
                    4 => {
                        if fh[4] & 0x1 == 0 {
                            // SETTINGS: ours, then the ack request.
                            stream.write_all(&[0, 0, 0, 4, 0, 0, 0, 0, 0]).unwrap();
                            stream.write_all(&[0, 0, 0, 4, 1, 0, 0, 0, 0]).unwrap();
                        }
                    }
                    1 => {
                        sid = u32::from_be_bytes([fh[5], fh[6], fh[7], fh[8]]) & 0x7fff_ffff;
                        let status = [
                            0x00u8, 0x07, b':', b's', b't', b'a', b't', b'u', b's', 0x03, b'2',
                            b'0', b'0',
                        ];
                        let mut headers = vec![0, 0, 13, 1, 0x4];
                        headers.extend_from_slice(&sid.to_be_bytes());
                        headers.extend_from_slice(&status);
                        stream.write_all(&headers).unwrap();
                        let mut data = vec![0, 0, 9, 0, 0x1];
                        data.extend_from_slice(&sid.to_be_bytes());
                        data.extend_from_slice(b"h2c works");
                        stream.write_all(&data).unwrap();
                        break;
                    }
                    _ => {} // WINDOW_UPDATE, PING, …: ignore
                }
            }
            let _ = stream.flush();
        });
        let resp = fetch_h2c(&format!("http://127.0.0.1:{port}/h2c")).expect("h2c fetch");
        assert_eq!(resp.status, 200);
        assert_eq!(resp.protocol, "HTTP/2");
        assert_eq!(resp.text(), "h2c works");
        handle.join().expect("h2c server thread");
    }

    #[test]
    fn protocol_field_reports_the_transport() {
        let (base, _log, handle) = start_server(1, |_| http_ok("<p>h1</p>", ""));
        let resp = fetch(&format!("{base}/h1")).expect("h1 fetch");
        assert_eq!(resp.protocol, "HTTP/1.1");
        handle.join().expect("server");

        let resp = fetch("file:///dev/null").expect("file read");
        assert_eq!(resp.protocol, "file");
    }
}
