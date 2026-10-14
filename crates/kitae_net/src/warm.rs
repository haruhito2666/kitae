//! G8: `rel=preconnect` / `rel=prefetch` warmup plus RFC 9218 priority
//! hints. Everything here is best-effort: warmups run on background
//! threads, never fail a load, and dedupe through an in-flight set.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// URLs with a queued or running prefetch — the dedup key.
fn in_flight() -> &'static Mutex<HashSet<String>> {
    static SET: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SET.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Warm the response cache for `url` off-thread (`rel=prefetch`). Returns
/// `false` when an identical prefetch is already queued or running.
pub fn prefetch(url: &str) -> bool {
    let url = url.to_string();
    if !in_flight().lock().unwrap().insert(url.clone()) {
        return false;
    }
    let worker_url = url.clone();
    let origin = crate::page_origin();
    let spawned = std::thread::Builder::new()
        .name("kitae-prefetch".into())
        .spawn(move || {
            let _ = crate::fetch_cached_for_origin(&worker_url, &origin);
            in_flight().lock().unwrap().remove(&worker_url);
        });
    if spawned.is_err() {
        in_flight().lock().unwrap().remove(&url);
        return false;
    }
    true
}

/// Prefetch a batch (`rel=prefetch` lists) on one worker thread, request
/// order preserved, duplicates and already-queued URLs dropped up front.
pub fn prefetch_many(urls: &[String]) {
    let mut batch: Vec<String> = Vec::new();
    {
        let mut set = in_flight().lock().unwrap();
        for url in urls {
            if set.insert(url.clone()) {
                batch.push(url.clone());
            }
        }
    }
    if batch.is_empty() {
        return;
    }
    let origin = crate::page_origin();
    let _ = std::thread::Builder::new()
        .name("kitae-prefetch".into())
        .spawn(move || {
            for url in batch {
                let _ = crate::fetch_cached_for_origin(&url, &origin);
                in_flight().lock().unwrap().remove(&url);
            }
        });
}

/// Open and discard one connection to `url` off-thread (`rel=preconnect`):
/// DNS, TCP and the TLS handshake are warmed. Honest limit: reqwest reuses
/// only connections it pooled itself, so this pays off when the same client
/// fetches the origin soon after — and never costs more than one HEAD.
pub fn preconnect(url: &str) {
    let url = url.to_string();
    let _ = std::thread::Builder::new()
        .name("kitae-preconnect".into())
        .spawn(move || {
            let request_url = crate::hsts_upgrade_url(&url).unwrap_or_else(|_| url.clone());
            let mut request = crate::client()
                .head(request_url)
                .timeout(Duration::from_secs(2));
            if crate::dnt_enabled() {
                request = request.header("DNT", "1");
            }
            let _ = request.send();
        });
}

/// RFC 9218 (§5) priority tiers: `urgency` 0 runs first, 7 last, and 3 is
/// the protocol default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestPriority {
    Urgent,
    High,
    Normal,
    Low,
}

impl RequestPriority {
    fn urgency(self) -> u8 {
        match self {
            RequestPriority::Urgent => 0,
            RequestPriority::High => 2,
            RequestPriority::Normal => 3,
            RequestPriority::Low => 5,
        }
    }
}

/// Append the RFC 9218 `Priority: urgency=N` header to a header list built
/// for `fetch_with` / `fetch_for_origin`.
pub fn apply_priority(headers: &mut Vec<(String, String)>, priority: RequestPriority) {
    headers.push((
        "Priority".to_string(),
        format!("urgency={}", priority.urgency()),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::support::{cache_test_lock, http_ok, start_server, wait_until};

    #[test]
    fn prefetch_warms_the_cache_and_dedups() {
        let _lock = cache_test_lock();
        crate::clear_cache();
        let before = crate::cached_len();
        let (base, log, handle) = start_server(1, |_| {
            std::thread::sleep(Duration::from_millis(150));
            http_ok("<p>warm</p>", "")
        });
        let url = format!("{base}/warm");
        assert!(prefetch(&url));
        assert!(!prefetch(&url), "an in-flight prefetch must dedup");
        assert!(
            wait_until(|| crate::cached_len() > before, Duration::from_secs(5)),
            "the prefetch must land in the response cache"
        );
        handle.join().expect("server");
        assert_eq!(log.lock().unwrap().len(), 1);
        // Warm cache: this must not reach the network again.
        let resp = crate::fetch_cached(&url).expect("cached response");
        assert!(resp.text().contains("warm"));
        assert_eq!(
            log.lock().unwrap().len(),
            1,
            "second read must be served from cache"
        );
        crate::clear_cache();
    }

    #[test]
    fn prefetch_many_preserves_order_and_dedups_upfront() {
        let _lock = cache_test_lock();
        crate::clear_cache();
        let before = crate::cached_len();
        let (base, log, handle) = start_server(3, |_| {
            std::thread::sleep(Duration::from_millis(120));
            http_ok("<p>batch</p>", "")
        });
        let urls: Vec<String> = (1..=3).map(|i| format!("{base}/b{i}")).collect();
        prefetch_many(&urls);
        // Resubmitted while the first batch is still running: full dedup.
        prefetch_many(&urls);
        assert!(
            wait_until(|| crate::cached_len() - before >= 3, Duration::from_secs(5)),
            "all three prefetches must land"
        );
        handle.join().expect("server");
        let requests = log.lock().unwrap();
        assert_eq!(
            requests.len(),
            3,
            "resubmission must queue nothing: {requests:?}"
        );
        let pos = |p: &str| {
            requests
                .iter()
                .position(|r| r.lines().next().unwrap_or("").contains(p))
                .expect("request present")
        };
        assert!(
            pos("/b1") < pos("/b2") && pos("/b2") < pos("/b3"),
            "the single worker preserves request order"
        );
        crate::clear_cache();
    }

    #[test]
    fn preconnect_issues_a_head_request() {
        let (base, log, handle) = start_server(1, |_| http_ok("", ""));
        preconnect(&format!("{base}/pre"));
        assert!(
            wait_until(|| log.lock().unwrap().len() == 1, Duration::from_secs(5)),
            "preconnect must warm the origin"
        );
        handle.join().expect("server");
        let requests = log.lock().unwrap();
        assert!(
            requests[0].starts_with("HEAD "),
            "preconnect sends HEAD: {:?}",
            requests[0]
        );
    }

    #[test]
    fn apply_priority_emits_rfc_9218_urgency() {
        let mut headers: Vec<(String, String)> = Vec::new();
        apply_priority(&mut headers, RequestPriority::Urgent);
        apply_priority(&mut headers, RequestPriority::Low);
        assert_eq!(
            headers,
            vec![
                ("Priority".to_string(), "urgency=0".to_string()),
                ("Priority".to_string(), "urgency=5".to_string()),
            ]
        );
    }
}
