//! Server-side data fetching — `use_resource`.
//!
//! A resource runs a fetch closure on a background OS thread (the server
//! must never block its render thread) and lands the result in a signal.
//! Values travel back through a channel as `Box<dyn Any>` because kitae_ui
//! signals are thread-local: the RECEIVING side is the drain, run by the
//! runtime's poll loop ([`drain`]) on the same thread that created the
//! resource, and it is the drain that touches the signal.
//!
//! When a drain completes a task, the resource's signal updates and the
//! runtime re-renders affected trees (kitae_live's `World::step` drains
//! every iteration and broadcasts patches when anything landed).
//!
//! A resource is slot-stable inside a component: the initial fetch runs
//! exactly once, no matter how many times the component re-renders, and
//! [`Resource::refetch`] re-runs the closure on demand ([`A5`]).
//!
//! Every fetch carries a generation number ([`Flight`]): a landing whose
//! generation is stale (a deps change kicked a newer fetch while it was in
//! the air) is discarded whole instead of overwriting fresh data, and
//! kicks that arrive while a fetch is already in flight COALESCE into one
//! follow-up — a debounce-less input cannot stack threads or reorder
//! writes.

use crate::signals::{use_signal, Signal};
use crate::vnode::View;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
#[cfg(test)]
use std::sync::atomic::Ordering;

/// Shared flight state for one resource: the generation counter that
/// orders landings, the in-flight latch that coalesces kicks, and the
/// current fetch runner.
struct Flight {
    /// Bumped whenever a fetch is kicked. A landing captured a generation;
    /// if it no longer matches, the fetch was superseded (a deps change
    /// kicked again mid-flight) and its result must be discarded.
    version: Cell<u64>,
    /// True from the moment a kick spawns a thread until its landing is
    /// processed — set synchronously on the UI thread, so kicks that
    /// arrive in the same burst reliably coalesce.
    inflight: Cell<bool>,
    /// One deferred re-fetch: kicks that arrived mid-flight collapse into
    /// this flag and fire exactly once when the in-air fetch lands.
    pending_rerun: Cell<bool>,
    /// The current fetch runner. Swappable so a deps-driven resource always
    /// re-runs the LATEST build's closure (a refetch after a deps change
    /// must fetch the new shape of the data, not the first build's).
    /// Holding it here makes an Rc cycle (runner → flight → runner);
    /// deliberate — a resource lives as long as its owning scope, and the
    /// cycle dies with it.
    runner: RefCell<Option<Rc<dyn Fn()>>>,
}

impl Flight {
    /// Request a fetch. While one is in the air the request coalesces into
    /// a single follow-up; `invalidate` additionally marks the in-air
    /// fetch stale so its landing is discarded — a deps change must not
    /// let old-shaped data overwrite the follow-up's fresh fetch.
    fn kick(&self, invalidate: bool) {
        if self.inflight.get() {
            if invalidate {
                self.version.set(self.version.get() + 1);
            }
            self.pending_rerun.set(true);
            return;
        }
        self.spawn();
    }

    fn spawn(&self) {
        let run = self.runner.borrow().clone();
        if let Some(run) = run {
            run();
        }
    }

    /// A fetch captured at generation `captured` finished. Clears the
    /// in-flight latch and fires the coalesced follow-up, if any — so the
    /// caller must have applied (or discarded) its value FIRST: a
    /// follow-up's `loading.set_internal(true)` must land on top of this
    /// landing's state, not underneath it.
    fn landed(&self, captured: u64) {
        let _current = captured == self.version.get();
        self.inflight.set(false);
        if self.pending_rerun.replace(false) {
            self.spawn();
        }
    }
}

/// One in-flight/finished fetch. Clone-cheap (signal handles + the flight).
pub struct Resource<T: 'static> {
    /// The fetched value once it arrived. While a refetch is running the
    /// previous value stays visible; it swaps when the fresh one lands.
    pub data: Signal<Option<T>>,
    /// True while a fetch (the initial one or a refetch) is in flight.
    pub loading: Signal<bool>,
    /// The failure message when the fetch panicked or reported an error.
    pub error: Signal<Option<String>>,
    /// Versioning + coalescing shared by every clone of this resource.
    flight: Rc<Flight>,
}

