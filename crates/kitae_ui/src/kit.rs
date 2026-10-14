//! Track A's remaining framework slices: error boundaries (A4), view
//! memoization (A6), the forms layer (A7), the SPA router core (A8), and
//! dev strictness (A9).
//!
//! All of it composes with the hook scopes: state here is slot-stable
//! inside `#[component]` bodies, hoistable outside them.

use crate::component;
use crate::signals::{use_ref, use_signal, Signal};
use crate::view;
use crate::vnode::{Children, View};

// ── A4 · error boundaries ───────────────────────────────────────────────────

/// Catch render panics from the children subtree and render `fallback`
/// instead. Siblings are untouched. The reactive machinery unwinds through
/// drop guards (the thread-local depth counters and the tracking stack
/// restore themselves on panic), so after the catch the world keeps
/// working: later renders run, signals update, effects re-run. The failed
/// subtree's hook slots keep whatever state they had when the panic hit.
#[component]
pub fn ErrorBoundary(fallback: View, children: Children) -> View {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| children.render())) {
        Ok(v) => v,
        Err(_) => fallback,
    }
}

// ── A6 · subtree memoization ────────────────────────────────────────────────

/// Rebuild this subtree only when `deps` changed — a list row whose deps
/// (its item's version) are unchanged returns the cached view, skipping
/// the whole rebuild. The diff still walks (and emits nothing), so ops
/// stay minimal; the win is skipping thousands of vnode constructions.
pub fn use_memo_view<V: Into<View> + 'static>(deps: u64, build: impl FnOnce() -> V) -> View {
    if crate::hooks::in_scope() {
        return crate::hooks::with_memo_slot(deps, || build().into());
    }
    build().into()
}

// ── A7 · forms layer ────────────────────────────────────────────────────────

/// Per-form state: the glue every form re-implements — field values,
/// touched/dirty tracking, per-field error strings, a submit counter.
/// Validation itself stays plain Rust (an author's `validate` fn writes
/// errors via [`Form::set_error`]); the Form only carries the state and
/// the `reset`. Cheap to clone — event handlers each take their own
/// handle (the state lives behind `Rc`s).
#[derive(Clone)]
pub struct Form {
    touched: Rc<RefCell<HashMap<String, Signal<bool>>>>,
    /// Live field values, keyed by name. Signals are allocated with
    /// [`crate::signals::signal`] (fresh, not slot-positioned) the first
    /// time a name appears — the map provides the stability, so fields may
    /// be registered in loops or conditionals without breaking hook order.
    values: Rc<RefCell<HashMap<String, Signal<String>>>>,
    /// The value each field started with — the baseline `is_dirty`
    /// compares against and [`Form::reset`] restores.
    initials: Rc<RefCell<HashMap<String, String>>>,
    errors: Rc<RefCell<HashMap<String, String>>>,
    submitted: Signal<u32>,
}

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// Create the form glue (slot-stable inside a component).
pub fn use_form() -> Form {
    let touched = use_ref(HashMap::new);
    let values = use_ref(HashMap::new);
    let initials = use_ref(HashMap::new);
    let errors = use_ref(HashMap::new);
    let submitted = use_signal(|| 0u32);
    Form {
        touched,
        values,
        initials,
        errors,
        submitted,
    }
}

impl Form {
    /// Register a field's touch state (idempotent per name). The touched
    /// map is consulted before any allocation, so calling this in a loop
    /// is safe — one signal per name, however often it's registered.
    pub fn field(&self, name: &str) -> Signal<bool> {
        self.touched
            .borrow_mut()
            .entry(name.to_string())
            .or_insert_with(|| crate::signals::signal(|| false))
            .clone()
    }

    /// Register a field's value with its initial content and return the
    /// live signal (idempotent per name; the initial is recorded once —
    /// later calls with different initials do NOT clobber the field).
    pub fn field_value(&self, name: &str, initial: impl Into<String>) -> Signal<String> {
        let initial = initial.into();
        self.initials
            .borrow_mut()
            .entry(name.to_string())
            .or_insert_with(|| initial.clone());
        self.values
            .borrow_mut()
            .entry(name.to_string())
            .or_insert_with(|| crate::signals::signal(move || initial))
            .clone()
    }

