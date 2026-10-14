//! C3 DevTools tests — scope tree, live signal values, patch log.

use kitae_ui::patch::PatchOp;
use kitae_ui::prelude::*;
use std::rc::Rc;

// The scope tree snapshot includes scopes entered so far this build; mount
// DevTools LAST among siblings so it sees them all.

#[component]
fn Leaf() -> View {
    let flag = use_signal(|| false);
    view! { p { "leaf {flag.get()}" } }
}

#[component]
fn Branch() -> View {
    view! {
        div {
            Leaf()
            Leaf()
        }
    }
}

#[component]
fn Trunk() -> View {
    view! { div { Branch() } }
}

#[component]
fn Counting(n: Signal<i32>) -> View {
    view! {
        div {
            p { "n={n}" }
            button(on:click = move |_| n.update(|v| *v += 1)) { "+1" }
        }
    }
}

#[test]
fn scope_tree_reports_names_nesting_and_slots() {
    kitae_ui::testing::reset();
    let mut app = TestApp::new(|| {
        view! {
            div {
                Trunk()
                DevTools(vec![], None)
            }
        }
    });

    // The direct snapshot: Trunk → Branch → two Leafs, then DevTools itself.
    let tree = scope_tree();
    assert_eq!(tree.len(), 2, "top-level scopes: Trunk + DevTools");
    assert_eq!(tree[0].name.as_deref(), Some("Trunk"));
    assert_eq!(tree[0].slots, 0);
    assert_eq!(tree[0].children.len(), 1);
    assert_eq!(tree[0].children[0].name.as_deref(), Some("Branch"));
    assert_eq!(tree[0].children[0].children.len(), 2);
    assert_eq!(
        tree[0].children[0].children[0].name.as_deref(),
        Some("Leaf")
    );
    // Leaf holds one use_signal slot; Branch/Trunk hold none.
    assert_eq!(tree[0].children[0].children[0].slots, 1);
    assert_eq!(tree[1].name.as_deref(), Some("DevTools"));

    // The panel renders the same tree, indented by depth. The two Leafs are
    // siblings — both at depth 2 — and DevTools is a top-level sibling.
    let rows = app.query_class("kit-devtools-scope");
    assert_eq!(rows.len(), 5, "Trunk, Branch, Leaf, Leaf, DevTools");
    assert!(rows[0].text().contains("Trunk"));
    assert!(rows[1].text().contains("Branch"));
    assert!(rows[2].text().contains("Leaf"));
    assert!(rows[4].text().contains("DevTools"));
    assert_eq!(rows[0].style("padding-left"), Some("6px"));
    assert_eq!(rows[1].style("padding-left"), Some("20px"));
    assert_eq!(rows[2].style("padding-left"), Some("34px"));
    assert_eq!(rows[3].style("padding-left"), Some("34px"));
    assert_eq!(rows[4].style("padding-left"), Some("6px"));

    // Graceful degradation with no optional pieces.
    assert!(app.text().contains("no signals inspected"));
    assert!(app.text().contains("no PatchLog attached"));
}

#[test]
fn panel_shows_live_inspected_values() {
    kitae_ui::testing::reset();
    let n = use_signal(|| 0);
    let log = PatchLog::new();
    let mut app = TestApp::new(move || {
        view! {
            div {
                Counting(n)
                DevTools(
                    vec![("n".to_string(), Rc::new(move || n.get().to_string()) as Rc<dyn Fn() -> String>)],
                    Some(log.clone()),
                )
            }
        }
    });

    let vals = app.query_class("kit-devtools-signal-value");
    assert_eq!(vals.len(), 1);
    assert_eq!(vals[0].text(), " = 0");

    app.click_text("+1");
    let vals = app.query_class("kit-devtools-signal-value");
    assert_eq!(
        vals[0].text(),
        " = 1",
        "the watched value follows the signal"
    );
    // Re-renders keep the tree section alive with names.
    assert!(app.text().contains("Counting"));
}

#[test]
fn patch_log_counts_and_renders_ops() {
    kitae_ui::testing::reset();
    let n = use_signal(|| 0);
    let log = PatchLog::new();
    let log_handle = log.clone();
    let mut app = TestApp::new(move || {
        view! {
            div {
                Counting(n)
                DevTools(vec![], Some(log_handle.clone()))
            }
        }
    });

    // Init stream: every node inserted — structural.
    log.record(app.ops());
    let c = log.counts();
    assert!(c.structural >= 1);
    assert_eq!(c.total, c.structural + c.property);

    // One click = the "n=1" SetText plus the panel updating its own display
    // (it renders the log, so recording changes it too — by design).
    let property_before = log.counts().property;
    app.click_text("+1");
    log.record(app.ops());
    assert!(log.counts().property > property_before);
    assert!(log.entries().iter().any(|op| matches!(
        op, PatchOp::SetText { text, .. } if text == "n=1"
    )));

    // The panel (re-rendered after recording) shows the recorded ops.
    app.refresh();
    let text = app.text();
    assert!(text.contains("structural"), "counts line: {}", text);
    assert!(text.contains("property"));
    assert!(text.contains("\"n=1\""), "op rows carry the Display form");

    // Clear drops the entries; the counters keep their totals (the panel
    // renders the log it's part of, so exact totals are feedback-dependent
    // — the contract is that clear() only drops entries).
    let total_before_clear = log.counts().total;
    assert!(app.click_text("Clear log"));
    assert_eq!(log.len(), 0);
    assert_eq!(log.counts().total, total_before_clear);
    let text = app.text();
    assert!(text.contains("no ops recorded yet"));
}

#[test]
fn patch_log_caps_entries_keeps_totals() {
    kitae_ui::testing::reset();
    let log = PatchLog::new();
    for i in 0..300u64 {
        log.record(&[PatchOp::SetText {
            node: NodeRef(i),
            text: format!("t{i}"),
        }]);
    }
    assert_eq!(log.len(), 256, "entries are bounded");
    assert_eq!(log.counts().total, 300, "totals count everything");
    assert_eq!(log.counts().property, 300);
    assert_eq!(log.counts().structural, 0);
    let entries = log.entries();
    assert!(
        matches!(&entries[0], PatchOp::SetText { text, .. } if text == "t44"),
        "oldest entries fell off the front"
    );
    assert!(matches!(&entries[255], PatchOp::SetText { text, .. } if text == "t299"));

    // Shared handles see the same log.
    let clone = log.clone();
    clone.clear();
    assert!(log.is_empty());
}