impl<T: 'static> Clone for Resource<T> {
    fn clone(&self) -> Self {
        Resource {
            data: self.data,
            loading: self.loading,
            error: self.error,
            flight: self.flight.clone(),
        }
    }
}

impl<T: Clone> Resource<T> {
    /// The fetched value once it arrived.
    pub fn get(&self) -> Option<T> {
        self.data.get()
    }

    /// Whether a fetch is currently in flight.
    pub fn is_loading(&self) -> bool {
        self.loading.get()
    }

    /// The failure message, if the last fetch failed. A successful fetch
    /// (or the start of a new one) clears it.
    pub fn error(&self) -> Option<String> {
        self.error.get()
    }

    /// Re-run the fetch closure: loading flips on until the fresh value
    /// lands (the old value stays rendered meanwhile). Kicks that arrive
    /// while a fetch is already in the air are COALESCED — they remember
    /// one follow-up fetch instead of spawning a thread each, so a
    /// debounce-less input can't stack up threads or out-of-order writes.
    pub fn refetch(&self) {
        self.flight.kick(false);
    }
}

thread_local! {
    static TASKS: RefCell<TaskBook> = RefCell::new(TaskBook::default());
    /// Identity-keyed result cache for [`use_cached_resource`]: one entry
    /// per key. Entries are dropped with [`clear_cached`] /
    /// [`clear_cached_all`] — the next call with that key fetches fresh.
    static CACHE: RefCell<HashMap<String, Box<dyn std::any::Any + Send>>> =
        RefCell::new(HashMap::new());
}

/// Drop one cached entry: the next [`use_cached_resource`] with this key
/// fetches fresh instead of seeding from the cache.
pub fn clear_cached(key: &str) {
    CACHE.with(|c| c.borrow_mut().remove(key));
}

/// Drop every cached entry.
pub fn clear_cached_all() {
    CACHE.with(|c| c.borrow_mut().clear());
}

type Value = Box<dyn std::any::Any + Send>;

/// One registered fetch: its (still-open) value receiver, the applier that
/// moves a landed value into the resource's signal, and the failure
/// callback — all created HERE, on the UI thread, never crossing threads.
/// `gen` is the generation this fetch was kicked at; the applier discards
/// the landing when a newer kick has superseded it.
struct Task {
    rx: Rc<std::sync::mpsc::Receiver<Value>>,
    apply: Rc<dyn Fn(Value, u64)>,
    fail: Rc<dyn Fn(u64)>,
    gen: u64,
}

#[derive(Default)]
struct TaskBook {
    pending: Vec<Task>,
}

/// Build the re-runnable fetch runner behind every resource flavor. Each
/// invocation spawns a fresh thread; the fetch is wrapped in
/// `catch_unwind`, so a panicking fetch becomes `Err(message)` instead of
/// a disconnected channel — the resource gets a real [`Resource::error`].
/// Internal state flips use `set_internal` (quiet — the initial fetch
/// fires mid-build, and the A9 render-purity check would see it).
///
/// The returned runner is registered on `flight` by the caller (the
/// Flight holds the latest runner; see [`Flight::runner`]).
fn spawn_runner<T, F>(
    fetch: F,
    data: Signal<Option<T>>,
    loading: Signal<bool>,
    error: Signal<Option<String>>,
    cache_key: Option<String>,
    flight: Rc<Flight>,
) -> Rc<dyn Fn()>
where
    T: Clone + PartialEq + Send + 'static,
    F: Fn() -> T + Clone + Send + 'static,
{
    Rc::new(move || {
        let fetch = fetch.clone();
        let gen = flight.version.get() + 1;
        flight.version.set(gen);
        flight.inflight.set(true);
        let (tx, rx) = std::sync::mpsc::channel::<Value>();
        loading.set_internal(true);
        error.set_internal(None);
        std::thread::spawn(move || {
            let outcome: Result<T, String> =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(fetch))
                    .map_err(|e| panic_message(e.as_ref()));
            let _ = tx.send(Box::new(outcome));
        });
        let apply_flight = flight.clone();
        let cache_key = cache_key.clone();
        let apply: Rc<dyn Fn(Value, u64)> = Rc::new(move |v: Value, gen: u64| {
            // Stale landing (a newer kick superseded this fetch): discard
            // whole — no state flip, no cache write.
            if gen == apply_flight.version.get() {
                // Loading ends when a value LANDS, typed or not — a type
                // mismatch must not leave the resource stuck "loading".
                loading.set(false);
                if let Ok(landing) = v.downcast::<Result<T, String>>() {
                    match *landing {
                        Ok(value) => {
                            error.set(None);
                            if let Some(key) = &cache_key {
                                CACHE.with(|c| {
                                    c.borrow_mut().insert(key.clone(), Box::new(value.clone()));
                                });
                            }
                            data.set(Some(value));
                        }
                        Err(msg) => error.set(Some(msg)),
                    }
                }
            }
            // Clear the latch / fire the coalesced follow-up AFTER the
            // value was applied, so a follow-up's loading flag lands on top.
            apply_flight.landed(gen);
        });
        let fail_flight = flight.clone();
        let fail: Rc<dyn Fn(u64)> = Rc::new(move |gen: u64| {
            if gen == fail_flight.version.get() {
                loading.set(false);
            }
            fail_flight.landed(gen);
        });
        TASKS.with(|t| {
            t.borrow_mut().pending.push(Task {
                rx: Rc::new(rx),
                apply,
                fail,
                gen,
            })
        });
    })
}

