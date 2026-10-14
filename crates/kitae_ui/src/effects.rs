//! Effects — closures that re-run when the signals they read change.

use crate::signals::{with_active, Subscriber};
use std::cell::Cell;
use std::cell::RefCell;
use std::rc::Rc;

/// A handle to a running effect; lets you force a re-run, or stop the
/// effect permanently (teardown). Effects normally live for the reactive
/// world's lifetime — call [`EffectHandle::stop`] when an effect must not
/// outlive the thing that created it (a component unmounted from the
/// tree, a swapped-out resource).
pub struct EffectHandle {
    run: Subscriber,
    cancelled: Rc<Cell<bool>>,
}

impl EffectHandle {
    /// Force the effect to re-run now. A stopped effect does not re-run.
    pub fn rerun(&self) {
        if self.cancelled.get() {
            return;
        }
        (self.run)();
    }

    /// Stop the effect: it never runs again (not even a re-run that was
    /// already queued when `stop` was called). Idempotent.
    pub fn stop(&self) {
        self.cancelled.set(true);
    }
}

/// Run `f` once now, then again every time a signal read inside it changes.
///
/// ```rust
/// use kitae_ui::prelude::*;
/// let name = use_signal(|| "kitae".to_string());
/// let runs = std::rc::Rc::new(std::cell::Cell::new(0));
/// let counter = runs.clone();
/// let handle = use_effect(move || {
///     let _ = name.get();
///     counter.set(counter.get() + 1);
/// });
/// assert_eq!(runs.get(), 1); // ran once immediately
/// name.set("again".into());
/// assert_eq!(runs.get(), 2); // re-ran on dependency change
/// handle.stop();             // teardown: no further re-runs
/// name.set("silent".into());
/// assert_eq!(runs.get(), 2);
/// ```
pub fn use_effect<F: Fn() + 'static>(f: F) -> EffectHandle {
    let cancelled = Rc::new(Cell::new(false));
    let run_cell: Rc<RefCell<Box<dyn Fn()>>> = Rc::new(RefCell::new(Box::new(f)));
    // Late-bound self reference so the re-runner can activate itself while it
    // runs (new reads inside the re-run subscribe it again).
    let me: Rc<RefCell<Option<Subscriber>>> = Rc::new(RefCell::new(None));

    let me2 = me.clone();
    let run2 = run_cell.clone();
    let cancelled2 = cancelled.clone();
    let sub: Subscriber = Rc::new(move || {
        // Stopped effects must not run — not even a re-run that was queued
        // in PENDING before stop() was called.
        if cancelled2.get() {
            return;
        }
        let active = me2
            .borrow()
            .as_ref()
            .expect("effect self-reference")
            .clone();
        with_active(active, || (run2.borrow())());
    });
    *me.borrow_mut() = Some(sub.clone());

    // Seed run. If it panics, the effect never became reachable through a
    // signal, so there is nothing to unsubscribe — let the panic propagate.
    with_active(sub.clone(), || (run_cell.borrow())());

    EffectHandle {
        run: sub,
        cancelled,
    }
}
