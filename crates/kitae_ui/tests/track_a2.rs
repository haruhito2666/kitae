//! Track A follow-ups: A3 named slots, A5 deps/error/suspense/cache, A6
//! `#[component(skip_eq)]`, A7 form dirty/error/reset, A8 router
//! history + params/query, A9 render-purity strictness.
//!
//! One test holds every `take_warnings()` drain so parallel test threads in
//! this binary can't steal each other's warnings.

use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use kitae_ui::diff::Mount;
use kitae_ui::hooks::wrap_render_strict;
use kitae_ui::kit::{query_param, route_param};
use kitae_ui::patch::NodeRef;
use kitae_ui::prelude::*;
use kitae_ui::render::render_html;
use kitae_ui::resource::{suspense_view, use_cached_resource, use_resource_with_deps};
use kitae_ui::vnode::Slots;

// ── A3 · named slots ────────────────────────────────────────────────────────

#[component]
fn Card(title: String, slots: Slots) -> View {
    let footer = if slots.has("footer") {
        slots.render("footer")
    } else {
        view! { span { "default footer" } }
    };
    view! {
        div(class: "card") {
            h2 { "{title}" }
            header { {slots.render("header")} }
            section { {slots.render("children")} }
            {footer}
        }
    }
}

#[test]
fn named_slots_render_where_spliced() {
    let v = view! {
        div {
            Card("Welcome".to_string()) {
                slot(name: "header") { nav { "site nav" } }
                slot(name: "footer") { span { "custom footer" } }
                p { "body text" }
            }
        }
    };
    let html = render_html(&v.nodes);
    assert!(html.contains("Welcome"), "{html}");
    assert!(html.contains("site nav"), "header slot rendered: {html}");
    assert!(
        html.contains("body text"),
        "leftover → children slot: {html}"
    );
    assert!(
        html.contains("custom footer"),
        "named footer slot rendered: {html}"
    );
    assert!(
        !html.contains("default footer"),
        "present slot wins over the fallback: {html}"
    );
    // Splice order: the header slot content precedes the children slot.
    let at_header = html.find("site nav").expect("header");
    let at_body = html.find("body text").expect("children");
    assert!(at_header < at_body, "slots splice where placed: {html}");
}

#[test]
fn named_slots_fallback_when_absent() {
    let v = view! {
        div {
            Card("T".to_string()) {
                slot(name: "children") { p { "just body" } }
            }
        }
    };
    let html = render_html(&v.nodes);
    assert!(html.contains("just body"), "{html}");
    assert!(
        html.contains("default footer"),
        "absent slot renders the component's fallback: {html}"
    );
}

#[test]
fn slots_are_lazy_and_queryable() {
    let built = Rc::new(std::cell::Cell::new(false));
    let flag = built.clone();
    let s = Slots::new(vec![(
        "a".to_string(),
        kitae_ui::__rt::make_children(move || {
            flag.set(true);
            view! { p { "A" } }
        }),
    )]);
    assert_eq!(s.names(), vec!["a".to_string()]);
    assert!(s.has("a"));
    assert!(!s.has("b"));
    assert!(!built.get(), "slot builds lazily, only on render");
    assert!(render_html(&s.render("a").nodes).contains("A"));
    assert!(built.get());
    assert!(
        render_html(&s.render("missing").nodes).is_empty(),
        "absent name renders the empty view"
    );
}

// ── A5 · deps-driven refetch, error state, suspense, identity cache ────────

#[component]
fn DepsTile(ver: u64, hits: Arc<AtomicU32>) -> View {
    let h = hits.clone();
    let _res = use_resource_with_deps(ver, move || h.fetch_add(1, Ordering::SeqCst));
    view! { p { "tile" } }
}

