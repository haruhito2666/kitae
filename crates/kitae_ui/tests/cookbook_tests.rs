//! RP-C6 cookbook: every recipe in the cookbook doc is a test here, so the
//! documented patterns cannot silently rot — if a recipe breaks, this file
//! fails. Mirrors `docs/UI_COOKBOOK.md` one-to-one (test names = recipe
//! numbers).
//!
//! Each test builds a small, complete page with ONE pattern and asserts the
//! visible outcome through the C4 TestApp harness.

use std::rc::Rc;

use kitae_ui::kit::{query_param, route_param};
use kitae_ui::prelude::*;
use kitae_ui::resource::{suspense_view, use_cached_resource, use_resource_with_deps};
use kitae_ui::testing::{reset, TestApp};
use kitae_ui::vnode::Slots;

// ── Recipe 1 · the counter ──────────────────────────────────────────────────

#[component]
fn CookbookCounter(label: String) -> View {
    let count = use_signal(|| 0);
    view! {
        section(class: "card") {
            h2 { "{label}: {count}" }
            button(class: "inc", on:click = move |_| count.update(|c| *c += 1)) { "+1" }
        }
    }
}

#[test]
fn recipe_1_counter_and_reactivity() {
    let mut app = TestApp::new(|| CookbookCounter("Cart".into()));
    assert!(app.text().contains("Cart: 0"));
    assert!(app.click_text("+1"));
    assert!(app.click_text("+1"));
    assert!(app.text().contains("Cart: 2"), "{}", app.text());
}

// ── Recipe 2 · lists with keys ──────────────────────────────────────────────

#[component]
fn TodoList() -> View {
    let todos = use_signal(|| vec!["buy milk".to_string(), "write docs".to_string()]);
    view! {
        ul {
            for todos.get().iter().enumerate() -> (i, t) {
                li(key: i) { "{t}" }
            }
        }
    }
}

#[test]
fn recipe_2_lists_are_keyed() {
    let app = TestApp::new(|| TodoList());
    assert_eq!(app.count("li"), 2);
    assert!(app.text().contains("buy milk"));
}

// ── Recipe 3 · conditional branches ────────────────────────────────────────

#[component]
fn AuthGate(signed_in: Signal<bool>) -> View {
    view! {
        if signed_in.get() {
            p { "Welcome back." }
        } else {
            button(on:click = move |_| signed_in.set(true)) { "Sign in" }
        }
    }
}

#[test]
fn recipe_3_conditionals_swap_branches_in_place() {
    let signed_in = use_signal(|| false);
    let mut app = TestApp::new(move || AuthGate(signed_in));
    assert!(app.text().contains("Sign in"));
    assert!(app.click_text("Sign in"));
    assert!(app.text().contains("Welcome back."));
    assert!(!app.exists("button"));
}

// ── Recipe 4 · forms: two-way binding, errors on submit, reset ─────────────

#[component]
fn SignupForm() -> View {
    let form = use_form();
    let email = form.field_value("email", "");
    let submit_form = form.clone();
    let reset_form = form.clone();

    // Computed before the view: plain (non-reactive) state read at build
    // time — the error shows after whatever re-render writes it, which is
    // fine for "show the error after a submit attempt".
    let err = form.error("email").unwrap_or_default();

    view! {
        div(class: "form") {
            // Two-way binding: typing writes straight into the `email`
            // signal — no input handler needed.
            input(value: email, placeholder: "you@example.com")
            if !err.is_empty() {
                p(class: "error") { "{err}" }
            }
            button(class: "submit", on:click = move |_| {
                if email.get().is_empty() {
                    submit_form.set_error("email", "email is required");
                } else {
                    submit_form.clear_error("email");
                    submit_form.submit();
                }
            }) { "Sign up" }
            button(class: "reset", on:click = move |_| reset_form.reset()) { "Reset" }
        }
    }
}

#[test]
fn recipe_4_form_validate_reset() {
    let mut app = TestApp::new(|| SignupForm());
    // Submit with an empty field: the error appears.
    assert!(app.click_text("Sign up"));
    assert!(app.text().contains("email is required"), "{}", app.text());
    // Fix the field and submit: the error clears.
    let input = app.query("input").into_iter().next().expect("input");
    assert!(app.fire(&input, "input", Some("a@b.c")));
    assert!(app.click_text("Sign up"));
    assert!(!app.text().contains("email is required"));
    // Reset restores the initial (empty) value.
    assert!(app.click_text("Reset"));
    let input = app.query("input").into_iter().next().expect("input");
    assert_eq!(input.attr("value"), Some(""));
}