fn panic_message(e: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = e.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = e.downcast_ref::<String>() {
        s.clone()
    } else {
        "fetch panicked".to_string()
    }
}

/// Run a fetch on a background thread; the result lands in the resource's
/// signal when the runtime next drains. `fetch` runs OFF the UI thread —
/// keep it Send and free of signals. The initial fetch runs exactly once
/// (slot-stable inside a component); [`Resource::refetch`] re-runs on demand.
pub fn use_resource<T, F>(fetch: F) -> Resource<T>
where
    T: Clone + PartialEq + Send + 'static,
    F: Fn() -> T + Clone + Send + 'static,
{
    let data: Signal<Option<T>> = use_signal(|| None);
    let loading: Signal<bool> = use_signal(|| false);
    let error: Signal<Option<String>> = use_signal(|| None);
    let build = move || {
        let flight = Rc::new(Flight {
            version: Cell::new(0),
            inflight: Cell::new(false),
            pending_rerun: Cell::new(false),
            runner: RefCell::new(None),
        });
        let run = spawn_runner(fetch, data, loading, error, None, flight.clone());
        *flight.runner.borrow_mut() = Some(run);
        let r = Resource {
            data,
            loading,
            error,
            flight,
        };
        // Slot-stable inside a component: one resource per hook position,
        // the initial fetch kicked exactly once — re-renders take the slot
        // hit and must not re-fetch.
        r.flight.kick(false);
        r
    };
    if crate::hooks::in_scope() {
        return crate::hooks::with_slot(build);
    }
    build()
}

/// [`use_resource`] that re-fetches whenever `deps` changes. The first
/// build fetches; a re-render with the SAME deps only returns the existing
/// resource (no thread, no landing task); a re-render with CHANGED deps
/// kicks a fresh fetch on the same slot-stable resource — the old value
/// stays visible until the fresh one lands, and a stale landing from the
/// old shape is discarded instead of overwriting the fresh fetch. Pass a
/// version counter or a hash of everything the fetch depends on.
pub fn use_resource_with_deps<T, F>(deps: u64, fetch: F) -> Resource<T>
where
    T: Clone + PartialEq + Send + 'static,
    F: Fn() -> T + Clone + Send + 'static,
{
    let data: Signal<Option<T>> = use_signal(|| None);
    let loading: Signal<bool> = use_signal(|| false);
    let error: Signal<Option<String>> = use_signal(|| None);
    if !crate::hooks::in_scope() {
        let flight = Rc::new(Flight {
            version: Cell::new(0),
            inflight: Cell::new(false),
            pending_rerun: Cell::new(false),
            runner: RefCell::new(None),
        });
        let run = spawn_runner(fetch, data, loading, error, None, flight.clone());
        *flight.runner.borrow_mut() = Some(run);
        let r = Resource {
            data,
            loading,
            error,
            flight,
        };
        r.flight.kick(true);
        return r;
    }
    let slot = crate::hooks::with_slot(move || DepsSlot {
        last: Rc::new(std::cell::Cell::new(None)),
        res: Resource {
            data,
            loading,
            error,
            // The flight is created ONCE and shared across every build; the
            // runner below is swapped in per build so a kick always runs the
            // latest closure.
            flight: Rc::new(Flight {
                version: Cell::new(0),
                inflight: Cell::new(false),
                pending_rerun: Cell::new(false),
                runner: RefCell::new(None),
            }),
        },
    });
    // OUTSIDE the hooks borrow: swapping the runner and kicking the fetch
    // may allocate hook slots elsewhere and must not re-enter the slot
    // machinery mid-lookup.
    let flight = slot.res.flight.clone();
    let run = spawn_runner(fetch, data, loading, error, None, flight.clone());
    *flight.runner.borrow_mut() = Some(run);
    if slot.last.get() != Some(deps) {
        slot.last.set(Some(deps));
        // Invalidate: while the old-shape fetch is in the air, its landing
        // must be discarded, and the coalesced follow-up re-runs THIS
        // build's runner (the new shape).
        flight.kick(true);
    }
    slot.res.clone()
}

