//! Track A completion tests: A4 error boundaries, A5 resources, A6
//! memoization, A7 forms, A8 router, A9 strictness.

use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use kitae_ui::diff::Mount;
use kitae_ui::hooks::wrap_render;
use kitae_ui::patch::NodeRef;
use kitae_ui::prelude::*;
use kitae_ui::render::render_html;
use kitae_ui::vnode::Event;

fn click_handler(m: &Mount, ops: &[PatchOp]) -> (u64, kitae_ui::vnode::Handler) {
    let r = ops
        .iter()
        .find_map(|op| match op {
            PatchOp::InsertNode { node, .. } if node.handler.is_some() => node.handler,
            _ => None,
        })
        .expect("handler-bearing node");
    let h = m.handlers()[&r][0].1.clone();
    (r, h)
}

// ── A4 · error boundaries ───────────────────────────────────────────────────

#[component]
fn Exploder(explode: Signal<bool>) -> View {
    if explode.get() {
        panic!("boom during render");
    }
    view! { p { "all good" } }
}

#[test]
fn error_boundary_catches_and_recovers() {
    let explode = use_signal(|| true);
    let render = wrap_render(Rc::new(move || {
        view! {
            div {
                ErrorBoundary(view! { p { "widget failed" } }) {
                    {Exploder(explode)}
                }
            }
        }
    }));
    let mut m = Mount::new(NodeRef(0));
    let v = render();
    let _ = m.init(&v);
    let html = render_html(&v.nodes);
    assert!(html.contains("widget failed"), "fallback rendered: {html}");
    assert!(!html.contains("all good"), "broken child not rendered");
    // Recovery: the panic source is gone → the real child returns.
    explode.set(false);
    let v2 = render();
    let _ = m.diff(&v2);
    assert!(
        render_html(&v2.nodes).contains("all good"),
        "recovers after the panic source is gone: {}",
        render_html(&v2.nodes)
    );
}

// ── A6 · subtree memoization ────────────────────────────────────────────────

#[component]
fn MemoRow(row: (u64, String)) -> View {
    let builds = use_ref(|| 0u32);
    let (ver, text) = row;
    let counted = builds.clone();
    // The counter lives INSIDE the memoized build: it only advances when
    // the closure actually runs, so the rendered count proves skipping.
    let body = use_memo_view(ver, move || {
        *counted.borrow_mut() += 1;
        let n = counted.borrow().to_string();
        view! { li { "{text} · built {n}" } }
    });
    let count = builds.borrow().to_string();
    view! {
        div { "builds: {count}" {body} }
    }
}

#[test]
fn memo_skips_rebuild_when_deps_unchanged() {
    let row = use_signal(|| (1u64, "alpha".to_string()));
    let render = wrap_render(Rc::new(move || {
        view! {
            ul { {MemoRow(row.get())} }
        }
    }));
    let mut m = Mount::new(NodeRef(0));
    let v1 = render();
    let _ = m.init(&v1);
    let v2 = render();
    let _ = m.diff(&v2);
    assert!(
        render_html(&v2.nodes).contains("builds: 1"),
        "deps unchanged → no rebuild: {}",
        render_html(&v2.nodes)
    );
    // Bump the deps: the row rebuilds exactly once more.
    row.update(|(ver, _)| *ver += 1);
    let v3 = render();
    let _ = m.diff(&v3);
    assert!(
        render_html(&v3.nodes).contains("builds: 2"),
        "deps changed → rebuilt: {}",
        render_html(&v3.nodes)
    );
}

#[component]
fn MemoThenHook(row: (u64, String)) -> View {
    // A memo whose deps change, followed by ANOTHER hook: the stale memo
    // slot is replaced in place — if it were inserted instead, the later
    // hook would read the memo's slot and the scope would corrupt.
    let body = use_memo_view(row.0, || view! { li { {row.1.clone()} } });
    let tail = use_signal(|| String::from("tail"));
    view! {
        li { {row.1.clone()} "|" {tail.get()} {body} }
    }
}