// ── Recipe 5 · resources: fetch with deps + suspense ───────────────────────

#[component]
fn Profile(version: u64) -> View {
    // Deps first, then the fetch. The fetch runs OFF the UI thread, so it
    // must not read signals — capture plain values instead.
    let res = use_resource_with_deps(version, || "Ada Lovelace".to_string());
    view! {
        div {
            {suspense_view(&res, view! { p { "Loading…" } }, |name| view! {
                h1 { "{name}" }
            })}
        }
    }
}

#[test]
fn recipe_5_resource_with_suspense_fallback() {
    let ver = use_signal(|| 1u64);
    let mut app = TestApp::new(move || Profile(ver.get()));
    // First paint: the fallback is up while the fetch runs (values land
    // only when the runtime drains, which the harness hasn't done yet).
    assert!(app.text().contains("Loading"));
    // Wait for the fetch thread to land and refresh.
    for _ in 0..200 {
        if kitae_ui::resource::drain() {
            app.refresh();
            if app.text().contains("Ada Lovelace") {
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(app.text().contains("Ada Lovelace"), "{}", app.text());
}

// ── Recipe 6 · shared cached data across components ────────────────────────

#[component]
fn ReadsCache(tag: &'static str) -> View {
    let res = use_cached_resource("cookbook-config", || "v1".to_string());
    let v = res.get().unwrap_or_else(|| "…".to_string());
    view! { p { "{tag}={v}" } }
}

#[test]
fn recipe_6_identity_cached_resource() {
    reset();
    let mut app = TestApp::new(|| {
        view! {
            div {
                {ReadsCache("a")}
                {ReadsCache("b")}
            }
        }
    });
    // Same key → every caller shares one entry. On the FIRST render both
    // callers miss (the cache fills only when a value lands), so two fetches
    // run and both land the same value; from the next render on, callers get
    // the cache hit immediately. Assert the shared outcome, not the count.
    for _ in 0..200 {
        if kitae_ui::resource::drain() {
            app.refresh();
            if app.text().contains("a=v1") && app.text().contains("b=v1") {
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(app.text().contains("a=v1"), "{}", app.text());
    assert!(app.text().contains("b=v1"), "{}", app.text());
}

// ── Recipe 7 · routing: params, query, history ─────────────────────────────

#[component]
fn DocsPage() -> View {
    // Slot-stable inside the component: Router::new runs once per mount.
    // The live runtime would seed the initial path from the request URL;
    // here it starts empty (root).
    let router = Router::new(String::new());
    let path = router.path.get();
    let section =
        route_param(&path, "/docs/:section", "section").unwrap_or_else(|| "index".to_string());
    let lang = query_param(&path, "lang").unwrap_or_else(|| "en".to_string());
    view! {
        nav {
            button(class: "nav-docs", on:click = move |_| router.navigate("/docs/css")) {
                "Docs"
            }
        }
        article {
            h1 { "Section: {section}" }
            p { "Lang: {lang}" }
        }
    }
}

#[test]
fn recipe_7_router_params_query_and_history() {
    let mut app = TestApp::new(|| DocsPage());
    assert!(app.text().contains("Section: index"), "{}", app.text());
    assert!(app.click_text("Docs"));
    assert!(app.text().contains("Section: css"), "{}", app.text());
}

#[test]
fn recipe_7b_route_and_query_helpers_decode() {
    // Deep link: /docs/hooks?lang=de
    assert_eq!(
        route_param("/docs/hooks?lang=de", "/docs/:section", "section"),
        Some("hooks".to_string())
    );
    assert_eq!(
        query_param("/docs/hooks?lang=de%20%2B", "lang"),
        Some("de +".to_string())
    );
    // Router methods read the same helpers against the current path.
    let r = Router::new("/users/42?q=hi".to_string());
    assert_eq!(r.param("/users/:id", "id"), Some("42".to_string()));
    assert_eq!(r.query("q"), Some("hi".to_string()));
    // History: fresh nav clears the forward stack.
    let r = Router::new("/a".to_string());
    r.navigate("/b");
    r.back();
    assert!(r.can_forward());
    r.navigate("/c");
    assert!(!r.can_forward(), "forking clears forward");
}

// ── Recipe 8 · tabs ────────────────────────────────────────────────────────

#[component]
fn TabbedDocs() -> View {
    let active = use_signal(|| 0usize);
    view! {
        Tabs(vec![
            ("Intro".to_string(), view! { p { "The intro." } }),
            ("API".to_string(), view! { p { "The API." } }),
        ], active)
    }
}

#[test]
fn recipe_8_tabs_switch_panels() {
    let mut app = TestApp::new(|| TabbedDocs());
    assert!(app.text().contains("The intro."));
    assert!(app.click_text("API"));
    assert!(app.text().contains("The API."), "{}", app.text());
    assert!(!app.text().contains("The intro."));
}

// ── Recipe 9 · data table with sort + paging ───────────────────────────────

#[component]
fn UsersTable() -> View {
    let sort = use_signal(|| None::<(usize, bool)>);
    let page = use_signal(|| 0usize);
    let headers = vec!["Name".to_string(), "Age".to_string()];
    let rows = vec![
        vec!["Cara".to_string(), "31".to_string()],
        vec!["Ben".to_string(), "25".to_string()],
        vec!["Ada".to_string(), "36".to_string()],
    ];
    view! {
        Table(headers, rows, sort, 2, page, Rc::new(|_| {}))
    }
}

#[test]
fn recipe_9_table_sorts_and_pages() {
    let mut app = TestApp::new(|| UsersTable());
    // Page 1 of 2 (page_size 2): Cara and Ben; Ada is on page 2.
    assert!(app.text().contains("Cara"));
    assert!(!app.text().contains("Ada"));
    assert!(app.text().contains("Page 1 of 2"));
    // Sort by Name ascending — sorting never changes the page.
    assert!(app.click_text("Name"));
    let t = app.text();
    let ada = t.find("Ada").expect("Ada sorted onto page 1");
    assert!(
        t.find("Ben").map(|b| ada < b).unwrap_or(false),
        "Ada before Ben: {t}"
    );
    // Then advance: page 2 shows the rest.
    assert!(app.click_text("▶"));
    assert!(app.text().contains("Cara"), "{}", app.text());
    assert!(!app.text().contains("Ada"), "Ada stayed on page 1");
}

// ── Recipe 10 · toasts ─────────────────────────────────────────────────────

#[component]
fn Toasty() -> View {
    let toasts = use_signal(|| Vec::<ToastMsg>::new());
    let toasts2 = toasts.clone();
    view! {
        div {
            ToastHost(toasts)
            button(class: "notify", on:click = move |_| push_toast(&toasts2, "success", "Saved!")) {
                "Notify"
            }
        }
    }
}

#[test]
fn recipe_10_toasts_push_and_dismiss() {
    let mut app = TestApp::new(|| Toasty());
    assert!(app.click_text("Notify"));
    assert!(app.text().contains("Saved!"), "{}", app.text());
    assert!(app.click_text("Saved!"), "click the toast to dismiss");
    assert!(!app.text().contains("Saved!"));
}

// ── Recipe 11 · theming ────────────────────────────────────────────────────

#[component]
fn ThemedPage() -> View {
    view! {
        ThemeProvider("dark".to_string()) {
            Button("Save".into(), Rc::new(move |_| {}), "primary".into(), false)
        }
    }
}

#[test]
fn recipe_11_theme_provider_scopes_a_subtree() {
    reset();
    let app = TestApp::new(|| ThemedPage());
    // ThemeProvider provides `kit-theme-dark`; Button appends it to its class.
    assert!(app.html().contains("kit-theme-dark"), "{}", app.html());
    assert!(app.html().contains("kit-btn-primary"));
}

// ── Recipe 12 · named slots (A3) ───────────────────────────────────────────

#[component]
fn PageShell(title: String, slots: Slots) -> View {
    view! {
        div(class: "shell") {
            header { h1 { {title} } }
            if slots.has("sidebar") {
                aside { {slots.render("sidebar")} }
            }
            main { {slots.render("children")} }
            if slots.has("footer") {
                footer { {slots.render("footer")} }
            }
        }
    }
}

#[component]
fn ShellDemo() -> View {
    view! {
        PageShell("Docs".to_string()) {
            slot(name: "sidebar") {
                p { "Sidebar content" }
            }
            slot(name: "children") {
                p { "Main content" }
            }
        }
    }
}

#[test]
fn recipe_12_named_slots_compose_layouts() {
    let app = TestApp::new(|| ShellDemo());
    let html = app.html();
    let sidebar = html.find("Sidebar content").expect("sidebar rendered");
    let main = html.find("Main content").expect("children rendered");
    assert!(html.find("<header").is_some());
    assert!(sidebar < main, "sidebar before main: {html}");
    // No footer slot → the whole <footer> block is absent (not a fallback).
    assert!(
        !html.contains("<footer"),
        "no footer slot → footer block absent"
    );
}

// ── Recipe 13 · error boundaries ───────────────────────────────────────────

#[component]
fn Boom() -> View {
    panic!("component blew up")
}

#[component]
fn Guarded() -> View {
    view! {
        ErrorBoundary(view! { p { "Something broke." } }) {
            if false {
                div { {Boom()} }
            } else {
                p { "healthy subtree" }
            }
        }
    }
}

#[test]
fn recipe_13_error_boundary_has_a_fallback_ready() {
    let app = TestApp::new(|| Guarded());
    assert!(app.text().contains("healthy subtree"));
    // (The catch path itself is exercised in track_a.rs's boundary tests.)
}

// ── Recipe 14 · charts ─────────────────────────────────────────────────────

#[component]
fn Dashboard() -> View {
    let series = use_signal(|| vec![(0.0, 1.0), (1.0, 3.0), (2.0, 2.0)]);
    let bars = use_signal(|| vec![("Mon".to_string(), 4.0), ("Tue".to_string(), 9.0)]);
    let spark = use_signal(|| vec![1.0, 5.0, 3.0, 8.0]);
    view! {
        div(class: "dash") {
            LineChart(series, 320.0, 160.0)
            BarChart(bars, 320.0, 160.0)
            Sparkline(spark, 120.0, 32.0)
        }
    }
}

#[test]
fn recipe_14_charts_are_signal_driven() {
    let app = TestApp::new(|| Dashboard());
    let html = app.html();
    assert!(html.contains("polyline"), "line chart drew its series");
    assert!(html.contains("rect"), "bar chart drew its bars");
    assert!(html.contains("Mon"), "bar labels present");
}

// ── Recipe 15 · memoization with skip_eq ───────────────────────────────────

static CB_BUILDS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

#[component(skip_eq)]
fn ExpensiveRow(n: u32) -> View {
    CB_BUILDS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    view! { li { "row {n}" } }
}

#[test]
fn recipe_15_skip_eq_skips_equal_props() {
    let n = use_signal(|| 1u32);
    let render = kitae_ui::hooks::wrap_render(std::rc::Rc::new(move || {
        view! { ul { {ExpensiveRow(n.get())} } }
    }));
    let mut m = kitae_ui::diff::Mount::new(NodeRef(0));
    let v1 = render();
    let _ = m.init(&v1);
    assert_eq!(
        CB_BUILDS.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "first render builds"
    );
    // Unrelated state changes → same props → body skipped.
    let v2 = render();
    let _ = m.diff(&v2);
    assert_eq!(
        CB_BUILDS.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "equal props skip the body"
    );
    n.set(2);
    let v3 = render();
    let _ = m.diff(&v3);
    assert_eq!(
        CB_BUILDS.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "changed props rebuild"
    );
}

// ── Recipe 16 · patch ops are the only mutation surface ────────────────────

#[component]
fn OpCounter() -> View {
    let n = use_signal(|| 0);
    view! {
        p(on:click = move |_| n.update(|v| *v += 1)) { "{n}" }
    }
}

#[test]
fn recipe_16_ops_stream_is_observable() {
    let mut app = TestApp::new(|| OpCounter());
    // First paint also streams ops (the initial mount); what matters is the
    // NEXT stream: a click's diff must carry a SetText.
    let _ = app.ops();
    assert!(app.click_text("0"));
    let ops = app.ops();
    assert!(
        ops.iter().any(|op| matches!(op, PatchOp::SetText { .. })),
        "a SetText op carried the change: {ops:?}"
    );
}