#[derive(Clone)]
struct DepsSlot<T: 'static> {
    last: Rc<std::cell::Cell<Option<u64>>>,
    res: Resource<T>,
}

/// [`use_resource`] backed by an identity cache: the FIRST call with a
/// given `key` fetches on a thread; every later call with the same key —
/// this render, a re-render, a different component, a different session of
/// the same process — returns the cached value immediately, no thread, no
/// loading window. [`clear_cached`] / [`clear_cached_all`] drop entries
/// (the next call with that key fetches fresh);
/// [`Resource::refetch`] refreshes the cached entry when the data truly
/// changed.
pub fn use_cached_resource<T, F>(key: impl Into<String>, fetch: F) -> Resource<T>
where
    T: Clone + PartialEq + Send + 'static,
    F: Fn() -> T + Clone + Send + 'static,
{
    let key = key.into();
    // Probe the cache before anything else: a hit seeds the resource
    // directly and skips the thread entirely.
    let hit: Option<T> = CACHE.with(|c| {
        c.borrow()
            .get(&key)
            .and_then(|v| v.downcast_ref::<T>().cloned())
    });
    let data: Signal<Option<T>> = use_signal(|| None);
    let loading: Signal<bool> = use_signal(|| false);
    let error: Signal<Option<String>> = use_signal(|| None);
    let probe_key = key.clone();
    let build = move || {
        let flight = Rc::new(Flight {
            version: Cell::new(0),
            inflight: Cell::new(false),
            pending_rerun: Cell::new(false),
            runner: RefCell::new(None),
        });
        let run = spawn_runner(
            fetch,
            data,
            loading,
            error,
            Some(key.clone()),
            flight.clone(),
        );
        *flight.runner.borrow_mut() = Some(run);
        let r = Resource {
            data,
            loading,
            error,
            flight,
        };
        if let Some(v) = &hit {
            r.data.set_internal(Some(v.clone()));
        } else {
            r.flight.kick(false);
        }
        r
    };
    if !crate::hooks::in_scope() {
        return build();
    }
    let r = crate::hooks::with_slot(build);
    // Re-render (the slot hit and skipped the closure above): a value may
    // have landed in the cache since the first mount — seed it quietly so
    // `get()` sees it without a refetch. A resource that already holds data
    // (its own fetch landed) is left alone.
    if r.data.get().is_none() {
        if let Some(v) = CACHE.with(|c| {
            c.borrow()
                .get(&probe_key)
                .and_then(|v| v.downcast_ref::<T>().cloned())
        }) {
            r.data.set_internal(Some(v));
        }
    }
    r
}

/// Suspense-shaped view: `fallback` while the resource is empty (loading,
/// not yet fetched, or failed), the mapped view once the value landed.
/// The classic `match res.get() { Some(v) => ready(v), None => fallback }`
/// every caller re-implements.
pub fn suspense_view<T: Clone + 'static, F: FnOnce(T) -> View>(
    res: &Resource<T>,
    fallback: View,
    ready: F,
) -> View {
    match res.get() {
        Some(value) => ready(value),
        None => fallback,
    }
}

