//! G7: Server-Sent Events (WHATWG HTML, "server-sent events"): a line-based
//! parser over a long-lived blocking GET plus automatic reconnection with
//! `Last-Event-ID`. There is no JS `EventSource` — the embedder drives the
//! stream from its own thread via [`SseStream::next_event`].

use std::io::Read;
use std::sync::OnceLock;
use std::time::Duration;

use crate::FetchError;

/// One dispatched SSE event.
///
/// `event` is the event type (`"message"` when the stream never named one);
/// `data` is the data lines joined with `\n`; `id` is the last event ID
/// string at dispatch time (it persists across events that carry no `id:`
/// line, per spec); `retry` is the stream's reconnection interval, which
/// `retry:` lines update the moment they are parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: String,
    pub data: String,
    pub id: Option<String>,
    pub retry: Duration,
}

/// A live Server-Sent Events subscription.
///
/// Per-connection parser state (event type, data buffer) resets on
/// reconnect; `last_id` and `retry` survive. Reconnection follows the spec
/// shape: on EOF or read error the stream waits `retry`, re-GETs the URL
/// with `Last-Event-ID`, and keeps going. A failed reconnection attempt
/// surfaces as `Err` (call again to keep trying while the budget lasts);
/// the stream ends quietly (`Ok(None)`) when the reconnect budget is spent,
/// or immediately for [`SseStream::next_event_once`].
pub struct SseStream {
    url: String,
    resp: Option<reqwest::blocking::Response>,
    pending: Vec<u8>,
    /// The UTF-8 BOM is still expected at the head of this connection.
    bom: bool,
    /// Event-type buffer — cleared at dispatch (spec: event type buffer).
    event: String,
    /// Data buffer — joined and cleared at dispatch.
    data: Vec<String>,
    /// `id:` field buffer — set at line-parse time (spec).
    id_buffer: Option<String>,
    /// Last event ID string — updated at dispatch from the buffer and sent
    /// as `Last-Event-ID` on reconnect.
    last_id: Option<String>,
    retry: Duration,
    reconnects_left: usize,
}

/// SSE connections may outlive any page load, so the client carries only a
/// connect budget — never the shared client's 30s total timeout.
fn stream_client() -> &'static reqwest::blocking::Client {
    static STREAM: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    STREAM.get_or_init(|| {
        let cfg = crate::CONFIG.get().cloned().unwrap_or_default();
        reqwest::blocking::Client::builder()
            .user_agent(cfg.user_agent)
            .connect_timeout(cfg.connect_timeout)
            .build()
            .expect("failed to build SSE client")
    })
}

/// Open a Server-Sent Events stream (G7).
pub fn sse_connect(raw_url: &str) -> Result<SseStream, FetchError> {
    SseStream::connect(raw_url)
}

impl SseStream {
    /// Connect and start streaming. Errors here are the initial fetch's
    /// failure: bad URL, non-200 status, wrong MIME type, or transport.
    pub fn connect(raw_url: &str) -> Result<SseStream, FetchError> {
        let mut stream = SseStream {
            url: raw_url.to_string(),
            resp: None,
            pending: Vec::new(),
            bom: true,
            event: String::new(),
            data: Vec::new(),
            id_buffer: None,
            last_id: None,
            retry: Duration::from_secs(3),
            reconnects_left: usize::MAX,
        };
        stream.open()?;
        Ok(stream)
    }

    /// Cap automatic reconnection (spec default: forever).
    pub fn set_reconnect_limit(&mut self, n: usize) {
        self.reconnects_left = n;
    }

    /// The last event ID string, as it would be sent on reconnect.
    pub fn last_event_id(&self) -> Option<&str> {
        self.last_id.as_deref()
    }

