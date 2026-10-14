//! One live session: the server-held half of a mounted component tree.
//!
//! State lives server-side by design (docs/UI_LIBRARY_PLAN.md): a client is
//! a *view* of it. Reconnect is therefore cheap — the session keeps every
//! emitted patch in a ring (up to [`MAX_HISTORY`]) with monotonic sequence
//! numbers, and a resuming client asks for everything past its last ack.
//!
//! The handler half: the author's `on:click` closures can't cross a socket.
//! kitae_ui keeps them in the `Mount`'s handler table; the session routes
//! `Event` frames into that table — the author's Rust runs on the server,
//! the DOM mutation shows up on the client.

use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

use kitae_ui::diff::Mount;
use kitae_ui::patch::NodeRef;
use kitae_ui::vnode::{Event, Handler, View};

/// Patch history cap. Patches are small (one op list per signal change);
/// 4096 of them is far beyond a session's useful resume window, and past it
/// the client just takes a fresh `Init`.
pub const MAX_HISTORY: usize = 4096;

/// A mounted live component tree.
pub struct Session {
    render: Rc<dyn Fn() -> View>,
    mount: Mount,
    /// Handler refs → (event, handler); mirrors `Mount`'s table but keyed
    /// for wire dispatch (the author's closures run HERE, server-side).
    handlers: HashMap<u64, Vec<(String, Handler)>>,
    /// (seq, encoded ops) for every diff since mount.
    history: Vec<(u64, Vec<kitae_ui::patch::PatchOp>)>,
    next_seq: u64,
    /// Last socket-driven activity — the TTL-eviction clock.
    last_seen: Instant,
    /// The request path this session was opened at (the SPA router seed).
    path: String,
}

impl Session {
    pub fn new(render: Rc<dyn Fn() -> View>) -> Session {
        // The request path at upgrade time — the SPA router's deep-link
        // seed. Re-set before every render below so interleaved sessions
        // each read their own path.
        let path = crate::request_path();
        Session {
            path,
            render: kitae_ui::hooks::wrap_render(render),
            mount: Mount::new(NodeRef(0)),
            handlers: HashMap::new(),
            history: Vec::new(),
            // Seq 0 is the hydration marker (the Init's patch); real
            // updates start at 1 so a client acking 0 means "hydrated,
            // seen nothing" — never "I have update 0".
            next_seq: 1,
            last_seen: Instant::now(),
        }
    }

    /// Mark socket activity (the TTL-eviction clock).
    pub fn touch(&mut self) {
        self.last_seen = Instant::now();
    }

    /// The request path this session opened at — hot reload re-resolves
    /// routes against it (`World::replace_routes`).
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Swap the render closure (hot reload of Rust edits), wrapped the same
    /// way `new` wraps. Takes effect at the next init/update — a swap ends
    /// in `refresh_all`, which inits every connected session immediately.
    /// The session keeps its signal state (mount refs reset on init; the
    /// old closure's image stays mapped — see `hotreload`).
    pub fn set_render(&mut self, render: Rc<dyn Fn() -> View>) {
        self.render = kitae_ui::hooks::wrap_render(render);
    }

    /// Last activity instant.
    pub fn last_seen(&self) -> Instant {
        self.last_seen
    }

    /// Initial mount: static HTML (the no-socket fallback page) + the ops a
    /// Live client replays to hydrate. Idempotent — every (re)connecting
    /// socket starts from a fresh mount; the SESSION (signal state, seq
    /// counter) is what survives the disconnect. The ref map in
    /// `NodeData.handler` tells the client which refs own handlers; the
    /// closures stay here.
    pub fn init(&mut self) -> (String, Vec<kitae_ui::patch::PatchOp>) {
        // Reset the mount (refs stay dense) and drop the patch history —
        // those ops address the OLD refs and would corrupt a fresh client
        // mount. `next_seq` stays monotonic so a client acking an old seq
        // still lands below it (empty replay, no re-Init loop).
        self.mount = Mount::new(NodeRef(0));
        self.history.clear();
        crate::set_request_path(&self.path);
        let view = (self.render)();
        let ops = self.mount.init(&view);
        self.sync_handlers();
        let html = kitae_ui::render::render_html(&view.nodes);
        (html, ops)
    }