/// Drain completed fetches: apply each result to its signal. True when
/// anything landed (the caller re-renders / broadcasts patches). Tasks
/// whose thread died before sending (kill -9 style) are reaped quietly
/// and mark the resource no-longer-loading; panicking fetches arrive as
/// `Err(message)` via catch_unwind instead.
///
/// Landings are collected under a short borrow and applied AFTER it is
/// released: applying a value flushes dependent effects synchronously, and
/// an effect that calls [`Resource::refetch`] re-enters [`TASKS`] — it
/// must not find the drain still holding the borrow.
pub fn drain() -> bool {
    enum Settled {
        Value(Rc<dyn Fn(Value, u64)>, Value, u64),
        Dead(Rc<dyn Fn(u64)>, u64),
    }
    let settled: Vec<Settled> = TASKS.with(|t| {
        let mut book = t.borrow_mut();
        let mut settled = Vec::new();
        book.pending.retain(|task| match task.rx.try_recv() {
            Ok(v) => {
                settled.push(Settled::Value(task.apply.clone(), v, task.gen));
                false // consumed; drop the receiver
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => true, // still waiting
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                settled.push(Settled::Dead(task.fail.clone(), task.gen));
                false
            }
        });
        settled
    });
    let mut landed = false;
    for s in settled {
        match s {
            Settled::Value(apply, v, gen) => {
                apply(v, gen);
                landed = true;
            }
            Settled::Dead(fail, gen) => fail(gen),
        }
    }
    landed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_lands_result_on_drain() {
        let r = use_resource(|| 21 * 2);
        assert!(r.get().is_none(), "not fetched yet");
        // The fetch thread needs a moment.
        for _ in 0..200 {
            if drain() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(r.get(), Some(42));
        // A later drain is a no-op.
        assert!(!drain());
    }

    #[test]
    fn failed_sender_is_dropped_quietly() {
        // Spawn a resource whose thread panics: the disconnecting channel
        // must be reaped, not spin the drain.
        let r = use_resource(|| -> u32 { panic!("boom") });
        for _ in 0..100 {
            if drain() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(r.get().is_none(), "a panicking fetch yields nothing");
        // Drains keep working afterwards.
        let r2 = use_resource(|| 1u8);
        for _ in 0..200 {
            if drain() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(r2.get(), Some(1));
    }

    #[test]
    fn type_mismatch_is_ignored_safely() {
        // A value of the wrong type (only possible via misuse) must not
        // corrupt the drain.
        let r: Resource<u32> = use_resource(|| 5u32);
        for _ in 0..200 {
            if drain() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(r.get(), Some(5));
    }

    #[test]
    fn refetch_flips_loading_and_lands_fresh_value() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let hits = std::sync::Arc::new(AtomicU32::new(0));
        let h = hits.clone();
        // fetch_add returns the PREVIOUS count: 0 then 1.
        let r: Resource<u32> = use_resource(move || h.fetch_add(1, Ordering::SeqCst));
        assert!(r.is_loading(), "in flight right after the initial fetch");
        assert_eq!(r.get(), None);
        for _ in 0..200 {
            if drain() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(r.get(), Some(0));
        assert!(!r.is_loading(), "landed → not loading");
        // Refetch: loading flips back on while the OLD value stays visible.
        r.refetch();
        assert!(r.is_loading(), "refetch flips loading on");
        assert_eq!(r.get(), Some(0), "stale value kept while refetching");
        for _ in 0..200 {
            if drain() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(r.get(), Some(1), "fresh value landed");
        assert!(!r.is_loading());
        assert_eq!(hits.load(Ordering::SeqCst), 2, "fetch ran exactly twice");
    }

    #[test]
    fn refetch_while_in_flight_coalesces() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let hits = std::sync::Arc::new(AtomicU32::new(0));
        let h = hits.clone();
        // The first fetch stays in the air for a while: every kick fired
        // during that window must coalesce into ONE follow-up instead of
        // stacking a thread per kick.
        let r: Resource<u32> = use_resource(move || {
            h.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(80));
            7u32
        });
        // inflight was set synchronously by the initial kick, so these all
        // land in the coalescing window.
        for _ in 0..10 {
            r.refetch();
        }
        // Give the single spawned thread a moment to start (fetch_add runs
        // first, then it sleeps): coalescing means no OTHER thread exists.
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "kicks during flight coalesce — no new threads yet"
        );
        // Drain the initial fetch; its landing fires the single follow-up.
        for _ in 0..400 {
            if drain() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(r.get(), Some(7));
        // Drain the coalesced follow-up.
        for _ in 0..400 {
            if drain() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "exactly one coalesced follow-up, not ten"
        );
        assert_eq!(r.get(), Some(7));
        assert!(!r.is_loading());
    }

    // A deps-driven resource: deps 0 sleeps 150ms, deps 1 returns
    // instantly. Change deps while the slow fetch is in the air — the
    // slow fetch's late landing must be discarded, not overwrite the
    // fresh value the fast fetch delivered.
    thread_local! {
        static SINK: RefCell<Option<Resource<String>>> = const { RefCell::new(None) };
    }

    #[crate::component]
    fn StaleTile(ver: u64) -> View {
        // `ver` is CAPTURED — the fetch runs on a background thread where
        // a thread_local read would see a fresh default.
        let res = use_resource_with_deps(ver, move || {
            if ver == 0 {
                std::thread::sleep(std::time::Duration::from_millis(150));
                "slow".to_string()
            } else {
                "fresh".to_string()
            }
        });
        SINK.with(|s| *s.borrow_mut() = Some(res));
        View::default()
    }

    #[test]
    fn stale_deps_result_never_overwrites_fresh() {
        let ver = use_signal(|| 0u64);
        let render = crate::hooks::wrap_render(Rc::new(move || StaleTile(ver.get())));
        let _v1 = render(); // deps 0: the slow fetch is in the air
        let res = SINK.with(|s| s.borrow().clone()).unwrap();
        assert!(res.is_loading(), "slow fetch in flight");
        // Deps change mid-flight: the in-air fetch is invalidated and a
        // coalesced follow-up is queued (it re-runs the NEW closure).
        ver.set(1);
        let _v2 = render();
        let res2 = SINK.with(|s| s.borrow().clone()).unwrap();
        // Slot stability is proven by the landing below: if the second build
        // had allocated a fresh resource, the "fresh" value would land in
        // THAT cell and `res` would never see it.
        assert_eq!(res.get(), None, "slow fetch still in flight");
        // Drain until the fresh value lands (~150ms: the slow landing is
        // discarded, which fires the instant follow-up).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while res2.get().is_none() && std::time::Instant::now() < deadline {
            drain();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(res2.get().as_deref(), Some("fresh"), "fresh fetch landed");
        // Past the slow fetch's 150ms: everything is settled and the value
        // is untouched — the stale landing never overwrote it.
        std::thread::sleep(std::time::Duration::from_millis(120));
        drain();
        assert_eq!(
            res2.get().as_deref(),
            Some("fresh"),
            "value stays fresh after the slow fetch would have landed"
        );
        assert!(!res2.is_loading());
    }

    #[test]
    fn clear_cached_forces_a_refetch() {
        clear_cached_all();
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let h = hits.clone();
        let r1: Resource<u32> = use_cached_resource("clear-cache-test", move || {
            h.fetch_add(1, Ordering::SeqCst) + 7
        });
        assert!(r1.get().is_none(), "cache miss → fetch on a thread");
        for _ in 0..200 {
            if drain() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(r1.get(), Some(7));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        // Second call with the same key: seeded instantly, no fetch.
        let r2: Resource<u32> = use_cached_resource("clear-cache-test", || 7u32);
        assert_eq!(r2.get(), Some(7), "cache hit seeds without loading");
        assert_eq!(hits.load(Ordering::SeqCst), 1, "no second fetch");
        // Clear the entry: the next call misses and fetches again.
        clear_cached("clear-cache-test");
        let h2 = hits.clone();
        let r3: Resource<u32> = use_cached_resource("clear-cache-test", move || {
            h2.fetch_add(1, Ordering::SeqCst);
            7
        });
        assert!(r3.get().is_none(), "cache cleared → miss → fetch");
        for _ in 0..200 {
            if drain() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(r3.get(), Some(7), "fresh fetch landed after clear");
        assert_eq!(hits.load(Ordering::SeqCst), 2, "exactly one refetch");
        clear_cached_all();
    }
}
