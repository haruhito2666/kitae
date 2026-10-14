//! G5: ranged fetches and resumable downloads.
//!
//! `fetch_range` speaks HTTP Range for subresource slicing. `download_to_file`
//! streams a body to disk through a `<dest>.part` file: an existing part
//! resumes with `Range` + `If-Range` (the ETag recorded in a `<dest>.meta`
//! sidecar), a 206 appends, and a 200 — the server ignored the range, or
//! `If-Range` detected the resource changed — rewrites from scratch. The
//! progress callback is the download-manager hook: returning false cancels,
//! leaving the part file in place for a later resume.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::sync::OnceLock;

use url::Url;

use crate::{FetchError, Response};

/// G5: a `Range: bytes=start-end` fetch. `end` is inclusive (HTTP range
/// semantics); `None` means "to the last byte". A server that ignores the
/// range answers 200 with the whole body — per spec that is surfaced as-is
/// (check `status` before trusting the slice); an unsatisfiable range
/// answers 416.
pub fn fetch_range(raw_url: &str, start: u64, end: Option<u64>) -> Result<Response, FetchError> {
    let value = match end {
        Some(e) => format!("bytes={start}-{e}"),
        None => format!("bytes={start}-"),
    };
    crate::fetch_for_origin(
        "GET",
        raw_url,
        &[("Range".to_string(), value)],
        None,
        &crate::page_origin(),
    )
}

/// Progress hook: `(downloaded_so_far, total_bytes)`. `total` is `None` when
/// the server sent no length. Return `false` to cancel.
pub type DownloadHook<'a> = &'a mut dyn FnMut(u64, Option<u64>) -> bool;