    /// Re-render (a handler mutated a signal) and return the patch with its
    /// sequence number. The ops are recorded for resume.
    pub fn update(&mut self) -> (u64, Vec<kitae_ui::patch::PatchOp>) {
        crate::set_request_path(&self.path);
        let view = (self.render)();
        let ops = self.mount.diff(&view);
        self.sync_handlers();
        let seq = self.next_seq;
        self.next_seq += 1;
        if !ops.is_empty() {
            self.history.push((seq, ops.clone()));
            if self.history.len() > MAX_HISTORY {
                self.history.remove(0);
            }
        }
        (seq, ops)
    }

    /// Patches past `acked` in order — what a resuming client replays.
    /// `None` when the ack predates the history window (fresh `Init`) or
    /// claims a seq we never issued (a lying/hostile peer — also `Init`).
    pub fn resume_from(&self, acked: u64) -> Option<Vec<(u64, Vec<kitae_ui::patch::PatchOp>)>> {
        if acked >= self.next_seq {
            return None;
        }
        // The ack must be a seq we still hold (or the pre-history 0).
        let oldest = self
            .history
            .first()
            .map(|(s, _)| s.saturating_sub(1))
            .unwrap_or(0);
        if acked + 1 < oldest {
            return None;
        }
        Some(
            self.history
                .iter()
                .filter(|(s, _)| *s > acked)
                .map(|(s, ops)| (*s, ops.clone()))
                .collect(),
        )
    }

    /// Route a client event into the author's handlers. True when a
    /// handler ran. This is the only entry point by which the network can
    /// make the tree change — and it can only run authored Rust.
    pub fn dispatch(&mut self, element: u64, name: &str, value: Option<String>) -> bool {
        let Some(hs) = self.handlers.get(&element) else {
            return false;
        };
        let Some(handler) = hs.iter().find(|(ev, _)| ev == name).map(|(_, h)| h.clone()) else {
            return false;
        };
        handler(Event {
            name: name.to_string(),
            value,
        });
        true
    }

    /// Rebuild the wire-dispatch table from the mount's live table.
    fn sync_handlers(&mut self) {
        self.handlers = self.mount.handlers().clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kitae_ui::prelude::*;

    fn counter_session() -> Session {
        let count = use_signal(|| 0u32);
        let render = move || {
            view! {
                button(on:click = move |_| count.update(|c| *c += 1)) { "Count: {count}" }
            }
        };
        Session::new(Rc::new(render))
    }

    #[test]
    fn init_yields_static_html_plus_ops() {
        let mut s = counter_session();
        let (html, ops) = s.init();
        assert!(html.contains("Count: 0"));
        assert!(!html.contains("on:"), "handlers never serialize into HTML");
        assert_eq!(ops.len(), 2, "button + its text node");
    }

    #[test]
    fn dispatch_runs_authored_handler_and_diffs() {
        let mut s = counter_session();
        s.init();
        // The button ref is 1 (0 is the mount root).
        assert!(s.dispatch(1, "click", None));
        let (_, ops) = s.update();
        assert!(matches!(
            ops.as_slice(),
            [kitae_ui::patch::PatchOp::SetText { text, .. }] if text == "Count: 1"
        ));
        // Unknown refs / events are refused, not executed.
        assert!(!s.dispatch(99, "click", None));
        assert!(!s.dispatch(1, "keydown", None));
    }

    #[test]
    fn resume_replays_missed_patches_only() {
        let mut s = counter_session();
        s.init();
        s.dispatch(1, "click", None);
        s.update(); // seq 1 (0 is the hydration marker)
        s.dispatch(1, "click", None);
        s.update(); // seq 2
        s.dispatch(1, "click", None);
        s.update(); // seq 3
                    // Ack 0 = "hydrated, seen nothing": every real update replays.
        let resume = s.resume_from(0).unwrap();
        assert_eq!(resume.len(), 3, "everything past the hydration marker");
        assert_eq!(resume[0].0, 1);
        let fresh = s.resume_from(3).unwrap();
        assert!(fresh.is_empty(), "client saw everything");
    }

    #[test]
    fn stale_ack_demands_fresh_init() {
        let mut s = counter_session();
        s.init();
        s.dispatch(1, "click", None);
        s.update();
        // A fabricated old ack (beyond no window) → None → re-Init.
        assert!(s.resume_from(u64::MAX - 2).is_none());
    }
}