#[test]
fn memo_deps_change_does_not_shift_later_hooks() {
    let row = use_signal(|| (1u64, "one".to_string()));
    let render = wrap_render(Rc::new(move || {
        view! { ul { {MemoThenHook(row.get())} } }
    }));
    let mut m = Mount::new(NodeRef(0));
    let v1 = render();
    let _ = m.init(&v1);
    assert!(
        render_html(&v1.nodes).contains("one|tail"),
        "first build shape: {}",
        render_html(&v1.nodes)
    );
    // Deps change → memo slot replaced IN PLACE; the tail signal after it
    // must stay in its own slot.
    row.set((2u64, "two".to_string()));
    let v2 = render();
    let _ = m.diff(&v2);
    assert!(
        render_html(&v2.nodes).contains("two|tail"),
        "memo rebuilt in place, tail intact: {}",
        render_html(&v2.nodes)
    );
}

// ── A7 · forms layer ────────────────────────────────────────────────────────

#[test]
fn form_tracks_touched_and_submits() {
    let form = use_form();
    form.field("email");
    assert!(!form.is_touched("email"));
    form.touch("email");
    assert!(form.is_touched("email"));
    assert_eq!(form.submit(), 1);
    assert_eq!(form.submits(), 1);
    // A second form is independent state.
    let other = use_form();
    assert!(!other.is_touched("email"));
    assert_eq!(other.submit(), 1);
}

// ── A8 · SPA router core ────────────────────────────────────────────────────

#[component]
fn DocsPage() -> View {
    view! { h1 { "The docs" } }
}

#[component]
fn HomePage() -> View {
    view! { h1 { "The home" } }
}

#[test]
fn router_navigates_in_place_via_link() {
    let router = Router::new("/".to_string());
    let render = wrap_render(Rc::new(move || {
        let page = if router.current() == "/docs" {
            DocsPage()
        } else {
            HomePage()
        };
        view! {
            div {
                Link(router, "/docs".to_string()) { "go docs" }
                {page}
            }
        }
    }));
    let mut m = Mount::new(NodeRef(0));
    let v1 = render();
    let ops = m.init(&v1);
    assert!(render_html(&v1.nodes).contains("The home"));
    // Click the Link: the router signal changes; the page patches in place.
    let (_r, handler) = click_handler(&m, &ops);
    handler(Event {
        name: "click".into(),
        value: None,
    });
    let v2 = render();
    let ops2 = m.diff(&v2);
    assert!(!ops2.is_empty(), "navigation produces a patch");
    let html = render_html(&v2.nodes);
    assert!(html.contains("The docs"), "navigated: {html}");
    assert!(!html.contains("The home"), "old page gone: {html}");
}

// ── A5 · resources (slot-stable fetch + refetch) ────────────────────────────

#[component]
fn DataTile(hits: Arc<AtomicU32>) -> View {
    let h = hits.clone();
    let _res = use_resource(move || h.fetch_add(1, Ordering::SeqCst));
    view! { p { "tile" } }
}

#[test]
fn resource_fetches_once_across_re_renders() {
    let hits = Arc::new(AtomicU32::new(0));
    let counted = hits.clone();
    let render = wrap_render(Rc::new(move || DataTile(counted.clone())));
    let mut m = Mount::new(NodeRef(0));
    let v1 = render();
    let _ = m.init(&v1);
    let v2 = render();
    let _ = m.diff(&v2);
    // The fetch runs on a spawned thread — wait for it to show up.
    let mut saw = 0;
    for _ in 0..100 {
        saw = hits.load(Ordering::SeqCst);
        if saw >= 1 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(saw, 1, "initial fetch ran exactly once");
    // Grace window: a (broken) per-render re-fetch would show up here.
    std::thread::sleep(std::time::Duration::from_millis(150));
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "second render must reuse the slot-stable resource, not re-fetch"
    );
}

// ── A9 · dev strictness ─────────────────────────────────────────────────────

#[test]
fn strict_warns_on_mixed_keyed_lists() {
    kitae_ui::kit::warn("strict-probe-warning");
    let mut m = Mount::new(NodeRef(0));
    let v1 = view! {
        ul { li(key: 1) { "keyed" } }
    };
    let _ = m.init(&v1);
    // A sibling WITHOUT a key joins the keyed one: mixed list → warning.
    let v2 = view! {
        ul { li(key: 1) { "keyed" } li { "unkeyed" } }
    };
    let _ = m.diff(&v2);
    let drained = kitae_ui::kit::take_warnings();
    assert!(
        drained.iter().any(|w| w.contains("mixes")),
        "mixed keyed/unkeyed list warned: {drained:?}"
    );
    assert!(drained.iter().any(|w| w.contains("strict-probe-warning")));
}
