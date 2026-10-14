//! C1 component kit — behavior tests through the TestApp harness.

use kitae_ui::components::*;
use kitae_ui::prelude::*;
use kitae_ui::testing::FuzzCfg;
use std::rc::Rc;

/// Click the first element carrying `class` (overlays are document-order
/// traps for `click_text`: the backdrop both contains every label and has
/// the close handler, so overlay tests target nodes).
fn click_node(app: &mut TestApp, class: &str) -> bool {
    let node = app.query_class(class).into_iter().next();
    match node {
        Some(n) => app.fire(&n, "click", None),
        None => false,
    }
}

// ── Button ──────────────────────────────────────────────────────────────────

#[test]
fn button_renders_variant_and_gates_disabled() {
    #[component]
    fn Row() -> View {
        let hits = use_signal(|| 0);
        let bump = Rc::new(move |_| hits.update(|v| *v += 1));
        view! {
            Button("Save".into(), bump.clone(), "primary".into(), false)
            Button("Delete".into(), bump.clone(), "danger".into(), false)
            Button("Locked".into(), bump.clone(), "secondary".into(), true)
        }
    }
    let mut app = TestApp::new(|| Row());
    let html = app.html();
    assert!(html.contains("kit-btn-primary"));
    assert!(html.contains("kit-btn-danger"));

    // Enabled buttons fire.
    assert!(app.click_text("Save"));
    assert!(app.click_text("Delete"));
    assert_eq!(app.query_class("kit-btn").len(), 3);

    // Disabled: presence-based attribute present, and — stronger than CSS —
    // no click handler on the element at all, so no path can activate it.
    let disabled = app
        .query_where(|n| n.tag() == "button" && n.attr("disabled").is_some())
        .into_iter()
        .next()
        .expect("disabled button present");
    assert!(!disabled.has_event("click"));
    let before = app.text();
    app.fire(&disabled, "click", None);
    assert_eq!(app.text(), before, "firing a disabled button does nothing");
}

// ── Modal ───────────────────────────────────────────────────────────────────

#[test]
fn modal_renders_consumes_panel_clicks_and_closes() {
    #[component]
    fn Sheet() -> View {
        let open = use_signal(|| true);
        let close = Rc::new(move |_| open.set(false));
        view! {
            Modal(open.get(), "Confirm".into(), close.clone()) {
                p { "Body text" }
            }
        }
    }
    let mut app = TestApp::new(|| Sheet());
    assert!(app.text().contains("Confirm"));
    assert!(app.text().contains("Body text"));
    let panel = app
        .query_class("kit-modal-panel")
        .into_iter()
        .next()
        .expect("panel rendered");
    // The panel itself swallows bubbled clicks.
    assert!(panel.has_event("click"));
    assert_eq!(app.query_class("kit-modal-backdrop").len(), 1);

    // A click on the backdrop node closes.
    let backdrop = app
        .query_class("kit-modal-backdrop")
        .into_iter()
        .next()
        .unwrap();
    assert!(app.fire(&backdrop, "click", None));
    assert!(
        app.query_class("kit-modal-backdrop").is_empty(),
        "closed via backdrop"
    );

    // Fresh world (same component fn would otherwise reuse slots); × closes.
    kitae_ui::testing::reset();
    let mut app = TestApp::new(|| Sheet());
    assert!(click_node(&mut app, "kit-modal-close"));
    assert!(
        app.query_class("kit-modal-panel").is_empty(),
        "closed via ×"
    );
}

// ── Drawer ──────────────────────────────────────────────────────────────────

#[test]
fn drawer_side_anchor_and_close() {
    #[component]
    fn Side() -> View {
        let open = use_signal(|| true);
        let close = Rc::new(move |_| open.set(false));
        view! {
            Drawer(open.get(), "right".into(), "Settings".into(), close.clone()) {
                p { "Drawer body" }
            }
        }
    }
    let mut app = TestApp::new(|| Side());
    let panel = app
        .query_class("kit-drawer")
        .into_iter()
        .next()
        .expect("drawer panel");
    assert!(panel.attr("class").unwrap().contains("kit-drawer-right"));
    assert!(panel.style("right").is_some(), "right-anchored");
    assert!(panel.style("left").is_none(), "no left anchor");
    assert!(app.text().contains("Drawer body"));
    assert!(click_node(&mut app, "kit-drawer-close"));
    assert!(app.query_class("kit-drawer").is_empty());
}

// ── Tabs ────────────────────────────────────────────────────────────────────

#[test]
fn tabs_switch_panels() {
    #[component]
    fn Pages() -> View {
        let active = use_signal(|| 0usize);
        view! {
            Tabs(
                vec![
                    ("One".into(), view! { p { "first page" } }),
                    ("Two".into(), view! { p { "second page" } }),
                ],
                active,
            )
        }
    }
    let mut app = TestApp::new(|| Pages());
    assert!(app.text().contains("first page"));
    assert!(!app.text().contains("second page"));
    assert!(app.click_text("Two"));
    assert!(app.text().contains("second page"));
    assert!(!app.text().contains("first page"));
    let active_tab = app
        .query_class("kit-tab-active")
        .into_iter()
        .next()
        .expect("one active tab");
    assert_eq!(active_tab.text().trim(), "Two");
}