#[test]
fn resource_with_deps_refetches_only_on_change() {
    let hits = Arc::new(AtomicU32::new(0));
    let ver = use_signal(|| 1u64);
    let counted = hits.clone();
    let render = wrap_render(Rc::new(move || DepsTile(ver.get(), counted.clone())));
    let mut m = Mount::new(NodeRef(0));
    let v1 = render();
    let _ = m.init(&v1);
    let v2 = render();
    let _ = m.diff(&v2);
    for _ in 0..100 {
        // The runtime's poll loop drains every tick; a landing is what
        // clears the in-flight latch, so a coalesced follow-up only fires
        // once the poll runs. Mirror it here.
        kitae_ui::resource::drain();
        if hits.load(Ordering::SeqCst) >= 1 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    std::thread::sleep(std::time::Duration::from_millis(150));
    assert_eq!(hits.load(Ordering::SeqCst), 1, "same deps → no refetch");
    // Bump the deps: exactly one more fetch.
    ver.set(2);
    let v3 = render();
    let _ = m.diff(&v3);
    for _ in 0..100 {
        kitae_ui::resource::drain();
        if hits.load(Ordering::SeqCst) >= 2 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(hits.load(Ordering::SeqCst), 2, "changed deps → refetch");
    std::thread::sleep(std::time::Duration::from_millis(150));
    assert_eq!(hits.load(Ordering::SeqCst), 2, "and only one");
}

#[test]
fn resource_error_surfaces_via_catch_unwind() {
    let r = use_resource(|| -> u32 { panic!("kaboom") });
    for _ in 0..200 {
        if r.error().is_some() {
            break;
        }
        if kitae_ui::resource::drain() && r.error().is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(r.error().as_deref(), Some("kaboom"), "panic → error state");
    assert!(r.get().is_none());
    assert!(!r.is_loading(), "a failed fetch is no longer loading");
}

#[test]
fn suspense_shows_fallback_until_ready() {
    let r = use_resource(|| 5u32);
    let v1 = suspense_view(&r, view! { p { "loading" } }, |v| {
        view! { p { "got {v}" } }
    });
    assert!(
        render_html(&v1.nodes).contains("loading"),
        "fallback while in flight: {}",
        render_html(&v1.nodes)
    );
    for _ in 0..200 {
        if kitae_ui::resource::drain() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let v2 = suspense_view(&r, view! { p { "loading" } }, |v| {
        view! { p { "got {v}" } }
    });
    assert!(
        render_html(&v2.nodes).contains("got 5"),
        "ready view once landed: {}",
        render_html(&v2.nodes)
    );
}

#[component]
fn CachedTile(hits: Arc<AtomicU32>, tag: &'static str) -> View {
    let h = hits.clone();
    let res = use_cached_resource("track-a2-shared-key", move || {
        h.fetch_add(1, Ordering::SeqCst);
        42u32
    });
    let v = res.get().unwrap_or(0);
    view! { p { "{tag}:{v}" } }
}

#[test]
fn cached_resource_hits_identity_cache_without_a_thread() {
    let hits = Arc::new(AtomicU32::new(0));
    let counted = hits.clone();
    let render = wrap_render(Rc::new(move || {
        view! {
            div {
                {CachedTile(counted.clone(), "a")}
                {CachedTile(counted.clone(), "b")}
            }
        }
    }));
    let mut m = Mount::new(NodeRef(0));
    let v1 = render();
    let _ = m.init(&v1);
    // Two identities miss the cache → two fetches (then they land).
    for _ in 0..200 {
        if kitae_ui::resource::drain() {
            continue;
        }
        if hits.load(Ordering::SeqCst) >= 2 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let before = hits.load(Ordering::SeqCst);
    assert!(
        before >= 2,
        "two identities fetched once each (got {before})"
    );
    // Re-render: both probe the cache, hit, and never spawn a thread.
    let v2 = render();
    let _ = m.diff(&v2);
    std::thread::sleep(std::time::Duration::from_millis(150));
    assert_eq!(
        hits.load(Ordering::SeqCst),
        before,
        "cache hit → no refetch"
    );
    let html = render_html(&v2.nodes);
    assert!(html.contains("a:42"), "{html}");
    assert!(html.contains("b:42"), "{html}");
}

// ── A6 · #[component(skip_eq)] ──────────────────────────────────────────────

static EQ_BUILDS: AtomicU32 = AtomicU32::new(0);

#[component(skip_eq)]
fn EqRow(n: u32) -> View {
    EQ_BUILDS.fetch_add(1, Ordering::SeqCst);
    view! { li { "row {n}" } }
}

#[test]
fn skip_eq_component_skips_identical_props() {
    let n = use_signal(|| 7u32);
    let render = wrap_render(Rc::new(move || view! { ul { {EqRow(n.get())} } }));
    let mut m = Mount::new(NodeRef(0));
    let v1 = render();
    let _ = m.init(&v1);
    assert_eq!(EQ_BUILDS.load(Ordering::SeqCst), 1, "first render builds");
    // The parent re-renders (an unrelated signal read changes nothing here,
    // but the render closure runs again) — identical props → body skipped.
    let v2 = render();
    let _ = m.diff(&v2);
    assert_eq!(
        EQ_BUILDS.load(Ordering::SeqCst),
        1,
        "identical props → body skipped, cached view reused"
    );
    // Changed props → exactly one more build.
    n.set(8);
    let v3 = render();
    let _ = m.diff(&v3);
    assert_eq!(
        EQ_BUILDS.load(Ordering::SeqCst),
        2,
        "changed props → rebuild"
    );
    assert!(
        render_html(&v3.nodes).contains("row 8"),
        "rebuilt view shows the new props: {}",
        render_html(&v3.nodes)
    );
}

// ── A7 · form dirty / error / reset ─────────────────────────────────────────

#[test]
fn form_tracks_dirty_errors_and_reset() {
    let form = use_form();
    let email = form.field_value("email", "a@b.c");
    assert_eq!(email.get(), "a@b.c");
    assert!(!form.is_dirty("email"));
    assert!(!form.is_dirty_any());
    email.set("x@y.z".to_string());
    assert!(form.is_dirty("email"));
    assert!(form.is_dirty_any());
    // The initial is recorded once: a later registration doesn't clobber.
    let again = form.field_value("email", "other");
    assert_eq!(again.get(), "x@y.z", "initial recorded once");
    // Errors survive until cleared/reset.
    form.set_error("email", "not an email");
    assert_eq!(form.error("email").as_deref(), Some("not an email"));
    form.clear_error("email");
    assert_eq!(form.error("email"), None);
    form.set_error("email", "still bad");
    form.touch("email");
    assert!(form.is_touched("email"));
    form.submit();
    assert_eq!(form.submits(), 1);
    // set_value auto-registers a never-declared field.
    form.set_value("nick", "kit");
    assert_eq!(form.value("nick"), "kit");
    assert!(form.is_dirty("nick"));
    // Reset restores initials, clears touched/errors, zeroes submits.
    form.reset();
    assert_eq!(email.get(), "a@b.c", "reset restores initials");
    assert_eq!(
        form.value("nick"),
        "",
        "auto-registered field resets to \"\""
    );
    assert!(!form.is_dirty("email"));
    assert!(!form.is_dirty_any());
    assert!(!form.is_touched("email"));
    assert_eq!(form.error("email"), None);
    assert_eq!(form.submits(), 0);
}

// ── A8 · router history, params, query ──────────────────────────────────────

#[test]
fn router_history_back_forward_and_forks() {
    let router = Router::new("/".to_string());
    assert!(!router.can_back());
    assert!(!router.can_forward());
    router.navigate("/docs");
    router.navigate("/docs"); // same path → no-op
    router.navigate("/pricing");
    assert_eq!(router.current(), "/pricing");
    assert!(router.can_back());
    assert!(!router.can_forward());
    router.back();
    assert_eq!(router.current(), "/docs");
    assert!(router.can_forward(), "a back step opens the forward stack");
    router.back();
    assert_eq!(router.current(), "/");
    assert!(!router.can_back());
    router.forward();
    assert_eq!(router.current(), "/docs");
    router.forward();
    assert_eq!(router.current(), "/pricing");
    assert!(!router.can_forward());
    // A fresh navigation forks history: the forward stack clears.
    router.back();
    router.navigate("/blog");
    assert_eq!(router.current(), "/blog");
    assert!(!router.can_forward(), "fork clears the forward stack");
    router.back();
    assert_eq!(router.current(), "/docs");
}

#[test]
fn router_params_and_query_decode() {
    assert_eq!(
        route_param("/users/42/posts/7", "/users/:id/posts/:post_id", "id").as_deref(),
        Some("42")
    );
    assert_eq!(
        route_param("/users/42/posts/7", "/users/:id/posts/:post_id", "post_id").as_deref(),
        Some("7")
    );
    // Literal segments must match; the query string is ignored when matching.
    assert_eq!(route_param("/users/42", "/posts/:id", "id"), None);
    assert_eq!(route_param("/users/42/extra", "/users/:id", "id"), None);
    assert_eq!(
        route_param("/users/a%20b", "/users/:id", "id").as_deref(),
        Some("a b")
    );
    assert_eq!(
        route_param("/users/42?tab=1", "/users/:id", "id").as_deref(),
        Some("42")
    );
    assert_eq!(
        query_param("/list?tab=2&sort=asc", "tab").as_deref(),
        Some("2")
    );
    assert_eq!(
        query_param("/list?tab=2&sort=asc", "sort").as_deref(),
        Some("asc")
    );
    assert_eq!(query_param("/list?tab=2&sort=asc", "missing"), None);
    // Percent-decoding and `+` → space.
    assert_eq!(query_param("/s?q=a%20b+c", "q").as_deref(), Some("a b c"));
    // Router methods read the current path.
    let r = Router::new("/users/9?tab=3".to_string());
    assert_eq!(r.param("/users/:id", "id").as_deref(), Some("9"));
    assert_eq!(r.query("tab").as_deref(), Some("3"));
}

// ── A9 · render purity ──────────────────────────────────────────────────────

#[component]
fn EffectSeeds() -> View {
    let n = use_signal(|| 0u32);
    let _h = use_effect(move || n.set(41));
    let v = n.get();
    view! { p { "n={v}" } }
}

#[test]
fn strictness_flags_impure_render_but_allows_effects() {
    // 1) A pure render: wrap_render_strict's double pass agrees — silent.
    {
        let label = use_signal(|| "pure".to_string());
        let render = wrap_render_strict(Rc::new(move || {
            let l = label.get();
            view! { p { "{l}" } }
        }));
        let _ = render();
    }
    let drained = kitae_ui::kit::take_warnings();
    assert!(
        !drained.iter().any(|w| w.contains("impure render")),
        "pure render is silent: {drained:?}"
    );

    // 2) Writing state during render: both detectors fire — the per-signal
    //    write warning and the two-passes-disagree warning.
    {
        let s = use_signal(|| 0u32);
        let render = wrap_render_strict(Rc::new(move || {
            s.set(s.get() + 1);
            let n = s.get();
            view! { p { "n={n}" } }
        }));
        let _ = render();
    }
    let drained = kitae_ui::kit::take_warnings();
    assert!(
        drained.iter().any(|w| w.contains("written during render")),
        "signal write during render warned: {drained:?}"
    );
    assert!(
        drained.iter().any(|w| w.contains("impure render")),
        "two passes disagreeing warned: {drained:?}"
    );

    // 3) The sanctioned path: a first-mount use_effect seeding state writes
    //    during render — suppressed, no warning.
    {
        let render = wrap_render(Rc::new(move || EffectSeeds()));
        let mut m = Mount::new(NodeRef(0));
        let v = render();
        let _ = m.init(&v);
        assert!(
            render_html(&v.nodes).contains("n=41"),
            "effect seeded the state: {}",
            render_html(&v.nodes)
        );
    }
    let drained = kitae_ui::kit::take_warnings();
    assert!(
        !drained.iter().any(|w| w.contains("written during render")),
        "effect writes are sanctioned: {drained:?}"
    );
}