    fn open(&mut self) -> Result<(), FetchError> {
        let mut url = url::Url::parse(&self.url)
            .map_err(|e| FetchError::InvalidUrl(format!("{}: {e}", self.url)))?;
        if url.scheme() != "http" && url.scheme() != "https" {
            return Err(FetchError::UnsupportedScheme(url.scheme().to_string()));
        }
        crate::privacy::apply_request_url_policy(&mut url, false)?;
        let host = url.host_str().unwrap_or("").to_string();
        let path = if url.path().is_empty() {
            "/"
        } else {
            url.path()
        };
        let is_https = url.scheme() == "https";
        let mut req = stream_client().get(url.clone());
        req = req
            .header("Accept", "text/event-stream")
            .header("Cache-Control", "no-cache");
        if let Some(id) = &self.last_id {
            req = req.header("Last-Event-ID", id);
        }
        if crate::dnt_enabled() {
            req = req.header("DNT", "1");
        }
        let origin = crate::page_origin();
        req = crate::attach_cookies_for_origin(req, &host, path, is_https, &origin, false);
        let resp = req
            .send()
            .map_err(|e| FetchError::Transport(format!("sse connect: {e}")))?;
        let final_url = resp.url().to_string();
        crate::check_final_url(&final_url, false)?;
        if let Some(value) = resp
            .headers()
            .get("strict-transport-security")
            .and_then(|value| value.to_str().ok())
        {
            crate::observe_hsts(&final_url, value);
        }
        let status = resp.status().as_u16();
        if status != 200 {
            return Err(FetchError::Http { status });
        }
        crate::store_cookies_from_for_origin(&resp, &origin);
        let mime = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let mime = mime.split(';').next().unwrap_or("").trim().to_string();
        if mime != "text/event-stream" {
            return Err(FetchError::Transport(format!(
                "sse: expected text/event-stream, got {mime:?}"
            )));
        }
        // Per-connection parser state resets; last_id and retry survive.
        self.resp = Some(resp);
        self.pending.clear();
        self.bom = true;
        self.event.clear();
        self.data.clear();
        Ok(())
    }

    /// Next event, reconnecting across EOFs and read errors until the
    /// reconnect budget is spent (`Ok(None)`).
    pub fn next_event(&mut self) -> Result<Option<SseEvent>, FetchError> {
        loop {
            if self.resp.is_none() && !self.reconnect()? {
                return Ok(None);
            }
            match self.pump() {
                Ok(Some(event)) => return Ok(Some(event)),
                Ok(None) | Err(_) => {
                    if !self.reconnect()? {
                        return Ok(None);
                    }
                }
            }
        }
    }

    fn reconnect(&mut self) -> Result<bool, FetchError> {
        if self.reconnects_left == 0 {
            return Ok(false);
        }
        self.reconnects_left -= 1;
        std::thread::sleep(self.retry);
        self.open()?;
        Ok(true)
    }

    /// Next event from the current connection only — no reconnection, so a
    /// clean EOF is simply `Ok(None)`.
    pub fn next_event_once(&mut self) -> Result<Option<SseEvent>, FetchError> {
        if self.resp.is_none() {
            return Ok(None);
        }
        self.pump()
    }

    /// One read + parse pass over the live response. `Ok(None)` means "no
    /// dispatch yet" or "clean EOF" — the caller decides.
    fn pump(&mut self) -> Result<Option<SseEvent>, FetchError> {
        let mut resp = self
            .resp
            .take()
            .ok_or_else(|| FetchError::Transport("sse: stream closed".into()))?;
        let mut chunk = [0u8; 4096];
        match resp.read(&mut chunk) {
            Ok(0) => {
                // Clean EOF — but a dispatch's blank line may already sit in
                // `pending`; drain the buffered lines before giving up (the
                // response stays taken either way).
                if self.pending.is_empty() {
                    Ok(None)
                } else {
                    Ok(self.feed(&[]))
                }
            }
            Ok(n) => {
                self.resp = Some(resp);
                Ok(self.feed(&chunk[..n]))
            }
            Err(e) => Err(FetchError::Transport(format!("sse read: {e}"))),
        }
    }

