//! `page-js` — opt-in page JavaScript through QuickJS.
//!
//! Owner-decision tier (see `SCRIPTING_DECISION.md`, amended 2026-10-14):
//! pages *may* execute their own scripts when the host enables page JS.
//! Everything here is **feature-gated (`page-js`)** and **off by
//! default** — without the opt-in, the pipeline is byte-identical to the
//! no-script engine.
//!
//! Design:
//! - One budgeted QuickJS runtime (`kitae_script::ScriptRuntime`) per
//!   page, built at the load's script phase and dropped at the next
//!   navigation.
//! - The DOM layer is a **shim over the real arena DOM**: all DOM access
//!   goes through small Rust global functions operating on stable
//!   element tokens (`token ↔ NodeId`), and the entire element/document
//!   JS surface is defined once in `BOOTSTRAP` (reviewable JavaScript,
//!   no per-wrapper Rust plumbing). Tokens give element identity: two
//!   `getElementById` calls for the same node return the same wrapper.
//! - Rust closures cannot hold the `Dom`, so each execution scope
//!   installs the `&mut Dom` in a thread-local slot ([`with_dom`]) and
//!   the token ops reach it through [`use_dom`]. **Invariant:** script
//!   code runs only inside a scope installed on the engine thread (one
//!   browser thread per page); an op arriving outside a scope is a
//!   silent no-op, never a crash.
//! - Event-loop callbacks (timer fns, listeners, fetch resolvers) live
//!   in JS-side registries on the `__kitae` global, keeping native
//!   promise and identity semantics between context scopes.
//! - `fetch()` rides the shared `kitae_net::pool`, origin-scoped to the
//!   page (S-1); completions settle real promises, and the host applies
//!   DOM changes through the ordinary relayout.
//!
//! Tier JS-1 surface (inventory in `SCRIPTING_DECISION.md`, amended
//! 2026-10-15): `document` (getElementById, body/documentElement/head,
//! createElement/textNode, querySelector(All) + getElementsByTagName over
//! the common selector grammar, title, write/writeln, readyState,
//! location), element surface (attributes, classList, textContent,
//! innerHTML, appendChild/insertBefore/removeChild/remove, full style
//! object including `style.prop =` via a Proxy, addEventListener/onclick
//! with preventDefault honored, value, children, parentElement,
//! element.querySelector(All)/closest), timers, `fetch` (GET →
//! text/json), `console`, `localStorage` (per-page), `navigator.userAgent`,
//! and ES modules — inline/external, static chains + dynamic `import()`,
//! budget-capped.
//!
//! Tier JS-2 surface (amended 2026-10-15): animation and scheduling
//! (`requestAnimationFrame`/`cancelAnimationFrame` draining from the
//! idle tick, `performance.now`, `queueMicrotask`, `requestIdleCallback`,
//! `MessageChannel`), geometry (`getBoundingClientRect`,
//! `offsetWidth`/`offsetHeight`, `innerWidth`/`innerHeight` — the engine
//! publishes node boxes + viewport at relayout; text runs are scored as
//! click targets alongside painted boxes), History API (pushState/
//! replaceState same-document, back/forward/go, popstate, location.hash),
//! full event propagation (capture → target → bubble, stopPropagation/
//! stopImmediatePropagation/currentTarget, window/document delegation),
//! and form semantics (`checked`, select-aware `value`, engine-dispatched
//! keydown/input/keyup/change/submit; preventDefault on submit suppresses
//! the engine's submission).
//!
//! Tier JS-3 surface (amended 2026-10-15, Track A's first step): the
//! standard constructor set (`Event`/`CustomEvent`/`MouseEvent`/
//! `KeyboardEvent` + `dispatchEvent`, `AbortController`/`AbortSignal`,
//! `DOMException`), listener options (`once`/`passive`/`signal`),
//! `URL`/`URLSearchParams` over the engine's URL handling, `Headers`/
//! `Request`/`Response` (fetch resolves any HTTP status), `FormData` +
//! multipart POST bodies over the pool's Raw path, `structuredClone`,
//! `element.dataset`, `getComputedStyle` (computed subset + used
//! width/height from the published geometry), and the scroll/selection/
//! focus reads (`scrollTo`/`scrollY`/`scrollIntoView`/`scrollTop`/
//! `focus()`/`blur()`/`activeElement`/`getSelection`). The host applies
//! script scroll/focus through the poll (pending channels), publishes
//! view state on every publish and the computed-style map at relayout.
//!
//! Tier JS-4 surface (amended 2026-10-16): `MutationObserver` (records
//! journaled by the DOM wrappers — attributes, characterData, childList —
//! with `subtree`/`attributeFilter`/old-value flags, delivered on the
//! microtask queue with the engine tick as the safety net),
//! `IntersectionObserver` (threshold crossings against the published
//! display-list geometry and scroll state), `ResizeObserver` (content-box
//! sizes from the same publish).
//!
//! Tier JS-5 surface (amended 2026-10-16): `XMLHttpRequest` (async only —
//! sync XHR is refused by design — riding the same fetch pool as `fetch`,
//! so custom headers and bodies take the pool's Raw path) and `Worker` as
//! a documented same-thread emulation (pool-fetched script evaluated with
//! `self`/`postMessage`/`addEventListener` bound to a port, messages
//! `structuredClone`d and queued until the script is ready, `terminate()`
//! stops delivery; no real parallelism).
//!
//! Tier JS-6 surface (amended 2026-10-16): Web Storage — `localStorage`
//! (process-wide per origin, in memory) and `sessionStorage` (scoped to
//! one `Browser`, surviving navigation inside it) as spec-shaped
//! `Storage` objects with Proxy named properties; opaque origins throw
//! `SecurityError`; no disk persistence and no cross-window `storage`
//! events (documented gaps). Canvas 2D — `<canvas>` is a replaced
//! element, `getContext('2d')` draws into a JS-side RGBA buffer
//! (rects/paths/arcs/curves, both fill rules, the affine transform set
//! with save/restore, ImageData reads/writes, `drawImage` from canvas/
//! `ImageData`/`<img>`, `toDataURL()` → PNG), and the buffer reaches the
//! engine once per poll tick through the `\0canvas:{node}` sentinel that
//! layout emits and the publish pass stamps. Text drawing, clip,
//! gradients/patterns/shadows, anti-aliasing and `toBlob` are the
//! recorded gaps.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use kitae_dom::{Dom, NodeId, NodeKind};
use kitae_script::rquickjs::function::Func;
use kitae_script::rquickjs::{Array, Function, Object, Value};

use kitae_script::{HostApi, ModuleFetcher, RuntimeConfig, ScriptRuntime};

/// Page-JS budgets. Pages are hostile territory: 64 MiB heap, 8 MiB
/// interpreter stack, 10 s wall clock per task.
pub const PAGE_MEMORY_BYTES: usize = 64 * 1024 * 1024;
pub const PAGE_STACK_BYTES: usize = 8 * 1024 * 1024;
pub const PAGE_DEADLINE_MS: u64 = 10_000;
/// External script caps: a script-hostile page cannot balloon fetch work.
pub(crate) const MAX_SCRIPTS: usize = 64;
pub(crate) const MAX_SCRIPT_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_SCRIPT_TOTAL_BYTES: usize = 4 * 1024 * 1024;
/// ES-module caps (a separate ledger from classic scripts): 96 modules,
/// 1 MiB each, 6 MiB total for one page, and one shared wall-clock budget
/// across every module fetch (imports and `<script type="module" src>`)
/// so an import chain cannot stall the load indefinitely.
pub(crate) const MAX_MODULES: usize = 96;
pub(crate) const MAX_MODULE_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_MODULE_TOTAL_BYTES: usize = 6 * 1024 * 1024;
pub(crate) const MODULE_FETCH_DEADLINE_MS: u64 = 12_000;

// ---------------------------------------------------------------------------
// The scoped DOM pointer (see module docs for the invariant)
// ---------------------------------------------------------------------------

thread_local! {
    static DOM: Cell<*mut Dom> = const { Cell::new(std::ptr::null_mut()) };
}

/// Install `dom` as the live DOM for the duration of `f`.
pub fn with_dom<R>(dom: &mut Dom, f: impl FnOnce(&mut Dom) -> R) -> R {
    DOM.with(|slot| {
        let outer = slot.get();
        slot.set(dom as *mut Dom);
        let out = f(dom);
        slot.set(outer);
        out
    })
}

/// Reach the installed DOM from a token op. `None` outside a scope.
/// ElementData-level attribute read through the arena (kitae_dom keeps
/// attribute storage on the element data, not the Dom).
fn dom_attr<'a>(dom: &'a Dom, node: NodeId, name: &str) -> Option<&'a str> {
    dom.element(node).and_then(|e| e.attr(name))
}

fn use_dom<R>(f: impl FnOnce(&mut Dom) -> R) -> Option<R> {
    DOM.with(|slot| {
        let p = slot.get();
        if p.is_null() {
            None
        } else {
            // Safety: set by `with_dom` on this thread, still in scope.
            Some(f(unsafe { &mut *p }))
        }
    })
}

/// Run a token op against the live DOM if the token resolves. Mutating
/// ops call [`touch`] themselves.
fn node_op<T>(shared: &Shared, token: u64, f: impl FnOnce(&mut Dom, NodeId) -> T) -> Option<T> {
    let node = shared.node_for(token)?;
    use_dom(|dom| f(dom, node))
}

// ---------------------------------------------------------------------------
// Shared session state reachable from every JS closure
// ---------------------------------------------------------------------------

struct Shared {
    token_of: Mutex<HashMap<NodeId, u64>>,
    node_of: Mutex<HashMap<u64, NodeId>>,
    next_token: AtomicU64,
    dirty: AtomicBool,
    logs: Mutex<Vec<String>>,
    /// Retained diagnostics (capped) — drained `logs` lose history.
    #[allow(dead_code)]
    history: Mutex<Vec<String>>,
    page_url: Mutex<String>,
    pending_nav: Mutex<Option<String>>,
    /// In-flight pooled fetches awaiting completion: (pool job id, JS
    /// promise id). The two id spaces are independent — the pool counts
    /// process-wide, the JS seq counts per page — and the mapping is the
    /// settle path's whole job.
    inflight: Mutex<Vec<(u64, u64)>>,
    /// Rust-side timer bookkeeping: id → (due, repeating, interval_ms).
    timers: Mutex<HashMap<u64, (Instant, bool, u64)>>,
    /// Latest publish: node → (x, y, w, h) union of display-list rects,
    /// plus the viewport, refreshed by the host after every relayout.
    geometry: Mutex<(HashMap<u32, (f32, f32, f32, f32)>, (f32, f32))>,
    /// `history.pushState`/`replaceState`: (resolved url, replace). The
    /// host applies URL-only state moves without reloading.
    pending_history: Mutex<Option<(String, bool)>>,
    /// Published view state: engine scroll (x always 0 — the engine has no
    /// horizontal scroll), focused node, selection text. Refreshed by the
    /// host at publish; `window.scrollY`/`document.activeElement`/
    /// `getSelection().toString()` read from here.
    view: Mutex<(f32, f32, Option<u32>, String)>,
    /// Script-requested scroll: window.scrollTo/scrollBy and
    /// element.scrollIntoView resolve to an absolute y; the host applies
    /// it in its poll (clamped like every other scroll path).
    pending_scroll: Mutex<Option<f32>>,
    /// element.focus()/blur(): outer None = nothing pending, Some(inner) =
    /// apply — inner None blurs, inner Some(token) focuses that element.
    pending_focus: Mutex<Option<Option<u64>>>,
    /// Computed-style text per node (a readable subset, used width/height
    /// overridden from geometry), refreshed by the host at publish.
    computed: Mutex<HashMap<u32, String>>,
    /// Web-storage session token (`sessionStorage` scope): one per
    /// `Browser`, threaded in at construction.
    session: u64,
}

impl Shared {
    fn trace(&self, line: String) {
        let mut h = self.history.lock().unwrap();
        if h.len() < 200 {
            h.push(line);
        }
    }

    fn token_for(&self, node: NodeId) -> u64 {
        let mut map = self.token_of.lock().unwrap();
        if let Some(t) = map.get(&node) {
            return *t;
        }
        let t = self.next_token.fetch_add(1, Ordering::Relaxed) + 1;
        map.insert(node, t);
        self.node_of.lock().unwrap().insert(t, node);
        t
    }

    fn node_for(&self, token: u64) -> Option<NodeId> {
        self.node_of.lock().unwrap().get(&token).copied()
    }

    fn touch(&self) {
        self.dirty.store(true, Ordering::Relaxed);
    }
}

impl HostApi for Shared {
    fn log(&self, message: &str) {
        (self.logs.lock().unwrap()).push(message.to_string());
    }
}

// ---------------------------------------------------------------------------
// Web storage (tier JS-6)
// ---------------------------------------------------------------------------

/// One entry list per scope. `localStorage`'s scope is `local|<origin>`:
/// process-wide and in memory, shared by every page of that origin — the
/// spec's model minus persistence (documented gap). `sessionStorage`'s is
/// `session|<token>|<origin>`, where the token names one `Browser`, so the
/// area is private to that browser and survives navigation inside it.
/// Entry order is insertion order: `key(i)` reports spec order, and a key
/// removed and re-added moves to the end.
type WebStorageMap = HashMap<String, Vec<(String, String)>>;

fn web_storage() -> &'static Mutex<WebStorageMap> {
    static STORE: OnceLock<Mutex<WebStorageMap>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn storage_scope(session: &str, origin: &str) -> String {
    if session.is_empty() {
        format!("local|{origin}")
    } else {
        format!("session|{session}|{origin}")
    }
}

/// Empty the store. Test scaffolding, not a page capability: storage is
/// deliberately shared between pages, so a test that wants a virgin origin
/// must ask for one.
#[cfg(test)]
pub(crate) fn reset_web_storage() {
    web_storage().lock().unwrap().clear();
}

// ---------------------------------------------------------------------------
// ES-module fetching (kitae_script::ModuleFetcher over the engine cache)
// ---------------------------------------------------------------------------

/// Counters behind one page's module budget — imports and
/// `<script type="module" src>` entries share the single ledger.
#[derive(Default)]
struct ModuleBudget {
    count: usize,
    bytes: usize,
    first_fetch: Option<Instant>,
}

/// The page's ES-module fetcher. Specifiers canonicalize against the
/// importing module's URL (empty base = classic-script `import()` → the
/// page URL); sources ride the same engine fetch cache and origin gates
/// as classic scripts and `fetch()`.
struct PageModuleFetcher {
    shared: Arc<Shared>,
    budget: Mutex<ModuleBudget>,
}