    /// Mark a field touched (on blur, or on first input — author's call).
    /// Auto-registers the field so dirty checks work before any typing.
    pub fn touch(&self, name: &str) {
        self.field(name).set(true);
    }

    /// Whether the field was touched.
    pub fn is_touched(&self, name: &str) -> bool {
        self.field(name).get()
    }

    /// Read a field's current value ("" for never-registered fields).
    pub fn value(&self, name: &str) -> String {
        self.values
            .borrow()
            .get(name)
            .map(|s| s.get())
            .unwrap_or_default()
    }

    /// Write a field's value, auto-registering it with a "" initial when
    /// the author never called [`Form::field_value`].
    pub fn set_value(&self, name: &str, v: impl Into<String>) {
        self.field_value(name, "");
        if let Some(s) = self.values.borrow().get(name) {
            s.set(v.into());
        }
    }

    /// The field drifted from its initial value.
    pub fn is_dirty(&self, name: &str) -> bool {
        match (
            self.values.borrow().get(name),
            self.initials.borrow().get(name),
        ) {
            (Some(v), Some(i)) => v.get() != *i,
            (Some(v), None) => !v.get().is_empty(),
            _ => false,
        }
    }

    /// Any field drifted from its initial value.
    pub fn is_dirty_any(&self) -> bool {
        let values = self.values.borrow();
        let initials = self.initials.borrow();
        values
            .iter()
            .any(|(name, v)| v.get() != initials.get(name).cloned().unwrap_or_default())
    }

    /// Record a validation failure for a field (survives until
    /// [`Form::clear_error`] / [`Form::reset`]).
    pub fn set_error(&self, name: &str, msg: impl Into<String>) {
        self.errors
            .borrow_mut()
            .insert(name.to_string(), msg.into());
    }

    /// Clear a field's validation failure.
    pub fn clear_error(&self, name: &str) {
        self.errors.borrow_mut().remove(name);
    }

    /// The field's validation failure, if any.
    pub fn error(&self, name: &str) -> Option<String> {
        self.errors.borrow().get(name).cloned()
    }

    /// Record a submit attempt; returns the attempt count so error text can
    /// show only after a real attempt ("show errors on submit").
    pub fn submit(&self) -> u32 {
        self.submitted.update(|n| *n += 1);
        self.submitted.get()
    }

    /// How many submits happened (0 = none — hide errors until then).
    pub fn submits(&self) -> u32 {
        self.submitted.get()
    }

    /// Back to square one: values return to their initials, touched flags
    /// clear, errors clear, the submit counter zeroes. Signals keep their
    /// identity, so bindings stay live.
    pub fn reset(&self) {
        let initials = self.initials.borrow();
        for (name, sig) in self.values.borrow().iter() {
            sig.set(initials.get(name).cloned().unwrap_or_default());
        }
        for sig in self.touched.borrow().values() {
            sig.set(false);
        }
        self.errors.borrow_mut().clear();
        self.submitted.set(0);
    }
}

// ── A8 · SPA router core ────────────────────────────────────────────────────

/// Client-side routing state: the current path as a signal, plus the
/// back/forward history stacks (session history without the browser —
/// `navigate` pushes, `back`/`forward` pop, a fresh navigation clears the
/// forward stack, exactly like a browser). `navigate` swaps pages in place
/// (a patch, not a page load); the live runtime seeds the initial path from
/// the upgrade URL so deep links work.
#[derive(Clone, Copy)]
pub struct Router {
    pub path: Signal<String>,
    back: Signal<Vec<String>>,
    forward: Signal<Vec<String>>,
}

impl Router {
    /// Create the router (slot-stable inside a component). `initial` comes
    /// from the runtime — `kitae_live::request_path()` on the server.
    pub fn new(initial: String) -> Router {
        Router {
            path: use_signal(move || initial),
            back: use_signal(|| Vec::new()),
            forward: use_signal(|| Vec::new()),
        }
    }

    /// Navigate in place: the next render shows the new page. The current
    /// path lands on the back stack; the forward stack clears (a fresh
    /// fork of history). Navigating to the current path is a no-op.
    pub fn navigate(&self, to: &str) {
        let cur = self.path.get();
        if cur == to {
            return;
        }
        self.back.update(|b| b.push(cur));
        self.forward.set(Vec::new());
        self.path.set(to.to_string());
    }