// ── Tooltip ─────────────────────────────────────────────────────────────────

#[test]
fn tooltip_toggles_on_click() {
    #[component]
    fn Hinted() -> View {
        view! {
            Tooltip("Useful hint".into()) {
                b { "Help" }
            }
        }
    }
    let mut app = TestApp::new(|| Hinted());
    assert!(
        !app.text().contains("Useful hint"),
        "bubble hidden initially"
    );
    assert!(app.click_text("Help"));
    assert!(
        app.text().contains("Useful hint"),
        "bubble shown after click"
    );
    assert!(app.click_text("Help"));
    assert!(!app.text().contains("Useful hint"), "click again hides");
}

// ── Toasts ──────────────────────────────────────────────────────────────────

#[test]
fn toast_push_and_dismiss() {
    // Hoist the signal out of the component so the test can push into it.
    let toasts = use_signal(Vec::new);
    #[component]
    fn Area(toasts: Signal<Vec<ToastMsg>>) -> View {
        view! {
            ToastHost(toasts)
        }
    }
    let mut app = TestApp::new(move || Area(toasts));
    assert!(
        app.query_class("kit-toasts").is_empty(),
        "empty stack renders nothing"
    );

    push_toast(&toasts, "error", "Disk full");
    app.refresh();
    assert!(app.text().contains("Disk full"));
    push_toast(&toasts, "success", "Saved");
    app.refresh();
    assert_eq!(app.query_class("kit-toast").len(), 2);
    assert!(
        app.query_class("kit-toast-error").len() == 1,
        "kind picks the accent class"
    );

    // Click a toast's node to dismiss it (click-to-dismiss, no timers in v1).
    let first = app.query_class("kit-toast").into_iter().next().unwrap();
    assert!(app.fire(&first, "click", None));
    assert!(!app.text().contains("Disk full"), "clicked toast dismissed");
    assert!(app.text().contains("Saved"));
}

// ── VirtualList ─────────────────────────────────────────────────────────────

#[test]
fn vlist_windows_and_pages() {
    let page = use_signal(|| 0usize);
    #[component]
    fn Paged(page: Signal<usize>) -> View {
        let items: Vec<String> = (0..25).map(|i| format!("item {i}")).collect();
        view! {
            VirtualList(items, 10, page, Rc::new(|_| ()))
        }
    }
    let mut app = TestApp::new(move || Paged(page));
    let text = app.text();
    assert!(
        text.contains("item 0") && text.contains("item 9"),
        "first page window"
    );
    assert!(!text.contains("item 10"), "page 1 excludes page 2");
    assert!(text.contains("Page 1 of 3"));

    assert!(app.click_text("▶"));
    let text = app.text();
    assert!(text.contains("item 10") && text.contains("item 19"));
    assert!(text.contains("Page 2 of 3"));

    assert!(app.click_text("▶"));
    assert!(app.text().contains("item 24"), "last page shows the tail");
    assert!(app.click_text("▶"), "Next is present past the end");
    assert!(
        app.text().contains("Page 3 of 3"),
        "saturates at the last page"
    );
    assert!(app.click_text("◀"));
    assert!(app.text().contains("Page 2 of 3"));
}

// ── Table ───────────────────────────────────────────────────────────────────

#[test]
fn table_sorts_numerically_and_paginates() {
    let page = use_signal(|| 0usize);
    let sort = use_signal(|| None::<(usize, bool)>);
    #[component]
    fn Grid(page: Signal<usize>, sort: Signal<Option<(usize, bool)>>) -> View {
        let headers = vec!["Name".to_string(), "Score".to_string()];
        let rows = vec![
            vec!["ann".into(), "10".into()],
            vec!["bob".into(), "9".into()],
            vec!["cid".into(), "2".into()],
            vec!["dee".into(), "100".into()],
            vec!["eve".into(), "1".into()],
        ];
        view! {
            Table(headers, rows, sort, 3, page, Rc::new(|_| ()))
        }
    }
    let mut app = TestApp::new(move || Grid(page, sort));
    assert!(app.text().contains("ann") && app.text().contains("bob"));
    assert!(!app.text().contains("dee"), "page 1 of 3-row pages");

    // Click the Score header: numeric sort, so 1 < 2 < 9 < 10 < 100 — not
    // the lexicographic "1" < "10" < "100" < "2" < "9". Row text is the
    // cells concatenated (no separator), hence "eve1".
    let score_hdr = app
        .query_where(|n| n.tag() == "button" && n.text().contains("Score"))
        .into_iter()
        .next()
        .unwrap();
    assert!(app.fire(&score_hdr, "click", None));
    let text = app.text();
    let mut pos = 0;
    for probe in ["eve1", "cid2", "bob9"] {
        let at = text
            .find(probe)
            .unwrap_or_else(|| panic!("{probe} present in {text}"));
        assert!(at >= pos, "rows in ascending numeric order");
        pos = at;
    }
    assert!(text.contains("▲"), "ascending marker shown");

    // Click again → descending.
    assert!(app.fire(&score_hdr, "click", None));
    let text = app.text();
    assert!(text.contains("▼"));
    let mut pos = 0;
    for probe in ["dee100", "ann10", "bob9"] {
        let at = text
            .find(probe)
            .unwrap_or_else(|| panic!("{probe} present in {text}"));
        assert!(at >= pos, "rows in descending numeric order");
        pos = at;
    }

    // Page 2 of the descending order holds the two smallest rows.
    assert!(app.click_text("▶"));
    let text = app.text();
    assert!(text.contains("cid2") && text.contains("eve1"));
}

