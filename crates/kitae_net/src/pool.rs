//! Phase 0: the shared background fetch pool.
//!
//! Top-level document fetches used to run synchronously on each tab's
//! engine thread: a slow site pinned that thread, and future surfaces
//! (feed refresh, mail polling, preloading) had no home at all. The pool
//! is the roadmap's fix: any number of producers submit jobs, a fixed set
//! of worker threads performs the exact fetch calls the engine thread
//! would have made — same response cache, same cookie jar, same origin
//! gate — and completions accumulate in a claim-by-id results store each
//! consumer drains on its own schedule (the engine drains per idle tick;
//! bulk submitters drain as their requests settle).
//!
//! Threading rules the pool exists to keep:
//! - kitae_net's `CURRENT_PAGE_ORIGIN` is deliberately a thread-local, so
//!   pool workers never read it. The job carries the page origin and the
//!   worker uses the explicit-origin entry points, keeping the S-1
//!   local-origin gate byte-identical to an engine-thread fetch.
//! - Results are claimed by job id, so several engines — and future
//!   non-engine consumers — share one pool without stealing each other's
//!   work.
//! - Unclaimed results are capped, oldest dropped first: an abandoned
//!   navigation (the user clicked elsewhere) stops occupying memory as
//!   newer completions arrive. Bulk submitters must claim promptly or
//!   accept the drop.
//!
//! There is no completion-notification channel: consumers poll at their
//! own cadence (the engine's idle tick runs every 2 ms — finer than a
//! frame), which keeps the pool free of per-consumer wiring.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};

/// Fixed worker count. Small on purpose: workers serve every tab in the
/// process, but HTTP waits — not CPU — are the bottleneck, and a modest
/// cap keeps a burst of tab loads polite to origins.
const POOL_WORKERS: usize = 4;

/// Unclaimed completions held before the oldest is dropped. Bulk work
/// beyond this must poll and claim as results land, not batch-claim at
/// the end.
const OUTCOME_CAP: usize = 64;

/// What a job asks the worker to run. The two variants mirror the two
/// fetch entry points a top-level navigation can take, so a pooled fetch
/// behaves exactly like the synchronous call it replaced:
/// [`FetchJobKind::CachedGet`] is the `fetch_cached` document path
/// (response cache, conditional revalidation, top-level privacy checks),
/// [`FetchJobKind::Raw`] is the `fetch_with` path (explicit method and
/// headers, never cached).
#[derive(Debug, Clone)]
pub enum FetchJobKind {
    /// Document GET through the response cache, exactly as `fetch_cached`
    /// would run it for this URL and page origin.
    CachedGet,
    /// Method request (`POST` form submits and friends) with extra
    /// headers and an optional body — the `fetch_with` path; responses
    /// never enter the cache.
    Raw {
        method: String,
        headers: Vec<(String, String)>,
        body: Option<Vec<u8>>,
    },
}

/// One finished job, held until its consumer claims it by id.
#[derive(Debug)]
pub struct FetchOutcome {
    /// The id `submit` returned for the job this completes.
    pub id: u64,
    /// The submitted URL (echoed so a consumer can match by id *and*
    /// target when bookkeeping several jobs).
    pub url: String,
    pub result: Result<crate::Response, crate::FetchError>,
}

struct Job {
    id: u64,
    url: String,
    /// Page origin the gate judges this request against (S-1). Empty
    /// means "no page context", which the gate treats as trusted-local.
    origin: String,
    kind: FetchJobKind,
}

struct Pool {
    /// Waiting jobs. `Condvar` so idle workers sleep until one arrives.
    queue: Mutex<VecDeque<Job>>,
    job_ready: Condvar,
    /// Done jobs not yet claimed, in completion order (the eviction key).
    results: Mutex<Results>,
    next_id: AtomicU64,
}

#[derive(Default)]
struct Results {
    by_id: HashMap<u64, FetchOutcome>,
    order: VecDeque<u64>,
}

fn pool() -> &'static Pool {
    static POOL: OnceLock<Pool> = OnceLock::new();
    POOL.get_or_init(|| {
        let p = Pool {
            queue: Mutex::new(VecDeque::new()),
            job_ready: Condvar::new(),
            results: Mutex::new(Results::default()),
            next_id: AtomicU64::new(1),
        };
        for n in 0..POOL_WORKERS {
            std::thread::Builder::new()
                .name(format!("kitae-fetch-{n}"))
                .spawn(worker_loop)
                .expect("kitae_net: could not spawn a fetch-pool worker");
        }
        p
    })
}

