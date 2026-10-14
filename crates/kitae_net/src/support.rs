//! Test-only harness shared by every kitae_net module's tests (moved out of
//! lib.rs so auth/download/ws/sse/warm/h3 tests reuse the same loopback
//! servers instead of duplicating them).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

/// Serve `n` requests on 127.0.0.1, then exit. `respond` maps the raw
/// request (head + body) to a raw response. Returns the base URL and the
/// request log (raw request strings, in order).
pub(crate) fn start_server(
    n: usize,
    respond: impl Fn(&str) -> String + Send + 'static,
) -> (String, Arc<Mutex<Vec<String>>>, std::thread::JoinHandle<()>) {
    start_server_raw(n, move |req| respond(req).into_bytes())
}

/// Byte-level variant: `respond` returns the raw response bytes, so
/// fixtures can carry compressed (non-UTF-8) bodies.
pub(crate) fn start_server_raw(
    n: usize,
    respond: impl Fn(&str) -> Vec<u8> + Send + 'static,
) -> (String, Arc<Mutex<Vec<String>>>, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let base = format!("http://{}", listener.local_addr().unwrap());
    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let log_thread = log.clone();
    let handle = std::thread::spawn(move || {
        for _ in 0..n {
            let Ok((stream, _)) = listener.accept() else {
                break;
            };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut head = String::new();
            let mut content_len = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() || line.is_empty() {
                    break;
                }
                if line == "\r\n" || line == "\n" {
                    break;
                }
                if let Some(v) = line
                    .strip_prefix("Content-Length:")
                    .or_else(|| line.strip_prefix("content-length:"))
                {
                    content_len = v.trim().parse().unwrap_or(0);
                }
                head.push_str(&line);
            }
            let mut body = vec![0u8; content_len];
            let _ = reader.read_exact(&mut body);
            let raw = format!("{head}{}", String::from_utf8_lossy(&body));
            log_thread.lock().unwrap().push(raw.clone());
            let response = respond(&raw);
            let _ = stream.try_clone();
            let mut writer = stream;
            let _ = writer.write_all(&response);
            let _ = writer.flush();
            // Drop = close (Connection: close below).
        }
    });
    (base, log, handle)
}

pub(crate) fn http_ok(body: &str, extra_headers: &str) -> String {
    // The head must end with a blank line before the body. Callers may pass
    // extra headers with or without a trailing CRLF — emit exactly one.
    let sep = if extra_headers.is_empty() || extra_headers.ends_with("\r\n") {
        "\r\n"
    } else {
        "\r\n\r\n"
    };
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}{sep}{body}",
        body.len()
    )
}

/// G3: raw-bytes 200 response, optionally stamped with a
/// `Content-Encoding`. Empty encoding = identity (no header emitted).
pub(crate) fn http_ok_bytes(body: &[u8], content_encoding: &str) -> Vec<u8> {
    let enc = if content_encoding.is_empty() {
        String::new()
    } else {
        format!("Content-Encoding: {content_encoding}\r\n")
    };
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n{enc}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut out = head.into_bytes();
    out.extend_from_slice(body);
    out
}

/// Tests that touch the process-wide response cache (or the process-wide
/// auth provider) hold this, so their clear/assert sequences stay isolated
/// under parallel test threads.
pub(crate) fn cache_test_lock() -> MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
}

pub(crate) fn privacy_test_lock() -> MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
}

/// Poll `pred` on a short interval until it holds or the deadline passes
/// (background-thread work — prefetch, preconnect — is inherently async to
/// the caller).
pub(crate) fn wait_until(mut pred: impl FnMut() -> bool, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if pred() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    pred()
}