// ── DatePicker ──────────────────────────────────────────────────────────────

#[test]
fn datepicker_picks_a_day_and_browses_months() {
    let value = use_signal(|| "2026-09-05".to_string());
    #[component]
    fn Cal(value: Signal<String>) -> View {
        view! {
            DatePicker(value)
        }
    }
    let mut app = TestApp::new(move || Cal(value));
    assert!(app.text().contains("09/2026"));
    assert!(app.text().contains("Selected: 2026-09-05"));

    let day15 = app
        .query_where(|n| n.tag() == "button" && n.text().trim() == "15")
        .into_iter()
        .next()
        .expect("day 15 present");
    assert!(day15.attr("class").unwrap().contains("kit-cal-day"));
    assert!(app.fire(&day15, "click", None));
    assert!(app.text().contains("Selected: 2026-09-15"));

    // Month navigation; ‹ from 09/2026 → 08/2026 and back.
    assert!(click_node(&mut app, "kit-cal-prev"));
    assert!(app.text().contains("08/2026"));
    assert!(click_node(&mut app, "kit-cal-next"));
    assert!(app.text().contains("09/2026"));
    assert!(click_node(&mut app, "kit-cal-next"));
    assert!(app.text().contains("10/2026"));
    // Browsing months doesn't touch the selection.
    assert!(app.text().contains("Selected: 2026-09-15"));
}

// ── ThemeProvider ───────────────────────────────────────────────────────────

#[test]
fn theme_provider_flows_to_components_and_defaults_quietly() {
    #[component]
    fn Themed() -> View {
        view! {
            ThemeProvider("dark".into()) {
                Button("Go".into(), Rc::new(|_| ()), "primary".into(), false)
            }
        }
    }
    let app = TestApp::new(|| Themed());
    let btn = app.query_class("kit-btn").into_iter().next().unwrap();
    let class = btn.attr("class").unwrap();
    assert!(
        class.contains("kit-theme-dark"),
        "theme token flows: {class}"
    );

    // No provider: components still render (empty theme token, no panic).
    #[component]
    fn Unthemed() -> View {
        view! {
            Button("Go".into(), Rc::new(|_| ()), "ghost".into(), false)
        }
    }
    let app = TestApp::new(|| Unthemed());
    let btn = app.query_class("kit-btn").into_iter().next().unwrap();
    assert!(btn.attr("class").unwrap().contains("kit-btn-ghost"));
}

// ── property fuzz over a composed kit app ───────────────────────────────────

#[test]
fn fuzz_the_kit_demo() {
    let toasts = use_signal(Vec::new);
    #[component]
    fn Kit(toasts: Signal<Vec<ToastMsg>>) -> View {
        let tab = use_signal(|| 0usize);
        let page = use_signal(|| 0usize);
        let sort = use_signal(|| None::<(usize, bool)>);
        let day = use_signal(|| "2026-09-01".to_string());
        view! {
            ThemeProvider("dark".into()) {
                Button("Act".into(), Rc::new(move |_| push_toast(&toasts, "info", "acted")), "primary".into(), false)
                Tooltip("tooltipped".into()) {
                    b { "Info" }
                }
                Tabs(
                    vec![
                        ("A".into(), view! { p { "alpha" } }),
                        ("B".into(), view! { p { "beta" } }),
                    ],
                    tab,
                )
                VirtualList((0..7).map(|i| format!("row {i}")).collect(), 3, page, Rc::new(|_| ()))
                Table(
                    vec!["K".into(), "V".into()],
                    vec![vec!["a".into(), "1".into()], vec!["b".into(), "2".into()]],
                    sort,
                    1,
                    page,
                    Rc::new(|_| ()),
                )
                DatePicker(day)
            }
            ToastHost(toasts)
        }
    }
    let mut app = TestApp::new(move || Kit(toasts));
    let report = app.fuzz(FuzzCfg {
        seed: 0x5EED,
        rounds: 150,
    });
    match report {
        Ok(r) => {
            assert!(r.fired > 50, "expected most rounds to act, got {r:?}");
            assert!(
                r.warnings.is_empty(),
                "strictness warnings: {:?}",
                r.warnings
            );
        }
        Err(f) => panic!("kit demo fuzz failed: {f}"),
    }
}