/// Download `url` to `dest`, streaming through `<dest>.part` (resumable —
/// see the module docs). Returns the final file size in bytes. The 16 KiB
/// read loop keeps memory flat no matter how large the body is.
pub fn download_to_file(
    raw_url: &str,
    dest: &Path,
    mut on_progress: Option<DownloadHook<'_>>,
) -> Result<u64, FetchError> {
    let part = sibling_path(dest, ".part");
    let meta = sibling_path(dest, ".meta");

    let mut parsed =
        Url::parse(raw_url).map_err(|e| FetchError::InvalidUrl(format!("{raw_url}: {e}")))?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(FetchError::UnsupportedScheme(parsed.scheme().to_string()));
    }
    crate::privacy::apply_request_url_policy(&mut parsed, false)?;
    let https = parsed.scheme() == "https";
    let host = parsed.host_str().unwrap_or("").to_string();
    let path = if parsed.path().is_empty() {
        "/"
    } else {
        parsed.path()
    };

    // Resume attempt: whatever is on disk anchors the Range, and the ETag
    // recorded last run (if any) lets the server refuse a stale resume.
    let resume_from = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    let mut headers: Vec<(String, String)> = Vec::new();
    if resume_from > 0 {
        headers.push(("Range".to_string(), format!("bytes={resume_from}-")));
        if let Ok(etag) = std::fs::read_to_string(&meta) {
            if !etag.is_empty() {
                headers.push(("If-Range".to_string(), etag));
            }
        }
    }
    crate::add_dnt_header(&mut headers);
    let origin = crate::page_origin();
    if !crate::cookies_allowed_for_host(&origin, https, &host) {
        headers.retain(|(name, _)| !name.eq_ignore_ascii_case("cookie"));
    }
    let explicit_cookie = headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("cookie"));

    // Downloads stream for minutes, so no whole-request budget — only the
    // connect handshake stays bounded. Own client: the shared one carries a
    // 30 s total timeout that would kill large transfers.
    let mut req = download_client().request(reqwest::Method::GET, parsed.clone());
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }
    req = crate::attach_cookies_for_origin(req, &host, path, https, &origin, explicit_cookie);
    let resp = req.send().map_err(|e| {
        // reqwest's top-level Display hides the root cause ("error sending
        // request"); walk the source chain so operators see the real fault.
        let mut chain = String::new();
        let mut src: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(&e);
        while let Some(s) = src {
            chain.push_str(&format!(" <- {s}"));
            src = s.source();
        }
        FetchError::Transport(format!("download failed: {e}{chain}"))
    })?;
    let status = resp.status().as_u16();
    let final_url = resp.url().to_string();
    crate::check_final_url(&final_url, false)?;
    if let Some(value) = resp
        .headers()
        .get("strict-transport-security")
        .and_then(|value| value.to_str().ok())
    {
        crate::observe_hsts(&final_url, value);
    }
    if !(200..300).contains(&status) {
        return Err(FetchError::Http { status });
    }
    let etag = resp
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    crate::store_cookies_from_for_origin(&resp, &origin);

    // Record the resume anchor before the first byte lands: a cancelled run
    // must still be resumable, and a completed one simply leaves it in place.
    let _ = std::fs::write(&meta, &etag);

    // 206 = appending to our part; 200 = the server ignored the range (or
    // If-Range rejected our snapshot) — restart from zero.
    let appending = status == 206 && resume_from > 0;
    let base = if appending { resume_from } else { 0 };
    let total = resp
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .map(|cl| base + cl);

    if let Some(hook) = on_progress.as_deref_mut() {
        if !hook(base, total) {
            return Err(FetchError::Cancelled);
        }
    }

    std::fs::create_dir_all(dest.parent().unwrap_or_else(|| Path::new(".")))
        .map_err(|e| FetchError::FileIo(format!("{}: {e}", dest.display())))?;
    let mut file = if appending {
        std::fs::OpenOptions::new().append(true).open(&part)
    } else {
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&part)
    }
    .map_err(|e| FetchError::FileIo(format!("{}: {e}", part.display())))?;

    let mut reader = resp;
    let mut downloaded = base;
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| FetchError::Transport(format!("reading body: {e}")))?;
        if n == 0 {
            break;
        }
        std::io::Write::write_all(&mut file, &buf[..n])
            .map_err(|e| FetchError::FileIo(format!("{}: {e}", part.display())))?;
        downloaded += n as u64;
        if let Some(hook) = on_progress.as_deref_mut() {
            if !hook(downloaded, total) {
                // Cancelled: keep the part file — the next call resumes.
                let _ = std::io::Write::flush(&mut file);
                return Err(FetchError::Cancelled);
            }
        }
    }
    std::io::Write::flush(&mut file)
        .map_err(|e| FetchError::FileIo(format!("{}: {e}", part.display())))?;
    drop(file);
    std::fs::rename(&part, dest)
        .map_err(|e| FetchError::FileIo(format!("{}: {e}", dest.display())))?;
    Ok(std::fs::metadata(dest)
        .map(|m| m.len())
        .unwrap_or(downloaded))
}

/// `<name><suffix>` without assuming an extension (`movie.bin` + `.part` →
/// `movie.bin.part`, not `movie.part`).
fn sibling_path(dest: &Path, suffix: &str) -> PathBuf {
    let mut s = dest.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

/// Dedicated client for long-running transfers: the shared client's 30 s
/// total-timeout budget would abort any download bigger than a few MB/s.
fn download_client() -> &'static reqwest::blocking::Client {
    static C: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    C.get_or_init(|| {
        let cfg = crate::CONFIG.get().cloned().unwrap_or_default();
        reqwest::blocking::Client::builder()
            .user_agent(cfg.user_agent)
            .connect_timeout(cfg.connect_timeout)
            .build()
            .expect("failed to build download client")
    })
}

// ── H1: the download manager ────────────────────────────────────────────────
//
// The engine fetches whole bodies into memory anyway, so a "download" here is
// a background thread that spills an already-fetched body into the user's
// download directory while a registry tracks progress. `kitae_core` polls
// [`downloads_snapshot`] and forwards changes to the embedder as
// notifications; the app renders them in its Downloads UI.