impl kitae_script::ModuleFetcher for PageModuleFetcher {
    fn resolve(&self, base: &str, name: &str) -> Result<String, String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("empty module specifier".into());
        }
        // v0 module space: relative, root-relative, and absolute http(s).
        // Bare specifiers ("react") have no registry; data:/blob: are not
        // v0 material either — both reject the chain cleanly.
        let resolvable = name.starts_with("./")
            || name.starts_with("../")
            || name.starts_with('/')
            || name.starts_with("//")
            || name.starts_with("http://")
            || name.starts_with("https://");
        if !resolvable {
            return Err(format!("bare module specifier '{name}' unsupported"));
        }
        let base = if base.is_empty() {
            self.shared.page_url.lock().unwrap().clone()
        } else {
            base.to_string()
        };
        kitae_net::resolve(&base, name)
            .ok_or_else(|| format!("unresolvable module specifier '{name}'"))
    }

    fn load(&self, url: &str) -> Result<String, String> {
        let mut budget = self.budget.lock().unwrap();
        if budget.count >= MAX_MODULES {
            let msg = "module budget: too many modules";
            self.shared.log(msg);
            return Err(msg.into());
        }
        if budget.bytes >= MAX_MODULE_TOTAL_BYTES {
            let msg = "module budget: too many module bytes";
            self.shared.log(msg);
            return Err(msg.into());
        }
        let first = *budget.first_fetch.get_or_insert_with(Instant::now);
        if first + Duration::from_millis(MODULE_FETCH_DEADLINE_MS) < Instant::now() {
            let msg = "module budget: fetch deadline exceeded";
            self.shared.log(msg);
            return Err(msg.into());
        }
        match kitae_net::fetch_cached(url) {
            Ok(resp) => {
                if resp.status >= 400 {
                    let msg = format!("module fetch {url}: HTTP {}", resp.status);
                    self.shared.log(&msg);
                    return Err(msg);
                }
                if resp.body.len() > MAX_MODULE_BYTES {
                    let msg = format!("module {url} oversized: {} bytes", resp.body.len());
                    self.shared.log(&msg);
                    return Err(msg);
                }
                budget.count += 1;
                budget.bytes += resp.body.len();
                Ok(String::from_utf8_lossy(&resp.body).into_owned())
            }
            Err(e) => {
                let msg = format!("module fetch {url}: {e}");
                self.shared.log(&msg);
                Err(msg)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Stats + the per-page runtime handle the Browser stores
// ---------------------------------------------------------------------------

/// Stats from the load-time script phase.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ScriptPhaseStats {
    pub scripts_seen: usize,
    pub scripts_ran: usize,
    pub scripts_ok: usize,
    pub scripts_failed: usize,
    pub modules_seen: usize,
    pub modules_ok: usize,
    pub modules_failed: usize,
}

/// The page's live script runtime + registries. Owned by the `Browser`
/// between the script phase and the next navigation.
pub struct JsPage {
    host: ScriptRuntime,
    shared: Arc<Shared>,
    /// Module ledger + fetch policy — both the host's loader and the
    /// script-phase collector drive this one instance.
    modules: Arc<PageModuleFetcher>,
}

impl JsPage {
    /// Build a runtime for `page_url` with the page-API surface installed.
    /// `session` scopes `sessionStorage` to the owning `Browser`.
    pub fn new(page_url: &str, session: u64) -> Result<Self, String> {
        let shared = Arc::new(Shared {
            token_of: Mutex::new(HashMap::new()),
            node_of: Mutex::new(HashMap::new()),
            next_token: AtomicU64::new(0),
            dirty: AtomicBool::new(false),
            logs: Mutex::new(Vec::new()),
            page_url: Mutex::new(page_url.to_string()),
            pending_nav: Mutex::new(None),
            inflight: Mutex::new(Vec::new()),
            timers: Mutex::new(HashMap::new()),
            history: Mutex::new(Vec::new()),
            geometry: Mutex::new((HashMap::new(), (0.0, 0.0))),
            pending_history: Mutex::new(None),
            view: Mutex::new((0.0, 0.0, None, String::new())),
            pending_scroll: Mutex::new(None),
            pending_focus: Mutex::new(None),
            computed: Mutex::new(HashMap::new()),
            session,
        });
        let modules = Arc::new(PageModuleFetcher {
            shared: shared.clone(),
            budget: Mutex::new(ModuleBudget::default()),
        });
        let host = ScriptRuntime::new(
            RuntimeConfig {
                memory_bytes: PAGE_MEMORY_BYTES,
                max_stack_bytes: PAGE_STACK_BYTES,
                deadline_ms: PAGE_DEADLINE_MS,
            },
            shared.clone(),
        )
        .map_err(|e| e.to_string())?;
        host.set_module_fetcher(modules.clone());
        let page = Self {
            host,
            shared,
            modules,
        };
        page.install().map_err(|e| e.to_string())?;
        Ok(page)
    }

    /// Run the load-time script phase against the freshly parsed DOM: all
    /// `<script>` blocks in document order (inline text; `src` fetched
    /// synchronously like stylesheets — engine cache, origin gate), then
    /// ES modules (deferred per spec: after the classic scripts, before
    /// the load events), then DOMContentLoaded + load dispatch.
    pub fn run_document_scripts(
        &mut self,
        dom: &mut Dom,
        root: NodeId,
        base_url: &str,
    ) -> ScriptPhaseStats {
        let mut stats = ScriptPhaseStats::default();
        let (sources, modules) = collect_scripts(dom, root, base_url, &mut stats, &self.modules);
        with_dom(dom, |dom| {
            if let Err(e) = self.host.run(BOOTSTRAP) {
                self.shared.log(&format!("bootstrap failed: {e}"));
                return;
            }
            for source in sources {
                stats.scripts_ran += 1;
                match self.host.run(&source) {
                    Ok(()) => stats.scripts_ok += 1,
                    Err(e) => {
                        stats.scripts_failed += 1;
                        self.shared.log(&format!("script error: {e}"));
                    }
                }
            }
            for module in modules {
                match self.host.run_module(&module.name, &module.source) {
                    Ok(()) => stats.modules_ok += 1,
                    Err(e) => {
                        stats.modules_failed += 1;
                        self.shared.log(&format!("module {}: {e}", module.name));
                    }
                }
            }
            // Microtasks from the page's own scripts settle before the
            // load events fire.
            let _ = self.host.pump_jobs();
            self.fire_doc_event("DOMContentLoaded", 0.0, 0.0);
            self.fire_doc_event("load", 0.0, 0.0);
            self.tick_canvases();
        });
        stats
    }

    /// Drain the page's event-loop slice: due timers, settled fetches, a
    /// queued `location.href` navigation. Callbacks run with `dom`
    /// installed (.without the scope they would silently no-op).
    /// Returns whether the DOM mutated (the host relayouts + republishes
    /// then).
    pub fn poll(&mut self, dom: &mut Dom) -> bool {
        with_dom(dom, |dom| {
            self.drain_timers();
            self.settle_fetches();
            self.tick_observers();
            self.tick_canvases();
        });
        self.take_pending_nav();
        self.shared.dirty.swap(false, Ordering::Relaxed)
    }

    /// One observers pass (tier JS-4): deliver journaled mutation records,
    /// then evaluate Intersection/Resize observers against the published
    /// geometry and view state. The host calls this once per poll, so a
    /// script's observers see engine-driven changes (scroll, resize,
    /// relayout) without any script-side polling.
    fn tick_observers(&self) {
        let _ = self.host.with_context(|ctx| {
            let globals = ctx.globals();
            if let Ok(tick) = globals.get::<_, Function>("__kitaeObserverTick") {
                let _ = tick.call::<_, ()>(());
            }
        });
        let _ = self.host.pump_jobs();
    }

    /// Hand the page's dirty canvas backing stores to the engine's raster
    /// cache (tier JS-6): one JS-to-Rust crossing per poll, not per draw
    /// call. The flush marks the page dirty, so the host's relayout picks
    /// the new pixels up on the same tick.
    fn tick_canvases(&self) {
        let _ = self.host.with_context(|ctx| {
            let globals = ctx.globals();
            if let Ok(flush) = globals.get::<_, Function>("__kitaeCanvasFlush") {
                let _ = flush.call::<_, bool>(());
            }
        });
        let _ = self.host.pump_jobs();
    }

    /// Console lines accumulated since the last drain (host notification).
    pub fn take_console(&self) -> Vec<String> {
        std::mem::take(&mut *self.shared.logs.lock().unwrap())
    }

    /// Retained diagnostics (capped; tests).
    #[cfg(test)]
    pub fn trace_snapshot(&self) -> Vec<String> {
        self.shared.history.lock().unwrap().clone()
    }

    /// Fire document-level listeners of `type` (delegation-style pages).
    /// Runs inside the DOM scope — listeners may mutate. Returns whether
    /// listeners ran.
    pub fn dispatch_doc_event(&mut self, dom: &mut Dom, type_name: &str, x: f32, y: f32) -> bool {
        let fired = with_dom(dom, |_dom| self.fire_doc_event(type_name, x, y));
        let dirty = self.shared.dirty.swap(false, Ordering::Relaxed) || fired > 0;
        dirty
    }

    /// Display-list geometry, refreshed by the host after every relayout:
    /// `getBoundingClientRect` / `innerWidth` answers come from here.
    pub fn set_geometry(&self, rects: HashMap<u32, (f32, f32, f32, f32)>, viewport: (f32, f32)) {
        *self.shared.geometry.lock().unwrap() = (rects, viewport);
    }

    /// Published view state (scroll, focused node, selection text), same
    /// refresh cadence as [`Self::set_geometry`].
    pub fn set_view_state(&self, scroll: (f32, f32), active: Option<u32>, selection: String) {
        *self.shared.view.lock().unwrap() = (scroll.0, scroll.1, active, selection);
    }

    /// Computed-style text per node (see `computed_css` on the host side).
    pub fn set_computed(&self, styles: HashMap<u32, String>) {
        *self.shared.computed.lock().unwrap() = styles;
    }

    /// A script-requested absolute scroll y, if any (applied by the host).
    pub fn take_pending_scroll(&self) -> Option<f32> {
        self.shared.pending_scroll.lock().unwrap().take()
    }

    /// A script-requested focus change: None = nothing pending, Some(None)
    /// = blur, Some(Some(token)) = focus the element behind `token`.
    pub fn take_pending_focus(&self) -> Option<Option<u64>> {
        self.shared.pending_focus.lock().unwrap().take()
    }

    /// Resolve a JS token back to its NodeId (host-applied script effects).
    pub fn node_for_token(&self, token: u64) -> Option<NodeId> {
        self.shared.node_for(token)
    }

    /// The stable JS token for `node` — host-driven dispatch (input/submit
    /// on engine-edited nodes) addresses elements through this.
    pub fn token_for_node(&self, node: NodeId) -> u64 {
        self.shared.token_for(node)
    }

    /// Test probe: evaluate `src` in the live context and stringify the
    /// result (registry inspection — tests only).
    #[cfg(test)]
    pub fn host_eval_for_test(&self, src: &str) -> Option<String> {
        self.host
            .with_context(|ctx| match ctx.eval::<String, _>(src) {
                Ok(s) => Some(s),
                Err(_) => {
                    let _ = ctx.catch();
                    None
                }
            })
    }

    /// URL-only history moves queued by `history.pushState`/`replaceState`.
    pub fn take_pending_history(&mut self) -> Option<(String, bool)> {
        self.shared.pending_history.lock().unwrap().take()
    }

    /// Dispatch `type` at `token` through the capture/bubble walk (element
    /// listeners, then document). `data` rides the event when present
    /// (input's `data`). Returns (fired, dirty, prevented).
    pub fn dispatch_element_event(
        &mut self,
        dom: &mut Dom,
        type_name: &str,
        token: u64,
        data: Option<&str>,
    ) -> (usize, bool, bool) {
        let mut fired = 0;
        let mut prevented = false;
        with_dom(dom, |dom| {
            let _ = dom; // installed for the listeners' benefit
            let _ = self
                .host
                .with_context(|ctx| -> Result<(), kitae_script::rquickjs::Error> {
                    let globals = ctx.globals();
                    let make: Function = globals.get("__kitaeMakeEvent")?;
                    let event: Object = make.call((type_name, 0.0f64, 0.0f64))?;
                    if let Some(d) = data {
                        event.set("data", d)?;
                    }
                    if let Ok(fire) = globals.get::<_, Function>("__kitaeDispatch") {
                        if let Ok(n) =
                            fire.call::<_, usize>((type_name, token as f64, event.clone()))
                        {
                            fired += n;
                        }
                    }
                    prevented = event.get::<_, bool>("defaultPrevented").unwrap_or(false);
                    Ok(())
                });
            let _ = self.host.pump_jobs();
        });
        let dirty = self.shared.dirty.swap(false, Ordering::Relaxed);
        (fired, dirty, prevented)
    }

    /// Dispatch a `click` to the element at `token` (hit-resolved by the
    /// host from display-list rects) and then to document-level
    /// listeners. Returns (listeners fired, DOM mutated).
    pub fn dispatch_click(
        &mut self,
        dom: &mut Dom,
        token: u64,
        x: f32,
        y: f32,
    ) -> (usize, bool, bool) {
        let mut fired = 0;
        let mut prevented = false;
        with_dom(dom, |dom| {
            let _ = dom; // installed for the listeners' benefit
            let _ = self
                .host
                .with_context(|ctx| -> Result<(), kitae_script::rquickjs::Error> {
                    let globals = ctx.globals();
                    // The event object lives JS-side: listeners calling
                    // preventDefault() mark it, and the mark is read after they
                    // run. One `__kitaeDispatch` walks capture → target → bubble
                    // → document, honoring stopPropagation/stopImmediate.
                    let make: Function = globals.get("__kitaeMakeEvent")?;
                    let event: Object = make.call(("click", x as f64, y as f64))?;
                    if let Ok(fire) = globals.get::<_, Function>("__kitaeDispatch") {
                        if let Ok(n) = fire.call::<_, usize>(("click", token as f64, event.clone()))
                        {
                            fired += n;
                        }
                    }
                    prevented = event.get::<_, bool>("defaultPrevented").unwrap_or(false);
                    Ok(())
                });
            let _ = self.host.pump_jobs();
        }); // with_dom
        let dirty = self.shared.dirty.swap(false, Ordering::Relaxed);
        (fired, dirty || fired > 0, prevented)
    }

    /// Navigation queued by `location.href = …`, if any.
    pub fn take_pending_nav(&mut self) -> Option<String> {
        self.shared.pending_nav.lock().unwrap().take()
    }

    fn drain_timers(&mut self) {
        let now = Instant::now();
        let due: Vec<(u64, bool, u64)> = {
            let timers = self.shared.timers.lock().unwrap();
            timers
                .iter()
                .filter(|(_, (at, _, _))| *at <= now)
                .map(|(id, (_, r, i))| (*id, *r, *i))
                .collect()
        };
        for (id, repeating, interval) in due {
            {
                let mut timers = self.shared.timers.lock().unwrap();
                if repeating {
                    if let Some(entry) = timers.get_mut(&id) {
                        *entry = (
                            Instant::now() + Duration::from_millis(interval),
                            true,
                            interval,
                        );
                    }
                } else {
                    timers.remove(&id);
                }
            }
            let dispatch_source = format!("__kitaeRunTimer({}, {});", id, repeating);
            if let Err(e) = self.host.run(&dispatch_source) {
                self.shared.trace(format!("timer dispatch error: {e}"));
            }
            let _ = self
                .host
                .pump_jobs()
                .map_err(|e| self.shared.log(&format!("job error: {e}")));
        }
    }

    fn settle_fetches(&mut self) {
        let empty = self.shared.inflight.lock().unwrap().is_empty();
        if empty {
            return;
        }
        // Snapshot, claim what landed, keep the rest pending — a poll
        // arriving before a response must not strand its fetch.
        let ids: Vec<(u64, u64)> = std::mem::take(&mut *self.shared.inflight.lock().unwrap());
        let pool_ids: Vec<u64> = ids.iter().map(|(pool, _)| *pool).collect();
        let results = kitae_net::pool::take_results(&pool_ids);
        let claimed: Vec<u64> = results.iter().map(|r| r.id).collect();
        let still_pending: Vec<(u64, u64)> = ids
            .iter()
            .filter(|(pool, _)| !claimed.contains(pool))
            .copied()
            .collect();
        self.shared.inflight.lock().unwrap().extend(still_pending);
        for r in &results {
            let js_id = ids
                .iter()
                .find(|(pool, _)| *pool == r.id)
                .map(|(_, js)| *js)
                .unwrap_or(r.id);
            let _ = js_id;
        }
        for r in results {
            let (ok, status, headers_blob, url, body) = match &r.result {
                Ok(resp) => {
                    let body = String::from_utf8_lossy(&resp.body).into_owned();
                    // What the transport exposes as response headers: the
                    // content-type and the byte length (the net layer keeps
                    // no generic header list — documented in the inventory).
                    let mut headers = String::new();
                    if !resp.content_type.is_empty() {
                        headers.push_str(&format!("content-type: {}\n", resp.content_type));
                    }
                    headers.push_str(&format!("content-length: {}\n", resp.body.len()));
                    (
                        resp.status < 400,
                        resp.status as u32,
                        headers,
                        resp.final_url.clone(),
                        body,
                    )
                }
                Err(e) => (false, 0, String::new(), r.url.clone(), e.to_string()),
            };
            // The JS promise id, not the pool's process-wide job id: the
            // resolvers registry is keyed by the sequencing the page saw.
            let js_id = ids
                .iter()
                .find(|(pool, _)| *pool == r.id)
                .map(|(_, js)| *js)
                .unwrap_or(r.id);
            match self.host.run(&format!(
                "__kitaeSettle({}, {}, {}, {}, {}, {});",
                js_id,
                ok,
                status,
                kitae_script_encode(&headers_blob),
                kitae_script_encode(&url),
                kitae_script_encode(&body)
            )) {
                Ok(()) => {}
                Err(e) => self.shared.trace(format!("fetch settle error: {e}")),
            }
        }
        let _ = self.host.pump_jobs();
    }

    fn fire_doc_event(&self, type_name: &str, x: f32, y: f32) -> usize {
        let fired = self.host.with_context(|ctx| {
            let globals = ctx.globals();
            let fire: kitae_script::rquickjs::Function = globals.get("__kitaeFireDoc")?;
            let ev = kitae_script::rquickjs::Object::new(ctx.clone())?;
            ev.set("type", type_name)?;
            ev.set("clientX", x)?;
            ev.set("clientY", y)?;
            ev.set("target", Value::new_null(ctx.clone()))?;
            ev.set(
                "preventDefault",
                Func::from(|| {
                    // v0: default actions (link navigation, form submit)
                    // stay engine-side; script cannot cancel them.
                }),
            )?;
            fire.call::<_, usize>((type_name, ev))
        });
        let _ = self.host.pump_jobs();
        fired.unwrap_or(0)
    }

    fn install(&self) -> Result<(), String> {
        self.host
            .with_context(|ctx| install_globals(ctx, self.shared.clone()))
            .map_err(|e| e.to_string())?;
        self.host.run(BOOTSTRAP).map_err(|e| e.to_string())?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Rust-side token ops (the data plane behind the BOOTSTRAP surface)
// ---------------------------------------------------------------------------

fn install_globals<'js>(
    ctx: &kitae_script::rquickjs::Ctx<'js>,
    shared: Arc<Shared>,
) -> Result<(), kitae_script::rquickjs::Error> {
    let globals = ctx.globals();

    // ── identity ──
    {
        let s = shared.clone();
        globals.set(
            "__kitaeById",
            Func::from(move |id: String| {
                use_dom(|dom| dom.get_element_by_id(&id).map(|n| s.token_for(n) as f64)).flatten()
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeParent",
            Func::from(move |token: f64| {
                node_op(&*s, token as u64, |dom, n| {
                    dom.node(n).parent.map(|p| s.token_for(p) as f64)
                })
                .flatten()
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeTagTokens",
            Func::from(
                move |ctx: kitae_script::rquickjs::Ctx<'js>,
                      tag: String|
                      -> kitae_script::rquickjs::Result<Array<'js>> {
                    let arr = Array::new(ctx.clone())?;
                    let lower = tag.to_ascii_lowercase();
                    if let Some(tokens) = use_dom(|dom| {
                        let mut out = Vec::new();
                        if let Some(root) = dom.root() {
                            dom.walk(root, &mut |n| {
                                if dom.tag(n) == lower.as_str() {
                                    out.push(s.token_for(n));
                                }
                            });
                        }
                        out
                    }) {
                        for (i, t) in tokens.into_iter().enumerate() {
                            arr.set(i, t as f64)?;
                        }
                    }
                    Ok(arr)
                },
            ),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeChildTokens",
            Func::from(
                move |ctx: kitae_script::rquickjs::Ctx<'js>,
                      token: f64|
                      -> kitae_script::rquickjs::Result<Array<'js>> {
                    let arr = Array::new(ctx.clone())?;
                    if let Some(tokens) = node_op(&*s, token as u64, |dom, n| {
                        dom.children(n)
                            .iter()
                            .map(|c| s.token_for(*c))
                            .collect::<Vec<_>>()
                    }) {
                        for (i, t) in tokens.into_iter().enumerate() {
                            arr.set(i, t as f64)?;
                        }
                    }
                    Ok(arr)
                },
            ),
        )?;
    }

    // ── reads ──
    {
        let s = shared.clone();
        globals.set(
            "__kitaeGetTag",
            Func::from(move |token: f64| {
                node_op(&*s, token as u64, |dom, n| dom.tag(n).to_string()).unwrap_or_default()
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeGetAttr",
            Func::from(move |token: f64, name: String| {
                node_op(&*s, token as u64, |dom, n| {
                    dom_attr(dom, n, name.as_str())
                        .unwrap_or_default()
                        .to_string()
                })
                .unwrap_or_default()
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeHasAttr",
            Func::from(move |token: f64, name: String| {
                node_op(&*s, token as u64, |dom, n| {
                    dom_attr(dom, n, name.as_str()).is_some()
                })
                .unwrap_or(false)
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeGetText",
            Func::from(move |token: f64| {
                node_op(&*s, token as u64, |dom, n| concat_text(dom, n)).unwrap_or_default()
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeOuterHtml",
            Func::from(move |token: f64| {
                node_op(&*s, token as u64, |dom, n| dom.outer_html(n)).unwrap_or_default()
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeGetTitle",
            Func::from(move || {
                use_dom(|dom| {
                    dom.root()
                        .and_then(|r| find_child_tag(dom, r, "head"))
                        .and_then(|head| find_child_tag(dom, head, "title"))
                        .map(|t| concat_text(dom, t))
                        .unwrap_or_default()
                })
                .unwrap_or_default()
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaePageUrl",
            Func::from(move || s.page_url.lock().unwrap().clone()),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeRect",
            Func::from(
                move |ctx: kitae_script::rquickjs::Ctx<'js>,
                      token: f64|
                      -> kitae_script::rquickjs::Result<Array<'js>> {
                    let arr = Array::new(ctx.clone())?;
                    let rect = {
                        let geo = s.geometry.lock().unwrap();
                        let rects = &geo.0;
                        use_dom(|dom| {
                            let node = s.node_for(token as u64)?;
                            let union =
                                |a: Option<(f32, f32, f32, f32)>, r: (f32, f32, f32, f32)| match a {
                                    None => Some(r),
                                    Some((ax, ay, aw, ah)) => {
                                        let x0 = ax.min(r.0);
                                        let y0 = ay.min(r.1);
                                        let x1 = (ax + aw).max(r.0 + r.2);
                                        let y1 = (ay + ah).max(r.1 + r.3);
                                        Some((x0, y0, x1 - x0, y1 - y0))
                                    }
                                };
                            if let Some(r) = rects.get(&node.0) {
                                return Some(*r);
                            }
                            // No direct box (inline wrappers): union the
                            // subtree's boxes so the answer stays usable.
                            let mut acc: Option<(f32, f32, f32, f32)> = None;
                            let mut stack = vec![node];
                            let mut steps = 0usize;
                            while let Some(n) = stack.pop() {
                                steps += 1;
                                if steps > 4096 {
                                    break;
                                }
                                if let Some(r) = rects.get(&n.0) {
                                    acc = union(acc, *r);
                                }
                                for c in dom.children(n) {
                                    stack.push(*c);
                                }
                            }
                            acc
                        })
                        .flatten()
                    };
                    let (x, y, w, h) = rect.unwrap_or((0.0, 0.0, 0.0, 0.0));
                    arr.set(0, x as f64)?;
                    arr.set(1, y as f64)?;
                    arr.set(2, w as f64)?;
                    arr.set(3, h as f64)?;
                    Ok(arr)
                },
            ),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeViewport",
            Func::from(move |axis: f64| -> f64 {
                let (_, viewport) = *s.geometry.lock().unwrap();
                if axis == 1.0 {
                    viewport.1 as f64
                } else {
                    viewport.0 as f64
                }
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaePushState",
            Func::from(move |url: String, replace: f64| {
                let base = s.page_url.lock().unwrap().clone();
                let resolved = if url.is_empty() {
                    base
                } else {
                    kitae_net::resolve(&base, &url).unwrap_or(url)
                };
                *s.page_url.lock().unwrap() = resolved.clone();
                *s.pending_history.lock().unwrap() = Some((resolved, replace != 0.0));
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeHash",
            Func::from(move || -> String {
                let url = s.page_url.lock().unwrap().clone();
                match url.find('#') {
                    Some(i) => url[i..].to_string(),
                    None => String::new(),
                }
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeSetHash",
            Func::from(move |hash: String| {
                let mut url = s.page_url.lock().unwrap().clone();
                if let Some(i) = url.find('#') {
                    url.truncate(i);
                }
                if !hash.is_empty() {
                    url.push('#');
                    url.push_str(hash.trim_start_matches('#'));
                }
                *s.page_url.lock().unwrap() = url.clone();
                *s.pending_history.lock().unwrap() = Some((url, true));
            }),
        )?;
        // ── JS-3: standard constructors & read paths ────────────────────
        let s = shared.clone();
        globals.set(
            "__kitaeResolveUrl",
            Func::from(move |base: String, input: String| -> String {
                let base = if base.is_empty() {
                    s.page_url.lock().unwrap().clone()
                } else {
                    base
                };
                kitae_net::resolve(&base, &input).unwrap_or(input)
            }),
        )?;
        // The spec-strict form: no page-context fallback. `new URL()` uses
        // this so a base-less relative input fails instead of silently
        // resolving against whatever document happens to be loaded.
        globals.set(
            "__kitaeResolveUrlStrict",
            Func::from(move |base: String, input: String| -> Option<String> {
                kitae_net::resolve(&base, &input)
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeAttrNames",
            Func::from(
                move |ctx: kitae_script::rquickjs::Ctx<'js>,
                      token: f64|
                      -> kitae_script::rquickjs::Result<Array<'js>> {
                    let arr = Array::new(ctx.clone())?;
                    if let Some(names) = node_op(&*s, token as u64, |dom, n| {
                        dom.element(n)
                            .map(|e| {
                                e.attributes
                                    .iter()
                                    .map(|(k, _)| k.clone())
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default()
                    }) {
                        for (i, name) in names.into_iter().enumerate() {
                            arr.set(i, name)?;
                        }
                    }
                    Ok(arr)
                },
            ),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeComputedStyle",
            Func::from(move |token: f64| -> String {
                let node = s.node_for(token as u64);
                let Some(node) = node else {
                    return String::new();
                };
                s.computed
                    .lock()
                    .unwrap()
                    .get(&node.0)
                    .cloned()
                    .unwrap_or_default()
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeScrollPos",
            Func::from(move |axis: f64| -> f64 {
                let (x, y, _, _) = s.view.lock().unwrap().clone();
                if axis == 1.0 {
                    y as f64
                } else {
                    x as f64
                }
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeScrollTo",
            Func::from(move |x: f64, y: f64| {
                let _ = x; // the engine scrolls vertically only
                *s.pending_scroll.lock().unwrap() = Some(y as f32);
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeScrollBy",
            Func::from(move |dx: f64, dy: f64| {
                let _ = dx;
                let (_, y, _, _) = s.view.lock().unwrap().clone();
                *s.pending_scroll.lock().unwrap() = Some(y + dy as f32);
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeScrollIntoView",
            Func::from(move |token: f64| {
                let node = s.node_for(token as u64);
                if let Some(node) = node {
                    let geo = s.geometry.lock().unwrap().clone();
                    if let Some((_, y, _, _)) = geo.0.get(&node.0) {
                        *s.pending_scroll.lock().unwrap() = Some(*y);
                    }
                }
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeActiveToken",
            Func::from(move || -> Option<f64> {
                let (_, _, active, _) = s.view.lock().unwrap().clone();
                let node = NodeId(active?);
                Some(s.token_for(node) as f64)
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeSelectionText",
            Func::from(move || -> String {
                let (_, _, _, selection) = s.view.lock().unwrap().clone();
                selection
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeFocusToken",
            Func::from(move |token: f64| {
                *s.pending_focus.lock().unwrap() = Some(Some(token as u64));
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeBlur",
            Func::from(move || {
                *s.pending_focus.lock().unwrap() = Some(None);
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeQueryAll",
            Func::from(
                move |ctx: kitae_script::rquickjs::Ctx<'js>,
                      selector: String|
                      -> kitae_script::rquickjs::Result<Array<'js>> {
                    let arr = Array::new(ctx.clone())?;
                    let matches = use_dom(|dom| query_all(dom, &selector)).unwrap_or_default();
                    for (i, n) in matches.iter().enumerate() {
                        arr.set(i, s.token_for(*n) as f64)?;
                    }
                    Ok(arr)
                },
            ),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeQueryAllIn",
            Func::from(
                move |ctx: kitae_script::rquickjs::Ctx<'js>,
                      token: f64,
                      selector: String|
                      -> kitae_script::rquickjs::Result<Array<'js>> {
                    let arr = Array::new(ctx.clone())?;
                    // Element-scoped queries search the scope node's own
                    // subtree — attached or not. Walking the document root
                    // and filtering (what a document query does) would miss
                    // detached trees, which element querySelector must still
                    // search. The scope itself never qualifies.
                    let matches = use_dom(|dom| {
                        let Some(scope) = s.node_for(token as u64) else {
                            return Vec::new();
                        };
                        let Some(list) = parse_selector_list(&selector) else {
                            return Vec::new();
                        };
                        let mut out = Vec::new();
                        dom.walk(scope, &mut |n| {
                            if n != scope && matches_parsed(dom, n, &list) {
                                out.push(n);
                            }
                        });
                        out
                    })
                    .unwrap_or_default();
                    for (i, n) in matches.iter().enumerate() {
                        arr.set(i, s.token_for(*n) as f64)?;
                    }
                    Ok(arr)
                },
            ),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeClosest",
            Func::from(move |token: f64, selector: String| -> f64 {
                node_op(&*s, token as u64, |dom, start| {
                    let Some(list) = parse_selector_list(&selector) else {
                        return f64::NAN;
                    };
                    let mut up = Some(start);
                    while let Some(n) = up {
                        if matches_parsed(dom, n, &list) {
                            return s.token_for(n) as f64;
                        }
                        up = dom.node(n).parent;
                    }
                    f64::NAN
                })
                .unwrap_or(f64::NAN)
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeGetStyle",
            Func::from(move |token: f64, name: String| {
                node_op(&*s, token as u64, |dom, n| {
                    inline_style(dom_attr(dom, n, "style").unwrap_or_default())
                        .get(name.to_ascii_lowercase().as_str())
                        .cloned()
                        .unwrap_or_default()
                })
                .unwrap_or_default()
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeGetStyleText",
            Func::from(move |token: f64| {
                node_op(&*s, token as u64, |dom, n| {
                    dom_attr(dom, n, "style").unwrap_or_default().to_string()
                })
                .unwrap_or_default()
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeStyleText",
            Func::from(move |token: f64, css: String| {
                node_op(&*s, token as u64, |dom, n| {
                    dom.set_attr(n, "style", css.trim());
                    s.touch();
                });
            }),
        )?;
    }

    // ── writes ──
    {
        let s = shared.clone();
        globals.set(
            "__kitaeCreateElement",
            Func::from(move |tag: String| {
                use_dom(|dom| {
                    let n = dom.create_element(tag.to_ascii_lowercase().as_str(), Vec::new());
                    s.token_for(n) as f64
                })
                .unwrap_or(f64::NAN)
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeCreateText",
            Func::from(move |text: String| {
                use_dom(|dom| {
                    let n = dom.create_text(text);
                    s.token_for(n) as f64
                })
                .unwrap_or(f64::NAN)
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeAppend",
            Func::from(move |p: f64, c: f64| {
                node_op(&*s, p as u64, |dom, parent| {
                    if let Some(child) = s.node_for(c as u64) {
                        if dom.node(child).parent.is_some() {
                            let old = dom.node(child).parent.unwrap();
                            dom.detach(old, child);
                        }
                        dom.append(parent, child);
                        s.touch();
                    }
                });
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeInsertBefore",
            Func::from(move |p: f64, b: f64, c: f64| {
                node_op(&*s, p as u64, |dom, parent| {
                    if let (Some(before), Some(child)) =
                        (s.node_for(b as u64), s.node_for(c as u64))
                    {
                        if let Some(old) = dom.node(child).parent {
                            dom.detach(old, child);
                        }
                        dom.insert_before(parent, before, child);
                        s.touch();
                    }
                });
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeRemoveChild",
            Func::from(move |p: f64, c: f64| {
                node_op(&*s, p as u64, |dom, parent| {
                    if let Some(child) = s.node_for(c as u64) {
                        if dom.node(child).parent == Some(parent) {
                            dom.detach(parent, child);
                            s.touch();
                        }
                    }
                });
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeDetach",
            Func::from(move |token: f64| {
                node_op(&*s, token as u64, |dom, n| {
                    if let Some(p) = dom.node(n).parent {
                        dom.detach(p, n);
                        s.touch();
                    }
                });
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeSetText",
            Func::from(move |token: f64, text: String| {
                node_op(&*s, token as u64, |dom, n| {
                    for child in dom.children(n).to_vec() {
                        dom.detach(n, child);
                    }
                    if !text.is_empty() {
                        let t = dom.create_text(text);
                        dom.append(n, t);
                    }
                    s.touch();
                });
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeSetAttr",
            Func::from(move |token: f64, name: String, v: String| {
                node_op(&*s, token as u64, |dom, n| {
                    dom.set_attr(n, name.as_str(), &v);
                    s.touch();
                });
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeRemoveAttr",
            Func::from(move |token: f64, name: String| {
                node_op(&*s, token as u64, |dom, n| {
                    dom.remove_attr(n, name.as_str());
                    s.touch();
                });
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeSelectValue",
            Func::from(move |token: f64| {
                node_op(&*s, token as u64, |dom, n| select_value(dom, n)).unwrap_or_default()
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeSetSelectValue",
            Func::from(move |token: f64, v: String| {
                node_op(&*s, token as u64, |dom, n| {
                    set_select_value(dom, n, &v);
                    s.touch();
                });
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeSetHtml",
            Func::from(move |token: f64, html: String| {
                node_op(&*s, token as u64, |dom, n| {
                    for child in dom.children(n).to_vec() {
                        dom.detach(n, child);
                    }
                    graft_fragment(dom, n, &html);
                    s.touch();
                });
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeClassOp",
            Func::from(move |token: f64, op: String, name: String| -> bool {
                let out = node_op(&*s, token as u64, |dom, n| class_op(dom, n, &op, &name))
                    .unwrap_or(false);
                if op != "contains" {
                    s.touch();
                }
                out
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeStyleSet",
            Func::from(move |token: f64, name: String, value: String| {
                node_op(&*s, token as u64, |dom, n| {
                    let mut props = inline_style(dom_attr(dom, n, "style").unwrap_or_default());
                    if value.is_empty() {
                        props.remove(name.to_ascii_lowercase().as_str());
                    } else {
                        props.insert(name.to_ascii_lowercase(), value);
                    }
                    let joined = props
                        .iter()
                        .map(|(k, v)| format!("{k}: {v}"))
                        .collect::<Vec<_>>()
                        .join("; ");
                    dom.set_attr(n, "style", &joined);
                    s.touch();
                });
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeSetTitle",
            Func::from(move |title: String| {
                let _ = use_dom(|dom| {
                    if let Some(root) = dom.root() {
                        if let Some(head) = find_child_tag(dom, root, "head") {
                            if let Some(t) = find_child_tag(dom, head, "title") {
                                // set_text is Text-node-only; an element's text
                                // lives in its children (see kitae_dom).
                                for child in dom.children(t).to_vec() {
                                    dom.detach(t, child);
                                }
                                if !title.is_empty() {
                                    let text = dom.create_text(title.clone());
                                    dom.append(t, text);
                                }
                                s.touch();
                            } else {
                                let el = dom.create_element("title", Vec::new());
                                let text = dom.create_text(title.clone());
                                dom.append(el, text);
                                dom.append(head, el);
                            }
                            s.touch();
                        }
                    }
                });
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeNav",
            Func::from(move |url: String| {
                *s.pending_nav.lock().unwrap() = Some(url);
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeDocWrite",
            Func::from(move |html: String| {
                let _ = use_dom(|dom| {
                    if let Some(root) = dom.root() {
                        if let Some(body) = find_child_tag(dom, root, "body") {
                            graft_fragment(dom, body, &html);
                            s.touch();
                        }
                    }
                });
            }),
        )?;
    }

    // ── timers & fetch plumbing ──
    {
        let s = shared.clone();
        globals.set(
            "__kitaeTimerSet",
            Func::from(move |id: f64, ms: f64, repeat: f64| {
                let ms = ms.max(0.0) as u64;
                s.timers.lock().unwrap().insert(
                    id as u64,
                    (
                        Instant::now() + Duration::from_millis(ms),
                        repeat != 0.0,
                        ms,
                    ),
                );
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeTimerClear",
            Func::from(move |id: Value| {
                // Only a numeric id can name a live timer; everything else
                // (null, undefined, junk) is a spec no-op.
                if let Some(id) = id.as_float() {
                    if id.is_finite() && id > 0.0 {
                        s.timers.lock().unwrap().remove(&(id as u64));
                    }
                } else if let Some(text) = id.as_string() {
                    let text = text.to_string().unwrap_or_default();
                    if let Ok(id) = text.trim().parse::<f64>() {
                        if id.is_finite() && id > 0.0 {
                            s.timers.lock().unwrap().remove(&(id as u64));
                        }
                    }
                }
            }),
        )?;
        let s = shared.clone();
        globals.set(
            "__kitaeFetchStart",
            Func::from(
                move |id: f64, url: String, method: String, headers_blob: String, body: String| {
                    let method = method.trim().to_ascii_uppercase();
                    let headers: Vec<(String, String)> = headers_blob
                        .lines()
                        .filter_map(|l| {
                            l.split_once(':')
                                .map(|(n, v)| (n.trim().to_string(), v.trim().to_string()))
                        })
                        .collect();
                    let has_body = !body.is_empty();
                    s.trace(format!(
                        "fetch-start id={id} url={url} method={method} headers={} body={}",
                        headers.len(),
                        body.len()
                    ));
                    // A plain GET with nothing extra keeps the document
                    // fetch path (response cache, conditional revalidation);
                    // anything with a method, headers, or a body rides the
                    // uncached Raw path — the same split as the engine's
                    // own form submits.
                    let plain_get =
                        (method.is_empty() || method == "GET") && headers.is_empty() && !has_body;
                    if !plain_get
                        && !matches!(
                            method.as_str(),
                            "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS"
                        )
                    {
                        // fetch() validates in JS; a request that still gets
                        // here is dropped loudly rather than guessed at.
                        s.trace(format!("fetch-start rejected method {method}"));
                        return;
                    }
                    let base = s.page_url.lock().unwrap().clone();
                    let absolute = kitae_net::resolve(&base, &url).unwrap_or(url);
                    let kind = if plain_get {
                        kitae_net::pool::FetchJobKind::CachedGet
                    } else {
                        kitae_net::pool::FetchJobKind::Raw {
                            method: if method.is_empty() {
                                "GET".to_string()
                            } else {
                                method
                            },
                            headers,
                            body: has_body.then(|| body.into_bytes()),
                        }
                    };
                    let job_id = kitae_net::pool::submit(&absolute, &base, kind);
                    s.inflight.lock().unwrap().push((job_id, id as u64));
                },
            ),
        )?;
    }

    // ── host-facing objects ──
    let navigator = kitae_script::rquickjs::Object::new(ctx.clone())?;
    navigator.set("userAgent", "Kitae/0.1 (QuickJS)")?;
    globals.set("navigator", navigator)?;

    let console = kitae_script::rquickjs::Object::new(ctx.clone())?;
    for (name, prefix) in [("log", ""), ("warn", "warn: "), ("error", "error: ")] {
        let s = shared.clone();
        let prefix = prefix.to_string();
        console.set(
            name,
            Func::from(move |args: kitae_script::rquickjs::function::Rest<Value>| {
                s.log(&format!("{}{}", prefix, join_values(&args)));
            }),
        )?;
    }
    globals.set("console", console)?;

    // Web storage (tier JS-6): the `Storage` objects themselves — Proxy
    // named properties included — are BOOTSTRAP JS over these calls. The
    // scope key is the document origin; `__kitaeSessionId` names this
    // browser and is empty for `localStorage`.
    globals.set(
        "__kitaeStorageGet",
        Func::from(|session: String, origin: String, key: String| {
            let map = web_storage().lock().unwrap();
            map.get(&storage_scope(&session, &origin))
                .and_then(|entries| entries.iter().find(|(k, _)| *k == key))
                .map(|(_, v)| v.clone())
        }),
    )?;
    globals.set(
        "__kitaeStorageSet",
        Func::from(
            |session: String, origin: String, key: String, value: String| {
                let mut map = web_storage().lock().unwrap();
                let entries = map.entry(storage_scope(&session, &origin)).or_default();
                match entries.iter_mut().find(|(k, _)| *k == key) {
                    Some((_, v)) => *v = value,
                    None => entries.push((key, value)),
                }
            },
        ),
    )?;
    globals.set(
        "__kitaeStorageRemove",
        Func::from(|session: String, origin: String, key: String| {
            let mut map = web_storage().lock().unwrap();
            if let Some(entries) = map.get_mut(&storage_scope(&session, &origin)) {
                entries.retain(|(k, _)| *k != key);
            }
        }),
    )?;
    globals.set(
        "__kitaeStorageClear",
        Func::from(|session: String, origin: String| {
            web_storage()
                .lock()
                .unwrap()
                .remove(&storage_scope(&session, &origin));
        }),
    )?;
    globals.set(
        "__kitaeStorageKey",
        Func::from(|session: String, origin: String, index: i32| {
            if index < 0 {
                return None;
            }
            let map = web_storage().lock().unwrap();
            map.get(&storage_scope(&session, &origin))
                .and_then(|entries| entries.get(index as usize))
                .map(|(k, _)| k.clone())
        }),
    )?;
    globals.set(
        "__kitaeStorageLength",
        Func::from(|session: String, origin: String| {
            let map = web_storage().lock().unwrap();
            map.get(&storage_scope(&session, &origin))
                .map(|entries| entries.len() as i32)
                .unwrap_or(0)
        }),
    )?;
    {
        let s = shared.clone();
        globals.set(
            "__kitaeSessionId",
            Func::from(move || s.session.to_string()),
        )?;
    }

    // Canvas (tier JS-6). The 2D context lives in JS (it owns the pixel
    // buffer); these are the two crossings: the flushed buffer registers
    // under the `\0canvas:{node}` sentinel layout emits, and `toDataURL`
    // encodes whatever is registered back into a PNG data URL. `drawImage`
    // of an `<img>` reuses the engine's own decode cache.
    {
        let s = shared.clone();
        globals.set(
            "__kitaeCanvasPublish",
            Func::from(move |token: f64, w: u32, h: u32, b64: String| {
                let Some(node) = s.node_for(token as u64) else {
                    return;
                };
                let Some(pixels) = crate::b64_decode(&b64) else {
                    return;
                };
                if w == 0 || h == 0 || pixels.len() != w as usize * h as usize * 4 {
                    return;
                }
                crate::qtsq_register_raster(&format!("\u{0}canvas:{}", node.0), w, h, pixels);
                s.touch();
            }),
        )?;
    }
    {
        let s = shared.clone();
        globals.set(
            "__kitaeCanvasDataUrl",
            Func::from(move |token: f64| {
                let node = s.node_for(token as u64)?;
                let raster = crate::qtsq_raster_for_url(&format!("\u{0}canvas:{}", node.0))?;
                let img = kitae_media::RgbaImage {
                    width: raster.width,
                    height: raster.height,
                    pixels: raster.pixels.as_ref().clone(),
                };
                let png = kitae_media::encode_png(&img).ok()?;
                Some(format!("data:image/png;base64,{}", crate::b64_encode(&png)))
            }),
        )?;
    }
    {
        let s = shared.clone();
        globals.set(
            "__kitaeImageRaster",
            Func::from(move |src: String| {
                if src.is_empty() {
                    return None;
                }
                let base = s.page_url.lock().unwrap().clone();
                let url = kitae_net::resolve(&base, &src)?;
                let raster =
                    crate::ensure_qtsq_raster(&url).or_else(|| crate::ensure_web_raster(&url))?;
                // A canvas pixel buffer is a JS-heap allocation on receipt:
                // cap what a single `drawImage` may pull across.
                if raster.width as usize * raster.height as usize * 4 > 8 * 1024 * 1024 {
                    return None;
                }
                Some(format!(
                    "{}|{}|{}",
                    raster.width,
                    raster.height,
                    crate::b64_encode(&raster.pixels)
                ))
            }),
        )?;
    }

    Ok(())
}

/// The page-API surface, in reviewable JavaScript. The Rust globals above
/// are the data plane (token ops); this is the shape scripts see.
const BOOTSTRAP: &str = r#"
var window = globalThis;
var self = globalThis;
// Kitae has no frames: the window is its own parent/top and has no opener.
window.parent = window;
window.top = window;
window.opener = null;
window.frames = window;
var __kitae = { seq: 0, timers: {}, res: {}, xhr: {}, onDoc: {}, onWin: {}, els: {}, canvases: {} };

var document = {
  readyState: "complete",
  getElementById: function (id) {
    var t = __kitaeById("" + id);
    return t == null ? null : __kitaeMakeElement(t);
  },
  createElement: function (t) {
    var tok = __kitaeCreateElement("" + t);
    return (tok == null || tok !== tok) ? null : __kitaeMakeElement(tok);
  },
  createElementNS: function (ns, t) {
    // Kitae's DOM has no foreign content; the local name is what exists.
    return document.createElement(t);
  },
  createTextNode: function (s) {
    var tok = __kitaeCreateText("" + s);
    return (tok == null || tok !== tok) ? null : __kitaeMakeElement(tok);
  },
  addEventListener: function (type, fn, opts) {
    if (typeof fn !== "function") return;
    var capture = opts === true || (opts && opts.capture);
    var list = (__kitae.onDoc["" + type] = __kitae.onDoc["" + type] || []);
    for (var i = 0; i < list.length; i++) {
      if (list[i].fn === fn && !!list[i].capture === capture) return;
    }
    list.push(__kitaeRecord(fn, opts));
  },
  removeEventListener: function (type, fn) {
    var list = __kitae.onDoc["" + type];
    if (!list) return;
    for (var i = 0; i < list.length; i++) {
      if (list[i].fn === fn) { list.splice(i, 1); return; }
    }
  },
  dispatchEvent: function (ev) {
    __kitaeEnsureEvent(ev);
    __kitaeDispatchDocTarget(ev);
    return !ev.defaultPrevented;
  },
  createEvent: function (kind) {
    var k = ("" + kind).toLowerCase();
    if (k === "event" || k === "events" || k === "htmlevents" || k === "svgevents") return new Event("", {});
    if (k === "customevent") return new CustomEvent("", {});
    if (k === "mouseevent" || k === "mouseevents") return new MouseEvent("", {});
    if (k === "keyboardevent" || k === "keyevents") return new KeyboardEvent("", {});
    throw new DOMException("createEvent: unsupported event interface '" + kind + "'", "NotSupportedError");
  },
  getSelection: function () { return window.getSelection(); },
  write: function (s) { __kitaeDocWrite("" + s); },
  writeln: function (s) { __kitaeDocWrite("" + s + "\n"); },
  querySelector: function (sel) {
    var ts = __kitaeQueryAll("" + sel);
    return ts.length ? __kitaeMakeElement(ts[0]) : null;
  },
  querySelectorAll: function (sel) {
    var ts = __kitaeQueryAll("" + sel), out = [];
    for (var i = 0; i < ts.length; i++) out.push(__kitaeMakeElement(ts[i]));
    return out;
  },
  getElementsByTagName: function (t) {
    var ts = __kitaeTagTokens("" + t), out = [];
    for (var i = 0; i < ts.length; i++) out.push(__kitaeMakeElement(ts[i]));
    return out;
  },
  getElementsByClassName: function (cls) {
    var ts = __kitaeQueryAll("." + cls), out = [];
    for (var i = 0; i < ts.length; i++) out.push(__kitaeMakeElement(ts[i]));
    return out;
  },
};
Object.defineProperty(document, "title", {
  get: function () { return __kitaeGetTitle(); },
  set: function (v) { __kitaeSetTitle("" + v); },
});
function __kitaeWrapTag(tag) {
  var t = __kitaeTagTokens(tag);
  return t.length ? __kitaeMakeElement(t[0]) : null;
}
Object.defineProperty(document, "body", { get: function () { return __kitaeWrapTag("body"); } });
Object.defineProperty(document, "documentElement", { get: function () { return __kitaeWrapTag("html"); } });
Object.defineProperty(document, "head", { get: function () { return __kitaeWrapTag("head"); } });
Object.defineProperty(document, "activeElement", {
  get: function () {
    var t = __kitaeActiveToken();
    if (t == null || t !== t) return __kitaeWrapTag("body");
    return __kitaeMakeElement(t);
  },
});
Object.defineProperty(document, "scrollingElement", {
  get: function () { return __kitaeWrapTag("html"); },
});
// getComputedStyle: the published computed subset, with used width/height
// layered in from geometry for painted boxes (a percentage width answers
// in px, like a real used value).
function getComputedStyle(el, pseudo) {
  var token = el && el.__token;
  var text = token === undefined ? "" : __kitaeComputedStyle(token);
  var props = {};
  if (text) {
    var decls = text.split(";");
    for (var i = 0; i < decls.length; i++) {
      var idx = decls[i].indexOf(":");
      if (idx > 0) props[decls[i].slice(0, idx).trim()] = decls[i].slice(idx + 1).trim();
    }
  }
  var api = {
    getPropertyValue: function (name) {
      name = __kitaeKebab("" + name).toLowerCase();
      if (token !== undefined && (name === "width" || name === "height")) {
        var r = __kitaeRect(token);
        var used = name === "width" ? r[2] : r[3];
        if (used > 0) return Math.round(used * 100) / 100 + "px";
      }
      return props[name] === undefined ? "" : props[name];
    },
    getPropertyPriority: function () { return ""; },
    cssText: text,
  };
  return new Proxy(api, {
    get: function (t, p) {
      if (typeof p === "string" && t[p] === undefined) {
        return t.getPropertyValue(p);
      }
      return t[p];
    },
  });
}
var location = {
  get href() { return __kitaePageUrl(); },
  set href(v) { __kitaeNav("" + v); },
  get hash() { return __kitaeHash(); },
  set hash(v) { __kitaeSetHash("" + v); },
  reload: function () { __kitaeNav("kitae:reload"); },
};
document.location = location;
window.location = location;

function __kitaeKebab(name) {
  return ("" + name).replace(/[A-Z]/g, function (m) { return "-" + m.toLowerCase(); });
}
function __kitaeCamel(name) {
  return ("" + name).replace(/-([a-z])/g, function (_, c) { return c.toUpperCase(); });
}
function __kitaeStyleFor(token) {
  return new Proxy({}, {
    get: function (t, p) {
      if (p === "setProperty") return function (k, v) {
        var old = __kitae.observers.length ? __kitaeGetStyleText(token) : null;
        __kitaeStyleSet(token, __kitaeKebab(k), "" + v);
        if (__kitae.observers.length) __kitaeMutateAttr(token, "style", old);
      };
      if (p === "getPropertyValue") return function (k) { return __kitaeGetStyle(token, __kitaeKebab(k)); };
      if (p === "removeProperty") return function (k) {
        var old = __kitae.observers.length ? __kitaeGetStyleText(token) : null;
        __kitaeStyleSet(token, __kitaeKebab(k), "");
        if (__kitae.observers.length) __kitaeMutateAttr(token, "style", old);
      };
      if (p === "cssText") return __kitaeGetStyleText(token);
      if (typeof p === "string") return __kitaeGetStyle(token, __kitaeKebab(p));
      return undefined;
    },
    set: function (t, p, v) {
      if (p === "cssText") {
        var old = __kitae.observers.length ? __kitaeGetStyleText(token) : null;
        __kitaeStyleText(token, "" + v);
        if (__kitae.observers.length) __kitaeMutateAttr(token, "style", old);
        return true;
      }
      if (typeof p !== "string") return true;
      var old2 = __kitae.observers.length ? __kitaeGetStyleText(token) : null;
      __kitaeStyleSet(token, __kitaeKebab(p), v == null ? "" : "" + v);
      if (__kitae.observers.length) __kitaeMutateAttr(token, "style", old2);
      return true;
    },
  });
}
// insertAdjacent*: the four positions, delegating to the same nav ops the
// spec defines them in terms of.
function __kitaeInsertRel(node, pos, nodes) {
  if (!node || !node.__token || !nodes || !nodes.length) return;
  var i;
  if (pos === "afterbegin") {
    var first = node.firstChild;
    for (i = 0; i < nodes.length; i++) node.insertBefore(nodes[i], first);
  } else if (pos === "beforeend") {
    for (i = 0; i < nodes.length; i++) node.appendChild(nodes[i]);
  } else if (pos === "beforebegin" || pos === "afterend") {
    var p = node.parentNode;
    if (!p) return;
    var ref = pos === "beforebegin" ? node : node.nextSibling;
    for (i = 0; i < nodes.length; i++) p.insertBefore(nodes[i], ref);
  }
}
function __kitaeSibling(token, dir) {
  var p = __kitaeParent(token);
  if (p == null) return null;
  var kids = __kitaeChildTokens(p);
  for (var i = 0; i < kids.length; i++) {
    if (kids[i] === token) {
      var j = i + dir;
      return (j >= 0 && j < kids.length) ? kids[j] : null;
    }
  }
  return null;
}
function __kitaeMakeElement(token) {
  if (__kitae.els[token]) return __kitae.els[token];
  // Cache FIRST: construction itself walks parents/children, and those
  // lookups recurse through this factory.
  var el = { __token: token };
  __kitae.els[token] = el;
  el.tagName = __kitaeGetTag(token).toUpperCase();
  // Kitae's DOM has elements and text nodes; text nodes report "" as
  // their tag, which is exactly how the wrapper tells them apart.
  el.nodeType = el.tagName === "" ? 3 : 1;
  Object.defineProperty(el, "id", {
    configurable: true,
    get: function () { return __kitaeGetAttr(token, "id"); },
    set: function (v) {
      if (__kitae.observers.length) __kitaeMutateAttr(token, "id", __kitaeGetAttr(token, "id"));
      __kitaeSetAttr(token, "id", v == null ? "" : "" + v);
    },
  });
  Object.defineProperty(el, "className", {
    configurable: true,
    get: function () { return __kitaeGetAttr(token, "class"); },
    set: function (v) {
      if (__kitae.observers.length) __kitaeMutateAttr(token, "class", __kitaeGetAttr(token, "class"));
      __kitaeSetAttr(token, "class", v == null ? "" : "" + v);
    },
  });
  el.getAttribute = function (n) { return __kitaeGetAttr(token, "" + n); };
  el.setAttribute = function (n, v) {
    n = "" + n;
    if (__kitae.observers.length) __kitaeMutateAttr(token, n, __kitaeHasAttr(token, n) ? __kitaeGetAttr(token, n) : null);
    __kitaeSetAttr(token, n, "" + v);
    if (el.tagName === "CANVAS" && (n === "width" || n === "height")) __kitaeCanvasReset(token);
  };
  el.removeAttribute = function (n) {
    n = "" + n;
    if (__kitae.observers.length && __kitaeHasAttr(token, n)) __kitaeMutateAttr(token, n, __kitaeGetAttr(token, n));
    __kitaeRemoveAttr(token, n);
  };
  el.hasAttribute = function (n) { return __kitaeHasAttr(token, "" + n); };
  el.appendChild = function (c) {
    if (c && c.__token) {
      __kitaeAppend(token, c.__token);
      __kitaeMutate("childList", token, { added: [c.__token] });
    }
    return c;
  };
  el.insertBefore = function (c, before) {
    if (c && c.__token) {
      var oldParent = c.parentNode;
      if (before && before.__token) __kitaeInsertBefore(token, before.__token, c.__token);
      else __kitaeAppend(token, c.__token); // spec: a null ref appends
      if (oldParent && oldParent.__token && oldParent.__token !== token) {
        __kitaeMutate("childList", oldParent.__token, { removed: [c.__token] });
      }
      __kitaeMutate("childList", token, { added: [c.__token] });
    }
    return c;
  };
  el.insertAdjacentText = function (pos, text) {
    __kitaeInsertRel(el, "" + pos, [document.createTextNode("" + text)]);
  };
  el.insertAdjacentHTML = function (pos, html) {
    var tmp = __kitaeMakeElement(__kitaeCreateElement("div"));
    tmp.innerHTML = "" + html;
    __kitaeInsertRel(el, "" + pos, tmp.childNodes.slice(0));
  };
  el.removeChild = function (c) {
    if (c && c.__token) {
      __kitaeRemoveChild(token, c.__token);
      __kitaeMutate("childList", token, { removed: [c.__token] });
    }
    return c;
  };
  el.remove = function () {
    var parent = el.parentNode;
    __kitaeDetach(token);
    if (parent && parent.__token) __kitaeMutate("childList", parent.__token, { removed: [token] });
  };
  // Navigation is live DOM state: a wrapper made before an appendChild
  // must still report the new children when read back.
  Object.defineProperty(el, "children", {
    configurable: true,
    get: function () {
      var out = [], toks = __kitaeChildTokens(token);
      for (var i = 0; i < toks.length; i++) out.push(__kitaeMakeElement(toks[i]));
      return out;
    },
  });
  Object.defineProperty(el, "childNodes", {
    configurable: true,
    get: function () { return el.children; },
  });
  Object.defineProperty(el, "firstChild", {
    configurable: true,
    get: function () {
      var toks = __kitaeChildTokens(token);
      return toks.length ? __kitaeMakeElement(toks[0]) : null;
    },
  });
  Object.defineProperty(el, "lastChild", {
    configurable: true,
    get: function () {
      var toks = __kitaeChildTokens(token);
      return toks.length ? __kitaeMakeElement(toks[toks.length - 1]) : null;
    },
  });
  Object.defineProperty(el, "parentElement", {
    configurable: true,
    get: function () {
      var p = __kitaeParent(token);
      return p == null ? null : __kitaeMakeElement(p);
    },
  });
  Object.defineProperty(el, "parentNode", {
    configurable: true,
    get: function () { return el.parentElement; },
  });
  Object.defineProperty(el, "nextSibling", {
    configurable: true,
    get: function () {
      var t = __kitaeSibling(token, 1);
      return t == null ? null : __kitaeMakeElement(t);
    },
  });
  Object.defineProperty(el, "previousSibling", {
    configurable: true,
    get: function () {
      var t = __kitaeSibling(token, -1);
      return t == null ? null : __kitaeMakeElement(t);
    },
  });
  el.classList = {
    add: function (n) {
      n = "" + n;
      var before = __kitaeGetAttr(token, "class");
      if (__kitae.observers.length && (" " + before + " ").indexOf(" " + n + " ") < 0) __kitaeMutateAttr(token, "class", before);
      __kitaeClassOp(token, "add", n);
    },
    remove: function (n) {
      n = "" + n;
      var before = __kitaeGetAttr(token, "class");
      if (__kitae.observers.length && (" " + before + " ").indexOf(" " + n + " ") >= 0) __kitaeMutateAttr(token, "class", before);
      __kitaeClassOp(token, "remove", n);
    },
    toggle: function (n) {
      n = "" + n;
      var before = __kitaeGetAttr(token, "class");
      var present = (" " + before + " ").indexOf(" " + n + " ") >= 0;
      if (__kitae.observers.length) __kitaeMutateAttr(token, "class", before);
      return __kitaeClassOp(token, present ? "remove" : "add", n);
    },
    contains: function (n) { return __kitaeClassOp(token, "contains", "" + n); },
  };
  el.querySelector = function (sel) {
    var ts = __kitaeQueryAllIn(token, "" + sel);
    return ts.length ? __kitaeMakeElement(ts[0]) : null;
  };
  el.querySelectorAll = function (sel) {
    var ts = __kitaeQueryAllIn(token, "" + sel), out = [];
    for (var i = 0; i < ts.length; i++) out.push(__kitaeMakeElement(ts[i]));
    return out;
  };
  el.closest = function (sel) {
    var t = __kitaeClosest(token, "" + sel);
    return (t == null || t !== t) ? null : __kitaeMakeElement(t);
  };
  el.style = __kitaeStyleFor(token);
  // textContent on an element replaces its children (childList records);
  // on a text node (empty tag) it is a characterData record.
  function __kitaeSetTextJournaled(v) {
    v = "" + v;
    if (__kitae.observers.length) {
      if (el.tagName === "") {
        __kitaeMutate("characterData", token, { oldValue: __kitaeGetText(token) });
        __kitaeSetText(token, v);
        return;
      }
      var before = __kitaeChildList(token);
      __kitaeSetText(token, v);
      __kitaeMutate("childList", token, { removed: before, added: __kitaeChildList(token) });
      return;
    }
    __kitaeSetText(token, v);
  }
  Object.defineProperty(el, "textContent", {
    get: function () { return __kitaeGetText(token); },
    set: __kitaeSetTextJournaled,
  });
  Object.defineProperty(el, "data", {
    configurable: true,
    get: function () { return __kitaeGetText(token); },
    set: __kitaeSetTextJournaled,
  });
  Object.defineProperty(el, "nodeValue", {
    configurable: true,
    get: function () { return __kitaeGetText(token); },
    set: __kitaeSetTextJournaled,
  });
  Object.defineProperty(el, "innerHTML", {
    get: function () { return __kitaeOuterHtml(token); },
    set: function (v) {
      v = "" + v;
      if (__kitae.observers.length) {
        var before = __kitaeChildList(token);
        __kitaeSetHtml(token, v);
        __kitaeMutate("childList", token, { removed: before, added: __kitaeChildList(token) });
        return;
      }
      __kitaeSetHtml(token, v);
    },
  });
  Object.defineProperty(el, "value", {
    get: function () {
      if (el.tagName === "SELECT") return __kitaeSelectValue(token);
      return __kitaeGetAttr(token, "value");
    },
    set: function (v) {
      if (el.tagName === "SELECT") { __kitaeSetSelectValue(token, "" + v); return; }
      __kitaeSetAttr(token, "value", "" + v);
    },
  });
  Object.defineProperty(el, "checked", {
    get: function () { return __kitaeHasAttr(token, "checked"); },
    set: function (v) {
      if (v) __kitaeSetAttr(token, "checked", "");
      else __kitaeRemoveAttr(token, "checked");
    },
  });
  Object.defineProperty(el, "onclick", {
    set: function (f) { if (typeof f === "function") __kitaeElOn(token, "click", f); },
    get: function () { return undefined; },
  });
  el.addEventListener = function (type, fn, opts) {
    if (typeof fn !== "function") return;
    var capture = opts === true || (opts && opts.capture);
    if (capture) {
      // Capture records live on the wrapper (see __kitaeDispatch's walk).
      if (!el.__capture) el.__capture = {};
      if (!el.__capture[type]) el.__capture[type] = [];
      var list = el.__capture[type];
      for (var i = 0; i < list.length; i++) if (list[i].fn === fn) return;
      list.push(__kitaeRecord(fn, opts));
    } else {
      __kitaeElOn(token, "" + type, fn, opts);
    }
  };
  el.removeEventListener = function (type, fn, opts) {
    var capture = opts === true || (opts && opts.capture);
    __kitaeElOff(token, "" + type, fn, capture);
  };
  el.dispatchEvent = function (ev) {
    __kitaeEnsureEvent(ev);
    __kitaeDispatch("" + ev.type, token, ev);
    return !ev.defaultPrevented;
  };
  el.focus = function () { __kitaeFocusToken(token); };
  el.blur = function () { __kitaeBlur(); };
  el.scrollIntoView = function () { __kitaeScrollIntoView(token); };
  Object.defineProperty(el, "dataset", {
    get: function () {
      if (!el.__dataset) {
        el.__dataset = new Proxy({}, {
          get: function (_, prop) {
            if (typeof prop !== "string") return undefined;
            var v = __kitaeGetAttr(token, "data-" + __kitaeKebab(prop));
            return v === "" ? undefined : v;
          },
          set: function (_, prop, value) {
            __kitaeSetAttr(token, "data-" + __kitaeKebab("" + prop), "" + value);
            return true;
          },
          has: function (_, prop) {
            return __kitaeHasAttr(token, "data-" + __kitaeKebab("" + prop));
          },
          deleteProperty: function (_, prop) {
            __kitaeRemoveAttr(token, "data-" + __kitaeKebab("" + prop));
            return true;
          },
          ownKeys: function () {
            var names = __kitaeAttrNames(token), out = [];
            for (var i = 0; i < names.length; i++) {
              if (names[i].indexOf("data-") === 0) out.push(__kitaeCamel(names[i].slice(5)));
            }
            return out;
          },
          getOwnPropertyDescriptor: function (_, prop) {
            if (__kitaeHasAttr(token, "data-" + __kitaeKebab("" + prop))) {
              return { enumerable: true, configurable: true };
            }
            return undefined;
          },
        });
      }
      return el.__dataset;
    },
  });
  if (el.tagName === "CANVAS") __kitaeCanvasElement(token, el);
  el.getBoundingClientRect = function () {
    var r = __kitaeRect(token);
    return {
      x: r[0], y: r[1], width: r[2], height: r[3],
      left: r[0], top: r[1], right: r[0] + r[2], bottom: r[1] + r[3],
    };
  };
  Object.defineProperty(el, "offsetWidth", { get: function () { return Math.round(__kitaeRect(token)[2]); } });
  Object.defineProperty(el, "offsetHeight", { get: function () { return Math.round(__kitaeRect(token)[3]); } });
  // The engine scrolls the document vertically only: every element's
  // scrollTop reads/writes the page scroll (the documentElement is the
  // one that matters to scripts); nested scrolling is a documented gap.
  Object.defineProperty(el, "scrollTop", {
    get: function () { return __kitaeScrollPos(1); },
    set: function (v) { __kitaeScrollTo(0, +v || 0); },
  });
  Object.defineProperty(el, "scrollLeft", {
    get: function () { return 0; },
    set: function () {},
  });
  return el;
}
// Listener records: {fn, once, signal}. Capture records live under
// el.__capture, bubble records under el.__listeners (see the wrapper's
// addEventListener), document/window delegation under __kitae.onDoc.
function __kitaeRecord(fn, opts) {
  opts = opts || {};
  return {
    fn: fn,
    once: !!(opts === true ? false : opts.once),
    passive: !!(opts === true ? false : opts.passive),
    capture: opts === true ? true : !!(opts && opts.capture),
    signal: (opts === true ? null : opts.signal) || null,
  };
}
__kitaeElOn = function (token, type, fn, opts) {
  var w = __kitae.els[token];
  if (!w) return;
  if (!w.__listeners) w.__listeners = {};
  if (!w.__listeners[type]) w.__listeners[type] = [];
  var list = w.__listeners[type];
  // Spec: re-adding the identical listener (same callback, same capture
  // flag) is a no-op.
  for (var i = 0; i < list.length; i++) if (list[i].fn === fn) return;
  list.push(__kitaeRecord(fn, opts));
};
__kitaeElOff = function (token, type, fn, capture) {
  var w = __kitae.els[token];
  if (!w) return;
  var list = capture
    ? (w.__capture && w.__capture[type])
    : (w.__listeners && w.__listeners[type]);
  if (!list) return;
  for (var i = 0; i < list.length; i++) {
    if (list[i].fn === fn) { list.splice(i, 1); return; }
  }
};
__kitaeFireDoc = function (type, event) {
  var fired = 0;
  var list = __kitae.onDoc[type];
  if (list) {
    event.currentTarget = document;
    event.eventPhase = Event.AT_TARGET;
    for (var i = 0; i < list.length; i++) {
      var rec = list[i];
      if (event.__immediateStopped) return fired;
      if (rec.signal && rec.signal.aborted) { list.splice(i, 1); i--; continue; }
      if (rec.once) { list.splice(i, 1); i--; }
      try { rec.fn.call(document, event); fired++; }
      catch (e) { console.error("listener", type, e); }
    }
  }
  // The engine's document-targeted events (load, DOMContentLoaded,
  // popstate) also reach window-level listeners, as in the browser.
  if (!event.__propStopped) fired += __kitaeFireWin(type, event, Event.BUBBLING_PHASE);
  event.currentTarget = null;
  return fired;
};
// Window-level listeners live in their own map: spec keeps document and
// window as distinct targets in the propagation path, so `currentTarget`
// differs between them.
__kitaeFireWin = function (type, event, phase) {
  var fired = 0;
  var list = __kitae.onWin[type];
  if (list) {
    event.currentTarget = window;
    event.eventPhase = phase === undefined ? Event.BUBBLING_PHASE : phase;
    for (var i = 0; i < list.length; i++) {
      var rec = list[i];
      if (event.__immediateStopped) return fired;
      if (rec.signal && rec.signal.aborted) { list.splice(i, 1); i--; continue; }
      if (rec.once) { list.splice(i, 1); i--; }
      try { rec.fn.call(window, event); fired++; }
      catch (e) { console.error("listener", type, e); }
    }
  }
  return fired;
};
// An event dispatched at document: document listeners are the target,
// window listeners are the single ancestor. An event dispatched at
// window never reaches document listeners (the path is just [window]).
__kitaeDispatchDocTarget = function (event) {
  event.__propStopped = false;
  event.__immediateStopped = false;
  event.target = document;
  event.srcElement = document;
  event.eventPhase = Event.AT_TARGET;
  var fired = __kitaeFireDoc("" + event.type, event);
  event.currentTarget = null;
  return fired;
};
__kitaeEnsureEvent = function (ev) {
  if (!ev || typeof ev !== "object" || typeof ev.type !== "string" ||
      typeof ev.preventDefault !== "function" || typeof ev.stopPropagation !== "function") {
    throw new TypeError("Failed to execute 'dispatchEvent': parameter 1 is not of type 'Event'.");
  }
};
__kitaeFireEl = function (type, token, event) {
  var fired = 0;
  var w = __kitae.els[token];
  if (w && w.__listeners && w.__listeners[type]) {
    var list = w.__listeners[type];
    event.target = w;
    event.currentTarget = w;
    for (var i = 0; i < list.length; i++) {
      var rec = list[i];
      if (rec.signal && rec.signal.aborted) { list.splice(i, 1); i--; continue; }
      if (rec.once) { list.splice(i, 1); i--; continue; }
      try { rec.fn.call(w, event); fired++; }
      catch (e) { console.error("listener", type, e); }
    }
  }
  return fired;
};

// ── URL / URLSearchParams ────────────────────────────────────────────
function __kitaeUrlParts(href) {
  var m = /^([A-Za-z][A-Za-z0-9+.-]*):(\/\/([^\/?#]*))?([^?#]*)(\?[^#]*)?(#.*)?$/.exec(href);
  if (!m) return null;
  var authority = m[3] || "";
  var userinfo = "", hostport = authority;
  var at = authority.lastIndexOf("@");
  if (at >= 0) { userinfo = authority.slice(0, at); hostport = authority.slice(at + 1); }
  var user = "", pass = "";
  var colon = userinfo.indexOf(":");
  if (colon >= 0) { user = userinfo.slice(0, colon); pass = userinfo.slice(colon + 1); }
  else user = userinfo;
  var host = hostport, port = "";
  var pc = hostport.lastIndexOf(":");
  if (pc >= 0 && hostport.indexOf("]") < pc) { host = hostport.slice(0, pc); port = hostport.slice(pc + 1); }
  return {
    protocol: m[1].toLowerCase() + ":",
    hasAuthority: !!m[2],
    username: user, password: pass,
    hostname: host, port: port,
    pathname: m[4] || "", search: m[5] || "", hash: m[6] || "",
  };
}
// The URL parser works on scalar values: lone surrogates become U+FFFD
// (spec "UTF-8 encode" step). Our Rust-side resolver takes real UTF-8, so
// the replacement happens here for every string that crosses it.
function __kitaeScalarString(s) {
  s = "" + s;
  var out = "";
  for (var i = 0; i < s.length; i++) {
    var c = s.charCodeAt(i);
    if (c >= 0xD800 && c <= 0xDBFF) {
      var d = i + 1 < s.length ? s.charCodeAt(i + 1) : 0;
      if (d >= 0xDC00 && d <= 0xDFFF) { out += s.charAt(i) + s.charAt(i + 1); i++; continue; }
      out += "\uFFFD";
    } else if (c >= 0xDC00 && c <= 0xDFFF) {
      out += "\uFFFD";
    } else {
      out += s.charAt(i);
    }
  }
  return out;
}
function URL(input, base) {
  if (!(this instanceof URL)) throw new TypeError("Class constructor URL cannot be invoked without 'new'");
  input = __kitaeScalarString(input);
  var baseHref = base === undefined || base === null ? "" : __kitaeScalarString(base);
  // Spec "basic URL parser" preprocessing: strip ASCII tab/newline
  // anywhere, then leading/trailing C0 controls and spaces.
  input = input.replace(/[\t\n\r]/g, "").replace(/^[\u0000-\u0020]+|[\u0000-\u0020]+$/g, "");
  var href;
  if (input === "") {
    // An empty input parses to the base minus its fragment.
    if (baseHref === "") throw new TypeError("Invalid URL");
    href = baseHref.split('#')[0];
    var emptyParts = __kitaeUrlParts(href);
    if (!emptyParts) throw new TypeError("Invalid URL");
    this.__p = emptyParts;
  } else {
    // Resolution base selection. With no base, only scheme-absolute input
    // parses; the sentinel keeps the resolver in absolute mode without
    // touching the loaded document (what the old page fallback did).
    var effBase = baseHref;
    if (effBase === "") {
      if (!/^[A-Za-z][A-Za-z0-9+.-]*:/.test(input)) throw new TypeError("Invalid URL");
      effBase = "kitae:no-base";
    }
    // A single slash after a special scheme means the authority still
    // follows ("http:/host" = "http://host", per the URL parser's
    // special-authority states); file: is excluded — its one-slash form
    // keeps an empty host.
    var oneSlash = /^(https?|wss?|ftp):[\/\\]([^\/\\]|$)/i.test(input);
    href = oneSlash
      ? __kitaeResolveUrlStrict(effBase, input.replace(/^([A-Za-z][A-Za-z0-9+.-]*):[\/\\]/, "$1://"))
      : __kitaeResolveUrlStrict(effBase, input);
    if (href == null && oneSlash) href = __kitaeResolveUrlStrict(effBase, input);
    if (href == null) throw new TypeError("Invalid URL");
    var p = __kitaeUrlParts(href);
    if (!p) throw new TypeError("Invalid URL");
    // Validation the lenient resolver leaves to the caller: ports are
    // digits within range; hostnames carry no forbidden characters; a
    // special-scheme host is ASCII-lowercased (IDNA canonical form).
    if (p.port !== "" && (!/^[0-9]+$/.test(p.port) || +p.port > 65535)) throw new TypeError("Invalid URL");
    if (/[\u0000-\u0020#/?@\\^|<>%]/.test(p.hostname)) throw new TypeError("Invalid URL");
    if (p.protocol === "http:" || p.protocol === "https:" || p.protocol === "ws:" || p.protocol === "wss:" || p.protocol === "ftp:") {
      p.hostname = p.hostname.toLowerCase();
    }
    this.__p = p;
  }
  this.__href = href;
  this.__params = null;
}
URL.prototype.__rebuild = function () {
  var p = this.__p, s = p.protocol;
  if (p.hasAuthority) {
    s += "//";
    if (p.username) s += p.username + (p.password ? ":" + p.password : "") + "@";
    s += p.hostname + (p.port ? ":" + p.port : "");
  }
  s += p.pathname + p.search + p.hash;
  this.__href = s;
  if (this.__params) this.__params = null;
};
Object.defineProperty(URL.prototype, "href", {
  get: function () { return this.__href; },
  set: function (v) {
    var href = __kitaeResolveUrl(this.__href, __kitaeScalarString(v));
    var p = __kitaeUrlParts(href);
    if (!p) throw new TypeError("Invalid URL");
    this.__p = p; this.__href = href; this.__params = null;
  },
});
["protocol", "username", "password", "hostname", "port", "pathname", "search", "hash"].forEach(function (name) {
  Object.defineProperty(URL.prototype, name, {
    get: function () { return this.__p[name]; },
    set: function (v) {
      var val = "" + v;
      if (name === "search" && val && val[0] !== "?") val = "?" + val;
      if (name === "hash" && val && val[0] !== '#') val = '#' + val;
      this.__p[name] = val;
      this.__rebuild();
    },
  });
});
Object.defineProperty(URL.prototype, "host", {
  get: function () { return this.__p.hostname + (this.__p.port ? ":" + this.__p.port : ""); },
  set: function (v) {
    var i = ("" + v).lastIndexOf(":");
    if (i >= 0) { this.__p.hostname = ("" + v).slice(0, i); this.__p.port = ("" + v).slice(i + 1); }
    else { this.__p.hostname = "" + v; this.__p.port = ""; }
    this.__rebuild();
  },
});
Object.defineProperty(URL.prototype, "origin", {
  get: function () {
    // Spec: only special schemes have a tuple origin; everything else
    // (non-special, file) is an opaque origin serialized as "null".
    var scheme = this.__p.protocol;
    if (scheme !== "http:" && scheme !== "https:" && scheme !== "ws:" && scheme !== "wss:" && scheme !== "ftp:") {
      return "null";
    }
    return scheme + "//" + this.host;
  },
});
Object.defineProperty(URL.prototype, "searchParams", {
  get: function () {
    if (!this.__params) {
      var self = this;
      var params = new URLSearchParams(this.__p.search);
      // `__rebuild` clears `self.__params` (the URL's component string
      // changed) — the callback must keep its own handle, not re-read the
      // field, or the second mutation in a row throws on null.
      params.__onChange = function () {
        var s = params.toString();
        self.__p.search = s ? "?" + s : "";
        self.__rebuild();
      };
      this.__params = params;
    }
    return this.__params;
  },
});
URL.prototype.toString = function () { return this.__href; };
URL.prototype.toJSON = function () { return this.__href; };
function URLSearchParams(init) {
  if (!(this instanceof URLSearchParams)) throw new TypeError("Class constructor URLSearchParams cannot be invoked without 'new'");
  this.__items = [];
  this.__onChange = null;
  var s = null;
  if (init === undefined || init === null) s = "";
  else if (typeof init === "string") s = init[0] === "?" ? init.slice(1) : init;
  else if (Array.isArray(init)) {
    for (var i = 0; i < init.length; i++) this.__items.push(["" + init[i][0], "" + init[i][1]]);
    return;
  } else if (typeof init === "object") {
    var ks = Object.keys(init);
    for (var i = 0; i < ks.length; i++) this.__items.push(["" + ks[i], "" + init[ks[i]]]);
    return;
  }
  if (s) {
    var parts = s.split("&");
    for (var i = 0; i < parts.length; i++) {
      if (!parts[i]) continue;
      var eq = parts[i].indexOf("=");
      var k = eq < 0 ? parts[i] : parts[i].slice(0, eq);
      var v = eq < 0 ? "" : parts[i].slice(eq + 1);
      try { k = decodeURIComponent(k.replace(/\+/g, " ")); } catch (e) {}
      try { v = decodeURIComponent(v.replace(/\+/g, " ")); } catch (e) {}
      this.__items.push([k, v]);
    }
  }
}
URLSearchParams.prototype.__changed = function () { if (this.__onChange) this.__onChange(); };
URLSearchParams.prototype.append = function (k, v) { this.__items.push(["" + k, "" + v]); this.__changed(); };
URLSearchParams.prototype.set = function (k, v) {
  k = "" + k;
  var found = false;
  for (var i = this.__items.length - 1; i >= 0; i--) {
    if (this.__items[i][0] === k) {
      if (found) this.__items.splice(i, 1);
      else { this.__items[i][1] = "" + v; found = true; }
    }
  }
  if (!found) this.__items.push([k, "" + v]);
  this.__changed();
};
URLSearchParams.prototype.get = function (k) {
  k = "" + k;
  for (var i = 0; i < this.__items.length; i++) if (this.__items[i][0] === k) return this.__items[i][1];
  return null;
};
URLSearchParams.prototype.getAll = function (k) {
  k = "" + k;
  var out = [];
  for (var i = 0; i < this.__items.length; i++) if (this.__items[i][0] === k) out.push(this.__items[i][1]);
  return out;
};
URLSearchParams.prototype.has = function (k) { return this.get(k) !== null; };
URLSearchParams.prototype.delete = function (k) {
  k = "" + k;
  for (var i = this.__items.length - 1; i >= 0; i--) if (this.__items[i][0] === k) this.__items.splice(i, 1);
  this.__changed();
};
URLSearchParams.prototype.sort = function () {
  this.__items.sort(function (a, b) { return a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0; });
  this.__changed();
};
URLSearchParams.prototype.forEach = function (fn, thisArg) {
  for (var i = 0; i < this.__items.length; i++) fn.call(thisArg, this.__items[i][1], this.__items[i][0], this);
};
function __kitaeIterable(items) {
  var i = 0;
  return {
    next: function () {
      return i < items.length
        ? { value: items[i++], done: false }
        : { value: undefined, done: true };
    },
  };
}
URLSearchParams.prototype.entries = function () {
  return __kitaeIterable(this.__items.map(function (p) { return p.slice(); }));
};
URLSearchParams.prototype.keys = function () {
  return __kitaeIterable(this.__items.map(function (p) { return p[0]; }));
};
URLSearchParams.prototype.values = function () {
  return __kitaeIterable(this.__items.map(function (p) { return p[1]; }));
};
URLSearchParams.prototype[Symbol.iterator] = URLSearchParams.prototype.entries;
URLSearchParams.prototype.toString = function () {
  var parts = [];
  for (var i = 0; i < this.__items.length; i++) {
    parts.push(
      encodeURIComponent(this.__items[i][0]).replace(/%20/g, "+") + "=" +
      encodeURIComponent(this.__items[i][1]).replace(/%20/g, "+")
    );
  }
  return parts.join("&");
};
// ── Headers / FormData / Response / Request ──────────────────────────
function Headers(init) {
  if (!(this instanceof Headers)) throw new TypeError("Class constructor Headers cannot be invoked without 'new'");
  this.__list = [];
  if (init instanceof Headers) this.__list = init.__list.map(function (p) { return p.slice(); });
  else if (Array.isArray(init)) { for (var i = 0; i < init.length; i++) this.append(init[i][0], init[i][1]); }
  else if (init && typeof init === "object") {
    var ks = Object.keys(init);
    for (var i = 0; i < ks.length; i++) this.append(ks[i], init[ks[i]]);
  }
}
Headers.prototype.__find = function (name) {
  name = ("" + name).toLowerCase();
  for (var i = 0; i < this.__list.length; i++) if (this.__list[i][0] === name) return i;
  return -1;
};
Headers.prototype.append = function (name, value) {
  name = ("" + name).toLowerCase();
  value = "" + value;
  if (/[\r\n]/.test(name) || /[\r\n]/.test(value)) throw new TypeError("Invalid header value");
  var i = this.__find(name);
  if (i >= 0) this.__list[i][1] += ", " + value;
  else this.__list.push([name, value]);
};
Headers.prototype.set = function (name, value) {
  name = ("" + name).toLowerCase();
  value = "" + value;
  if (/[\r\n]/.test(name) || /[\r\n]/.test(value)) throw new TypeError("Invalid header value");
  var i = this.__find(name);
  if (i >= 0) this.__list[i] = [name, value];
  else this.__list.push([name, value]);
};
Headers.prototype.get = function (name) { var i = this.__find(name); return i < 0 ? null : this.__list[i][1]; };
Headers.prototype.has = function (name) { return this.__find(name) >= 0; };
Headers.prototype.delete = function (name) { var i = this.__find(name); if (i >= 0) this.__list.splice(i, 1); };
Headers.prototype.forEach = function (fn, thisArg) {
  for (var i = 0; i < this.__list.length; i++) fn.call(thisArg, this.__list[i][1], this.__list[i][0], this);
};
Headers.prototype.entries = function () {
  return __kitaeIterable(this.__list.map(function (p) { return p.slice(); }));
};
Headers.prototype[Symbol.iterator] = Headers.prototype.entries;
// Serialized onto the wire as `name: value` lines (the fetch transport
// takes a plain string; see __kitaeFetchStart).
Headers.prototype.__blob = function () {
  var out = [];
  for (var i = 0; i < this.__list.length; i++) out.push(this.__list[i][0] + ": " + this.__list[i][1]);
  return out.join("\n");
};
function FormData(form) {
  if (!(this instanceof FormData)) throw new TypeError("Class constructor FormData cannot be invoked without 'new'");
  this.__items = [];
  if (form && form.__token !== undefined) {
    var fields = __kitaeQueryAllIn(form.__token, "input[name],select[name],textarea[name]");
    for (var i = 0; i < fields.length; i++) {
      var el = __kitaeMakeElement(fields[i]);
      var tag = (el.tagName || "").toLowerCase();
      var type = ("" + (__kitaeGetAttr(fields[i], "type") || "text")).toLowerCase();
      var name = __kitaeGetAttr(fields[i], "name");
      if (!name) continue;
      if (tag === "input" && (type === "checkbox" || type === "radio")) {
        if (el.checked) this.append(name, el.value === "" ? "on" : el.value);
      } else if (tag === "input" && type === "file") {
        continue; // file values are not supported in v0
      } else if (tag === "select") {
        this.append(name, el.value);
      } else {
        this.append(name, el.value);
      }
    }
  }
}
FormData.prototype.append = function (name, value) { this.__items.push(["" + name, value === undefined ? "null" : "" + value]); };
FormData.prototype.set = function (name, value) {
  name = "" + name;
  var found = false;
  for (var i = this.__items.length - 1; i >= 0; i--) {
    if (this.__items[i][0] === name) {
      if (found) this.__items.splice(i, 1);
      else { this.__items[i][1] = value === undefined ? "null" : "" + value; found = true; }
    }
  }
  if (!found) this.__items.push([name, value === undefined ? "null" : "" + value]);
};
FormData.prototype.get = function (name) {
  name = "" + name;
  for (var i = 0; i < this.__items.length; i++) if (this.__items[i][0] === name) return this.__items[i][1];
  return null;
};
FormData.prototype.getAll = function (name) {
  name = "" + name;
  var out = [];
  for (var i = 0; i < this.__items.length; i++) if (this.__items[i][0] === name) out.push(this.__items[i][1]);
  return out;
};
FormData.prototype.has = function (name) { return this.get(name) !== null; };
FormData.prototype.delete = function (name) {
  name = "" + name;
  for (var i = this.__items.length - 1; i >= 0; i--) if (this.__items[i][0] === name) this.__items.splice(i, 1);
};
FormData.prototype.forEach = function (fn, thisArg) {
  for (var i = 0; i < this.__items.length; i++) fn.call(thisArg, this.__items[i][1], this.__items[i][0], this);
};
FormData.prototype.entries = function () {
  return __kitaeIterable(this.__items.map(function (p) { return p.slice(); }));
};
FormData.prototype.__multipart = function (boundary) {
  var out = [];
  for (var i = 0; i < this.__items.length; i++) {
    out.push("--" + boundary);
    out.push('Content-Disposition: form-data; name="' + this.__items[i][0].replace(/"/g, "%22") + '"');
    out.push("");
    out.push(this.__items[i][1]);
  }
  out.push("--" + boundary + "--");
  out.push("");
  return out.join("\r\n");
};
function Response(body, init) {
  if (!(this instanceof Response)) throw new TypeError("Class constructor Response cannot be invoked without 'new'");
  init = init || {};
  this.__body = body === undefined || body === null ? "" : "" + body;
  this.status = init.status === undefined ? 200 : +init.status;
  this.ok = this.status >= 200 && this.status < 300;
  this.statusText = init.statusText === undefined ? (this.status === 200 ? "OK" : "") : "" + init.statusText;
  this.headers = init.headers instanceof Headers ? init.headers : new Headers(init.headers);
  this.url = init.url === undefined ? "" : "" + init.url;
  this.redirected = false;
  this.bodyUsed = false;
}
Response.prototype.text = function () { this.bodyUsed = true; return Promise.resolve(this.__body); };
Response.prototype.json = function () { this.bodyUsed = true; return Promise.resolve(JSON.parse(this.__body)); };
Response.prototype.arrayBuffer = function () {
  this.bodyUsed = true;
  return Promise.resolve(__kitaeUtf8Buffer(this.__body));
};
Response.prototype.clone = function () {
  return new Response(this.__body, {
    status: this.status, statusText: this.statusText, headers: this.headers, url: this.url,
  });
};
function __kitaeUtf8Buffer(s) {
  var bytes = [];
  for (var i = 0; i < s.length; i++) {
    var c = s.charCodeAt(i);
    if (c < 0x80) bytes.push(c);
    else if (c < 0x800) bytes.push(0xC0 | (c >> 6), 0x80 | (c & 63));
    else if (c >= 0xD800 && c < 0xDC00 && i + 1 < s.length) {
      var c2 = s.charCodeAt(++i);
      var cp = 0x10000 + ((c & 0x3FF) << 10) + (c2 & 0x3FF);
      bytes.push(0xF0 | (cp >> 18), 0x80 | ((cp >> 12) & 63), 0x80 | ((cp >> 6) & 63), 0x80 | (cp & 63));
    } else {
      bytes.push(0xE0 | (c >> 12), 0x80 | ((c >> 6) & 63), 0x80 | (c & 63));
    }
  }
  var buf = new ArrayBuffer(bytes.length);
  var view = new Uint8Array(buf);
  for (var i = 0; i < bytes.length; i++) view[i] = bytes[i];
  return buf;
}
function Request(input, init) {
  if (!(this instanceof Request)) throw new TypeError("Class constructor Request cannot be invoked without 'new'");
  init = init || {};
  if (input instanceof Request) {
    this.url = input.url;
    this.method = init.method ? ("" + init.method).toUpperCase() : input.method;
    this.headers = init.headers
      ? (init.headers instanceof Headers ? init.headers : new Headers(init.headers))
      : input.headers;
    this.body = init.body === undefined ? input.body : init.body;
    this.signal = init.signal || input.signal || null;
  } else {
    this.url = __kitaeResolveUrl("", "" + input);
    this.method = init.method ? ("" + init.method).toUpperCase() : "GET";
    this.headers = init.headers instanceof Headers ? init.headers : new Headers(init.headers);
    this.body = init.body === undefined ? null : init.body;
    this.signal = init.signal || null;
  }
}
__kitaeTakeTimer = function (id, keep) {
  var f = __kitae.timers[id];
  if (!keep) delete __kitae.timers[id];
  return f;
};
__kitaeRunTimer = function (id, keep) {
  var f = __kitae.timers[id];
  if (!keep) delete __kitae.timers[id];
  if (typeof f !== "function") return false;
  try { f(); }
  catch (e) { console.error("timer", id, e, e && e.stack ? e.stack : ""); }
  return true;
};
__kitaeSettle = function (id, ok, status, headersBlob, url, body) {
  var peer = __kitae.xhr[id];
  if (peer) {
    delete __kitae.xhr[id];
    peer.__ship(status > 0, status, headersBlob, url, body);
    return true;
  }
  var entry = __kitae.res[id];
  if (!entry) return false;
  delete __kitae.res[id];
  // Standard fetch semantics: any HTTP status resolves (ok reflects the
  // class); only transport failures (status 0) reject.
  if (status > 0) {
    var headers = new Headers();
    var lines = ("" + headersBlob).split("\n");
    for (var i = 0; i < lines.length; i++) {
      var idx = lines[i].indexOf(": ");
      if (idx > 0) headers.append(lines[i].slice(0, idx), lines[i].slice(idx + 2));
    }
    var statusText = "";
    if (status === 200) statusText = "OK";
    else if (status === 404) statusText = "Not Found";
    else if (status === 500) statusText = "Internal Server Error";
    entry.res(new Response(body, { status: status, statusText: statusText, headers: headers, url: url }));
  } else {
    entry.rej(new TypeError("" + (body || "fetch failed")));
  }
  return true;
};
// ── Observers (tier JS-4) ────────────────────────────────────────────
// Mutations are journaled at the wrapper/engine call sites that perform
// them (setAttribute, textContent, appendChild, classList, style, ...).
// Delivery runs from __kitaeObserverTick (the host calls it once per
// poll) and also on a microtask, so observers that await a microtask see
// their records promptly.
__kitae.observers = [];
__kitae.pendingMut = [];
__kitae.mutScheduled = false;
function __kitaeMutate(type, token, data) {
  if (!__kitae.observers.length) return;
  var rec = data || {};
  rec.type = type;
  rec.token = token;
  __kitae.pendingMut.push(rec);
  if (!__kitae.mutScheduled) {
    __kitae.mutScheduled = true;
    // Spec timing: the observer callback runs as a microtask queued at
    // mutation time, so script that awaits a microtask sees its records.
    Promise.resolve().then(__kitaeDeliverMutations);
  }
}
function __kitaeMutateAttr(token, name, oldValue) {
  __kitaeMutate("attributes", token, { name: "" + name, oldValue: oldValue });
}
function __kitaeChildList(token) {
  var toks = __kitaeChildTokens(token), out = [];
  for (var i = 0; i < toks.length; i++) out.push(toks[i]);
  return out;
}
function __kitaeInSubtree(token, root) {
  var t = token;
  while (t != null && t === t) {
    if (t === root) return true;
    t = __kitaeParent(t);
  }
  return false;
}
function __kitaeMutMatches(reg, rec) {
  var o = reg.options;
  if (rec.type === "attributes") {
    if (!o.attributes) return false;
    if (o.attributeFilter) {
      var found = false;
      for (var i = 0; i < o.attributeFilter.length; i++) {
        if (o.attributeFilter[i] === rec.name) { found = true; break; }
      }
      if (!found) return false;
    }
  } else if (rec.type === "characterData") {
    if (!o.characterData) return false;
  } else if (!o.childList) {
    return false;
  }
  return o.subtree ? __kitaeInSubtree(rec.token, reg.token) : rec.token === reg.token;
}
function __kitaeMutRecordFor(reg, rec) {
  var o = reg.options;
  var out = {
    type: rec.type,
    target: __kitaeMakeElement(rec.token),
    addedNodes: [],
    removedNodes: [],
    previousSibling: null,
    nextSibling: null,
    attributeName: null,
    attributeNamespace: null,
    oldValue: null,
  };
  if (rec.type === "childList") {
    var i;
    for (i = 0; rec.added && i < rec.added.length; i++) out.addedNodes.push(__kitaeMakeElement(rec.added[i]));
    for (i = 0; rec.removed && i < rec.removed.length; i++) out.removedNodes.push(__kitaeMakeElement(rec.removed[i]));
  } else if (rec.type === "attributes") {
    out.attributeName = rec.name;
    if (o.attributeOldValue) out.oldValue = rec.oldValue === undefined ? null : rec.oldValue;
  } else if (o.characterDataOldValue) {
    out.oldValue = rec.oldValue === undefined ? null : rec.oldValue;
  }
  return out;
}
__kitaeMatchMutations = function () {
  __kitae.mutScheduled = false;
  if (!__kitae.pendingMut.length) return;
  var queue = __kitae.pendingMut;
  __kitae.pendingMut = [];
  for (var i = 0; i < __kitae.observers.length; i++) {
    var obs = __kitae.observers[i];
    for (var r = 0; r < queue.length; r++) {
      for (var g = 0; g < obs.__regs.length; g++) {
        if (__kitaeMutMatches(obs.__regs[g], queue[r])) {
          obs.__queue.push(__kitaeMutRecordFor(obs.__regs[g], queue[r]));
          break;
        }
      }
    }
  }
};
__kitaeDeliverMutations = function () {
  __kitaeMatchMutations();
  for (var i = 0; i < __kitae.observers.length; i++) {
    var obs = __kitae.observers[i];
    if (!obs.__queue.length) continue;
    var recs = obs.__queue.splice(0);
    try { obs.__cb.call(obs, recs, obs); }
    catch (e) { console.error("MutationObserver", e); }
  }
};
function MutationObserver(cb) {
  if (!(this instanceof MutationObserver)) throw new TypeError("Class constructor MutationObserver cannot be invoked without 'new'");
  if (typeof cb !== "function") throw new TypeError("Failed to construct 'MutationObserver': parameter 1 is not a function");
  this.__cb = cb;
  this.__regs = [];
  this.__queue = [];
}
MutationObserver.prototype.observe = function (node, options) {
  if (!node || !node.__token) throw new TypeError("Failed to execute 'observe': parameter 1 is not of type 'Node'");
  options = options || {};
  if (!options.childList && !options.attributes && !options.characterData &&
      !options.attributeOldValue && !options.characterDataOldValue &&
      !(options.attributeFilter && options.attributeFilter.length)) {
    throw new TypeError("Failed to execute 'observe': The options object must set at least one of 'attributes', 'characterData', or 'childList' to true.");
  }
  var filtered = {
    childList: !!options.childList,
    attributes: !!(options.attributes || options.attributeOldValue || (options.attributeFilter && options.attributeFilter.length)),
    characterData: !!(options.characterData || options.characterDataOldValue),
    subtree: !!options.subtree,
    attributeOldValue: !!options.attributeOldValue,
    characterDataOldValue: !!options.characterDataOldValue,
    attributeFilter: options.attributeFilter && options.attributeFilter.length ? Array.prototype.slice.call(options.attributeFilter) : null,
  };
  for (var i = 0; i < this.__regs.length; i++) {
    if (this.__regs[i].token === node.__token) { this.__regs[i].options = filtered; return; }
  }
  this.__regs.push({ token: node.__token, options: filtered });
  if (__kitae.observers.indexOf(this) < 0) __kitae.observers.push(this);
};
MutationObserver.prototype.disconnect = function () {
  this.__regs.length = 0;
  this.__queue.length = 0;
  var i = __kitae.observers.indexOf(this);
  if (i >= 0) __kitae.observers.splice(i, 1);
};
MutationObserver.prototype.takeRecords = function () {
  __kitaeMatchMutations();
  return this.__queue.splice(0);
};
function __kitaeRectObj(x, y, w, h) {
  return {
    x: x, y: y, width: w, height: h,
    top: y, left: x, right: x + w, bottom: y + h,
  };
}
function __kitaeElementBox(token) {
  var r = __kitaeRect(token);
  if (r == null || r !== r) return null;
  return { x: +r[0] || 0, y: +r[1] || 0, w: +r[2] || 0, h: +r[3] || 0 };
}
function __kitaeIntersectionEntry(target, token) {
  var b = __kitaeElementBox(token) || { x: 0, y: 0, w: 0, h: 0 };
  var vw = __kitaeViewport(0), vh = __kitaeViewport(1);
  var sy = __kitaeScrollPos(1);
  var vx = b.x, vy = b.y - sy;
  var ix = Math.max(0, Math.min(vx + b.w, vw) - Math.max(vx, 0));
  var iy = Math.max(0, Math.min(vy + b.h, vh) - Math.max(vy, 0));
  var area = b.w * b.h;
  var ratio = area > 0 ? (ix * iy) / area : 0;
  return {
    target: target,
    boundingClientRect: __kitaeRectObj(vx, vy, b.w, b.h),
    intersectionRect: __kitaeRectObj(Math.max(vx, 0), Math.max(vy, 0), ix, iy),
    rootBounds: __kitaeRectObj(0, 0, vw, vh),
    intersectionRatio: ratio,
    isIntersecting: ratio > 0,
    time: Date.now(),
  };
}
function __kitaeCrossedThreshold(prev, ratio, thresholds) {
  if (prev === undefined) return true;
  if (prev === ratio) return false;
  for (var i = 0; i < thresholds.length; i++) {
    var t = thresholds[i];
    if ((prev < t && ratio >= t) || (prev >= t && ratio < t)) return true;
  }
  return (prev === 0) !== (ratio === 0);
}
function IntersectionObserver(cb, options) {
  if (!(this instanceof IntersectionObserver)) throw new TypeError("Class constructor IntersectionObserver cannot be invoked without 'new'");
  if (typeof cb !== "function") throw new TypeError("Failed to construct 'IntersectionObserver': parameter 1 is not a function");
  options = options || {};
  this.root = options.root || null;
  this.rootMargin = options.rootMargin === undefined ? "0px" : "" + options.rootMargin;
  var th = options.threshold === undefined ? [0]
    : (typeof options.threshold === "object" ? options.threshold : [options.threshold]);
  var list = [];
  for (var i = 0; i < th.length; i++) list.push(+th[i]);
  this.thresholds = list.length ? list : [0];
  this.__cb = cb;
  this.__targets = [];
  this.__queue = [];
}
IntersectionObserver.prototype.observe = function (el, root) {
  if (!el || !el.__token) throw new TypeError("Failed to execute 'observe': parameter 1 is not of type 'Element'");
  for (var i = 0; i < this.__targets.length; i++) {
    if (this.__targets[i].token === el.__token) return;
  }
  this.__targets.push({ token: el.__token, el: el, ratio: undefined });
  if (__kitae.observers.indexOf(this) < 0) __kitae.observers.push(this);
};
IntersectionObserver.prototype.unobserve = function (el) {
  if (!el || !el.__token) return;
  for (var i = 0; i < this.__targets.length; i++) {
    if (this.__targets[i].token === el.__token) { this.__targets.splice(i, 1); break; }
  }
  if (!this.__targets.length) {
    var j = __kitae.observers.indexOf(this);
    if (j >= 0) __kitae.observers.splice(j, 1);
  }
};
IntersectionObserver.prototype.disconnect = function () {
  this.__targets.length = 0;
  this.__queue.length = 0;
  var j = __kitae.observers.indexOf(this);
  if (j >= 0) __kitae.observers.splice(j, 1);
};
IntersectionObserver.prototype.takeRecords = function () { return this.__queue.splice(0); };
function ResizeObserver(cb) {
  if (!(this instanceof ResizeObserver)) throw new TypeError("Class constructor ResizeObserver cannot be invoked without 'new'");
  if (typeof cb !== "function") throw new TypeError("Failed to construct 'ResizeObserver': parameter 1 is not a function");
  this.__cb = cb;
  this.__targets = [];
  this.__queue = [];
}
ResizeObserver.prototype.observe = function (el) {
  if (!el || !el.__token) throw new TypeError("Failed to execute 'observe': parameter 1 is not of type 'Element'");
  for (var i = 0; i < this.__targets.length; i++) {
    if (this.__targets[i].token === el.__token) return;
  }
  this.__targets.push({ token: el.__token, el: el, w: undefined, h: undefined });
  if (__kitae.observers.indexOf(this) < 0) __kitae.observers.push(this);
};
ResizeObserver.prototype.unobserve = function (el) {
  if (!el || !el.__token) return;
  for (var i = 0; i < this.__targets.length; i++) {
    if (this.__targets[i].token === el.__token) { this.__targets.splice(i, 1); break; }
  }
  if (!this.__targets.length) {
    var j = __kitae.observers.indexOf(this);
    if (j >= 0) __kitae.observers.splice(j, 1);
  }
};
ResizeObserver.prototype.disconnect = function () {
  this.__targets.length = 0;
  this.__queue.length = 0;
  var j = __kitae.observers.indexOf(this);
  if (j >= 0) __kitae.observers.splice(j, 1);
};
ResizeObserver.prototype.takeRecords = function () { return this.__queue.splice(0); };
function __kitaeObserverTick() {
  // Always run the mutation pass: the microtask may already have matched
  // records into observer queues, and those still need their callback.
  __kitaeDeliverMutations();
  for (var i = 0; i < __kitae.observers.length; i++) {
    var obs = __kitae.observers[i];
    if (obs instanceof IntersectionObserver) {
      for (var t = 0; t < obs.__targets.length; t++) {
        var entry = __kitaeIntersectionEntry(obs.__targets[t].el, obs.__targets[t].token);
        if (__kitaeCrossedThreshold(obs.__targets[t].ratio, entry.intersectionRatio, obs.thresholds)) {
          obs.__targets[t].ratio = entry.intersectionRatio;
          obs.__queue.push(entry);
        }
      }
    } else if (obs instanceof ResizeObserver) {
      for (var t2 = 0; t2 < obs.__targets.length; t2++) {
        var b = __kitaeElementBox(obs.__targets[t2].token);
        if (!b) continue;
        if (obs.__targets[t2].w === undefined || obs.__targets[t2].w !== b.w || obs.__targets[t2].h !== b.h) {
          obs.__targets[t2].w = b.w;
          obs.__targets[t2].h = b.h;
          obs.__queue.push({
            target: obs.__targets[t2].el,
            contentRect: __kitaeRectObj(b.x, b.y, b.w, b.h),
            borderBoxSize: [{ inlineSize: b.w, blockSize: b.h }],
            contentBoxSize: [{ inlineSize: b.w, blockSize: b.h }],
          });
        }
      }
    }
    if (obs instanceof IntersectionObserver || obs instanceof ResizeObserver) {
      if (obs.__queue.length) {
        var entries = obs.__queue.splice(0);
        try { obs.__cb.call(obs, entries, obs); }
        catch (e) { console.error("Observer", e); }
      }
    }
  }
}
// ── Events: constructor classes + the engine's factory ────────────────
function DOMException(message, name) {
  var e = Error.call(this, message);
  this.name = name || "Error";
  this.message = message === undefined ? "" : "" + message;
  if (!e.stack && e.stack !== undefined) this.stack = e.stack;
}
DOMException.prototype = Object.create(Error.prototype);
function Event(type, init) {
  if (!(this instanceof Event)) throw new TypeError("Class constructor Event cannot be invoked without 'new'");
  if (arguments.length < 1) throw new TypeError("Failed to construct 'Event': 1 argument required, but only 0 present.");
  init = init || {};
  this.type = "" + type;
  this.bubbles = !!init.bubbles;
  this.cancelable = !!init.cancelable;
  this.composed = !!init.composed;
  this.defaultPrevented = false;
  this.target = null;
  this.srcElement = null;
  this.currentTarget = null;
  this.eventPhase = 0;
  this.returnValue = true;
  this.isTrusted = false;
  this.timeStamp = Date.now();
  this.__propStopped = false;
  this.__immediateStopped = false;
}
Event.NONE = 0;
Event.CAPTURING_PHASE = 1;
Event.AT_TARGET = 2;
Event.BUBBLING_PHASE = 3;
Event.prototype.preventDefault = function () {
  if (this.cancelable) { this.defaultPrevented = true; this.returnValue = false; }
};
Event.prototype.stopPropagation = function () { this.__propStopped = true; };
Event.prototype.stopImmediatePropagation = function () { this.__propStopped = true; this.__immediateStopped = true; };
Event.prototype.initEvent = function (type, bubbles, cancelable) {
  this.type = "" + type;
  this.bubbles = !!bubbles;
  this.cancelable = !!cancelable;
  this.defaultPrevented = false;
  this.returnValue = true;
};
Event.prototype.composedPath = function () {
  var out = [], t = this.target;
  while (t) { out.push(t); t = t.parentElement || null; }
  out.push(document);
  return out;
};
function CustomEvent(type, init) {
  if (!(this instanceof CustomEvent)) throw new TypeError("Class constructor CustomEvent cannot be invoked without 'new'");
  init = init || {};
  Event.call(this, type, init);
  this.detail = init.detail === undefined ? null : init.detail;
}
CustomEvent.prototype = Object.create(Event.prototype);
function MouseEvent(type, init) {
  if (!(this instanceof MouseEvent)) throw new TypeError("Class constructor MouseEvent cannot be invoked without 'new'");
  init = init || {};
  Event.call(this, type, init);
  this.clientX = +init.clientX || 0;
  this.clientY = +init.clientY || 0;
  this.button = +init.button || 0;
}
MouseEvent.prototype = Object.create(Event.prototype);
function KeyboardEvent(type, init) {
  if (!(this instanceof KeyboardEvent)) throw new TypeError("Class constructor KeyboardEvent cannot be invoked without 'new'");
  init = init || {};
  Event.call(this, type, init);
  this.key = init.key === undefined ? "" : "" + init.key;
  this.code = init.code === undefined ? "" : "" + init.code;
  this.ctrlKey = !!init.ctrlKey; this.shiftKey = !!init.shiftKey;
  this.altKey = !!init.altKey; this.metaKey = !!init.metaKey;
}
KeyboardEvent.prototype = Object.create(Event.prototype);
// A standalone EventTarget (scripts use it as a lightweight bus). The
// document/window/element wrappers implement the same listener semantics
// through the engine maps; this class carries its own.
function EventTarget() {
  if (!(this instanceof EventTarget)) throw new TypeError("Class constructor EventTarget cannot be invoked without 'new'");
  this.__listeners = {};
  this.__capture = {};
}
EventTarget.prototype.addEventListener = function (type, fn, opts) {
  if (typeof fn !== "function") return;
  var map = (opts === true || (opts && opts.capture)) ? this.__capture : this.__listeners;
  var list = (map["" + type] = map["" + type] || []);
  for (var i = 0; i < list.length; i++) if (list[i].fn === fn) return;
  list.push(__kitaeRecord(fn, opts));
};
EventTarget.prototype.removeEventListener = function (type, fn, opts) {
  var map = (opts === true || (opts && opts.capture)) ? this.__capture : this.__listeners;
  var list = map["" + type];
  if (!list) return;
  for (var i = 0; i < list.length; i++) {
    if (list[i].fn === fn) { list.splice(i, 1); return; }
  }
};
EventTarget.prototype.dispatchEvent = function (ev) {
  __kitaeEnsureEvent(ev);
  var self = this, type = "" + ev.type;
  ev.__propStopped = false;
  ev.__immediateStopped = false;
  ev.target = self;
  ev.srcElement = self;
  function run(list) {
    if (!list || !list.length) return;
    ev.currentTarget = self;
    ev.eventPhase = Event.AT_TARGET;
    for (var i = 0; i < list.length; i++) {
      if (ev.__immediateStopped) return;
      var rec = list[i];
      if (rec.signal && rec.signal.aborted) { list.splice(i, 1); i--; continue; }
      if (rec.once) { list.splice(i, 1); i--; }
      try { rec.fn.call(self, ev); }
      catch (e) { console.error("listener", type, e); }
    }
  }
  run(self.__capture[type]);
  run(self.__listeners[type]);
  ev.currentTarget = null;
  return !ev.defaultPrevented;
};
// AbortController/AbortSignal: listener removal (`addEventListener`'s
// `signal:` option) and fetch cancellation share one pair.
function AbortSignal() {
  if (!(this instanceof AbortSignal)) throw new TypeError("Class constructor AbortSignal cannot be invoked without 'new'");
  this.aborted = false;
  this.reason = undefined;
  this.onabort = null;
  this.__listeners = [];
}
AbortSignal.prototype.addEventListener = function (type, fn) {
  if ("" + type === "abort" && typeof fn === "function") this.__listeners.push(fn);
};
AbortSignal.prototype.removeEventListener = function (type, fn) {
  if ("" + type !== "abort") return;
  var i = this.__listeners.indexOf(fn);
  if (i >= 0) this.__listeners.splice(i, 1);
};
AbortSignal.prototype.__abort = function (reason) {
  if (this.aborted) return;
  this.aborted = true;
  this.reason = reason === undefined
    ? new DOMException("This operation was aborted", "AbortError")
    : reason;
  if (typeof this.onabort === "function") {
    try { this.onabort.call(this); } catch (e) { console.error("abort", e); }
  }
  var list = this.__listeners.slice();
  for (var i = 0; i < list.length; i++) {
    try { list[i].call(this); } catch (e) { console.error("abort", e); }
  }
};
function AbortController() {
  if (!(this instanceof AbortController)) throw new TypeError("Class constructor AbortController cannot be invoked without 'new'");
  this.signal = new AbortSignal();
}
AbortController.prototype.abort = function (reason) { this.signal.__abort(reason); };
// ── XMLHttpRequest (tier JS-5) ────────────────────────────────────────
// The classic async read surface, riding fetch's pool plumbing: `send`
// registers a ship callback and the settle path fills the object. Async
// only — synchronous XHR is refused by design (documented gap), and the
// readyState ladder reports OPENED then DONE (no HEADERS_RECEIVED/LOADING
// intermediate states in this emulation).
function XMLHttpRequest() {
  if (!(this instanceof XMLHttpRequest)) throw new TypeError("Class constructor XMLHttpRequest cannot be invoked without 'new'");
  this.readyState = 0;
  this.status = 0;
  this.statusText = "";
  this.response = null;
  this.responseText = "";
  this.responseType = "";
  this.responseURL = "";
  this.withCredentials = false;
  this.timeout = 0;
  this.onreadystatechange = null;
  this.onload = null;
  this.onerror = null;
  this.onloadend = null;
  this.__method = "GET";
  this.__url = "";
  this.__headers = [];
  this.__resp = "";
  this.__id = 0;
  this.__aborted = false;
  this.__listeners = {};
}
XMLHttpRequest.UNSENT = 0;
XMLHttpRequest.OPENED = 1;
XMLHttpRequest.HEADERS_RECEIVED = 2;
XMLHttpRequest.LOADING = 3;
XMLHttpRequest.DONE = 4;
XMLHttpRequest.prototype.open = function (method, url, async) {
  if (async === false) {
    throw new DOMException("Synchronous XMLHttpRequest is not supported by this engine", "NotSupportedError");
  }
  this.__method = ("" + method).toUpperCase();
  this.__url = "" + url;
  this.__headers = [];
  this.__aborted = false;
  this.status = 0;
  this.statusText = "";
  this.response = null;
  this.responseText = "";
  this.readyState = 1;
  this.__fire("readystatechange");
};
XMLHttpRequest.prototype.setRequestHeader = function (name, value) {
  if (this.readyState !== 1) throw new DOMException("setRequestHeader called before open", "InvalidStateError");
  this.__headers.push(["" + name, "" + value]);
};
XMLHttpRequest.prototype.__fire = function (kind, ev) {
  try {
    if (kind === "readystatechange" && typeof this.onreadystatechange === "function") this.onreadystatechange(ev);
    if (kind === "load" && typeof this.onload === "function") this.onload(ev);
    if (kind === "error" && typeof this.onerror === "function") this.onerror(ev);
    if (kind === "loadend" && typeof this.onloadend === "function") this.onloadend(ev);
  } catch (e) { console.error("xhr", e); }
  var list = this.__listeners[kind];
  if (list) {
    for (var i = 0; i < list.length; i++) {
      try { list[i].call(this, ev); } catch (e) { console.error("xhr", e); }
    }
  }
};
XMLHttpRequest.prototype.send = function (body) {
  var self = this;
  if (this.readyState !== 1) throw new DOMException("send called before open", "InvalidStateError");
  this.__id = ++__kitae.seq;
  var blob = "";
  for (var i = 0; i < this.__headers.length; i++) blob += this.__headers[i][0] + ": " + this.__headers[i][1] + "\n";
  __kitae.xhr[this.__id] = {
    __ship: function (ok, status, headersBlob, url, text) {
      if (self.__aborted) return;
      self.__resp = headersBlob;
      self.responseURL = url;
      self.status = ok ? status : 0;
      self.statusText = status === 200 ? "OK"
        : (status === 404 ? "Not Found" : (status === 500 ? "Internal Server Error" : ""));
      var payload = ok ? text : "";
      if (self.responseType === "json") {
        var parsed = null;
        if (ok) { try { parsed = JSON.parse(payload); } catch (e) { parsed = null; } }
        self.response = parsed;
        self.responseText = payload;
      } else if (self.responseType === "" || self.responseType === "text") {
        self.response = payload;
        self.responseText = payload;
      } else {
        self.response = payload;
        self.responseText = payload;
      }
      self.readyState = 4;
      self.__fire("readystatechange");
      self.__fire(ok ? "load" : "error");
      self.__fire("loadend");
    },
  };
  __kitaeFetchStart(this.__id, this.__url, this.__method, blob, body == null ? "" : "" + body);
};
XMLHttpRequest.prototype.abort = function () {
  this.__aborted = true;
  if (this.__id && __kitae.xhr[this.__id]) delete __kitae.xhr[this.__id];
  if (this.readyState !== 0 && this.readyState !== 4) {
    this.readyState = 4;
    this.status = 0;
    this.__fire("readystatechange");
    this.__fire("error");
    this.__fire("loadend");
  }
};
XMLHttpRequest.prototype.getResponseHeader = function (name) {
  var want = ("" + name).toLowerCase();
  var lines = ("" + this.__resp).split("\n");
  for (var i = 0; i < lines.length; i++) {
    var idx = lines[i].indexOf(": ");
    if (idx > 0 && lines[i].slice(0, idx).toLowerCase() === want) return lines[i].slice(idx + 2);
  }
  return null;
};
XMLHttpRequest.prototype.getAllResponseHeaders = function () { return this.__resp; };
XMLHttpRequest.prototype.overrideMimeType = function () {};
XMLHttpRequest.prototype.addEventListener = function (type, fn) {
  if (typeof fn !== "function") return;
  var list = (this.__listeners["" + type] = this.__listeners["" + type] || []);
  for (var i = 0; i < list.length; i++) if (list[i] === fn) return;
  list.push(fn);
};
XMLHttpRequest.prototype.removeEventListener = function (type, fn) {
  var list = this.__listeners["" + type];
  if (!list) return;
  for (var i = 0; i < list.length; i++) if (list[i] === fn) { list.splice(i, 1); return; }
};
// ── Worker (tier JS-5): same-thread emulation ────────────────────────
// The worker script is fetched through the pool and evaluated in a
// function scope with a `self` object mirroring the worker global.
// Messages structured-clone both ways. No parallelism, no importScripts,
// no worker-side fetch — the documented lens on this tier.
function Worker(url) {
  if (!(this instanceof Worker)) throw new TypeError("Class constructor Worker cannot be invoked without 'new'");
  var owner = this;
  this.onmessage = null;
  this.onerror = null;
  this.__listeners = {};
  this.__terminated = false;
  this.__ready = false;
  this.__outbox = [];
  var handle = {
    onmessage: null,
    onmessageerror: null,
    postMessage: function (v) {
      if (owner.__terminated) return;
      var ev = { type: "message", data: structuredClone(v) };
      try {
        if (typeof owner.onmessage === "function") owner.onmessage(ev);
      } catch (e) { console.error("worker", e); }
      var list = owner.__listeners["message"];
      if (list) for (var i = 0; i < list.length; i++) list[i].call(owner, ev);
    },
    addEventListener: function (type, fn) {
      if (typeof fn !== "function") return;
      (owner.__listeners["" + type] = owner.__listeners["" + type] || []).push(fn);
    },
    removeEventListener: function (type, fn) {
      var list = owner.__listeners["" + type];
      if (!list) return;
      for (var i = 0; i < list.length; i++) if (list[i] === fn) { list.splice(i, 1); return; }
    },
    importScripts: function () {
      throw new DOMException("importScripts is not supported in this engine's worker emulation", "NotSupportedError");
    },
    close: function () { owner.__terminated = true; },
    setTimeout: setTimeout,
    setInterval: setInterval,
    clearTimeout: clearTimeout,
    clearInterval: clearInterval,
    console: console,
    structuredClone: structuredClone,
  };
  handle.self = handle;
  handle.globalThis = handle;
  this.__handle = handle;
  this.__url = "" + url;
  this.__id = ++__kitae.seq;
  var resolved = __kitaeResolveUrl(__kitaePageUrl(), "" + url);
  handle.location = { href: resolved, origin: "", pathname: "", search: "", hash: "" };
  __kitae.xhr[this.__id] = {
    __ship: function (ok, status, headersBlob, u, text) {
      if (owner.__terminated) return;
      if (!ok) { owner.__fail("worker script failed to load (status " + status + ")"); return; }
      try {
        var run = new Function(
          "self", "postMessage", "addEventListener", "removeEventListener",
          "close", "importScripts", "setTimeout", "setInterval",
          "clearTimeout", "clearInterval", "console", "location", "structuredClone",
          text + "\n//# sourceURL=" + u
        );
        run(handle, handle.postMessage, handle.addEventListener, handle.removeEventListener,
            handle.close, handle.importScripts, setTimeout, setInterval,
            clearTimeout, clearInterval, console, handle.location, structuredClone);
      } catch (e) {
        owner.__fail("" + e);
        return;
      }
      owner.__ready = true;
      for (var i = 0; i < owner.__outbox.length; i++) owner.__deliver(owner.__outbox[i]);
      owner.__outbox.length = 0;
    },
  };
  __kitaeFetchStart(this.__id, resolved, "GET", "", "");
}
Worker.prototype.__deliver = function (v) {
  if (this.__terminated) return;
  var ev = { data: structuredClone(v) };
  var handler = this.__handle.onmessage;
  if (typeof handler === "function") {
    try { handler.call(this.__handle, ev); }
    catch (e) { this.__fail("" + e); }
  }
};
Worker.prototype.__fail = function (message) {
  if (this.__terminated) return;
  var ev = { type: "error", message: message };
  try {
    if (typeof this.onerror === "function") this.onerror(ev);
  } catch (e) { console.error("worker", e); }
  var list = this.__listeners["error"];
  if (list) for (var i = 0; i < list.length; i++) list[i].call(this, ev);
};
Worker.prototype.postMessage = function (v) {
  if (this.__terminated) return;
  if (!this.__ready) { this.__outbox.push(v); return; }
  this.__deliver(v);
};
Worker.prototype.terminate = function () {
  this.__terminated = true;
  this.__outbox.length = 0;
};
Worker.prototype.addEventListener = function (type, fn) {
  if (typeof fn !== "function") return;
  var list = (this.__listeners["" + type] = this.__listeners["" + type] || []);
  for (var i = 0; i < list.length; i++) if (list[i] === fn) return;
  list.push(fn);
};
Worker.prototype.removeEventListener = function (type, fn) {
  var list = this.__listeners["" + type];
  if (!list) return;
  for (var i = 0; i < list.length; i++) if (list[i] === fn) { list.splice(i, 1); return; }
};
// The engine's own events (click from the hit-test, typing/change/submit,
// load) are built here so every dispatch path shares one event shape.
__kitaeMakeEvent = function (type, x, y) {
  var ev = new MouseEvent(type, {
    clientX: x, clientY: y,
    bubbles: type !== "popstate",
    cancelable: type === "click" || type === "submit",
  });
  ev.isTrusted = true;
  return ev;
};
// structuredClone: the JSON-adjacent value space plus cycles — objects,
// arrays, Date, Map, Set, typed arrays, ArrayBuffer. Functions/symbols
// throw DataCloneError, like the real one.
function structuredClone(value) {
  var seen = [];
  function clone(v) {
    if (v === null || typeof v !== "object") {
      if (typeof v === "function" || typeof v === "symbol") {
        throw new DOMException("could not be cloned", "DataCloneError");
      }
      return v;
    }
    for (var i = 0; i < seen.length; i++) if (seen[i][0] === v) return seen[i][1];
    if (v instanceof Date) return new Date(v.getTime());
    if (v instanceof RegExp) return new RegExp(v.source, v.flags);
    if (typeof Map === "function" && v instanceof Map) {
      var m = new Map(); seen.push([v, m]);
      v.forEach(function (val, key) { m.set(clone(key), clone(val)); });
      return m;
    }
    if (typeof Set === "function" && v instanceof Set) {
      var s = new Set(); seen.push([v, s]);
      v.forEach(function (val) { s.add(clone(val)); });
      return s;
    }
    if (v instanceof ArrayBuffer) return v.slice(0);
    if (ArrayBuffer.isView && ArrayBuffer.isView(v)) {
      var copy = new v.constructor(clone(v.buffer));
      seen.push([v, copy]);
      return copy;
    }
    var out = Array.isArray(v) ? [] : {};
    seen.push([v, out]);
    if (Array.isArray(v)) {
      for (var i = 0; i < v.length; i++) out[i] = clone(v[i]);
    } else {
      var keys = Object.keys(v);
      for (var i = 0; i < keys.length; i++) {
        var k = keys[i];
        try { out[k] = clone(v[k]); }
        catch (e) { throw e; }
      }
    }
    return out;
  }
  return clone(value);
}
__kitaeDispatch = function (type, token, event) {
  event.__propStopped = false;
  event.__immediateStopped = false;
  var chain = [], t = token;
  while (t != null && t === t) { chain.push(t); t = __kitaeParent(t); }
  if (!chain.length) return 0;
  var wrappers = [];
  for (var i = 0; i < chain.length; i++) wrappers.push(__kitaeMakeElement(chain[i]));
  event.target = wrappers[0];
  event.srcElement = wrappers[0];
  var fired = 0;
  // One listener list = records {fn, once, capture, signal}. Runs in
  // place so a once listener (or one whose signal aborted) leaves before
  // it fires. `wantCapture` filters the mixed document/window maps.
  function runList(list, w, phase, wantCapture) {
    if (!list || !list.length) return;
    event.currentTarget = w;
    event.eventPhase = phase;
    for (var i = 0; i < list.length; i++) {
      if (event.__immediateStopped) return;
      var rec = list[i];
      if (wantCapture !== undefined && !!rec.capture !== wantCapture) continue;
      if (rec.signal && rec.signal.aborted) { list.splice(i, 1); i--; continue; }
      if (rec.once) { list.splice(i, 1); i--; }
      try { rec.fn.call(w, event); fired++; }
      catch (e) { console.error("listener", type, e); }
    }
  }
  function fireOn(w, capture, phase) {
    if (event.__propStopped) return;
    var list = capture
      ? (w.__capture && w.__capture[type])
      : (w.__listeners && w.__listeners[type]);
    runList(list, w, phase, capture);
  }
  // Capture: window → document → root → parent. Target: both lists, at
  // AT_TARGET. Bubble (bubbling events only, per spec): parent → root →
  // document → window.
  runList(__kitae.onWin[type], window, Event.CAPTURING_PHASE, true);
  if (!event.__propStopped) runList(__kitae.onDoc[type], document, Event.CAPTURING_PHASE, true);
  for (var i = wrappers.length - 1; i >= 1; i--) fireOn(wrappers[i], true, Event.CAPTURING_PHASE);
  fireOn(wrappers[0], true, Event.AT_TARGET);
  fireOn(wrappers[0], false, Event.AT_TARGET);
  if (event.bubbles) {
    for (var i = 1; i < wrappers.length; i++) fireOn(wrappers[i], false, Event.BUBBLING_PHASE);
    if (!event.__propStopped) runList(__kitae.onDoc[type], document, Event.BUBBLING_PHASE, false);
    if (!event.__propStopped) runList(__kitae.onWin[type], window, Event.BUBBLING_PHASE, false);
  }
  event.currentTarget = null;
  return fired;
};
function requestAnimationFrame(fn) {
  if (typeof fn !== "function") return 0;
  var id = ++__kitae.seq;
  __kitae.timers[id] = function () { fn(Date.now()); };
  __kitaeTimerSet(id, 16, 0);
  return id;
}
function cancelAnimationFrame(id) { clearTimeout(id); }
var performance = {
  now: function () { return Date.now(); },
  timeOrigin: 0,
};
function queueMicrotask(fn) { Promise.resolve().then(fn); }
function requestIdleCallback(fn) {
  if (typeof fn !== "function") return 0;
  var id = ++__kitae.seq;
  __kitae.timers[id] = function () {
    fn({ didTimeout: false, timeRemaining: function () { return 50; } });
  };
  __kitaeTimerSet(id, 1, 0);
  return id;
}
function cancelIdleCallback(id) { clearTimeout(id); }
function __kitaePort() {
  var p = { onmessage: null, onmessageerror: null, __handlers: [], __peer: null };
  p.postMessage = function (v) {
    var target = p.__peer;
    setTimeout(function () { __kitaeDeliver(target, v); }, 0);
  };
  p.addEventListener = function (t, fn) {
    if (t === "message" && typeof fn === "function") p.__handlers.push(fn);
  };
  p.removeEventListener = function (t, fn) {
    var i = p.__handlers.indexOf(fn);
    if (i >= 0) p.__handlers.splice(i, 1);
  };
  p.start = function () {};
  p.close = function () {};
  return p;
}
function __kitaeDeliver(target, v) {
  if (!target) return;
  var msg = { data: v, target: target, ports: [] };
  if (typeof target.onmessage === "function") {
    try { target.onmessage.call(target, msg); }
    catch (e) { console.error("onmessage", e); }
  }
  for (var i = 0; i < target.__handlers.length; i++) {
    try { target.__handlers[i].call(target, msg); }
    catch (e) { console.error("onmessage", e); }
  }
}
function MessageChannel() {
  this.port1 = __kitaePort();
  this.port2 = __kitaePort();
  this.port1.__peer = this.port2;
  this.port2.__peer = this.port1;
}
var history = {
  length: 1,
  state: null,
  pushState: function (s, t, url) {
    history.state = s;
    __kitaePushState(url == null ? "" : "" + url, 0);
  },
  replaceState: function (s, t, url) {
    history.state = s;
    __kitaePushState(url == null ? "" : "" + url, 1);
  },
  back: function () { __kitaeNav("kitae:back"); },
  forward: function () { __kitaeNav("kitae:forward"); },
  go: function (d) {
    d = +d || 0;
    if (d < 0) history.back();
    else if (d > 0) history.forward();
  },
};
window.history = history;
window.addEventListener = function (type, fn, opts) {
  if (typeof fn !== "function") return;
  var capture = opts === true || (opts && opts.capture);
  var list = (__kitae.onWin["" + type] = __kitae.onWin["" + type] || []);
  for (var i = 0; i < list.length; i++) {
    if (list[i].fn === fn && !!list[i].capture === capture) return;
  }
  list.push(__kitaeRecord(fn, opts));
};
window.removeEventListener = function (type, fn) {
  var list = __kitae.onWin["" + type];
  if (!list) return;
  for (var i = 0; i < list.length; i++) {
    if (list[i].fn === fn) { list.splice(i, 1); return; }
  }
};
window.dispatchEvent = function (ev) {
  __kitaeEnsureEvent(ev);
  ev.__propStopped = false;
  ev.__immediateStopped = false;
  ev.target = window;
  ev.srcElement = window;
  ev.eventPhase = Event.AT_TARGET;
  __kitaeFireWin("" + ev.type, ev, Event.AT_TARGET);
  ev.currentTarget = null;
  return !ev.defaultPrevented;
};
window.scrollTo = function (x, y) {
  if (typeof x === "object" && x !== null) __kitaeScrollTo(0, +x.top || 0);
  else __kitaeScrollTo(+x || 0, +y || 0);
};
window.scrollBy = function (x, y) {
  if (typeof x === "object" && x !== null) __kitaeScrollBy(0, +x.top || 0);
  else __kitaeScrollBy(+x || 0, +y || 0);
};
window.getSelection = function () {
  var text = __kitaeSelectionText();
  return {
    toString: function () { return text; },
    isCollapsed: text.length === 0,
    rangeCount: text.length === 0 ? 0 : 1,
    anchorNode: null,
    focusNode: null,
    removeAllRanges: function () {},
    collapse: function () {},
  };
};
Object.defineProperty(window, "scrollX", { configurable: true, get: function () { return __kitaeScrollPos(0); } });
Object.defineProperty(window, "scrollY", { configurable: true, get: function () { return __kitaeScrollPos(1); } });
Object.defineProperty(window, "pageXOffset", { configurable: true, get: function () { return __kitaeScrollPos(0); } });
Object.defineProperty(window, "pageYOffset", { configurable: true, get: function () { return __kitaeScrollPos(1); } });
// The standard constructor set, exposed for scripts that feature-detect.
window.Event = Event;
window.EventTarget = EventTarget;
// The Node interface's constants (kitae's DOM: elements, text nodes).
window.Node = {
  ELEMENT_NODE: 1,
  ATTRIBUTE_NODE: 2,
  TEXT_NODE: 3,
  CDATA_SECTION_NODE: 4,
  PROCESSING_INSTRUCTION_NODE: 7,
  COMMENT_NODE: 8,
  DOCUMENT_NODE: 9,
  DOCUMENT_TYPE_NODE: 10,
  DOCUMENT_FRAGMENT_NODE: 11,
};
window.MutationObserver = MutationObserver;
window.IntersectionObserver = IntersectionObserver;
window.ResizeObserver = ResizeObserver;
window.CustomEvent = CustomEvent;
window.MouseEvent = MouseEvent;
window.KeyboardEvent = KeyboardEvent;
window.AbortController = AbortController;
window.AbortSignal = AbortSignal;
window.DOMException = DOMException;
window.URL = URL;
window.URLSearchParams = URLSearchParams;
window.Headers = Headers;
window.Request = Request;
window.Response = Response;
window.FormData = FormData;
window.structuredClone = structuredClone;
window.getComputedStyle = getComputedStyle;
window.XMLHttpRequest = XMLHttpRequest;
window.Worker = Worker;
// `window` is the persistent globalThis and BOOTSTRAP evaluates more than
// once per page (install + script phase), so these descriptors must be
// redefinable — `configurable: true` or the second run throws.
Object.defineProperty(window, "innerWidth", { configurable: true, get: function () { return __kitaeViewport(0); } });
Object.defineProperty(window, "innerHeight", { configurable: true, get: function () { return __kitaeViewport(1); } });
window.MessageChannel = MessageChannel;
window.performance = performance;
// ── Web storage (tier JS-6) ───────────────────────────────────────
// One `Storage` implementation backs both areas. The scope key is the
// document origin; an opaque origin serializes as "null" and every
// operation throws SecurityError, matching the spec's refusal. The session
// token names this browser: `sessionStorage` is private to it, while
// `localStorage` is shared per origin across the whole process (no
// persistence to disk — documented gap).
function __kitaeStorageOrigin() {
  var p = __kitaeUrlParts(__kitaePageUrl());
  if (!p) return "null";
  var scheme = p.protocol;
  if (scheme !== "http:" && scheme !== "https:" && scheme !== "ws:" && scheme !== "wss:" && scheme !== "ftp:") {
    return "null";
  }
  return scheme + "//" + p.hostname + (p.port ? ":" + p.port : "");
}
function __kitaeMakeStorage(session) {
  var scope = function () {
    var origin = __kitaeStorageOrigin();
    if (origin === "null") {
      throw new DOMException("Storage is not available for opaque origins", "SecurityError");
    }
    return origin;
  };
  // The Rust side answers `undefined` for "no such key" (`Option::None`
  // crosses as undefined); the spec's `getItem`/`key`/named getter answer
  // null, so the read path normalizes once, here.
  var read = function (key) {
    var v = __kitaeStorageGet(session, scope(), key);
    return v === undefined ? null : v;
  };
  // Methods sit on a prototype, as in the spec: named properties shadow
  // nothing on the object itself, so `Object.keys(storage)` lists stored
  // keys only and `getItem` keeps working after `storage.getItem = 1`.
  var proto = {
    getItem: function (k) { return read("" + k); },
    setItem: function (k, v) { __kitaeStorageSet(session, scope(), "" + k, "" + v); },
    removeItem: function (k) { __kitaeStorageRemove(session, scope(), "" + k); },
    clear: function () { __kitaeStorageClear(session, scope()); },
    key: function (i) {
      var n = Math.floor(+i);
      var k = __kitaeStorageKey(session, scope(), isFinite(n) ? n : 0);
      return k === undefined ? null : k;
    },
  };
  var api = Object.create(proto);
  return new Proxy(api, {
    get: function (t, p) {
      if (p === "length") return __kitaeStorageLength(session, scope());
      if (typeof p === "string" && !(p in t)) {
        var v = read(p);
        return v === null ? undefined : v;
      }
      return t[p];
    },
    set: function (t, p, v) {
      if (p === "length") return true;
      if (typeof p === "string") {
        __kitaeStorageSet(session, scope(), p, "" + v);
        return true;
      }
      t[p] = v;
      return true;
    },
    deleteProperty: function (t, p) {
      if (typeof p === "string" && p !== "length") __kitaeStorageRemove(session, scope(), p);
      return true;
    },
    has: function (t, p) {
      if (p === "length") return true;
      if (typeof p === "string" && !(p in t)) return read(p) !== null;
      return p in t;
    },
    ownKeys: function () {
      var out = [];
      var n = __kitaeStorageLength(session, scope());
      for (var i = 0; i < n; i++) {
        var k = __kitaeStorageKey(session, scope(), i);
        if (typeof k === "string") out.push(k);
      }
      return out;
    },
    getOwnPropertyDescriptor: function (t, p) {
      if (typeof p === "string" && p !== "length" && !(p in t)) {
        var v = read(p);
        if (v !== null) return { value: v, writable: true, enumerable: true, configurable: true };
        return undefined;
      }
      return Object.getOwnPropertyDescriptor(t, p);
    },
  });
}
window.localStorage = __kitaeMakeStorage("");
window.sessionStorage = __kitaeMakeStorage(__kitaeSessionId());
// ── Canvas 2D (tier JS-6) ────────────────────────────────────────────
// The backing store is a JS-side RGBA buffer per canvas element. The
// engine sees it through the `\0canvas:{node}` sentinel as a decoded
// raster; `__kitaeCanvasFlush` hands over dirty buffers once per poll
// tick, never once per draw call. Documented gaps: no text drawing
// (`fillText`/`strokeText` are no-ops, `measureText` estimates), no
// `clip`, no gradients/patterns/shadows/filters, no anti-aliasing
// (hard-edged spans), `lineCap`/`lineJoin`/`miterLimit`/dash fixed, and
// `globalCompositeOperation` is accepted but always source-over.
var __kitaeCvBytes = typeof Uint8ClampedArray === "function" ? Uint8ClampedArray : Uint8Array;
var __kitaeCvChars = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
function __kitaeB64Encode(bytes) {
  var out = "", n = bytes.length, i = 0, b;
  for (; i + 2 < n; i += 3) {
    b = (bytes[i] << 16) | (bytes[i + 1] << 8) | bytes[i + 2];
    out += __kitaeCvChars.charAt(b >> 18 & 63) + __kitaeCvChars.charAt(b >> 12 & 63) +
           __kitaeCvChars.charAt(b >> 6 & 63) + __kitaeCvChars.charAt(b & 63);
  }
  if (n - i === 1) {
    b = bytes[i] << 16;
    out += __kitaeCvChars.charAt(b >> 18 & 63) + __kitaeCvChars.charAt(b >> 12 & 63) + "==";
  } else if (n - i === 2) {
    b = (bytes[i] << 16) | (bytes[i + 1] << 8);
    out += __kitaeCvChars.charAt(b >> 18 & 63) + __kitaeCvChars.charAt(b >> 12 & 63) +
           __kitaeCvChars.charAt(b >> 6 & 63) + "=";
  }
  return out;
}
function __kitaeB64Decode(s) {
  var clean = ("" + s).replace(/[^A-Za-z0-9+/]/g, "");
  var out = [], acc = 0, bits = 0, v;
  for (var i = 0; i < clean.length; i++) {
    v = __kitaeCvChars.indexOf(clean.charAt(i));
    if (v < 0) continue;
    acc = (acc << 6) | v;
    bits += 6;
    if (bits >= 8) { bits -= 8; out.push((acc >> bits) & 255); }
  }
  return out;
}
// `btoa`/`atob` ride the same codecs: pages need them and the liquid
// (Latin-1) string convention is the spec's own.
function btoa(data) {
  var s = "" + data, bytes = [];
  for (var i = 0; i < s.length; i++) {
    var c = s.charCodeAt(i);
    if (c > 255) throw new DOMException("btoa: character out of range", "InvalidCharacterError");
    bytes.push(c);
  }
  return __kitaeB64Encode(bytes);
}
function atob(data) {
  var clean = ("" + data).replace(/[^A-Za-z0-9+/=]/g, "");
  if (clean.length % 4 !== 0) throw new DOMException("atob: invalid input", "InvalidCharacterError");
  var bytes = __kitaeB64Decode(clean), out = "";
  for (var i = 0; i < bytes.length; i++) out += String.fromCharCode(bytes[i]);
  return out;
}
var __kitaeCvNamed = {
  black: [0, 0, 0, 1], silver: [192, 192, 192, 1], gray: [128, 128, 128, 1],
  grey: [128, 128, 128, 1], white: [255, 255, 255, 1], maroon: [128, 0, 0, 1],
  red: [255, 0, 0, 1], purple: [128, 0, 128, 1], fuchsia: [255, 0, 255, 1],
  magenta: [255, 0, 255, 1], green: [0, 128, 0, 1], lime: [0, 255, 0, 1],
  olive: [128, 128, 0, 1], yellow: [255, 255, 0, 1], navy: [0, 0, 128, 1],
  blue: [0, 0, 255, 1], teal: [0, 128, 128, 1], aqua: [0, 255, 255, 1],
  cyan: [0, 255, 255, 1], orange: [255, 165, 0, 1], transparent: [0, 0, 0, 0],
};
// A CSS colour the 2D context can paint: hex (3/4/6/8 digits), rgb()/rgba()
// and the named subset above. Anything else is rejected, so an invalid
// assignment leaves the previous style in place (spec behaviour).
function __kitaeCvColor(v) {
  if (v === null || v === undefined) return null;
  var s = ("" + v).trim().toLowerCase();
  if (s.charAt(0) === '\x23') {
    var h = s.slice(1);
    var hex2 = function (t) { var q = parseInt(t, 16); return isFinite(q) ? q : -1; };
    var r, g, b, a = 255;
    if (h.length === 3 || h.length === 4) {
      r = hex2(h.charAt(0) + h.charAt(0));
      g = hex2(h.charAt(1) + h.charAt(1));
      b = hex2(h.charAt(2) + h.charAt(2));
      if (h.length === 4) a = hex2(h.charAt(3) + h.charAt(3));
    } else if (h.length === 6 || h.length === 8) {
      r = hex2(h.slice(0, 2));
      g = hex2(h.slice(2, 4));
      b = hex2(h.slice(4, 6));
      if (h.length === 8) a = hex2(h.slice(6, 8));
    } else {
      return null;
    }
    if (r < 0 || g < 0 || b < 0 || a < 0) return null;
    return [r, g, b, a / 255];
  }
  var m = /^rgba?\(([^)]*)\)$/.exec(s);
  if (m) {
    var parts = m[1].split(/[,\s\/]+/).filter(function (t) { return t !== ""; });
    if (parts.length < 3) return null;
    var num = function (t, scale) {
      t = t.trim();
      if (t.charAt(t.length - 1) === "%") return parseFloat(t) / 100 * scale;
      return parseFloat(t);
    };
    var rr = num(parts[0], 255), gg = num(parts[1], 255), bb = num(parts[2], 255);
    var aa = parts.length > 3 ? parseFloat(parts[3]) : 1;
    if (!isFinite(rr) || !isFinite(gg) || !isFinite(bb) || !isFinite(aa)) return null;
    return [rr, gg, bb, aa];
  }
  var named = __kitaeCvNamed[s];
  return named ? named.slice() : null;
}
function __kitaeCvApply(m, x, y) {
  return [m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5]];
}
// `m`, then `n` — canvas `transform()` composes on the right.
function __kitaeCvCompose(m, n) {
  return [
    m[0] * n[0] + m[2] * n[1],
    m[1] * n[0] + m[3] * n[1],
    m[0] * n[2] + m[2] * n[3],
    m[1] * n[2] + m[3] * n[3],
    m[0] * n[4] + m[2] * n[5] + m[4],
    m[1] * n[4] + m[3] * n[5] + m[5],
  ];
}
function __kitaeCanvasSize(token) {
  var w = parseInt(__kitaeGetAttr(token, "width") || "", 10);
  var h = parseInt(__kitaeGetAttr(token, "height") || "", 10);
  return [isFinite(w) && w > 0 ? w : 300, isFinite(h) && h > 0 ? h : 150];
}
function __kitaeCanvasRec(token) {
  var rec = __kitae.canvases[token];
  if (rec) return rec;
  var size = __kitaeCanvasSize(token);
  rec = __kitae.canvases[token] = {
    w: size[0],
    h: size[1],
    data: new __kitaeCvBytes(size[0] * size[1] * 4),
    dirty: true,
    ctx: null,
  };
  return rec;
}
// Spec: setting `width`/`height` (attribute or IDL) resets the rendering
// context to its default state and clears the buffer.
function __kitaeCanvasReset(token) {
  var rec = __kitae.canvases[token];
  if (!rec) return;
  var size = __kitaeCanvasSize(token);
  rec.w = size[0];
  rec.h = size[1];
  rec.data = new __kitaeCvBytes(size[0] * size[1] * 4);
  rec.dirty = true;
  if (rec.ctx && rec.ctx.__reset) rec.ctx.__reset();
}
function __kitaeCanvasSetSize(token, w, h) {
  if (w !== null && w !== undefined) __kitaeSetAttr(token, "width", "" + w);
  if (h !== null && h !== undefined) __kitaeSetAttr(token, "height", "" + h);
  __kitaeCanvasReset(token);
}
function __kitaeCanvasFlushOne(token) {
  var rec = __kitae.canvases[token];
  if (!rec || !rec.dirty) return;
  rec.dirty = false;
  __kitaeCanvasPublish(token, rec.w, rec.h, __kitaeB64Encode(rec.data));
}
function __kitaeCanvasFlush() {
  var store = __kitae.canvases;
  for (var t in store) __kitaeCanvasFlushOne(+t);
  return true;
}
// Scanline fill over device-space rings (the path stores device points:
// the transform applies where each point was added, as the spec has it).
// `evenOdd` selects the alternate rule; `put` receives every covered pixel.
function __kitaeCvFillRings(rec, rings, evenOdd, put) {
  var minY = Infinity, maxY = -Infinity, r, i, k;
  for (r = 0; r < rings.length; r++) {
    for (i = 0; i < rings[r].length; i++) {
      if (rings[r][i][1] < minY) minY = rings[r][i][1];
      if (rings[r][i][1] > maxY) maxY = rings[r][i][1];
    }
  }
  if (minY === Infinity) return;
  var y0 = Math.max(0, Math.floor(minY)), y1 = Math.min(rec.h - 1, Math.ceil(maxY));
  for (var y = y0; y <= y1; y++) {
    var sy = y + 0.5, xs = [], dirs = [];
    for (r = 0; r < rings.length; r++) {
      var ring = rings[r];
      for (i = 0; i < ring.length; i++) {
        var a = ring[i], b = ring[(i + 1) % ring.length];
        if (a[1] === b[1]) continue;
        var lo = a[1] < b[1] ? a : b, hi = a[1] < b[1] ? b : a;
        if (sy < lo[1] || sy >= hi[1]) continue;
        xs.push(lo[0] + (sy - lo[1]) / (hi[1] - lo[1]) * (hi[0] - lo[0]));
        dirs.push(a[1] < b[1] ? 1 : -1);
      }
    }
    if (!xs.length) continue;
    var idx = [];
    for (k = 0; k < xs.length; k++) idx.push(k);
    idx.sort(function (p, q) { return xs[p] - xs[q]; });
    var wind = 0;
    for (k = 0; k + 1 < idx.length; k++) {
      wind += evenOdd ? 1 : dirs[idx[k]];
      var inside = evenOdd ? wind % 2 === 1 : wind !== 0;
      if (!inside) continue;
      var from = Math.max(0, Math.ceil(xs[idx[k]] - 0.5));
      var to = Math.min(rec.w - 1, Math.floor(xs[idx[k + 1]] - 0.5));
      for (var x = from; x <= to; x++) put(x, y);
    }
  }
}
// Thick polylines: every pixel whose centre sits within `width/2` of a
// segment. Caps and joins are square-ish overlaps (documented).
function __kitaeCvStrokeRings(rec, rings, width, closed, put) {
  var hw = (width > 0 ? width : 1) / 2, hw2 = hw * hw;
  for (var r = 0; r < rings.length; r++) {
    var ring = rings[r];
    for (var i = 0; i + 1 < ring.length; i++) {
      var a = ring[i], b = ring[i + 1];
      var xmin = Math.max(0, Math.floor(Math.min(a[0], b[0]) - hw - 1));
      var xmax = Math.min(rec.w - 1, Math.ceil(Math.max(a[0], b[0]) + hw + 1));
      var ymin = Math.max(0, Math.floor(Math.min(a[1], b[1]) - hw - 1));
      var ymax = Math.min(rec.h - 1, Math.ceil(Math.max(a[1], b[1]) + hw + 1));
      var dx = b[0] - a[0], dy = b[1] - a[1], len2 = dx * dx + dy * dy;
      for (var y = ymin; y <= ymax; y++) {
        for (var x = xmin; x <= xmax; x++) {
          var px = x + 0.5 - a[0], py = y + 0.5 - a[1];
          var t = len2 > 0 ? (px * dx + py * dy) / len2 : 0;
          if (t < 0) t = 0;
          if (t > 1) t = 1;
          var qx = px - t * dx, qy = py - t * dy;
          if (qx * qx + qy * qy <= hw2) put(x, y);
        }
      }
    }
  }
}
function __kitaeCanvasContext(token, el, rec) {
  var state = {
    fillStyle: "rgb(0, 0, 0)",
    strokeStyle: "rgb(0, 0, 0)",
    fill: [0, 0, 0, 1],
    stroke: [0, 0, 0, 1],
    lineWidth: 1,
    globalAlpha: 1,
    m: [1, 0, 0, 1, 0, 0],
    path: [],
    cur: null,
  };
  var stack = [];
  var put = function (color) {
    return function (x, y) {
      var a = color[3] * state.globalAlpha;
      if (a <= 0) return;
      var d = rec.data, i = (y * rec.w + x) * 4;
      var da = d[i + 3] / 255;
      var oa = a + da * (1 - a);
      if (oa <= 0) { d[i] = 0; d[i + 1] = 0; d[i + 2] = 0; d[i + 3] = 0; return; }
      d[i] = Math.round((color[0] * a + d[i] * da * (1 - a)) / oa);
      d[i + 1] = Math.round((color[1] * a + d[i + 1] * da * (1 - a)) / oa);
      d[i + 2] = Math.round((color[2] * a + d[i + 2] * da * (1 - a)) / oa);
      d[i + 3] = Math.round(oa * 255);
    };
  };
  var clearPut = function (x, y) {
    var d = rec.data, i = (y * rec.w + x) * 4;
    d[i] = 0; d[i + 1] = 0; d[i + 2] = 0; d[i + 3] = 0;
  };
  var startSub = function () {
    state.cur = [];
    state.path.push(state.cur);
    return state.cur;
  };
  var addUser = function (x, y) {
    var p = __kitaeCvApply(state.m, +x || 0, +y || 0);
    addDevice(p[0], p[1]);
  };
  // `closePath` leaves the current point at the subpath start: a later
  // segment opens a fresh subpath there (spec).
  var addDevice = function (x, y) {
    if (!state.cur) { startSub(); }
    else if (state.cur.__closed) {
      var first = state.cur[0];
      var sub = startSub();
      sub.push([first[0], first[1]]);
    }
    state.cur.push([x, y]);
  };
  var ringOf = function (sub) {
    var ring = [], i;
    for (i = 0; i < sub.length; i++) ring.push(sub[i]);
    if (ring.length > 1 && sub.__closed) ring.push(ring[0]);
    return ring;
  };
  var rings = function () {
    var out = [];
    for (var i = 0; i < state.path.length; i++) {
      var sub = state.path[i];
      if (sub.length > 1) out.push(ringOf(sub));
      else if (sub.length === 1) out.push([sub[0], sub[0]]);
    }
    return out;
  };
  var rectSub = function (x, y, w, h) {
    var sub = [];
    sub.push(__kitaeCvApply(state.m, x, y));
    sub.push(__kitaeCvApply(state.m, x + w, y));
    sub.push(__kitaeCvApply(state.m, x + w, y + h));
    sub.push(__kitaeCvApply(state.m, x, y + h));
    sub.__closed = true;
    return sub;
  };
  var arcPoints = function (cx, cy, rx, ry, rot, s, e, ccw) {
    var out = [], steps = Math.max(8, Math.ceil(Math.abs(e - s) / 0.1963));
    var cr = Math.cos(rot), sr = Math.sin(rot);
    for (var i = 0; i <= steps; i++) {
      var t = ccw ? s - (s - e) * (i / steps) : s + (e - s) * (i / steps);
      var ux = rx * Math.cos(t), uy = ry * Math.sin(t);
      out.push(__kitaeCvApply(state.m, cx + ux * cr - uy * sr, cy + ux * sr + uy * cr));
    }
    return out;
  };
  var ctx = {
    canvas: el,
    __rec: rec,
    save: function () {
      stack.push({
        fillStyle: state.fillStyle, strokeStyle: state.strokeStyle,
        fill: state.fill.slice(), stroke: state.stroke.slice(),
        lineWidth: state.lineWidth, globalAlpha: state.globalAlpha, m: state.m.slice(),
      });
    },
    restore: function () {
      var s = stack.pop();
      if (!s) return;
      state.fillStyle = s.fillStyle;
      state.strokeStyle = s.strokeStyle;
      state.fill = s.fill;
      state.stroke = s.stroke;
      state.lineWidth = s.lineWidth;
      state.globalAlpha = s.globalAlpha;
      state.m = s.m;
    },
    __reset: function () {
      stack.length = 0;
      state.fillStyle = "rgb(0, 0, 0)";
      state.strokeStyle = "rgb(0, 0, 0)";
      state.fill = [0, 0, 0, 1];
      state.stroke = [0, 0, 0, 1];
      state.lineWidth = 1;
      state.globalAlpha = 1;
      state.m = [1, 0, 0, 1, 0, 0];
      state.path = [];
      state.cur = null;
    },
    translate: function (x, y) {
      state.m = __kitaeCvCompose(state.m, [1, 0, 0, 1, +x || 0, +y || 0]);
    },
    scale: function (x, y) {
      state.m = __kitaeCvCompose(state.m, [+x || 0, 0, 0, +y || 0, 0, 0]);
    },
    rotate: function (a) {
      var c = Math.cos(+a || 0), s = Math.sin(+a || 0);
      state.m = __kitaeCvCompose(state.m, [c, s, -s, c, 0, 0]);
    },
    transform: function (a, b, c, d, e, f) {
      state.m = __kitaeCvCompose(state.m, [+a || 0, +b || 0, +c || 0, +d || 0, +e || 0, +f || 0]);
    },
    setTransform: function (a, b, c, d, e, f) {
      state.m = a === undefined
        ? [1, 0, 0, 1, 0, 0]
        : [+a || 0, +b || 0, +c || 0, +d || 0, +e || 0, +f || 0];
    },
    resetTransform: function () { state.m = [1, 0, 0, 1, 0, 0]; },
    getTransform: function () {
      return { a: state.m[0], b: state.m[1], c: state.m[2], d: state.m[3], e: state.m[4], f: state.m[5] };
    },
    beginPath: function () { state.path = []; state.cur = null; },
    moveTo: function (x, y) { state.cur = null; addUser(x, y); },
    lineTo: function (x, y) { addUser(x, y); },
    closePath: function () { if (state.cur) state.cur.__closed = true; },
    rect: function (x, y, w, h) {
      var sub = rectSub(+x || 0, +y || 0, +w || 0, +h || 0);
      state.path.push(sub);
      state.cur = sub;
    },
    arc: function (cx, cy, r, s, e, ccw) {
      var pts = arcPoints(+cx || 0, +cy || 0, +r || 0, +r || 0, 0, +s || 0, +e || 0, !!ccw);
      if (!state.cur || state.cur.__closed) startSub();
      for (var i = 0; i < pts.length; i++) state.cur.push(pts[i]);
    },
    ellipse: function (cx, cy, rx, ry, rot, s, e, ccw) {
      if (e === undefined) { e = s; s = rot; rot = 0; }
      var pts = arcPoints(+cx || 0, +cy || 0, +rx || 0, +ry || 0, +rot || 0, +s || 0, +e || 0, !!ccw);
      if (!state.cur || state.cur.__closed) startSub();
      for (var i = 0; i < pts.length; i++) state.cur.push(pts[i]);
    },
    quadraticCurveTo: function (cpx, cpy, x, y) {
      if (!state.cur || !state.cur.length) addUser(0, 0);
      var p0 = state.cur[state.cur.length - 1];
      var c = __kitaeCvApply(state.m, +cpx || 0, +cpy || 0);
      var p = __kitaeCvApply(state.m, +x || 0, +y || 0);
      for (var i = 1; i <= 16; i++) {
        var t = i / 16, u = 1 - t;
        addDevice(u * u * p0[0] + 2 * u * t * c[0] + t * t * p[0],
                  u * u * p0[1] + 2 * u * t * c[1] + t * t * p[1]);
      }
    },
    bezierCurveTo: function (c1x, c1y, c2x, c2y, x, y) {
      if (!state.cur || !state.cur.length) addUser(0, 0);
      var p0 = state.cur[state.cur.length - 1];
      var a = __kitaeCvApply(state.m, +c1x || 0, +c1y || 0);
      var b = __kitaeCvApply(state.m, +c2x || 0, +c2y || 0);
      var c = __kitaeCvApply(state.m, +x || 0, +y || 0);
      for (var i = 1; i <= 24; i++) {
        var t = i / 24, u = 1 - t;
        addDevice(u * u * u * p0[0] + 3 * u * u * t * a[0] + 3 * u * t * t * b[0] + t * t * t * c[0],
                  u * u * u * p0[1] + 3 * u * u * t * a[1] + 3 * u * t * t * b[1] + t * t * t * c[1]);
      }
    },
    fill: function (rule) {
      __kitaeCvFillRings(rec, rings(), ("" + rule).toLowerCase() === "evenodd", put(state.fill));
      rec.dirty = true;
    },
    stroke: function () {
      __kitaeCvStrokeRings(rec, rings(), state.lineWidth, true, put(state.stroke));
      rec.dirty = true;
    },
    fillRect: function (x, y, w, h) {
      __kitaeCvFillRings(rec, [rectSub(+x || 0, +y || 0, +w || 0, +h || 0)], false, put(state.fill));
      rec.dirty = true;
    },
    strokeRect: function (x, y, w, h) {
      __kitaeCvStrokeRings(rec, [rectSub(+x || 0, +y || 0, +w || 0, +h || 0)], state.lineWidth, true, put(state.stroke));
      rec.dirty = true;
    },
    clearRect: function (x, y, w, h) {
      // Spec: clearRect ignores globalAlpha and the blend state.
      __kitaeCvFillRings(rec, [rectSub(+x || 0, +y || 0, +w || 0, +h || 0)], false, clearPut);
      rec.dirty = true;
    },
    // Text drawing needs the engine's text stack inside the raster path;
    // the methods stay callable so pages that label their canvas run.
    fillText: function () {},
    strokeText: function () {},
    measureText: function (t) {
      var m = /([0-9]+(?:\.[0-9]+)?)px/.exec("" + ctx.font);
      var size = m ? parseFloat(m[1]) : 10;
      return { width: ("" + (t === undefined ? "" : t)).length * size * 0.5 };
    },
    createImageData: function (w, h) {
      w = Math.max(0, Math.floor(+w) || 0);
      h = Math.max(0, Math.floor(+h) || 0);
      return { width: w, height: h, data: new __kitaeCvBytes(w * h * 4), colorSpace: "srgb" };
    },
    getImageData: function (x, y, w, h) {
      x = Math.floor(+x || 0);
      y = Math.floor(+y || 0);
      w = Math.max(0, Math.floor(+w) || 0);
      h = Math.max(0, Math.floor(+h) || 0);
      var out = new __kitaeCvBytes(w * h * 4), d = rec.data;
      for (var ry = 0; ry < h; ry++) {
        var sy = y + ry;
        if (sy < 0 || sy >= rec.h) continue;
        for (var rx = 0; rx < w; rx++) {
          var sx = x + rx;
          if (sx < 0 || sx >= rec.w) continue;
          var si = (sy * rec.w + sx) * 4, di = (ry * w + rx) * 4;
          out[di] = d[si];
          out[di + 1] = d[si + 1];
          out[di + 2] = d[si + 2];
          out[di + 3] = d[si + 3];
        }
      }
      return { width: w, height: h, data: out, colorSpace: "srgb" };
    },
    putImageData: function (img, dx, dy) {
      if (!img || !img.data) return;
      dx = Math.floor(+dx || 0);
      dy = Math.floor(+dy || 0);
      var w = img.width | 0, h = img.height | 0, d = rec.data;
      for (var ry = 0; ry < h; ry++) {
        var ty = dy + ry;
        if (ty < 0 || ty >= rec.h) continue;
        for (var rx = 0; rx < w; rx++) {
          var tx = dx + rx;
          if (tx < 0 || tx >= rec.w) continue;
          var si = (ry * w + rx) * 4, ti = (ty * rec.w + tx) * 4;
          d[ti] = img.data[si];
          d[ti + 1] = img.data[si + 1];
          d[ti + 2] = img.data[si + 2];
          d[ti + 3] = img.data[si + 3];
        }
      }
      rec.dirty = true;
    },
    drawImage: function (src) {
      var image = __kitaeCanvasSource(src);
      if (!image) return;
      var a = arguments;
      var sx = 0, sy = 0, sw = image.w, sh = image.h, dx, dy, dw, dh;
      if (a.length >= 9) {
        sx = +a[1] || 0;
        sy = +a[2] || 0;
        sw = +a[3] || 0;
        sh = +a[4] || 0;
        dx = +a[5] || 0;
        dy = +a[6] || 0;
        dw = +a[7] || 0;
        dh = +a[8] || 0;
      } else if (a.length >= 5) {
        dx = +a[1] || 0;
        dy = +a[2] || 0;
        dw = +a[3] || 0;
        dh = +a[4] || 0;
      } else {
        dx = +a[1] || 0;
        dy = +a[2] || 0;
        dw = sw;
        dh = sh;
      }
      if (dw === 0 || dh === 0 || sw === 0 || sh === 0) return;
      // The destination rect's device image is a parallelogram; pixels
      // inside it map back to source coordinates by one 2x2 solve.
      var c0 = __kitaeCvApply(state.m, dx, dy);
      var c1 = __kitaeCvApply(state.m, dx + dw, dy);
      var c3 = __kitaeCvApply(state.m, dx, dy + dh);
      var ax = c1[0] - c0[0], ay = c1[1] - c0[1];
      var bx = c3[0] - c0[0], by = c3[1] - c0[1];
      var det = ax * by - ay * bx;
      if (det === 0) return;
      var minX = Math.max(0, Math.floor(Math.min(c0[0], c1[0], c0[0] + bx, c1[0] + bx)));
      var maxX = Math.min(rec.w - 1, Math.ceil(Math.max(c0[0], c1[0], c0[0] + bx, c1[0] + bx)));
      var minY = Math.max(0, Math.floor(Math.min(c0[1], c1[1], c0[1] + by, c1[1] + by)));
      var maxY = Math.min(rec.h - 1, Math.ceil(Math.max(c0[1], c1[1], c0[1] + by, c1[1] + by)));
      var alpha = state.globalAlpha, d = rec.data, sd = image.data;
      for (var py = minY; py <= maxY; py++) {
        for (var px = minX; px <= maxX; px++) {
          var gx = px + 0.5 - c0[0], gy = py + 0.5 - c0[1];
          var u = (gx * by - gy * bx) / det;
          var v = (ax * gy - ay * gx) / det;
          if (u < 0 || u >= 1 || v < 0 || v >= 1) continue;
          var sxp = Math.min(image.w - 1, Math.max(0, Math.floor(sx + u * sw)));
          var syp = Math.min(image.h - 1, Math.max(0, Math.floor(sy + v * sh)));
          var si = (syp * image.w + sxp) * 4, ti = (py * rec.w + px) * 4;
          var sa = (sd[si + 3] / 255) * alpha;
          if (sa <= 0) continue;
          var da = d[ti + 3] / 255;
          var oa = sa + da * (1 - sa);
          if (oa <= 0) continue;
          d[ti] = Math.round((sd[si] * sa + d[ti] * da * (1 - sa)) / oa);
          d[ti + 1] = Math.round((sd[si + 1] * sa + d[ti + 1] * da * (1 - sa)) / oa);
          d[ti + 2] = Math.round((sd[si + 2] * sa + d[ti + 2] * da * (1 - sa)) / oa);
          d[ti + 3] = Math.round(oa * 255);
        }
      }
      rec.dirty = true;
    },
    toDataURL: function () {
      __kitaeCanvasFlushOne(token);
      return __kitaeCanvasDataUrl(token) || "data:,";
    },
    drawFocusIfNeeded: function () {},
    isPointInPath: function () { return false; },
    setLineDash: function () {},
    getLineDash: function () { return []; },
    createLinearGradient: function () {
      throw new DOMException("Canvas gradients are not supported by this engine", "NotSupportedError");
    },
    createRadialGradient: function () {
      throw new DOMException("Canvas gradients are not supported by this engine", "NotSupportedError");
    },
    createConicGradient: function () {
      throw new DOMException("Canvas gradients are not supported by this engine", "NotSupportedError");
    },
    createPattern: function () { return null; },
    font: "10px sans-serif",
    textAlign: "start",
    textBaseline: "alphabetic",
    direction: "ltr",
    lineCap: "butt",
    lineJoin: "miter",
    miterLimit: 10,
    lineDashOffset: 0,
    shadowBlur: 0,
    shadowColor: "rgba(0, 0, 0, 0)",
    shadowOffsetX: 0,
    shadowOffsetY: 0,
    globalCompositeOperation: "source-over",
    imageSmoothingEnabled: true,
    imageSmoothingQuality: "low",
    filter: "none",
    letterSpacing: "0px",
    wordSpacing: "0px",
  };
  Object.defineProperty(ctx, "fillStyle", {
    configurable: true,
    get: function () { return state.fillStyle; },
    set: function (v) {
      var c = __kitaeCvColor(v);
      if (!c) return;
      state.fill = c;
      state.fillStyle = "" + v;
    },
  });
  Object.defineProperty(ctx, "strokeStyle", {
    configurable: true,
    get: function () { return state.strokeStyle; },
    set: function (v) {
      var c = __kitaeCvColor(v);
      if (!c) return;
      state.stroke = c;
      state.strokeStyle = "" + v;
    },
  });
  Object.defineProperty(ctx, "lineWidth", {
    configurable: true,
    get: function () { return state.lineWidth; },
    set: function (v) { v = +v; if (isFinite(v) && v > 0) state.lineWidth = v; },
  });
  Object.defineProperty(ctx, "globalAlpha", {
    configurable: true,
    get: function () { return state.globalAlpha; },
    set: function (v) { v = +v; if (isFinite(v) && v >= 0 && v <= 1) state.globalAlpha = v; },
  });
  return ctx;
}
// A `drawImage` source: another canvas, an `ImageData`, or an `<img>`
// (whose pixels come across from the engine's decode cache).
function __kitaeCanvasSource(src) {
  if (!src) return null;
  if (src.__token && __kitae.canvases[src.__token]) {
    var rec = __kitae.canvases[src.__token];
    return { w: rec.w, h: rec.h, data: rec.data };
  }
  if (src.__token && src.tagName === "IMG") {
    var info = __kitaeImageRaster(src.getAttribute("src") || "");
    if (!info) return null;
    var parts = ("" + info).split("|");
    var w = parseInt(parts[0], 10), h = parseInt(parts[1], 10);
    if (!isFinite(w) || !isFinite(h)) return null;
    return { w: w, h: h, data: __kitaeB64Decode(parts[2] || "") };
  }
  if (typeof src.width === "number" && typeof src.height === "number" && src.data) {
    return { w: src.width, h: src.height, data: src.data };
  }
  return null;
}
// The `HTMLCanvasElement` surface a page actually touches.
function __kitaeCanvasElement(token, el) {
  Object.defineProperty(el, "width", {
    configurable: true,
    get: function () { return __kitaeCanvasSize(token)[0]; },
    set: function (v) { __kitaeCanvasSetSize(token, Math.max(0, Math.floor(+v) || 0), null); },
  });
  Object.defineProperty(el, "height", {
    configurable: true,
    get: function () { return __kitaeCanvasSize(token)[1]; },
    set: function (v) { __kitaeCanvasSetSize(token, null, Math.max(0, Math.floor(+v) || 0)); },
  });
  el.getContext = function (kind) {
    if (("" + kind).toLowerCase() !== "2d") return null;
    var rec = __kitaeCanvasRec(token);
    if (!rec.ctx) rec.ctx = __kitaeCanvasContext(token, el, rec);
    return rec.ctx;
  };
  el.toDataURL = function () {
    __kitaeCanvasFlushOne(token);
    return __kitaeCanvasDataUrl(token) || "data:,";
  };
}
function setTimeout(fn, ms) {
  var id = ++__kitae.seq;
  __kitae.timers[id] = typeof fn === "function" ? fn : new Function("" + fn);
  __kitaeTimerSet(id, +ms || 0, 0);
  return id;
}
function setInterval(fn, ms) {
  var id = ++__kitae.seq;
  __kitae.timers[id] = typeof fn === "function" ? fn : new Function("" + fn);
  __kitaeTimerSet(id, +ms || 0, 1);
  return id;
}
function clearTimeout(id) {
  // Spec: any value is accepted; null/undefined mean "nothing to clear".
  if (id === null || id === undefined) return;
  delete __kitae.timers[id];
  __kitaeTimerClear(id);
}
function clearInterval(id) {
  clearTimeout(id);
}
function fetch(input, init) {
  var req;
  try {
    req = input instanceof Request ? new Request(input, init) : new Request("" + input, init || {});
  } catch (e) {
    return Promise.reject(e);
  }
  var method = (req.method || "GET").toUpperCase();
  if (method !== "GET" && method !== "POST" && method !== "PUT" && method !== "PATCH" &&
      method !== "DELETE" && method !== "HEAD" && method !== "OPTIONS") {
    return Promise.reject(new TypeError("fetch: unsupported method " + method));
  }
  var headers = req.headers instanceof Headers ? req.headers : new Headers(req.headers);
  var body = "";
  if (req.body !== null && req.body !== undefined) {
    if (method === "GET" || method === "HEAD") {
      return Promise.reject(new TypeError("fetch: a GET/HEAD request cannot have a body"));
    }
    if (req.body instanceof FormData) {
      var boundary = "----KitaeFormBoundary" +
        Math.random().toString(36).slice(2) + Math.random().toString(36).slice(2);
      body = req.body.__multipart(boundary);
      if (!headers.has("content-type")) {
        headers.set("content-type", "multipart/form-data; boundary=" + boundary);
      }
    } else if (req.body instanceof URLSearchParams) {
      body = req.body.toString();
      if (!headers.has("content-type")) {
        headers.set("content-type", "application/x-www-form-urlencoded");
      }
    } else {
      body = "" + req.body;
      if (!headers.has("content-type")) headers.set("content-type", "text/plain;charset=UTF-8");
    }
  }
  var id = ++__kitae.seq;
  return new Promise(function (res, rej) {
    var signal = req.signal;
    if (signal && signal.aborted) { rej(signal.reason); return; }
    __kitae.res[id] = { res: res, rej: rej };
    if (signal && typeof signal.addEventListener === "function") {
      signal.addEventListener("abort", function () {
        // The pool job keeps running (nothing cancels it mid-flight in
        // v0) — the promise rejects and the late settle finds no entry.
        if (__kitae.res[id]) { delete __kitae.res[id]; rej(signal.reason); }
      });
    }
    __kitaeFetchStart(id, req.url, method, headers.__blob(), body);
  });
}
"#;

// ---------------------------------------------------------------------------
// Script collection, fragment grafting, selectors, small DOM helpers
// ---------------------------------------------------------------------------

/// All `<script>` blocks in document order: inline text, or `src` fetched
/// synchronously (engine cache + S-1 origin gate, like stylesheets).
/// `type="module"` blocks are routed to the module list instead — external
/// entries fetch through the page's module budget; inline entries get a
/// canonical name anchored to the page URL (URL joins drop the fragment,
/// so relative imports in an inline module resolve like the page's own).
#[allow(clippy::type_complexity)]
fn collect_scripts(
    dom: &mut Dom,
    root: NodeId,
    base_url: &str,
    stats: &mut ScriptPhaseStats,
    modules: &PageModuleFetcher,
) -> (Vec<String>, Vec<ModuleScript>) {
    let mut order: Vec<NodeId> = Vec::new();
    dom.walk(root, &mut |n| {
        if dom.tag(n) == "script" {
            order.push(n);
        }
    });
    stats.scripts_seen = order.len();
    let mut out = Vec::new();
    let mut module_out: Vec<ModuleScript> = Vec::new();
    let mut total_bytes = 0usize;
    for node in order {
        if out.len() >= MAX_SCRIPTS || total_bytes >= MAX_SCRIPT_TOTAL_BYTES {
            break;
        }
        let attrs = match dom.element(node) {
            Some(e) => e.attributes.clone(),
            None => continue,
        };
        let type_name = attrs
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("type"))
            .map(|(_, v)| v.to_ascii_lowercase())
            .unwrap_or_default();
        let src = attrs
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("src"))
            .map(|(_, v)| v.clone());
        if type_name == "module" {
            stats.modules_seen += 1;
            match src {
                Some(src) => {
                    let url = kitae_net::resolve(base_url, &src).unwrap_or(src);
                    match modules.load(&url) {
                        Ok(source) => module_out.push(ModuleScript { name: url, source }),
                        Err(_) => stats.modules_failed += 1, // loader logged
                    }
                }
                None => {
                    let text: String = dom
                        .children(node)
                        .iter()
                        .map(|c| dom.node(*c).text.clone())
                        .collect();
                    module_out.push(ModuleScript {
                        name: format!("{base_url}#kitae-inline-{}", module_out.len()),
                        source: text,
                    });
                }
            }
            continue;
        }
        if !type_name.is_empty()
            && type_name != "text/javascript"
            && type_name != "application/javascript"
        {
            continue; // importmap, data blocks: not v0 runtime code
        }
        match src {
            Some(src) => {
                let url = kitae_net::resolve(base_url, &src).unwrap_or(src);
                match kitae_net::fetch_cached(&url) {
                    Ok(resp) if resp.body.len() <= MAX_SCRIPT_BYTES => {
                        total_bytes += resp.body.len();
                        out.push(String::from_utf8_lossy(&resp.body).into_owned());
                    }
                    Ok(resp) => {
                        stats.scripts_failed += 1;
                        shared_log_entry(&url, format!("oversized: {} bytes", resp.body.len()));
                    }
                    Err(e) => {
                        stats.scripts_failed += 1;
                        shared_log_entry(&url, e.to_string());
                    }
                }
            }
            None => {
                let text: String = dom
                    .children(node)
                    .iter()
                    .map(|c| dom.node(*c).text.clone())
                    .collect();
                out.push(text);
            }
        }
    }
    (out, module_out)
}

/// A `<script type="module">` block: canonical module name + source.
struct ModuleScript {
    name: String,
    source: String,
}

fn shared_log_entry(url: &str, message: String) {
    eprintln!("[kitae page-js] {url}: {message}");
}

/// Parse `html` in the parent element's context and graft the nodes under
/// `parent` (innerHTML/document.write writes land in the live tree).
fn graft_fragment(dom: &mut Dom, parent: NodeId, html: &str) {
    let (frag, frag_root) = kitae_html::parse_fragment(html, dom.tag(parent));
    let children: Vec<NodeId> = frag.children(frag_root).to_vec();
    for child in children {
        copy_subtree(dom, &frag, child, parent);
    }
}

fn copy_subtree(dst: &mut Dom, src: &Dom, node: NodeId, under: NodeId) {
    match src.node(node).kind {
        NodeKind::Element => {
            let (tag, attrs) = match src.element(node) {
                Some(e) => (e.tag.clone(), e.attributes.clone()),
                None => return,
            };
            let copy = dst.create_element(&tag, attrs);
            dst.append(under, copy);
            for child in src.children(node).to_vec() {
                copy_subtree(dst, src, child, copy);
            }
        }
        NodeKind::Text => {
            let text = src.node(node).text.clone();
            if !text.is_empty() {
                let copy = dst.create_text(text);
                dst.append(under, copy);
            }
        }
    }
}

/// All descendant text, concatenated (textContent's getter).
fn concat_text(dom: &Dom, node: NodeId) -> String {
    let mut out = String::new();
    if dom.node(node).kind == NodeKind::Text {
        out.push_str(&dom.node(node).text);
        return out;
    }
    for child in dom.children(node) {
        out.push_str(&concat_text(dom, *child));
    }
    out
}

fn find_child_tag(dom: &Dom, parent: NodeId, tag: &str) -> Option<NodeId> {
    dom.children(parent)
        .iter()
        .find(|c| dom.tag(**c) == tag)
        .copied()
}

/// One `<option>`'s value: its `value` attribute, else its trimmed text
/// (browsers derive the value from the label when the attribute is absent).
fn option_value(dom: &Dom, option: NodeId) -> String {
    dom_attr(dom, option, "value")
        .map(|s| s.to_string())
        .unwrap_or_else(|| concat_text(dom, option).trim().to_string())
}

/// A `<select>`'s current value: the first `selected` option's value,
/// else the first option's (browsers auto-select the first option).
fn select_value(dom: &Dom, select: NodeId) -> String {
    let mut first: Option<String> = None;
    for &c in dom.children(select) {
        if dom.tag(c) != "option" {
            continue;
        }
        let v = option_value(dom, c);
        if dom_attr(dom, c, "selected").is_some() {
            return v;
        }
        if first.is_none() {
            first = Some(v);
        }
    }
    first.unwrap_or_default()
}

/// Point the `selected` marker at the option matching `value`; the first
/// option when nothing matches (select.value's setter).
fn set_select_value(dom: &mut Dom, select: NodeId, value: &str) {
    let opts: Vec<NodeId> = dom
        .children(select)
        .iter()
        .copied()
        .filter(|&c| dom.tag(c) == "option")
        .collect();
    let mut matched = false;
    for &o in &opts {
        if option_value(dom, o) == value {
            dom.set_attr(o, "selected", "");
            matched = true;
        } else {
            dom.remove_attr(o, "selected");
        }
    }
    if !matched {
        for &o in opts.iter().skip(1) {
            dom.remove_attr(o, "selected");
        }
        if let Some(&f) = opts.first() {
            dom.set_attr(f, "selected", "");
        }
    }
}

/// A readable computed-style subset as CSS text — the `getComputedStyle`
/// getPropertyValue read path. v0 serializes the properties scripts read
/// most, in the values the cascade computed them to; used-pixel resolution
/// for width/height is layered on JS-side from the published geometry
/// (painted boxes), so a percentage width still answers with its used px.
pub(crate) fn computed_css(style: &kitae_style::ComputedStyle) -> String {
    use kitae_style::{BorderStyle, Display, Position, Visibility};
    fn num(v: f32) -> String {
        if (v - v.round()).abs() < 1e-4 {
            format!("{}", v.round() as i64)
        } else {
            format!("{v}")
        }
    }
    fn term(t: &kitae_css::LenTerm) -> String {
        if t.pct == 0.0 {
            format!("{}px", num(t.px))
        } else if t.px == 0.0 {
            format!("{}%", num(t.pct))
        } else {
            format!("calc({}px + {}%)", num(t.px), num(t.pct))
        }
    }
    fn len(l: kitae_css::Len) -> String {
        use kitae_css::Len::*;
        match l {
            Px(v) => format!("{}px", num(v)),
            Sum { px, pct } => {
                if pct == 0.0 {
                    format!("{}px", num(px))
                } else if px == 0.0 {
                    format!("{}%", num(pct))
                } else {
                    format!("calc({}px + {}%)", num(px), num(pct))
                }
            }
            Min(ts, n) => {
                let parts: Vec<String> = ts.iter().take(n as usize).map(term).collect();
                format!("min({})", parts.join(", "))
            }
            Max(ts, n) => {
                let parts: Vec<String> = ts.iter().take(n as usize).map(term).collect();
                format!("max({})", parts.join(", "))
            }
            Clamp(ts) => {
                let parts: Vec<String> = ts.iter().map(term).collect();
                format!("clamp({})", parts.join(", "))
            }
        }
    }
    fn size(s: &kitae_css::SizeVal) -> String {
        use kitae_css::SizeVal::*;
        match s {
            Len(l) => len(*l),
            MinContent => "min-content".into(),
            MaxContent => "max-content".into(),
            FitContent(None) => "fit-content".into(),
            FitContent(Some(l)) => format!("fit-content({})", len(*l)),
        }
    }
    fn color(c: u32) -> String {
        let (r, g, b, a) = ((c >> 24) & 255, (c >> 16) & 255, (c >> 8) & 255, c & 255);
        if a == 255 {
            format!("rgb({r}, {g}, {b})")
        } else {
            format!("rgba({r}, {g}, {b}, {})", num(a as f32 / 255.0))
        }
    }
    fn border_style(s: BorderStyle) -> &'static str {
        match s {
            BorderStyle::None => "none",
            BorderStyle::Hidden => "hidden",
            BorderStyle::Solid => "solid",
            BorderStyle::Dashed => "dashed",
            BorderStyle::Dotted => "dotted",
            BorderStyle::Double => "double",
            _ => "solid",
        }
    }
    let display = match style.display {
        Display::None => "none",
        Display::Block => "block",
        Display::Inline => "inline",
        Display::InlineBlock => "inline-block",
        Display::ListItem => "list-item",
        Display::Flex => "flex",
        Display::Grid => "grid",
        Display::Table => "table",
        Display::TableRow => "table-row",
        Display::TableCell => "table-cell",
        Display::TableRowGroup => "table-row-group",
        Display::TableCaption => "table-caption",
        Display::TableColumn => "table-column",
        Display::TableColumnGroup => "table-column-group",
    };
    let position = match style.position {
        Position::Static => "static",
        Position::Relative => "relative",
        Position::Absolute => "absolute",
        Position::Fixed => "fixed",
        Position::Sticky => "sticky",
    };
    let visibility = match style.visibility {
        Visibility::Visible => "visible",
        Visibility::Hidden => "hidden",
        Visibility::Collapse => "collapse",
    };
    let text_align = match style.text_align {
        kitae_style::TextAlign::Left => "left",
        kitae_style::TextAlign::Center => "center",
        kitae_style::TextAlign::Right => "right",
    };
    let white_space = if style.white_space_nowrap {
        "nowrap"
    } else if style.white_space_pre && style.white_space_wrap {
        "pre-wrap"
    } else if style.white_space_pre {
        "pre"
    } else {
        "normal"
    };
    let mut decoration: Vec<&str> = Vec::new();
    if style.underline {
        decoration.push("underline");
    }
    if style.overline {
        decoration.push("overline");
    }
    if style.line_through {
        decoration.push("line-through");
    }
    let decoration = if decoration.is_empty() {
        "none".to_string()
    } else {
        decoration.join(" ")
    };
    let mut out = String::with_capacity(320);
    out.push_str(&format!(
        "display: {display}; position: {position}; visibility: {visibility}; \
         color: {}; background-color: {}; opacity: {}; ",
        color(style.color),
        color(style.background),
        num(style.opacity)
    ));
    out.push_str(&format!(
        "font-size: {}px; font-weight: {}; font-style: {}; ",
        num(style.font_size),
        style.font_weight,
        if style.italic { "italic" } else { "normal" }
    ));
    if !style.font_family.is_empty() {
        out.push_str(&format!("font-family: {}; ", style.font_family));
    }
    out.push_str(&format!(
        "text-align: {text_align}; line-height: {}; white-space: {white_space}; \
         text-decoration: {decoration}; ",
        num(style.line_height)
    ));
    if let Some(w) = &style.width {
        out.push_str(&format!("width: {}; ", size(w)));
    }
    if let Some(h) = &style.height {
        out.push_str(&format!("height: {}; ", size(h)));
    }
    out.push_str(&format!(
        "margin: {} {} {} {}; padding: {} {} {} {}; ",
        len(style.margin.top),
        len(style.margin.right),
        len(style.margin.bottom),
        len(style.margin.left),
        len(style.padding.top),
        len(style.padding.right),
        len(style.padding.bottom),
        len(style.padding.left)
    ));
    for (name, side) in [
        ("top", &style.border.top),
        ("right", &style.border.right),
        ("bottom", &style.border.bottom),
        ("left", &style.border.left),
    ] {
        if !matches!(side.style, BorderStyle::None | BorderStyle::Hidden) {
            out.push_str(&format!(
                "border-{name}-width: {}; border-{name}-style: {}; ",
                len(side.width),
                border_style(side.style)
            ));
        }
    }
    out
}

/// `a: b; c: d` → map (declaration-style inline style).
fn inline_style(raw: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for decl in raw.split(';') {
        let Some((name, value)) = decl.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim().to_string();
        if !name.is_empty() {
            out.insert(name, value);
        }
    }
    out
}

enum ClassOp {
    Add,
    Remove,
}

fn class_op(dom: &mut Dom, node: NodeId, op: &str, name: &str) -> bool {
    let current = dom_attr(dom, node, "class").unwrap_or_default().to_string();
    let mut classes: Vec<String> = current
        .split_ascii_whitespace()
        .map(str::to_string)
        .collect();
    let had = classes.iter().any(|c| c == name);
    match op {
        "add" => {
            if !had {
                classes.push(name.to_string());
                let _ = dom.set_attr(node, "class", &classes.join(" "));
            }
            had
        }
        "remove" => {
            if had {
                classes.retain(|c| c != name);
                let _ = dom.set_attr(node, "class", &classes.join(" "));
            }
            !had
        }
        "toggle" => {
            if had {
                classes.retain(|c| c != name);
                let _ = dom.set_attr(node, "class", &classes.join(" "));
            } else {
                classes.push(name.to_string());
                let _ = dom.set_attr(node, "class", &classes.join(" "));
            }
            !had
        }
        _ => had, // "contains"
    }
}

// ---------------------------------------------------------------------------
// Selectors: comma lists over complex selectors — compounds (`tag`, `#id`,
// `.class`, `*`, `[attr …]`, `:first-child`/`:last-child`/`:nth-child`/
// `:not(...)`) joined by descendant, child (`>`), next-sibling (`+`), and
// subsequent-sibling (`~`) combinators. Unknown syntax fails the selector
// (matches nothing) rather than approximating a match.
// ---------------------------------------------------------------------------

/// Hard caps so a hostile selector cannot recurse or scan unboundedly.
const MAX_SELECTOR_LEN: usize = 512;
const MAX_SELECTOR_PARTS: usize = 32;
const MAX_SELECTOR_ALTERNATIVES: usize = 32;

#[derive(Debug, Clone, PartialEq)]
enum AttrTest {
    Exists,
    Equals(String),
    /// `[attr~="word"]` — the value is one whitespace-separated word.
    Includes(String),
    /// `[attr|="prefix"]` — equals, or equals followed by `-`.
    DashMatch(String),
    Prefix(String),
    Suffix(String),
    Contains(String),
}

#[derive(Debug, Clone, PartialEq)]
enum Pseudo {
    FirstChild,
    LastChild,
    /// `:nth-child(an+b)`: (a, b) with a 1-based position.
    NthChild(i32, i32),
    Not(Box<Compound>),
}

#[derive(Debug, Default, Clone, PartialEq)]
struct Compound {
    tag: Option<String>,
    id: Option<String>,
    classes: Vec<String>,
    attrs: Vec<(String, AttrTest)>,
    pseudos: Vec<Pseudo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Combinator {
    /// The relation to the compound on its left; the first part's value is
    /// a placeholder that matching never consults.
    Descendant,
    Child,
    NextSibling,
    SubsequentSibling,
}

#[derive(Debug, Clone)]
struct Complex {
    parts: Vec<(Combinator, Compound)>,
}

fn parse_selector_list(selector: &str) -> Option<Vec<Complex>> {
    let selector = selector.trim();
    if selector.is_empty() || selector.len() > MAX_SELECTOR_LEN {
        return None;
    }
    let mut alternatives = split_alternatives(selector);
    if alternatives.len() > MAX_SELECTOR_ALTERNATIVES {
        return None;
    }
    let mut out = Vec::with_capacity(alternatives.len());
    for alt in alternatives.drain(..) {
        let complex = parse_complex(&alt)?;
        out.push(complex);
    }
    Some(out)
}

/// Commas inside quotes or brackets do not split alternatives.
fn split_alternatives(selector: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut current = String::new();
    for c in selector.chars() {
        match quote {
            Some(q) => {
                current.push(c);
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '"' | '\'' => {
                    quote = Some(c);
                    current.push(c);
                }
                '[' => {
                    depth += 1;
                    current.push(c);
                }
                ']' => {
                    depth = depth.saturating_sub(1);
                    current.push(c);
                }
                ',' if depth == 0 => out.push(std::mem::take(&mut current)),
                _ => current.push(c),
            },
        }
    }
    out.push(current);
    out.retain(|s| !s.trim().is_empty());
    out
}

fn skip_ws(chars: &[char], i: &mut usize) {
    while *i < chars.len() && chars[*i].is_ascii_whitespace() {
        *i += 1;
    }
}

fn read_name(chars: &[char], i: &mut usize) -> String {
    let start = *i;
    while *i < chars.len() && (chars[*i].is_ascii_alphanumeric() || matches!(chars[*i], '-' | '_'))
    {
        *i += 1;
    }
    chars[start..*i]
        .iter()
        .collect::<String>()
        .trim()
        .to_string()
}

fn parse_complex(spec: &str) -> Option<Complex> {
    let chars: Vec<char> = spec.chars().collect();
    let mut i = 0;
    let mut parts: Vec<(Combinator, Compound)> = Vec::new();
    let mut pending = Combinator::Descendant;
    loop {
        skip_ws(&chars, &mut i);
        if i >= chars.len() {
            return None; // dangling combinator
        }
        let compound = parse_compound_at(&chars, &mut i)?;
        parts.push((pending, compound));
        if parts.len() > MAX_SELECTOR_PARTS {
            return None;
        }
        skip_ws(&chars, &mut i);
        if i >= chars.len() {
            break;
        }
        pending = match chars[i] {
            '>' => {
                i += 1;
                Combinator::Child
            }
            '+' => {
                i += 1;
                Combinator::NextSibling
            }
            '~' => {
                i += 1;
                Combinator::SubsequentSibling
            }
            _ => Combinator::Descendant,
        };
    }
    Some(Complex { parts })
}

fn parse_compound_at(chars: &[char], i: &mut usize) -> Option<Compound> {
    let mut out = Compound::default();
    let mut any = false;
    // Leading type selector or universal.
    let start = *i;
    while *i < chars.len()
        && (chars[*i].is_ascii_alphanumeric() || chars[*i] == '-' || chars[*i] == '_')
    {
        *i += 1;
    }
    if *i > start {
        let tag: String = chars[start..*i].iter().collect();
        if tag != "*" {
            out.tag = Some(tag.to_ascii_lowercase());
        }
        any = true;
    }
    while *i < chars.len() {
        match chars[*i] {
            '#' => {
                *i += 1;
                let id = read_name(chars, i);
                if id.is_empty() {
                    return None;
                }
                out.id = Some(id);
                any = true;
            }
            '.' => {
                *i += 1;
                let class = read_name(chars, i);
                if class.is_empty() {
                    return None;
                }
                out.classes.push(class);
                any = true;
            }
            '[' => {
                parse_attr_test(chars, i, &mut out)?;
                any = true;
            }
            ':' => {
                let pseudo = parse_pseudo(chars, i)?;
                out.pseudos.push(pseudo);
                any = true;
            }
            _ => break,
        }
    }
    if any {
        Some(out)
    } else {
        None
    }
}

fn parse_attr_test(chars: &[char], i: &mut usize, out: &mut Compound) -> Option<()> {
    *i += 1; // '['
    skip_ws(chars, i);
    let name = read_name(chars, i);
    if name.is_empty() {
        return None;
    }
    skip_ws(chars, i);
    let test = match chars.get(*i) {
        Some(']') => AttrTest::Exists,
        Some('=') => {
            *i += 1;
            AttrTest::Equals(read_value(chars, i)?)
        }
        Some(op @ ('^' | '$' | '*' | '~' | '|')) => {
            *i += 1;
            if chars.get(*i) != Some(&'=') {
                return None;
            }
            *i += 1;
            let value = read_value(chars, i)?;
            match op {
                '^' => AttrTest::Prefix(value),
                '$' => AttrTest::Suffix(value),
                '*' => AttrTest::Contains(value),
                '~' => AttrTest::Includes(value),
                _ => AttrTest::DashMatch(value),
            }
        }
        _ => return None,
    };
    skip_ws(chars, i);
    if chars.get(*i) != Some(&']') {
        // The `i`/`s` case flags are not v0 syntax: reject, never mismatch.
        return None;
    }
    *i += 1;
    out.attrs.push((name.to_ascii_lowercase(), test));
    Some(())
}

fn read_value(chars: &[char], i: &mut usize) -> Option<String> {
    skip_ws(chars, i);
    match chars.get(*i) {
        Some(&q @ ('"' | '\'')) => {
            *i += 1;
            let start = *i;
            while *i < chars.len() && chars[*i] != q {
                *i += 1;
            }
            let value: String = chars.get(start..*i)?.iter().collect();
            if chars.get(*i) != Some(&q) {
                return None;
            }
            *i += 1;
            Some(value)
        }
        Some(_) => {
            let start = *i;
            while *i < chars.len() && chars[*i] != ']' && !chars[*i].is_ascii_whitespace() {
                *i += 1;
            }
            Some(
                chars[start..*i]
                    .iter()
                    .collect::<String>()
                    .trim()
                    .to_string(),
            )
        }
        None => None,
    }
}

fn parse_pseudo(chars: &[char], i: &mut usize) -> Option<Pseudo> {
    *i += 1; // ':'
    if chars.get(*i) == Some(&':') {
        return None; // pseudo-elements are out
    }
    let name = read_name(chars, i);
    match name.as_str() {
        "first-child" => Some(Pseudo::FirstChild),
        "last-child" => Some(Pseudo::LastChild),
        "nth-child" => {
            if chars.get(*i) != Some(&'(') {
                return None;
            }
            *i += 1;
            let start = *i;
            while *i < chars.len() && chars[*i] != ')' {
                *i += 1;
            }
            let raw: String = chars.get(start..*i)?.iter().collect();
            if chars.get(*i) != Some(&')') {
                return None;
            }
            *i += 1;
            let (a, b) = parse_nth(&raw)?;
            Some(Pseudo::NthChild(a, b))
        }
        "not" => {
            if chars.get(*i) != Some(&'(') {
                return None;
            }
            *i += 1;
            skip_ws(chars, i);
            let inner = parse_compound_at(chars, i)?;
            if chars.get(*i) != Some(&')') {
                return None;
            }
            *i += 1;
            Some(Pseudo::Not(Box::new(inner)))
        }
        _ => None,
    }
}

/// `an+b`: `odd`, `even`, `3`, `2n`, `2n+1`, `-n+3` (and `+n-2`).
fn parse_nth(raw: &str) -> Option<(i32, i32)> {
    let s = raw.trim().to_ascii_lowercase();
    match s.as_str() {
        "odd" => return Some((2, 1)),
        "even" => return Some((2, 0)),
        _ => {}
    }
    let Some(n_pos) = s.find('n') else {
        let b: i32 = s.trim().parse().ok()?;
        return Some((0, b));
    };
    let a = match s[..n_pos].trim() {
        "" => 1,
        "+" => 1,
        "-" => -1,
        other => other.parse().ok()?,
    };
    let rest = s[n_pos + 1..].trim();
    let b = match rest {
        "" => 0,
        "+" | "-" => return None,
        r => {
            let magnitude: i32 = r
                .strip_prefix('+')
                .or_else(|| r.strip_prefix('-'))
                .unwrap_or(r)
                .trim()
                .parse()
                .ok()?;
            if r.starts_with('-') {
                -magnitude
            } else {
                magnitude
            }
        }
    };
    Some((a, b))
}

fn attr_test_matches(test: &AttrTest, actual: &str) -> bool {
    match test {
        AttrTest::Exists => true,
        AttrTest::Equals(v) => actual == v,
        AttrTest::Includes(w) => {
            !w.is_empty() && actual.split_ascii_whitespace().any(|part| part == w)
        }
        AttrTest::DashMatch(v) => {
            actual == v
                || actual
                    .strip_prefix(v.as_str())
                    .is_some_and(|rest| rest.starts_with('-'))
        }
        AttrTest::Prefix(v) => !v.is_empty() && actual.starts_with(v.as_str()),
        AttrTest::Suffix(v) => !v.is_empty() && actual.ends_with(v.as_str()),
        AttrTest::Contains(v) => !v.is_empty() && actual.contains(v.as_str()),
    }
}

/// 1-based position among the parent's element children (root: 1).
fn element_index(dom: &Dom, node: NodeId) -> usize {
    let Some(parent) = dom.node(node).parent else {
        return 1;
    };
    let mut idx = 0;
    for child in dom.children(parent) {
        if dom.node(*child).kind == NodeKind::Element {
            idx += 1;
            if *child == node {
                return idx;
            }
        }
    }
    idx
}

fn is_last_element_child(dom: &Dom, node: NodeId) -> bool {
    let Some(parent) = dom.node(node).parent else {
        return true;
    };
    let mut last = None;
    for child in dom.children(parent) {
        if dom.node(*child).kind == NodeKind::Element {
            last = Some(*child);
        }
    }
    last == Some(node)
}

fn prev_element_sibling(dom: &Dom, node: NodeId) -> Option<NodeId> {
    let parent = dom.node(node).parent?;
    let mut prev = None;
    for child in dom.children(parent) {
        if *child == node {
            return prev;
        }
        if dom.node(*child).kind == NodeKind::Element {
            prev = Some(*child);
        }
    }
    None
}

fn matches_compound(dom: &Dom, node: NodeId, compound: &Compound) -> bool {
    if dom.node(node).kind != NodeKind::Element {
        return false;
    }
    if let Some(tag) = &compound.tag {
        if dom.tag(node) != tag.as_str() {
            return false;
        }
    }
    if let Some(id) = &compound.id {
        if dom_attr(dom, node, "id") != Some(id.as_str()) {
            return false;
        }
    }
    for class in &compound.classes {
        match dom_attr(dom, node, "class") {
            Some(classes) => {
                if !classes
                    .split_ascii_whitespace()
                    .any(|c| c == class.as_str())
                {
                    return false;
                }
            }
            None => return false,
        }
    }
    for (name, test) in &compound.attrs {
        let Some(actual) = dom_attr(dom, node, name) else {
            return false;
        };
        if !attr_test_matches(test, actual) {
            return false;
        }
    }
    for pseudo in &compound.pseudos {
        let ok = match pseudo {
            Pseudo::FirstChild => element_index(dom, node) == 1,
            Pseudo::LastChild => is_last_element_child(dom, node),
            Pseudo::NthChild(a, b) => {
                let idx = element_index(dom, node) as i32;
                let delta = idx - b;
                if *a == 0 {
                    delta == 0
                } else {
                    delta % a == 0 && delta / a >= 0
                }
            }
            Pseudo::Not(inner) => !matches_compound(dom, node, inner),
        };
        if !ok {
            return false;
        }
    }
    true
}

/// Match one complex selector against `node`, right to left.
fn matches_seq(dom: &Dom, node: NodeId, parts: &[(Combinator, Compound)]) -> bool {
    let Some((_, compound)) = parts.last() else {
        return false;
    };
    if !matches_compound(dom, node, compound) {
        return false;
    }
    if parts.len() == 1 {
        return true;
    }
    let prefix = &parts[..parts.len() - 1];
    match parts.last().unwrap().0 {
        Combinator::Child => dom
            .node(node)
            .parent
            .is_some_and(|p| matches_seq(dom, p, prefix)),
        Combinator::Descendant => {
            let mut anc = dom.node(node).parent;
            while let Some(a) = anc {
                if matches_seq(dom, a, prefix) {
                    return true;
                }
                anc = dom.node(a).parent;
            }
            false
        }
        Combinator::NextSibling => {
            prev_element_sibling(dom, node).is_some_and(|s| matches_seq(dom, s, prefix))
        }
        Combinator::SubsequentSibling => {
            let mut s = prev_element_sibling(dom, node);
            while let Some(sb) = s {
                if matches_seq(dom, sb, prefix) {
                    return true;
                }
                s = prev_element_sibling(dom, sb);
            }
            false
        }
    }
}

fn matches_parsed(dom: &Dom, node: NodeId, list: &[Complex]) -> bool {
    list.iter()
        .any(|complex| matches_seq(dom, node, &complex.parts))
}

/// Match `sel` against `node` (document API — parse once per query via
/// [`query_all`] instead; this is for one-off checks and tests).
fn matches_selector(dom: &Dom, node: NodeId, selector: &str) -> bool {
    parse_selector_list(selector).is_some_and(|list| matches_parsed(dom, node, &list))
}

fn query_all(dom: &Dom, selector: &str) -> Vec<NodeId> {
    let Some(list) = parse_selector_list(selector) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Some(root) = dom.root() {
        dom.walk(root, &mut |n| {
            if matches_parsed(dom, n, &list) {
                out.push(n);
            }
        });
    }
    out
}

fn is_descendant_of(dom: &Dom, node: NodeId, ancestor: NodeId) -> bool {
    let mut up = dom.node(node).parent;
    while let Some(a) = up {
        if a == ancestor {
            return true;
        }
        up = dom.node(a).parent;
    }
    false
}

fn join_values(args: &kitae_script::rquickjs::function::Rest<Value>) -> String {
    args.0
        .iter()
        .map(|v| match v.as_string() {
            Some(s) => s.to_string().unwrap_or_default(),
            None => {
                if v.is_undefined() {
                    "undefined".into()
                } else if v.is_null() {
                    "null".into()
                } else if let Some(f) = v.as_number() {
                    format!("{}", f)
                } else if let Some(b) = v.as_bool() {
                    format!("{}", b)
                } else {
                    "[object]".to_string()
                }
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page_dom(html: &str) -> (Dom, NodeId) {
        // The engine's parse: (dom, html-element).
        let (dom, root) = crate::parse(html);
        (dom, root.expect("page has an <html> root"))
    }

    #[test]
    fn selector_parser_accepts_the_supported_grammar() {
        // Compound with every part shape.
        let c = parse_compound_at(
            &"a.link#main[href=\"x\"]".chars().collect::<Vec<_>>(),
            &mut 0,
        )
        .expect("parses");
        assert_eq!(c.tag.as_deref(), Some("a"));
        assert_eq!(c.id.as_deref(), Some("main"));
        assert_eq!(c.classes, vec!["link"]);
        assert_eq!(c.attrs, vec![("href".into(), AttrTest::Equals("x".into()))]);
        // Attribute operators.
        for (spec, want) in [
            ("[data-x]", AttrTest::Exists),
            ("[data-x^=\"ht\"]", AttrTest::Prefix("ht".into())),
            ("[data-x$=\"ml\"]", AttrTest::Suffix("ml".into())),
            ("[data-x*=\"tt\"]", AttrTest::Contains("tt".into())),
            ("[data-x~=\"word\"]", AttrTest::Includes("word".into())),
            ("[data-x|=\"en\"]", AttrTest::DashMatch("en".into())),
        ] {
            let c = parse_compound_at(&spec.chars().collect::<Vec<_>>(), &mut 0)
                .unwrap_or_else(|| panic!("{spec} parses"));
            assert_eq!(c.attrs[0].1, want, "{spec}");
        }
        // Pseudos, including an+b shapes.
        let nth = |raw: &str| parse_nth(raw).expect("nth");
        assert_eq!(nth("odd"), (2, 1));
        assert_eq!(nth("even"), (2, 0));
        assert_eq!(nth("3"), (0, 3));
        assert_eq!(nth("2n"), (2, 0));
        assert_eq!(nth("2n+1"), (2, 1));
        assert_eq!(nth("-n+3"), (-1, 3));
        assert_eq!(nth("+n-2"), (1, -2));
        // Unknown syntax still fails the selector outright.
        assert!(parse_selector_list("a:hover").is_none(), "unknown pseudo");
        assert!(
            parse_selector_list("p > span::before").is_none(),
            "pseudo-elements"
        );
        assert!(parse_selector_list("[lang=en i]").is_none(), "case flag");
    }

    #[test]
    fn selector_matching_walks_combinators_and_pseudos() {
        let (dom, root) = page_dom(
            "<html><body>\
             <ul id=\"list\">\
             <li class=\"first item\" data-tag=\"alpha one\">one</li>\
             <li class=\"item\" data-tag=\"beta\">two</li>\
             <li class=\"item\" data-tag=\"gamma-two\">three</li>\
             <li>four</li>\
             </ul>\
             <p><em id=\"deep\">deep</em></p>\
             </body></html>",
        );
        let items: Vec<String> = query_all(&dom, "ul > li.item")
            .iter()
            .map(|n| concat_text(&dom, *n))
            .collect();
        assert_eq!(items, vec!["one", "two", "three"], "child combinator");
        // Sibling combinators.
        let three = query_all(&dom, "li.first + li");
        assert_eq!(three.len(), 1);
        assert_eq!(concat_text(&dom, three[0]), "two");
        let rest = query_all(&dom, "li.first ~ li");
        assert_eq!(rest.len(), 3);
        // Attribute operators.
        assert_eq!(query_all(&dom, "[data-tag*=\"two\"]").len(), 1);
        assert_eq!(query_all(&dom, "[data-tag^=\"al\"]").len(), 1);
        assert_eq!(query_all(&dom, "[data-tag~=\"one\"]").len(), 1);
        // Structural pseudos.
        assert_eq!(
            concat_text(&dom, query_all(&dom, "li:first-child")[0]),
            "one"
        );
        assert_eq!(
            concat_text(&dom, query_all(&dom, "li:last-child")[0]),
            "four"
        );
        let odds: Vec<String> = query_all(&dom, "li:nth-child(odd)")
            .iter()
            .map(|n| concat_text(&dom, *n))
            .collect();
        assert_eq!(odds, vec!["one", "three"]);
        assert_eq!(query_all(&dom, "li:not(.item)").len(), 1);
        // Comma lists and descendant chains through non-element context.
        let mixed: Vec<String> = query_all(&dom, "#deep, li:last-child")
            .iter()
            .map(|n| concat_text(&dom, *n))
            .collect();
        assert_eq!(mixed, vec!["four", "deep"]);
        assert_eq!(query_all(&dom, "body em#deep").len(), 1);
        assert!(
            query_all(&dom, "ul em").is_empty(),
            "no cross-subtree match"
        );
        assert!(!matches_selector(&dom, root, "html > li"), "root guard");
    }

    /// Body text with script/style subtrees excluded — what a reader
    /// actually sees.
    fn page_text(dom: &Dom, root: NodeId) -> String {
        fn visible(dom: &Dom, node: NodeId, out: &mut String) {
            if dom_tag_is(dom, node, "script") || dom_tag_is(dom, node, "style") {
                return;
            }
            match dom.node(node).kind {
                NodeKind::Text => out.push_str(&dom.node(node).text),
                NodeKind::Element => {
                    for child in dom.children(node) {
                        visible(dom, *child, out);
                    }
                }
            }
        }
        let mut out = String::new();
        if let Some(body) = find_child_tag(dom, root, "body") {
            visible(dom, body, &mut out);
        }
        out
    }

    fn dom_tag_is(dom: &Dom, node: NodeId, tag: &str) -> bool {
        dom.element(node).map(|e| e.tag.as_str()) == Some(tag)
    }

    #[test]
    fn inline_script_writes_into_the_dom() {
        let (mut dom, root) = page_dom(
            "<html><head id=\"hd\"></head><body><p id=\"x\"></p><script>\
             document.getElementById('x').textContent = 'injected words';\
             </script></body></html>",
        );
        let mut page = JsPage::new("https://example.test/page", 1).expect("runtime");
        let stats = page.run_document_scripts(&mut dom, root, "https://example.test/page");
        assert_eq!(stats.scripts_ran, 1);
        assert_eq!(stats.scripts_ok, 1, "clean script");
        assert!(
            page_text(&dom, root).contains("injected words"),
            "script text landed in the tree: {:?}",
            page_text(&dom, root)
        );
    }

    #[test]
    fn reader_writes_query_selectors_and_classes() {
        let (mut dom, root) = page_dom(
            "<html><head><title>t</title></head><body>\
             <div id=\"box\" class=\"a\"></div><script>\
             var b = document.querySelector('#box');\
             b.classList.add('b');\
             b.innerHTML = '<em>built</em>';\
             document.title = 'scripted title';\
             if (!b.classList.contains('a')) throw new Error('class lost');\
             </script></body></html>",
        );
        let mut page = JsPage::new("https://example.test/page", 1).expect("runtime");
        page.run_document_scripts(&mut dom, root, "https://example.test/page");
        let body_text = page_text(&dom, root);
        assert!(
            page_text(&dom, root).contains("built"),
            "innerHTML grafted: {body_text:?}"
        );
        let title_text = find_child_tag(&dom, root, "head")
            .and_then(|h| find_child_tag(&dom, h, "title"))
            .map(|t| concat_text(&dom, t))
            .unwrap_or_default();
        assert_eq!(
            title_text, "scripted title",
            "document.title setter reached the tree"
        );
    }

    #[test]
    fn timers_fire_and_mutate_on_poll() {
        let (mut dom, root) = page_dom(
            "<html><body><p id=\"x\">start</p><script>\
             setTimeout(function(){\
               try { document.getElementById('x').textContent = 'timed words'; }\
               catch (e) { }\
             }, 20);\
             document.write('timer armed');\
             </script></body></html>",
        );
        let mut page = JsPage::new("https://example.test/page", 1).expect("runtime");
        page.run_document_scripts(&mut dom, root, "https://example.test/page");
        assert!(
            !page_text(&dom, root).contains("timed words"),
            "the callback must not fire before its deadline"
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            std::thread::sleep(std::time::Duration::from_millis(10));
            page.poll(&mut dom);
            if page_text(&dom, root).contains("timed words") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the timer never fired or never mutated; body: {:?}",
                page_text(&dom, root)
            );
        }
    }

    #[test]
    fn broken_scripts_report_failure_and_leave_a_still_tree() {
        let (mut dom, root) = page_dom(
            "<html><body><p>static words</p><script>this is }} not js</script></body></html>",
        );
        let mut page = JsPage::new("https://example.test/page", 1).expect("runtime");
        let stats = page.run_document_scripts(&mut dom, root, "https://example.test/page");
        assert_eq!(stats.scripts_failed, 1);
        assert!(
            page_text(&dom, root).contains("static words"),
            "the static tree is intact"
        );
    }
}

/// Encode a Rust string as a JavaScript string literal (settle payloads
/// pass through eval'd one-liners; never interpolate raw bytes).
fn kitae_script_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '<' => out.push_str("\\x3c"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\x{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