fn worker_loop() {
    loop {
        let job = {
            let mut q = pool().queue.lock().unwrap();
            loop {
                if let Some(job) = q.pop_front() {
                    break job;
                }
                q = pool().job_ready.wait(q).unwrap();
            }
        };
        let outcome = execute(&job);
        let mut r = pool().results.lock().unwrap();
        // Cap unclaimed completions: drop the oldest, never the one just
        // produced — the newest answer is the one a consumer most likely
        // still wants (an abandoned load's stale completion is exactly
        // what eviction is for).
        while r.by_id.len() >= OUTCOME_CAP {
            if let Some(oldest) = r.order.pop_front() {
                r.by_id.remove(&oldest);
            }
        }
        r.order.push_back(job.id);
        r.by_id.insert(job.id, outcome);
    }
}

/// The worker-side fetch. Same entry points, same arguments, same gate as
/// the synchronous engine-thread call — only the thread differs.
fn execute(job: &Job) -> FetchOutcome {
    let result = match &job.kind {
        FetchJobKind::CachedGet => crate::fetch_cached_kind(
            &job.url,
            &job.origin,
            crate::top_level_request(&job.url, &job.origin),
        ),
        FetchJobKind::Raw {
            method,
            headers,
            body,
        } => crate::fetch_for_origin_kind(
            method,
            &job.url,
            headers,
            body.as_deref(),
            &job.origin,
            true,
        ),
    };
    FetchOutcome {
        id: job.id,
        url: job.url.clone(),
        result,
    }
}

/// Queue a fetch on the shared pool. Returns the job id to poll for
/// (claim with [`take_results`]). The queue itself is unbounded —
/// producers rate their own work; the cap guarantees only that
/// *unclaimed* answers eventually go away.
pub fn submit(url: &str, origin: &str, kind: FetchJobKind) -> u64 {
    let p = pool();
    let id = p.next_id.fetch_add(1, Ordering::Relaxed);
    p.queue.lock().unwrap().push_back(Job {
        id,
        url: url.to_string(),
        origin: origin.trim().to_string(),
        kind,
    });
    p.job_ready.notify_one();
    id
}

