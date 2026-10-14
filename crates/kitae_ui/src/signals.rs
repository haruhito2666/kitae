//! Signals — the reactive state cell behind `use_signal`.
//!
//! Handles are **Copy**: the plan's component syntax hands signals to `move`
//! closures (`let add = move |_| { todos.update(..); draft.set(..) };`) and
//! keeps using the originals afterwards, so the handle itself must copy like
//! a `Leptos`/`Dioxus` signal — it does, by naming a cell in a thread-local
//! slab. The core is thread-local by design: kitae runs one engine thread per
//! tab and one world per live session, so reactivity never crosses threads.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::fmt;
use std::marker::PhantomData;
use std::rc::Rc;

pub(crate) type Subscriber = Rc<dyn Fn()>;

thread_local! {
    /// Subscribers collecting dependencies during an effect run.
    static ACTIVE: RefCell<Vec<Subscriber>> = RefCell::new(Vec::new());
    /// Nesting depth of effect runs; nonzero means notifications defer.
    static DEPTH: Cell<usize> = Cell::new(0);
    /// Subscribers queued while inside an effect run.
    static PENDING: RefCell<Vec<Subscriber>> = RefCell::new(Vec::new());
    /// All live state cells, addressed by signal id.
    static SLAB: RefCell<Slab> = RefCell::new(Slab::new());
    /// Signal ids already warned for a write-during-render (A9): one
    /// warning per signal, not one per write.
    static RENDER_WARNED: RefCell<Vec<u32>> = RefCell::new(Vec::new());
    /// Nesting depth of subscriber/effect runs. Writes from inside an
    /// effect are the point of effects, so the A9 render-purity check is
    /// suppressed while this is nonzero — even when the effect itself was
    /// triggered mid-render (a first-mount `use_effect` seeding state).
    static EFFECT_DEPTH: Cell<usize> = Cell::new(0);
}

/// Whether a subscriber/effect body is running on this thread.
pub(crate) fn in_effect() -> bool {
    EFFECT_DEPTH.with(|d| d.get() > 0)
}

/// A9 strictness: a signal write inside a component build is the classic
/// impure-render bug (it can loop: the write notifies subscribers that
/// re-render mid-build). Warn once per signal id; the warning always lands
/// in the strictness log ([`crate::kit::take_warnings`]), stderr only under
/// `KITAE_STRICT=1`.
pub(crate) fn check_render_write(id: u32) {
    // Effect bodies are the sanctioned place to write state (and a
    // first-mount effect runs mid-render) — never warn for them.
    if !crate::hooks::is_rendering() || in_effect() {
        return;
    }
    let fresh = RENDER_WARNED.with(|w| {
        let mut w = w.borrow_mut();
        if w.contains(&id) {
            false
        } else {
            w.push(id);
            true
        }
    });
    if fresh {
        let owner =
            crate::hooks::current_scope_name().unwrap_or_else(|| "<root render>".to_string());
        crate::kit::warn(format!(
            "signal #{id} written during render (in {owner}) — renders must be pure; \
             move the write into an effect or event handler"
        ));
    }
}

/// A type-erased state cell living in the slab.
trait AnyCell {
    fn as_any(&self) -> &dyn Any;
    fn subscribe(&self, sub: Subscriber);
    fn notify(&self);
    fn version(&self) -> u64;
}

struct SignalCell<T> {
    value: RefCell<T>,
    version: Cell<u64>,
    subs: RefCell<Vec<Subscriber>>,
}

impl<T: 'static> AnyCell for SignalCell<T> {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn subscribe(&self, sub: Subscriber) {
        let mut subs = self.subs.borrow_mut();
        if !subs.iter().any(|s| Rc::ptr_eq(s, &sub)) {
            subs.push(sub);
        }
    }

    fn notify(&self) {
        // Clone the subscriber list out before running anything: a subscriber
        // may write to this same signal or subscribe others mid-flight.
        let subs = self.subs.borrow().clone();
        if DEPTH.get() == 0 {
            for s in subs {
                run_guarded(&s);
            }
            flush();
        } else {
            PENDING.with(|p| p.borrow_mut().extend(subs));
        }
    }

    fn version(&self) -> u64 {
        self.version.get()
    }
}

struct Slab {
    /// One slot per allocated id (`None` = freed and reusable). The
    /// generation disambiguates handles across reuse: a `Signal` handle
    /// names (id, gen), and a gen mismatch means the handle outlived a
    /// [`Signal::dispose`] — fail loudly rather than silently reading
    /// whatever type now lives in the recycled slot.
    cells: Vec<Option<Slot>>,
    /// Freed (id, next generation) pairs — the free list alloc reuses.
    free: Vec<(u32, u64)>,
}