    /// Feed bytes into the WHATWG line parser; `Some` on the first dispatch
    /// the bytes produced.
    fn feed(&mut self, chunk: &[u8]) -> Option<SseEvent> {
        self.pending.extend_from_slice(chunk);
        // Strip the UTF-8 BOM at the head of the stream (a chunk may split
        // it — hold bytes until the BOM is decided).
        if self.bom {
            const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];
            if self.pending.starts_with(&BOM) {
                self.pending.drain(..3);
                self.bom = false;
            } else if BOM.starts_with(&self.pending) {
                return None;
            } else {
                self.bom = false;
            }
        }
        loop {
            // Terminators are \r\n, \r, or \n; a lone \r at the buffer end
            // could still grow into \r\n, so wait for one more byte.
            let pos = self
                .pending
                .iter()
                .position(|&b| b == b'\n' || b == b'\r')?;
            let cr = self.pending[pos] == b'\r';
            if cr && pos + 1 == self.pending.len() {
                return None;
            }
            let line: Vec<u8> = self.pending.drain(..pos).collect();
            self.pending.remove(0); // the \r or \n just found
            if cr && self.pending.first() == Some(&b'\n') {
                self.pending.remove(0);
            }
            let line = String::from_utf8_lossy(&line).into_owned();
            if let Some(event) = self.process_line(&line) {
                return Some(event);
            }
        }
    }

    /// One line: comments ignored, fields applied (`id`/`retry`
    /// immediately), blank lines dispatch. `Some` on dispatch.
    fn process_line(&mut self, line: &str) -> Option<SseEvent> {
        if line.is_empty() {
            return self.dispatch();
        }
        if line.starts_with(':') {
            return None; // comment
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => self.event = value.to_string(),
            "data" => self.data.push(value.to_string()),
            "id" => {
                self.id_buffer = if value.is_empty() {
                    None
                } else {
                    Some(value.to_string())
                };
            }
            // Retry applies the moment the line is parsed, not at dispatch.
            "retry" => {
                if let Ok(ms) = value.parse::<u64>() {
                    self.retry = Duration::from_millis(ms);
                }
            }
            _ => {} // unknown fields are ignored per spec
        }
        None
    }

    /// Dispatch the buffered event; an empty data buffer resets and is
    /// ignored (spec).
    fn dispatch(&mut self) -> Option<SseEvent> {
        if self.data.is_empty() {
            self.event.clear();
            return None;
        }
        self.last_id = self.id_buffer.clone();
        let event = SseEvent {
            event: if self.event.is_empty() {
                "message".to_string()
            } else {
                std::mem::take(&mut self.event)
            },
            data: self.data.join("\n"),
            id: self.last_id.clone(),
            retry: self.retry,
        };
        self.data.clear();
        Some(event)
    }

    #[cfg(test)]
    fn parser_only() -> SseStream {
        SseStream {
            url: String::new(),
            resp: None,
            pending: Vec::new(),
            bom: true,
            event: String::new(),
            data: Vec::new(),
            id_buffer: None,
            last_id: None,
            retry: Duration::from_secs(3),
            reconnects_left: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::support::{privacy_test_lock, start_server, start_server_raw};

    fn sse_ok(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn bom_crlf_and_lone_cr_edges() {
        let mut s = SseStream::parser_only();
        // BOM split across chunks; a lone \r at a chunk boundary must wait
        // (it could still be the head of \r\n).
        assert_eq!(s.feed(&[0xEF, 0xBB]), None);
        assert_eq!(s.feed(&[0xBF, b'd']), None);
        assert_eq!(s.feed(b"ata: bo"), None);
        assert_eq!(s.feed(b"nj\r"), None);
        let event = s.feed(b"\ndata: two\r\n\r\n").expect("dispatch");
        assert_eq!(event.event, "message");
        assert_eq!(event.data, "bonj\ntwo");
        assert_eq!(event.id, None);
    }

    #[test]
    fn comments_and_unknown_fields_are_ignored_and_empty_id_resets() {
        let mut s = SseStream::parser_only();
        let event = s
            .feed(b": keepalive\n: note\nfoo: bar\nid: 7\ndata: x\nid:\ndata: y\n\n")
            .expect("dispatch");
        assert_eq!(event.data, "x\ny");
        assert_eq!(event.id, None, "a bare `id:` resets the last event ID");
        assert_eq!(s.last_event_id(), None);
    }

    #[test]
    fn reconnects_with_last_event_id_and_reapplies_retry() {
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = counter.clone();
        let (base, log, handle) = start_server(2, move |_req| {
            let i = count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if i == 0 {
                sse_ok("retry: 10\r\n\r\nevent: add\r\ndata: hello\r\nid: 1\r\n\r\ndata: second\r\n\r\n")
            } else {
                sse_ok("retry: 50\r\n\r\ndata: third\r\n\r\n")
            }
        });
        let mut stream = crate::sse_connect(&format!("{base}/one")).expect("connect");
        stream.set_reconnect_limit(1);

        let e1 = stream.next_event().expect("e1").expect("e1");
        assert_eq!(e1.event, "add");
        assert_eq!(e1.data, "hello");
        assert_eq!(e1.id.as_deref(), Some("1"));
        assert_eq!(e1.retry, Duration::from_millis(10));

        let e2 = stream.next_event().expect("e2").expect("e2");
        assert_eq!(e2.event, "message", "unnamed events dispatch as message");
        assert_eq!(e2.data, "second");
        assert_eq!(e2.id.as_deref(), Some("1"), "last event ID persists");

        // Connection 1 EOFs: one reconnect carrying Last-Event-ID: 1, and
        // connection 2's `retry: 50` re-applies immediately.
        let e3 = stream.next_event().expect("e3").expect("e3");
        assert_eq!(e3.data, "third");
        assert_eq!(e3.retry, Duration::from_millis(50));

        assert_eq!(stream.next_event().expect("budget spent"), None);
        handle.join().expect("server");

        let requests = log.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            !requests[0].contains("last-event-id:"),
            "first connect is fresh: {:?}",
            requests[0]
        );
        assert!(
            requests[1].contains("last-event-id: 1"),
            "reconnect replays the ID: {:?}",
            requests[1]
        );
        assert!(requests[0].contains("accept: text/event-stream"));
        assert!(requests[0].contains("cache-control: no-cache"));
    }

    #[test]
    fn reconnects_after_a_body_read_error() {
        let responses = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = responses.clone();
        let (base, _log, handle) = start_server_raw(2, move |_request| {
            if count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\nnot-a-chunk\r\n".to_vec()
            } else {
                sse_ok("data: recovered\r\n\r\n").into_bytes()
            }
        });
        let mut stream = crate::sse_connect(&base).expect("initial connection");
        stream.retry = Duration::from_millis(1);
        stream.set_reconnect_limit(1);
        let event = stream
            .next_event()
            .expect("read error should reconnect")
            .expect("event");
        assert_eq!(event.data, "recovered");
        handle.join().expect("server");
    }

    #[test]
    fn dnt_reaches_sse_requests() {
        let _lock = privacy_test_lock();
        crate::reset_privacy_config();
        crate::set_dnt_enabled(true);
        let (base, log, handle) = start_server(1, |_req| sse_ok("data: one\n\n"));
        let mut stream = crate::sse_connect(&base).expect("connect");
        assert!(stream.next_event_once().unwrap().is_some());
        handle.join().unwrap();
        assert!(log.lock().unwrap()[0]
            .to_ascii_lowercase()
            .contains("dnt: 1"));
        crate::reset_privacy_config();
    }

    #[test]
    fn next_event_once_never_reconnects() {
        let (base, log, handle) = start_server(1, |_| sse_ok("data: only\r\n\r\n"));
        let mut stream = crate::sse_connect(&base).expect("connect");
        let event = stream.next_event_once().expect("event").expect("some");
        assert_eq!(event.event, "message");
        assert_eq!(event.data, "only");
        handle.join().expect("server");
        assert_eq!(stream.next_event_once().expect("eof"), None);
        assert_eq!(
            log.lock().unwrap().len(),
            1,
            "next_event_once must not reconnect after EOF"
        );
    }
}