/// Claim the listed jobs' completions, in the order the ids were given.
/// Ids with no result yet (or already claimed, or evicted) are simply
/// absent — polling repeatedly is the intended pattern.
pub fn take_results(ids: &[u64]) -> Vec<FetchOutcome> {
    let mut r = pool().results.lock().unwrap();
    let mut out = Vec::new();
    for id in ids {
        if let Some(o) = r.by_id.remove(id) {
            if let Some(pos) = r.order.iter().position(|&k| k == *id) {
                r.order.remove(pos);
            }
            out.push(o);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::support::{cache_test_lock, http_ok, start_server, wait_until};
    use std::time::Duration;

    fn claim(ids: &[u64], timeout: Duration) -> Vec<FetchOutcome> {
        let mut out = Vec::new();
        wait_until(
            || {
                out = take_results(ids);
                !out.is_empty()
            },
            timeout,
        );
        out
    }

    #[test]
    fn submit_a_get_and_claim_its_result() {
        let _lock = cache_test_lock();
        crate::clear_cache();
        let (base, log, handle) = start_server(1, |_| http_ok("<p>pool body</p>", ""));
        let url = format!("{base}/doc");
        let id = submit(&url, "", FetchJobKind::CachedGet);
        let out = claim(&[id], Duration::from_secs(5));
        assert_eq!(out.len(), 1, "the completion must arrive");
        let outcome = out.into_iter().next().unwrap();
        assert_eq!(outcome.id, id);
        assert_eq!(outcome.url, url);
        let resp = outcome.result.expect("pooled GET succeeds");
        assert_eq!(resp.status, 200);
        let body = String::from_utf8_lossy(&resp.body).into_owned();
        assert!(body.contains("pool body"), "body: {body:?}");
        handle.join().expect("server");
        assert_eq!(log.lock().unwrap().len(), 1);
        // Claimed results cannot be claimed twice.
        assert!(take_results(&[id]).is_empty());
        // The pooled document GET also warms the response cache, exactly
        // like a synchronous fetch_cached would.
        let cached = crate::fetch_cached(&url).expect("cache hit");
        assert!(String::from_utf8_lossy(&cached.body).contains("pool body"));
        assert_eq!(log.lock().unwrap().len(), 1, "cache must absorb the reread");
        crate::clear_cache();
    }

    #[test]
    fn the_origin_gate_runs_on_the_worker() {
        let with_origin = submit(
            "file:///definitely/not/real.html",
            "https://network.test",
            FetchJobKind::CachedGet,
        );
        let out = claim(&[with_origin], Duration::from_secs(5));
        match &out[0].result {
            Err(crate::FetchError::LocalOrigin(denied)) => {
                assert_eq!(denied.page_url, "https://network.test");
            }
            other => panic!("network-page file:// fetch must be refused: {other:?}"),
        }
        // Empty origin = no page context = trusted-local (the same rule
        // the engine thread applies).
        let path =
            std::env::temp_dir().join(format!("kitae_pool_local_{}.html", std::process::id()));
        std::fs::write(&path, "<p>local pool page</p>").expect("write fixture");
        let no_ctx = submit(
            &format!("file://{}", path.display()),
            "",
            FetchJobKind::CachedGet,
        );
        let out = claim(&[no_ctx], Duration::from_secs(5));
        let resp = out
            .into_iter()
            .next()
            .unwrap()
            .result
            .expect("local fetch with no page context");
        assert!(
            String::from_utf8_lossy(&resp.body).contains("local pool page"),
            "body: {:?}",
            String::from_utf8_lossy(&resp.body)
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn raw_requests_post_and_never_cache() {
        let _lock = cache_test_lock();
        crate::clear_cache();
        let (base, log, handle) = start_server(1, |req| {
            http_ok(&format!("<p>echo: {}</p>", req.contains("q=pool+post")), "")
        });
        let id = submit(
            &format!("{base}/submit"),
            "",
            FetchJobKind::Raw {
                method: "POST".into(),
                headers: vec![(
                    "Content-Type".into(),
                    "application/x-www-form-urlencoded".into(),
                )],
                body: Some(b"q=pool+post".to_vec()),
            },
        );
        let out = claim(&[id], Duration::from_secs(5));
        let resp = out
            .into_iter()
            .next()
            .unwrap()
            .result
            .expect("POST succeeds");
        assert!(
            String::from_utf8_lossy(&resp.body).contains("echo: true"),
            "body: {:?}",
            String::from_utf8_lossy(&resp.body)
        );
        let requests = log.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0].starts_with("POST "),
            "raw job sends its method: {:?}",
            requests[0]
        );
        assert!(requests[0].contains("q=pool+post"), "body is sent");
        assert_eq!(
            crate::cached_len(),
            0,
            "raw (POST) responses never enter the cache"
        );
        handle.join().expect("server");
    }

    #[test]
    fn claims_only_touch_their_own_ids() {
        let _lock = cache_test_lock();
        crate::clear_cache();
        let (base, _log, handle) = start_server(1, |_| http_ok("<p>solo</p>", ""));
        // Job ids start at 1, so 0 was never issued: claiming it must be a
        // no-op against the same results store.
        assert!(take_results(&[0]).is_empty(), "unknown ids claim nothing");
        let id = submit(&format!("{base}/solo"), "", FetchJobKind::CachedGet);
        let out = claim(&[id], Duration::from_secs(5));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, id);
        // One claim removes the result; a second attempt (mixed with an
        // unknown id) claims nothing.
        assert!(take_results(&[id, 0]).is_empty());
        handle.join().expect("server");
        crate::clear_cache();
    }

    #[test]
    fn unclaimed_results_are_capped_oldest_first() {
        let _lock = cache_test_lock();
        crate::clear_cache();
        let (base, log, handle) = start_server(OUTCOME_CAP + 4, |_| http_ok("<p>bulk</p>", ""));
        let ids: Vec<u64> = (0..OUTCOME_CAP + 4)
            .map(|i| submit(&format!("{base}/bulk{i}"), "", FetchJobKind::CachedGet))
            .collect();
        // The loopback server serves one request at a time, so outcomes
        // land in submission order: after every request was handled, the
        // unclaimed store holds exactly the newest OUTCOME_CAP of them,
        // and the four oldest completions were evicted.
        assert!(
            wait_until(
                || log.lock().unwrap().len() >= OUTCOME_CAP + 4,
                Duration::from_secs(10)
            ),
            "every submitted job must be executed"
        );
        let claimed = take_results(&ids);
        assert_eq!(
            claimed.len(),
            OUTCOME_CAP,
            "only the newest completions survive eviction"
        );
        let have = |id: u64| claimed.iter().any(|o| o.id == id);
        assert!(!have(ids[0]) && !have(ids[3]), "the oldest four are gone");
        assert!(have(ids[OUTCOME_CAP + 3]), "the newest survives");
        handle.join().expect("server");
        crate::clear_cache();
    }
}