struct Slot {
    gen: u64,
    cell: Rc<dyn AnyCell>,
}

impl Slab {
    fn new() -> Slab {
        Slab {
            cells: Vec::new(),
            free: Vec::new(),
        }
    }

    fn alloc(&mut self, cell: Rc<dyn AnyCell>) -> (u32, u64) {
        match self.free.pop() {
            Some((id, gen)) => {
                self.cells[id as usize] = Some(Slot { gen, cell });
                (id, gen)
            }
            None => {
                let id = self.cells.len() as u32;
                self.cells.push(Some(Slot { gen: 0, cell }));
                (id, 0)
            }
        }
    }

    fn get(&self, id: u32, gen: u64) -> Rc<dyn AnyCell> {
        match self.cells.get(id as usize) {
            Some(Some(slot)) if slot.gen == gen => slot.cell.clone(),
            Some(Some(_)) => panic!(
                "signal handle is stale — it was disposed and its slab slot was reused \
                 (Signal::dispose was called on it)"
            ),
            _ => panic!("signal used after its world was torn down"),
        }
    }
}

fn with_cell<R>(id: u32, gen: u64, f: impl FnOnce(&Rc<dyn AnyCell>) -> R) -> R {
    SLAB.with(|s| {
        let cell = s.borrow().get(id, gen);
        f(&cell)
    })
}

fn track(cell: &Rc<dyn AnyCell>) {
    ACTIVE.with(|a| {
        if let Some(sub) = a.borrow().last().cloned() {
            cell.subscribe(sub);
        }
    });
}

fn run_guarded(s: &Subscriber) {
    let _g = track_guard(None);
    s();
}

/// Restores the reactive-tracking state on drop — panic-safe. An unwind
/// through an effect or subscriber (caught by an error boundary or a test
/// harness) used to leave DEPTH/EFFECT_DEPTH/ACTIVE elevated forever, which
/// silently froze every later reactive flush on the thread. With the guard,
/// a caught panic degrades to "that update was lost" and the world keeps
/// working.
struct TrackGuard {
    pushed_active: bool,
}

impl Drop for TrackGuard {
    fn drop(&mut self) {
        if self.pushed_active {
            ACTIVE.with(|a| {
                a.borrow_mut().pop();
            });
        }
        EFFECT_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
        DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// Enter a tracked region: counters up (and the tracking stack pushed when
/// `push_active` is given), restored by the guard on scope exit or unwind.
fn track_guard(push_active: Option<&Subscriber>) -> TrackGuard {
    if let Some(sub) = push_active {
        ACTIVE.with(|a| a.borrow_mut().push(sub.clone()));
    }
    DEPTH.with(|d| d.set(d.get() + 1));
    EFFECT_DEPTH.with(|d| d.set(d.get() + 1));
    TrackGuard {
        pushed_active: push_active.is_some(),
    }
}

fn flush() {
    loop {
        let batch: Vec<Subscriber> = PENDING.with(|p| {
            let mut q = p.borrow_mut();
            if q.is_empty() {
                return Vec::new();
            }
            // Dedup by identity: a write that crossed several dependencies
            // should re-run an effect once.
            let mut out: Vec<Subscriber> = Vec::new();
            'each: for s in q.drain(..) {
                for o in &out {
                    if Rc::ptr_eq(o, &s) {
                        continue 'each;
                    }
                }
                out.push(s);
            }
            out
        });
        if batch.is_empty() {
            return;
        }
        for s in batch {
            run_guarded(&s);
        }
    }
}

/// Run `f` with `sub` as the active tracking subscriber.
pub(crate) fn with_active<R>(sub: Subscriber, f: impl FnOnce() -> R) -> R {
    let g = track_guard(Some(&sub));
    let out = f();
    drop(g);
    if DEPTH.get() == 0 {
        flush();
    }
    out
}

/// A shared reactive value. The handle is `Copy` — clone it freely, pass it
/// into `move` closures, keep using the original. Read with [`Signal::get`],
/// write with [`Signal::set`] / [`Signal::update`].
pub struct Signal<T> {
    id: u32,
    /// Slab generation the cell was allocated at — detects use of a handle
    /// whose slot was freed (via [`Signal::dispose`]) and reused.
    gen: u64,
    // Raw pointer keeps the handle !Send/!Sync: cells live in thread-local
    // storage and must be used from the thread that created them.
    _marker: PhantomData<*mut T>,
}

impl<T> Copy for Signal<T> {}
impl<T> Clone for Signal<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Signal<T> {
    fn cell(&self) -> Rc<dyn AnyCell> {
        with_cell(self.id, self.gen, |c| c.clone())
    }
}

impl<T: Clone + 'static> Signal<T> {
    /// Read the current value, registering the active effect (if any) as a
    /// dependent.
    pub fn get(&self) -> T {
        let cell = self.cell();
        track(&cell);
        let typed = cell
            .as_any()
            .downcast_ref::<SignalCell<T>>()
            .expect("signal type mismatch (slab corruption)")
            .clone();
        let out = typed
            .value
            .try_borrow()
            .expect(
                "signal read while it is being written — an update closure read its \
                 own signal; capture the value before the update instead",
            )
            .clone();
        out
    }
}

