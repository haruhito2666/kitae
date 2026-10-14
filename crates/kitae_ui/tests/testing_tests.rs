//! C4 testing-utilities tests: query, fire, two-way bindings, ops
//! assertions, the property harness, and isolation.

use std::rc::Rc;

use kitae_ui::patch::PatchOp;
use kitae_ui::prelude::*;
use kitae_ui::testing::{reset, FuzzCfg, TestApp};
use kitae_ui::vnode::Event;

#[component]
fn Counter(label: String) -> View {
    let count = use_signal(|| 0);
    view! {
        section(class: "card") {
            h2 { "{label}: {count}" }
            button(class: "inc", on:click = move |_| count.update(|c| *c += 1)) { "+1" }
            button(class: "dec", on:click = move |_| count.update(|c| *c -= 1)) { "-1" }
            if count.get() >= 3 {
                p(class: "warn") { "That's a lot." }
            }
        }
    }
}

// ── render + query ──────────────────────────────────────────────────────────

#[test]
fn renders_and_queries_the_tree() {
    let mut app = TestApp::new(|| Counter("Score".into()));
    assert!(app.text().contains("Score: 0"));
    let secs = app.query("section");
    assert_eq!(secs.len(), 1);
    assert_eq!(secs[0].attr("class"), Some("card"));
    assert_eq!(app.count("button"), 2);
    assert!(app.exists("h2"));
    assert!(!app.exists("table"));
    // A container's text includes its children.
    assert!(secs[0].text().contains("+1"));
    // Index path into the tree: section > second child (the inc button).
    let btn = app.node_at(&[0, 1]).expect("section > button");
    assert_eq!(btn.tag(), "button");
    assert_eq!(btn.attr("class"), Some("inc"));
}

#[test]
fn query_class_matches_space_separated_tokens() {
    let mut app = TestApp::new(|| {
        view! {
            div {
                p(class: "a b") { "one" }
                p(class: "ab") { "two" }
            }
        }
    });
    let hits = app.query_class("a");
    assert_eq!(hits.len(), 1, "'a' must not match 'ab'");
    assert!(hits[0].text().contains("one"));
}

// ── fire events + assert on output ──────────────────────────────────────────

#[test]
fn click_updates_state_and_output() {
    let mut app = TestApp::new(|| Counter("Score".into()));
    assert!(app.click_text("+1"));
    assert!(app.text().contains("Score: 1"));
    assert!(app.click_text("+1"));
    assert!(app.click_text("+1"));
    assert!(app.text().contains("Score: 3"));
    // The conditional branch appeared once the count crossed 3.
    assert_eq!(app.count("p"), 1);
    assert!(app.query_class("warn")[0].text().contains("a lot"));
    // Decrement back below the threshold: the branch disappears.
    assert!(app.click_text("-1"));
    assert_eq!(app.count("p"), 0);
    assert!(!app.click_text("no such button"));
}

#[test]
fn click_tag_and_ops_stream() {
    let mut app = TestApp::new(|| Counter("N".into()));
    assert!(app.ops().len() > 0, "init ops recorded");
    assert!(app.click_tag("button"));
    // After a dispatch the last op stream is exactly the diff.
    assert!(
        app.ops()
            .iter()
            .any(|op| matches!(op, PatchOp::SetText { .. })),
        "one SetText for the count: {:?}",
        app.ops()
    );
    // Clicking again changes only the count text again.
    assert!(app.click_tag("button"));
    assert_eq!(app.ops().len(), 1);
}