/// Lifecycle of one managed download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadState {
    /// The writer thread is still spilling the body to disk.
    Active,
    /// The file is complete at `path`.
    Done,
    /// The write failed; the message is the I/O error text.
    Failed(String),
}

/// One entry in the global download registry.
#[derive(Debug, Clone)]
pub struct DownloadInfo {
    pub id: u32,
    pub url: String,
    /// Final destination (the `.part` file while state is `Active`).
    pub path: PathBuf,
    pub filename: String,
    pub mime: String,
    pub total: Option<u64>,
    pub received: u64,
    pub state: DownloadState,
}

static DOWNLOADS: OnceLock<Mutex<Vec<DownloadInfo>>> = OnceLock::new();
static NEXT_DOWNLOAD_ID: AtomicU32 = AtomicU32::new(1);
static DOWNLOAD_DIR: OnceLock<Mutex<PathBuf>> = OnceLock::new();

fn downloads_registry() -> &'static Mutex<Vec<DownloadInfo>> {
    DOWNLOADS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Set the directory downloads land in (the app points this at the user's
/// choice; the default is `~/Downloads`). Applies to downloads begun after
/// the call.
pub fn set_download_dir(dir: PathBuf) {
    let m = DOWNLOAD_DIR.get_or_init(|| Mutex::new(default_download_dir()));
    *m.lock().unwrap_or_else(|e| e.into_inner()) = dir;
}

/// The current download directory (`~/Downloads` until overridden).
pub fn download_dir() -> PathBuf {
    DOWNLOAD_DIR
        .get_or_init(|| Mutex::new(default_download_dir()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

fn default_download_dir() -> PathBuf {
    let mut p = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    p.push("Downloads");
    p
}

/// A consistent copy of the registry, oldest download first.
pub fn downloads_snapshot() -> Vec<DownloadInfo> {
    downloads_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Begin a managed download of an already-fetched body. Spawns the writer
/// thread and returns the download id immediately. `suggested_name` should
/// already be a bare filename (see [`suggested_filename`]); it is sanitized
/// and uniquified against the destination directory here.
pub fn begin_download(raw_url: &str, body: Vec<u8>, mime: &str, suggested_name: &str) -> u32 {
    let id = NEXT_DOWNLOAD_ID.fetch_add(1, Ordering::Relaxed);
    let dir = download_dir();
    let filename = {
        // Names taken by live/finished downloads count as occupied even when
        // their file hasn't hit disk yet — keeps rapid same-name downloads
        // deterministic.
        let reg = downloads_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let taken: Vec<String> = reg.iter().map(|d| d.filename.clone()).collect();
        unique_filename(&dir, &sanitize_filename(suggested_name), &taken)
    };
    let path = dir.join(&filename);
    let total = Some(body.len() as u64);
    downloads_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(DownloadInfo {
            id,
            url: raw_url.to_string(),
            path: path.clone(),
            filename: filename.clone(),
            mime: mime.to_string(),
            total,
            received: 0,
            state: DownloadState::Active,
        });

    // The body is in memory; the thread only writes it out in chunks so the
    // registry's progress is observable (and large bodies don't produce one
    // giant syscall).
    let spawned = std::thread::Builder::new()
        .name(format!("kitae-dl-{id}"))
        .spawn(move || {
            let part = sibling_path(&path, ".part");
            let outcome = spill_body(&part, &body, id, total);
            let state = match outcome {
                Ok(()) => {
                    if let Err(e) = std::fs::rename(&part, &path) {
                        DownloadState::Failed(format!("{}: {e}", path.display()))
                    } else {
                        DownloadState::Done
                    }
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&part);
                    DownloadState::Failed(e)
                }
            };
            let mut reg = downloads_registry()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = reg.iter_mut().find(|d| d.id == id) {
                entry.state = state;
                entry.received = entry.total.unwrap_or(entry.received);
            }
        });
    if spawned.is_err() {
        // No threads available: record the failure instead of dropping the
        // download silently.
        let mut reg = downloads_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = reg.iter_mut().find(|d| d.id == id) {
            entry.state = DownloadState::Failed("spawning the download thread failed".into());
        }
    }
    id
}

/// Write `body` through `part` in 64 KiB chunks, updating the registry after
/// each chunk.
fn spill_body(part: &Path, body: &[u8], id: u32, total: Option<u64>) -> Result<(), String> {
    use std::io::Write;
    if let Some(parent) = part.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let mut file = std::fs::File::create(part).map_err(|e| format!("{}: {e}", part.display()))?;
    let mut received = 0u64;
    for chunk in body.chunks(64 * 1024) {
        file.write_all(chunk)
            .map_err(|e| format!("{}: {e}", part.display()))?;
        received += chunk.len() as u64;
        let mut reg = downloads_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = reg.iter_mut().find(|d| d.id == id) {
            entry.received = received;
            entry.total = total;
        }
    }
    file.flush()
        .map_err(|e| format!("{}: {e}", part.display()))?;
    Ok(())
}

/// RFC 6266/5987: pull a filename out of `Content-Disposition`, preferring
/// the `filename*` form (percent-decoded) over the plain quoted form.
pub fn filename_from_disposition(value: &str) -> Option<String> {
    let mut plain: Option<String> = None;
    for part in value.split(';').skip(1) {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix("filename*=") {
            // charset'lang'percent-encoded — any charset other than UTF-8 is
            // treated as bytes and lossily decoded with the rest.
            if let Some((_charset, rest)) = rest.split_once('\'') {
                if let Some((_lang, encoded)) = rest.split_once('\'') {
                    if let Some(decoded) = percent_decode_utf8(encoded) {
                        if !decoded.is_empty() {
                            return Some(decoded);
                        }
                    }
                }
            }
        } else if let Some(rest) = part.strip_prefix("filename=") {
            let name = rest.trim().trim_matches('"').to_string();
            if !name.is_empty() {
                plain = Some(name);
            }
        }
    }
    plain.filter(|s| !s.trim().is_empty())
}

/// Percent-decode `s` into a UTF-8 string (invalid sequences are dropped).
fn percent_decode_utf8(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = &bytes[i + 1..i + 3];
            let hi = (hex[0] as char).to_digit(16);
            let lo = (hex[1] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).ok()
}

/// The download's display name: `Content-Disposition`'s filename, else the
/// URL's last path segment percent-decoded, else "download". The result is
/// sanitized but not uniquified.
pub fn suggested_filename(raw_url: &str, disposition: &str) -> String {
    let from_disposition = filename_from_disposition(disposition);
    let from_url = Url::parse(raw_url).ok().and_then(|u| {
        u.path_segments()
            .and_then(|mut segs| segs.next_back())
            .and_then(percent_decode_utf8)
    });
    let name = from_disposition
        .or(from_url)
        .unwrap_or_else(|| "download".to_string());
    sanitize_filename(&name)
}

/// Strip path separators and control characters; never return something
/// dot-prefixed-and-empty or a traversal (".." becomes "download").
pub fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| !c.is_control() && *c != '/' && *c != '\\' && *c != ':')
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').trim();
    if trimmed.is_empty() || trimmed == ".." {
        "download".to_string()
    } else {
        trimmed.to_string()
    }
}

/// `report.pdf` → `report (2).pdf` when `report.pdf` (or its part file)
/// already exists in `dir` or was claimed by a registry entry. Filesystem
/// probe, so it must run at begin time.
fn unique_filename(dir: &Path, name: &str, taken: &[String]) -> String {
    let mut candidate = name.to_string();
    let mut n = 2;
    loop {
        let dest = dir.join(&candidate);
        if !taken.iter().any(|t| t == &candidate)
            && !dest.exists()
            && !sibling_path(&dest, ".part").exists()
        {
            return candidate;
        }
        let stem = Path::new(name)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(name);
        let ext = Path::new(name).extension().and_then(|s| s.to_str());
        candidate = match ext {
            Some(ext) if !ext.is_empty() => format!("{stem} ({n}).{ext}"),
            _ => format!("{stem} ({n})"),
        };
        n += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::support::{http_ok, http_ok_bytes, start_server, start_server_raw, wait_until};

    /// The two `begin_download` tests below point the process-global
    /// download directory at their own temp dirs; cargo runs a binary's
    /// tests on parallel threads, so they serialize on this lock to keep
    /// each other's writes out of the wrong directory.
    static DOWNLOAD_DIR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The 30-byte fixture resource, served with ETag "fixed" and real
    /// Range support.
    fn range_server() -> (
        String,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        std::thread::JoinHandle<()>,
    ) {
        start_server_raw(1, move |req| {
            let whole: &[u8] = b"012345678901234567890123456789";
            let lower = req.to_ascii_lowercase();
            let common = "Content-Type: application/octet-stream\r\nETag: \"fixed\"\r\n";
            if let Some(pos) = lower.find("range: bytes=") {
                let rest = &lower[pos + "range: bytes=".len()..];
                let endp = rest.find('-').unwrap_or(rest.len());
                let start: usize = rest[..endp].trim().parse().unwrap_or(0);
                if start == 0 || start >= whole.len() {
                    return http_ok_bytes(whole, "");
                }
                let tail = &whole[start..];
                format!(
                    "HTTP/1.1 206 Partial Content\r\n{common}Content-Range: bytes {start}-{}/{len}\r\nContent-Length: {clen}\r\nConnection: close\r\n\r\n",
                    whole.len() - 1,
                    len = whole.len(),
                    clen = tail.len()
                )
                .into_bytes()
                .tap_tail(tail)
            } else {
                format!(
                    "HTTP/1.1 200 OK\r\n{common}Content-Length: {}\r\nConnection: close\r\n\r\n",
                    whole.len()
                )
                .into_bytes()
                .tap_tail(whole)
            }
        })
    }

    // Tiny helper so the fixture can append a body after a String head
    // without a second format pass.
    trait TapTail {
        fn tap_tail(self, tail: &[u8]) -> Vec<u8>;
    }
    impl TapTail for Vec<u8> {
        fn tap_tail(mut self, tail: &[u8]) -> Vec<u8> {
            self.extend_from_slice(tail);
            self
        }
    }

    #[test]
    fn range_requests_surface_206_and_ignored_ranges() {
        let (base, log, handle) = start_server(2, |req| {
            if req.to_ascii_lowercase().contains("range: bytes=5-9") {
                "HTTP/1.1 206 Partial Content\r\nContent-Type: text/plain\r\nContent-Range: bytes 5-9/20\r\nContent-Length: 5\r\nConnection: close\r\n\r\nworld".to_string()
            } else {
                http_ok("01234567890123456789", "")
            }
        });
        let r = fetch_range(&format!("{base}/blob"), 5, Some(9)).expect("ranged fetch");
        assert_eq!(r.status, 206);
        assert_eq!(r.body, b"world".to_vec());
        // A server that ignores the range answers 200 with the whole body —
        // surfaced as-is, per spec.
        let r = fetch_range(&format!("{base}/blob"), 5, None).expect("open-ended fetch");
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"01234567890123456789".to_vec());
        assert_eq!(log.lock().unwrap().len(), 2);
        handle.join().unwrap();
    }

    #[test]
    fn download_cancels_through_the_progress_hook() {
        let dir = std::env::temp_dir().join(format!("kitae_dl_cancel_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("file.bin");
        let (base, log, handle) = start_server(1, |_req| {
            http_ok(
                "012345678901234567890123456789",
                "Content-Type: application/octet-stream",
            )
        });
        let mut calls = 0u32;
        let result = download_to_file(
            &format!("{base}/file"),
            &dest,
            Some(&mut |_downloaded, total| {
                calls += 1;
                assert_eq!(total, Some(30));
                false // cancel on the very first report
            }),
        );
        match result {
            Err(FetchError::Cancelled) => {}
            other => panic!("expected Cancelled, got {other:?}"),
        }
        assert_eq!(calls, 1);
        assert!(!dest.exists(), "cancelled download must not finish");
        assert!(wait_until(
            || log.lock().unwrap().len() == 1,
            std::time::Duration::from_secs(2)
        ));
        handle.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_resumes_a_partial_part_file() {
        let dir = std::env::temp_dir().join(format!("kitae_dl_resume_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("file.bin");
        let part = dir.join("file.bin.part");
        let meta = dir.join("file.bin.meta");
        // Simulate a previous cancelled run: 10 bytes on disk, ETag recorded.
        std::fs::write(&part, b"0123456789").unwrap();
        std::fs::write(&meta, "\"fixed\"").unwrap();
        let (base, log, handle) = range_server();
        let mut progress: Vec<(u64, Option<u64>)> = Vec::new();
        let len = download_to_file(
            &format!("{base}/file"),
            &dest,
            Some(&mut |d, t| {
                progress.push((d, t));
                true
            }),
        )
        .expect("resumed download");
        assert_eq!(len, 30);
        assert_eq!(
            std::fs::read(&dest).unwrap(),
            b"012345678901234567890123456789".to_vec()
        );
        assert!(!part.exists(), "part file is renamed away on completion");
        let requests = log.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0].contains("range: bytes=10-"),
            "resume must range from the part length: {:?}",
            requests[0]
        );
        assert!(requests[0].contains("if-range: \"fixed\""));
        assert_eq!(
            progress[0],
            (10, Some(30)),
            "first report starts at the resume point"
        );
        handle.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_resume_falls_back_to_a_full_rewrite() {
        let dir = std::env::temp_dir().join(format!("kitae_dl_stale_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("file.bin");
        let part = dir.join("file.bin.part");
        // Part from an older generation of the resource; server ignores the
        // range (answers 200) so the download must start over and win.
        std::fs::write(&part, b"STALESTALE").unwrap();
        let (base, _log, handle) = start_server_raw(1, move |req| {
            let whole: &[u8] = b"012345678901234567890123456789";
            if req.to_ascii_lowercase().contains("range:") {
                format!("HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nETag: \"fixed\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", whole.len())
                    .into_bytes()
                    .tap_tail(whole)
            } else {
                http_ok_bytes(whole, "")
            }
        });
        let len = download_to_file(&format!("{base}/file"), &dest, None).expect("rewrite download");
        assert_eq!(len, 30);
        assert_eq!(
            std::fs::read(&dest).unwrap(),
            b"012345678901234567890123456789".to_vec()
        );
        handle.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_200_download_surfaces_the_status() {
        let (base, _log, handle) = start_server(1, |_req| {
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
        });
        let dest = std::env::temp_dir().join(format!("kitae_dl_404_{}", std::process::id()));
        let err = download_to_file(&format!("{base}/nope"), &dest, None).unwrap_err();
        assert_eq!(err, FetchError::Http { status: 404 });
        assert!(!dest.exists());
        handle.join().unwrap();
        let _ = std::fs::remove_file(&dest);
    }

    // ── H1: the download manager ───────────────────────────────────────────

    #[test]
    fn filename_from_disposition_parses_quoted_and_star_forms() {
        assert_eq!(
            filename_from_disposition("attachment; filename=\"real name.bin\""),
            Some("real name.bin".to_string())
        );
        assert_eq!(
            filename_from_disposition("attachment; filename=unquoted.txt"),
            Some("unquoted.txt".to_string())
        );
        assert_eq!(
            filename_from_disposition("attachment; filename*=UTF-8''na%C3%AFve%20file.txt"),
            Some("naïve file.txt".to_string())
        );
        assert_eq!(
            filename_from_disposition("inline"),
            None,
            "no filename parameter → None"
        );
        assert_eq!(
            filename_from_disposition("attachment; filename=\"  \""),
            None,
            "whitespace-only name is rejected"
        );
    }

    #[test]
    fn suggested_names_prefer_disposition_then_url() {
        assert_eq!(
            suggested_filename("https://example.com/dir/report%20final.pdf", ""),
            "report final.pdf"
        );
        assert_eq!(
            suggested_filename(
                "https://example.com/a/b.zip",
                "attachment; filename=\"real name.bin\""
            ),
            "real name.bin"
        );
        assert_eq!(suggested_filename("https://example.com/", ""), "download");
    }

    #[test]
    fn sanitize_filename_never_traverses_or_escapes() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "etcpasswd");
        assert_eq!(sanitize_filename("..\\..\\evil.txt"), "evil.txt");
        assert_eq!(sanitize_filename("a/b\\c:d.txt"), "abcd.txt");
        assert_eq!(sanitize_filename(""), "download");
        assert_eq!(sanitize_filename("   "), "download");
        assert_eq!(sanitize_filename("..."), "download");
        assert_eq!(sanitize_filename("installer.dmg"), "installer.dmg");
    }

    #[test]
    fn begin_download_writes_the_body_and_reports_progress() {
        let _dir_guard = DOWNLOAD_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("kitae_dl_mgr_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        set_download_dir(dir.clone());
        let body = vec![7u8; 200 * 1024]; // spans several 64 KiB chunks
        let id = begin_download(
            "https://example.com/files/mgr_blob.bin",
            body.clone(),
            "application/octet-stream",
            "mgr_blob.bin",
        );
        let done = |id: u32| {
            wait_until(
                || {
                    downloads_snapshot()
                        .iter()
                        .find(|d| d.id == id)
                        .map(|d| d.state == DownloadState::Done)
                        .unwrap_or(false)
                },
                std::time::Duration::from_secs(5),
            )
        };
        assert!(done(id), "download must reach Done");
        let info = downloads_snapshot()
            .into_iter()
            .find(|d| d.id == id)
            .unwrap();
        assert_eq!(info.filename, "mgr_blob.bin");
        assert_eq!(info.url, "https://example.com/files/mgr_blob.bin");
        assert_eq!(info.mime, "application/octet-stream");
        assert_eq!(info.received, 200 * 1024);
        assert_eq!(info.total, Some(200 * 1024));
        assert_eq!(info.path, dir.join("mgr_blob.bin"));
        assert_eq!(std::fs::read(&info.path).unwrap(), body);
        assert!(!sibling_path(&info.path, ".part").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn begin_download_uniquifies_duplicate_names() {
        let _dir_guard = DOWNLOAD_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("kitae_dl_dup_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        set_download_dir(dir.clone());
        let id1 = begin_download(
            "https://example.com/mgr_dup.bin",
            vec![1u8; 1024],
            "",
            "mgr_dup.bin",
        );
        let id2 = begin_download(
            "https://example.com/mgr_dup.bin",
            vec![2u8; 1024],
            "",
            "mgr_dup.bin",
        );
        let both_done = |ids: &[u32]| {
            wait_until(
                || {
                    let snap = downloads_snapshot();
                    ids.iter().all(|id| {
                        snap.iter()
                            .find(|d| d.id == *id)
                            .map(|d| d.state == DownloadState::Done)
                            .unwrap_or(false)
                    })
                },
                std::time::Duration::from_secs(5),
            )
        };
        assert!(both_done(&[id1, id2]));
        let snap = downloads_snapshot();
        let mut names: Vec<String> = snap
            .iter()
            .filter(|d| d.id == id1 || d.id == id2)
            .map(|d| d.filename.clone())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec!["mgr_dup (2).bin".to_string(), "mgr_dup.bin".to_string()]
        );
        for d in snap.iter().filter(|d| d.id == id1 || d.id == id2) {
            let expected = if d.filename == "mgr_dup.bin" {
                1u8
            } else {
                2u8
            };
            assert_eq!(std::fs::read(&d.path).unwrap(), vec![expected; 1024]);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