// Writes need no `Clone` on `T`: the plan's Todos example pushes non-`Copy`
// structs (`Todo { text, done }`) into a `Signal<Vec<Todo>>`.
impl<T: 'static> Signal<T> {
    /// Replace the value and notify dependents — unconditionally. For a
    /// write that skips notification when the value is unchanged, use
    /// [`Signal::set_if_changed`] (needs `T: PartialEq`).
    ///
    /// Panics on re-entrant write from *inside* the notification of this
    /// same signal (`set` → subscriber → `set` on the same signal): that
    /// used to be a RefCell double-borrow panic anyway; now it fails with
    /// a message naming the actual bug. Writes from inside an *effect* are
    /// unaffected — that is the sanctioned pattern.
    pub fn set(&self, value: T) {
        let cell = self.cell();
        self.write(cell, value);
    }

    /// Replace the value and notify dependents — unless it equals the
    /// current one, in which case this is a no-op (no version bump, no
    /// dependent re-runs). The cheap win for high-frequency inputs
    /// (mousemove, scroll): dependents only re-run on real change.
    pub fn set_if_changed(&self, value: T)
    where
        T: PartialEq,
    {
        let cell = self.cell();
        let typed = cell
            .as_any()
            .downcast_ref::<SignalCell<T>>()
            .expect("signal type mismatch (slab corruption)");
        if *typed.value.borrow() == value {
            return;
        }
        self.write(cell, value);
    }

    /// Mutate the value in place and notify dependents.
    pub fn update(&self, f: impl FnOnce(&mut T)) {
        self.update_returning(|v| {
            f(v);
            None::<()>
        });
    }

    /// Framework-internal write that skips the render-purity check — for
    /// machinery that must seed state while a build is running (the
    /// resource layer flipping `loading`, or filling a cached fetch). The
    /// write itself is the same; only the A9 warning is suppressed.
    pub(crate) fn set_internal(&self, value: T) {
        let cell = self.cell();
        self.write(cell, value);
    }

    /// The shared write path: version bump, store, notify. Panics with a
    /// real message on re-entrant write to the same signal from inside its
    /// own notification (the borrow is held across `notify`).
    fn write(&self, cell: Rc<dyn AnyCell>, value: T) {
        check_render_write(self.id);
        let typed = cell
            .as_any()
            .downcast_ref::<SignalCell<T>>()
            .expect("signal type mismatch (slab corruption)");
        let mut guard = typed.value.try_borrow_mut().expect(
            "re-entrant write to a signal from inside its own notification — \
                 defer the write (e.g. queue it and do it after the handler returns)",
        );
        typed.version.set(typed.version.get() + 1);
        *guard = value;
        drop(guard);
        cell.notify();
    }

    /// Mutate in place and notify, returning whatever `f` returns. The
    /// mutation and notify use the same re-entrancy discipline as
    /// [`Signal::write`].
    fn update_returning<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        check_render_write(self.id);
        let cell = self.cell();
        let typed = cell
            .as_any()
            .downcast_ref::<SignalCell<T>>()
            .expect("signal type mismatch (slab corruption)");
        let mut guard = typed.value.try_borrow_mut().expect(
            "re-entrant write to a signal from inside its own notification — \
                 defer the write (e.g. queue it and do it after the handler returns)",
        );
        typed.version.set(typed.version.get() + 1);
        let out = f(&mut guard);
        drop(guard);
        cell.notify();
        out
    }
}

impl<T> Signal<T> {
    /// Current version — bump on every write; lets runtimes skip no-op diffs.
    pub fn version(&self) -> u64 {
        self.cell().version()
    }