#[test]
fn html_matches_the_engine_init_shape() {
    let mut app = TestApp::new(|| Counter("N".into()));
    let html = app.html();
    assert!(html.contains(r#"class="card""#));
    assert!(html.contains("N: 0"));
    assert!(app.click_text("+1"));
    assert!(app.html().contains("N: 1"));
}

// ── two-way bindings ────────────────────────────────────────────────────────

#[component]
fn Draft() -> View {
    let draft = use_signal(String::new);
    let echo = use_signal(String::new);
    let len = draft.get().chars().count().to_string();
    view! {
        div {
            input(value: draft, on:input = move |e: Event| {
                echo.set(e.value.unwrap_or_default());
            })
            p { "echo: {echo}" }
            span { "len: {len}" }
        }
    }
}

#[test]
fn set_value_fires_input_and_writes_binding() {
    let mut app = TestApp::new(Draft);
    assert!(app.set_value("input", "hello"));
    // Handler ran (echo reflects the event payload) …
    assert!(app.text().contains("echo: hello"), "{}", app.text());
    // … AND the two-way binding wrote back into `draft` (len came from it).
    assert!(app.text().contains("len: 5"), "{}", app.text());
    // Overwrite, not append.
    assert!(app.set_value("input", "hi"));
    assert!(app.text().contains("echo: hi"));
    assert!(app.text().contains("len: 2"));
    // set_value on a tag with no input path reports false.
    assert!(!app.set_value("p", "x"));
}

#[test]
fn binding_writeback_alone_renders_through() {
    // No on:input handler at all — the binding write-back is still the
    // documented runtime contract and must re-render the tree.
    #[component]
    fn Bare() -> View {
        let name = use_signal(String::new);
        view! {
            div {
                input(value: name, placeholder: "name")
                p { "hi {name}" }
            }
        }
    }
    let mut app = TestApp::new(Bare);
    assert!(app.text().contains("hi "));
    assert!(app.set_value("input", "Ada"));
    assert!(app.text().contains("hi Ada"), "{}", app.text());
    // The binding round-trips into the rendered attribute too.
    let input = &app.query("input")[0];
    assert_eq!(input.attr("value"), Some("Ada"));
}

// ── arbitrary events ────────────────────────────────────────────────────────

#[test]
fn fire_routes_custom_events_and_submit() {
    // Signals hoisted OUTSIDE the render closure: the closure is not a
    // #[component], so in-closure hooks would allocate fresh every render.
    let items = use_signal(Vec::new);
    let draft = use_signal(String::new);
    let mut app = TestApp::new(move || {
        view! {
            form(on:submit = move |_| {
                let t = draft.get().trim().to_string();
                if !t.is_empty() {
                    draft.set(String::new());
                    items.update(move |v| v.push(t));
                }
            }) {
                input(value: draft, on:input = move |e: Event| draft.set(e.value.unwrap_or_default()))
                ul { for items.iter().enumerate() -> (i, t) { li(key: i) { "{t}" } } }
            }
        }
    });
    assert!(app.set_value("input", " first "));
    let form = &app.query("form")[0];
    assert!(app.fire(form, "submit", None));
    assert!(app.text().contains("first"));
    // Draft reset through the binding (round-trips into the attribute).
    let input = &app.query("input")[0];
    assert_eq!(input.attr("value"), Some(""));
    // Firing an event the element doesn't carry is a no-op returning false.
    assert!(!app.fire(form, "dblclick", None));
    // Second item lands in the keyed list.
    assert!(app.set_value("input", "second"));
    let form = &app.query("form")[0];
    assert!(app.fire(form, "submit", None));
    assert_eq!(app.count("li"), 2);
}

// ── the property harness ────────────────────────────────────────────────────

#[test]
fn fuzz_survives_arbitrary_event_sequences() {
    let mut app = TestApp::new(|| Counter("F".into()));
    let report = app
        .fuzz(FuzzCfg {
            seed: 7,
            rounds: 200,
        })
        .expect("no panic");
    assert_eq!(report.rounds, 200);
    assert!(report.fired > 100, "most rounds should act: {report:?}");
    // Text stays well-formed regardless of the sequence taken.
    assert!(app.text().contains("F: "));
}

#[test]
fn fuzz_reports_panicking_handlers() {
    let boom = use_signal(|| false);
    let mut app = TestApp::new(move || {
        view! {
            div {
                button(on:click = move |_| boom.set(true)) { "arm" }
                button(on:click = move |_| {
                    if boom.get() { panic!("render-adjacent blowup"); }
                }) { "boom" }
            }
        }
    });
    let err = app
        .fuzz(FuzzCfg {
            seed: 1,
            rounds: 50,
        })
        .expect_err("the seeded sequence arms, then blows up");
    assert!(err.detail.contains("blowup"), "{err}");
    // The app is still usable after a caught fuzz failure.
    assert!(app.text().contains("arm"));
}

#[test]
fn fuzz_drains_strict_warnings() {
    // Hoisted signal; the button forces diffs (warnings are emitted by
    // diff_list, not by init). The keyed and unkeyed items must be DIRECT
    // element children of the ul — that is what the check counts.
    let clicks = use_signal(|| 0u32);
    let mut app = TestApp::new(move || {
        view! {
            button(on:click = move |_| clicks.update(|c| *c += 1)) { "tick" }
            ul {
                li { "a" }
                li(key: "s") { "b" }
                li { "c" }
            }
        }
    });
    let report = app.fuzz(FuzzCfg { seed: 3, rounds: 8 }).expect("no panic");
    assert!(
        report.warnings.iter().any(|w| w.contains("keyed")),
        "mixed keyed list warned: {report:?}"
    );
}

// ── isolation ───────────────────────────────────────────────────────────────

#[test]
fn reset_gives_fresh_hook_slots() {
    // Mount the same component function twice on one thread: without a
    // reset the second app would reuse the first's positional hook slots.
    let mut a = TestApp::new(|| Counter("A".into()));
    assert!(a.click_text("+1"));
    assert!(a.text().contains("A: 1"));
    reset();
    let mut b = TestApp::new(|| Counter("A".into()));
    assert!(b.text().contains("A: 0"), "fresh slots: {}", b.text());
    // Hoisted signals survive resets — they live in the slab, not scopes.
    let hoisted = use_signal(|| 41);
    assert_eq!(hoisted.get(), 41);
}

#[test]
fn refresh_with_no_state_change_is_silent() {
    let mut app = TestApp::new(|| Counter("S".into()));
    let ops = app.refresh();
    assert!(ops.is_empty(), "re-render without writes diffs to nothing");
    assert!(app.ops().is_empty());
    assert!(app.text().contains("S: 0"));
}

// ── components with children (composition through queries) ──────────────────

#[component]
fn Card(title: String, children: Children) -> View {
    view! {
        div(class: "kit-card") {
            h3 { "{title}" }
            div(class: "kit-card-body") { {children} }
        }
    }
}

#[test]
fn children_compose_through_queries() {
    let mut app = TestApp::new(|| {
        view! {
            Card("Hello".into()) {
                p { "body text" }
            }
        }
    });
    let body = &app.query_class("kit-card-body")[0];
    assert!(body.text().contains("body text"));
    // The title landed in its own heading, the body in the body slot.
    assert_eq!(app.query("h3").len(), 1);
    assert!(app.query_text("Hello")[0].text().contains("Hello"));
    assert_eq!(Rc::strong_count(&Rc::new(())), 1); // Rc import sanity
}