    /// Step back one entry (no-op when there is nothing behind).
    pub fn back(&self) {
        if !self.can_back() {
            return;
        }
        let cur = self.path.get();
        let mut popped = None;
        self.back.update(|b| popped = b.pop());
        if let Some(prev) = popped {
            self.forward.update(|f| f.push(cur));
            self.path.set(prev);
        }
    }

    /// Step forward one entry (no-op when there is nothing ahead).
    pub fn forward(&self) {
        if !self.can_forward() {
            return;
        }
        let cur = self.path.get();
        let mut popped = None;
        self.forward.update(|f| popped = f.pop());
        if let Some(next) = popped {
            self.back.update(|b| b.push(cur));
            self.path.set(next);
        }
    }

    /// Whether [`Router::back`] has somewhere to go.
    pub fn can_back(&self) -> bool {
        !self.back.get().is_empty()
    }

    /// Whether [`Router::forward`] has somewhere to go.
    pub fn can_forward(&self) -> bool {
        !self.forward.get().is_empty()
    }

    /// The current path.
    pub fn current(&self) -> String {
        self.path.get()
    }

    /// Extract a named parameter from the current path against a pattern
    /// like `/users/:id/posts/:post_id`.
    pub fn param(&self, pattern: &str, name: &str) -> Option<String> {
        route_param(&self.current(), pattern, name)
    }

    /// Read a query-string value from the current path (`?tab=2&sort=asc`).
    pub fn query(&self, key: &str) -> Option<String> {
        query_param(&self.current(), key)
    }
}

/// Match `path` against a `pattern` with `:name` segments and return the
/// value captured for `name`. Literal segments must match exactly; the
/// query string is ignored.
pub fn route_param(path: &str, pattern: &str, name: &str) -> Option<String> {
    let path = path.split('?').next().unwrap_or(path);
    let segs = |s: &str| -> Vec<String> {
        s.split('/')
            .filter(|p| !p.is_empty())
            .map(|p| p.to_string())
            .collect()
    };
    let path_segs = segs(path);
    let pattern_segs = segs(pattern);
    if path_segs.len() != pattern_segs.len() {
        return None;
    }
    for (pat, val) in pattern_segs.iter().zip(path_segs) {
        if let Some(cap) = pat.strip_prefix(':') {
            if cap == name {
                return Some(url_decode(&val));
            }
        } else if pat != &val {
            return None;
        }
    }
    None
}

/// Read one `key` from a path's query string (`/list?tab=2` →
/// `query_param(path, "tab") == Some("2")`). Keys and values are
/// percent-decoded; `+` decodes to a space.
pub fn query_param(path: &str, key: &str) -> Option<String> {
    let query = path.split_once('?')?.1;
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if url_decode(k) == key {
            return Some(url_decode(v));
        }
    }
    None
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// An in-page navigation link: clicking patches the router instead of
/// loading a new page.
#[component]
pub fn Link(router: Router, to: String, children: Children) -> View {
    let href = to.clone();
    let target = to.clone();
    view! {
        a(class: "kitae-link", href: href, on:click = move |_| router.navigate(&target)) { {children} }
    }
}

// ── A9 · dev strictness ─────────────────────────────────────────────────────

thread_local! {
    /// Strictness warnings for THIS thread's world. The reactive core is
    /// thread-local by design (one engine thread per tab, one world per
    /// live session), so warnings are too: draining one session's warnings
    /// never steals another session's, and unrelated worlds don't
    /// serialize on a process-global lock.
    static WARNINGS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// Strict mode is on when `KITAE_STRICT=1` (set it in `kitae dev`).
pub fn is_enabled() -> bool {
    std::env::var("KITAE_STRICT").as_deref() == Ok("1")
}

/// Record a strictness warning — always collected (tests assert on these),
/// echoed to stderr in strict mode. Collected per-thread: drain the
/// warnings of the thread that rendered.
pub fn warn(msg: impl Into<String>) {
    let m: String = msg.into();
    if is_enabled() {
        eprintln!("[kitae strict] {m}");
    }
    WARNINGS.with(|w| w.borrow_mut().push(m));
}

/// Drain collected warnings for the current thread (tests / tooling).
pub fn take_warnings() -> Vec<String> {
    WARNINGS.with(|w| std::mem::take(&mut *w.borrow_mut()))
}