    /// Free this signal's slab slot for reuse. Every later use of ANY
    /// handle to it panics (loudly naming the stale handle) — even before
    /// the slot is reused — so dispose only state you know is dead: a
    /// removed list row's per-row signal, a closed panel's scratch state.
    /// Subscribers still registered on the cell are dropped with it.
    ///
    /// Do NOT dispose a signal another live component still reads.
    pub fn dispose(&self) {
        SLAB.with(|s| {
            let mut s = s.borrow_mut();
            match s.cells.get_mut(self.id as usize) {
                Some(Some(slot)) if slot.gen == self.gen => {
                    s.cells[self.id as usize] = None;
                    s.free.push((self.id, self.gen.wrapping_add(1)));
                }
                _ => {} // already disposed — idempotent no-op
            }
        });
    }
}

impl<T: Clone + fmt::Display + 'static> fmt::Display for Signal<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.get().fmt(f)
    }
}

impl<T: Clone + Default + 'static> Default for Signal<T> {
    fn default() -> Self {
        use_signal(T::default)
    }
}

/// Owned snapshot iterator, so the `for todos.iter() -> (i, todo)` shape in
/// `view!` works directly on a `Signal<Vec<T>>` without borrow juggling.
impl<T: Clone + 'static> Signal<Vec<T>> {
    pub fn iter(&self) -> std::vec::IntoIter<T> {
        self.get().into_iter()
    }
}

/// Create a signal from an initial value.
///
/// ```rust
/// use kitae_ui::prelude::*;
/// let count = use_signal(|| 0);
/// count.update(|c| *c += 1);
/// assert_eq!(count.get(), 1);
/// ```
///
/// Called inside a `#[component]` body, the signal is **slot-stable**: the
/// same component invocation gets the same signal on every re-render (the
/// rule-of-hooks applies — don't call it conditionally). Called outside a
/// component (the hoisted pattern), it allocates a fresh cell as before.
pub fn use_signal<T, F: FnOnce() -> T>(init: F) -> Signal<T>
where
    T: 'static,
{
    if crate::hooks::in_scope() {
        return crate::hooks::with_slot(move || alloc_cell(init));
    }
    alloc_cell(init)
}

fn alloc_cell<T: 'static>(init: impl FnOnce() -> T) -> Signal<T> {
    let cell: Rc<SignalCell<T>> = Rc::new(SignalCell {
        value: RefCell::new(init()),
        version: Cell::new(0),
        subs: RefCell::new(Vec::new()),
    });
    let (id, gen) = SLAB.with(|s| s.borrow_mut().alloc(cell));
    Signal {
        id,
        gen,
        _marker: PhantomData,
    }
}

/// Allocate a fresh signal unconditionally — NOT slot-stable. For state
/// that manages its own identity instead of a hook position: the form
/// kit's per-field values live in a name-keyed map, so each field's signal
/// is created exactly once, when its name first appears (fields may be
/// registered in loops without consuming a hook slot each iteration).
pub fn signal<T: 'static>(init: impl FnOnce() -> T) -> Signal<T> {
    alloc_cell(init)
}

/// Devtools snapshot of the signal slab: `(id, version)` for every live
/// cell, in id order. Versions bump on every write — a tool can show churn
/// without needing the type-erased values.
pub fn debug_cells() -> Vec<(u32, u64)> {
    SLAB.with(|s| {
        s.borrow()
            .cells
            .iter()
            .enumerate()
            .filter_map(|(i, c)| c.as_ref().map(|slot| (i as u32, slot.cell.version())))
            .collect()
    })
}

/// A mutable value that survives re-renders — React's `useRef`.
///
/// Returns `Rc<RefCell<T>>`; inside a component it is slot-stable, outside
/// one it is created fresh per call (hoist it, like a signal).
///
/// ```rust
/// use kitae_ui::prelude::*;
/// use std::cell::RefCell;
/// use std::rc::Rc;
/// let clicks = use_ref(|| 0u32);
/// *clicks.borrow_mut() += 1;
/// assert_eq!(*clicks.borrow(), 1);
/// ```
pub fn use_ref<T: 'static>(init: impl FnOnce() -> T) -> Rc<RefCell<T>> {
    if crate::hooks::in_scope() {
        return crate::hooks::with_slot(move || Rc::new(RefCell::new(init())));
    }
    Rc::new(RefCell::new(init()))
}

/// A stable handle to one mounted element — mark it with `node_ref: h`.
///
/// ```rust
/// use kitae_ui::prelude::*;
/// let input = use_node_ref();
/// let h1 = input.id();
/// let h2 = input.id();
/// assert_eq!(h1, h2, "the handle is stable");
/// ```
pub fn use_node_ref() -> crate::vnode::NodeHandle {
    if crate::hooks::in_scope() {
        return crate::hooks::with_slot(|| crate::vnode::alloc_node_handle());
    }
    crate::vnode::alloc_node_handle()
}
