//! The React-parity framework-core slice (RP-A1/A2/A3): position-stable
//! hooks, context, and children — each proven at the render level.

use std::rc::Rc;

use kitae_ui::diff::Mount;
use kitae_ui::hooks::wrap_render;
use kitae_ui::patch::NodeRef;
use kitae_ui::prelude::*;
use kitae_ui::render::render_html;
use kitae_ui::vnode::Event;

/// A component whose state hook lives INSIDE the body — the pattern the
/// hoisted-signal rule used to forbid.
#[component]
fn Counter() -> View {
    let count = use_signal(|| 0u32);
    view! {
        button(on:click = move |_| count.update(|c| *c += 1)) { "Count: {count}" }
    }
}

fn button_ref(ops: &[PatchOp]) -> u64 {
    ops.iter()
        .find_map(|op| match op {
            PatchOp::InsertNode { node, .. } if node.handler.is_some() => node.handler,
            _ => None,
        })
        .expect("handler-bearing node")
}

#[test]
fn hooks_inside_components_survive_re_renders() {
    let render = wrap_render(Rc::new(|| Counter()));
    let mut m = Mount::new(NodeRef(0));
    let v1 = render();
    let ops = m.init(&v1);
    let btn = button_ref(&ops);
    let handler = m.handlers()[&btn]
        .iter()
        .find(|(ev, _)| ev == "click")
        .expect("click handler")
        .1
        .clone();
    handler(Event {
        name: "click".into(),
        value: None,
    });
    // Re-render: the SAME component invocation must see Count: 1 — the
    // slot-stable signal survived the build.
    let v2 = render();
    let html = render_html(&v2.nodes);
    assert!(html.contains("Count: 1"), "hook state survived: {html}");
}

#[test]
fn fresh_invocation_gets_fresh_state() {
    // A sibling invocation in a different tree position is a DIFFERENT
    // scope: independent state.
    let render = wrap_render(Rc::new(|| {
        view! {
            div {
                {Counter()}
                {Counter()}
            }
        }
    }));
    let v = render();
    let html = render_html(&v.nodes);
    assert_eq!(
        html.matches("Count: 0").count(),
        2,
        "two independent counters: {html}"
    );
}

#[component]
fn RenderCounter() -> View {
    let n = use_ref(|| 0u32);
    *n.borrow_mut() += 1;
    let shown = n.borrow().to_string();
    view! {
        p { "renders: {shown}" }
    }
}

#[test]
fn use_ref_persists_across_re_renders() {
    let render = wrap_render(Rc::new(|| RenderCounter()));
    let mut m = Mount::new(NodeRef(0));
    let v1 = render();
    let _ = m.init(&v1);
    let v2 = render();
    let html = render_html(&v2.nodes);
    // Fresh-per-call would show 1 again; slot stability shows 2.
    assert!(html.contains("renders: 2"), "same Rc across builds: {html}");
}

#[component]
fn Provider(theme: Signal<String>, children: Children) -> View {
    // Provide BEFORE interpolating the children: the deferred braces build
    // inside this scope, so nested consumers find the value.
    provide_context(theme);
    view! {
        div { {children} }
    }
}

#[component]
fn Consumer() -> View {
    let theme = use_context::<Signal<String>>();
    view! {
        p { "theme is {theme}" }
    }
}

#[test]
fn context_flows_down_and_stays_reactive() {
    let theme = use_signal(|| "dark".to_string());
    let render = wrap_render(Rc::new(move || {
        view! {
            Provider(theme) {
                {Consumer()}
            }
        }
    }));
    let mut m = Mount::new(NodeRef(0));
    let v1 = render();
    let _ = m.init(&v1);
    assert!(
        render_html(&v1.nodes).contains("theme is dark"),
        "initial context value flows to the consumer: {}",
        render_html(&v1.nodes)
    );
    // The consumer reads the provider's signal: a provider-side write
    // re-renders the consumer through ordinary signal reactivity.
    theme.set("light".to_string());
    let v2 = render();
    let _ = m.diff(&v2);
    assert!(
        render_html(&v2.nodes).contains("theme is light"),
        "context is reactive: {}",
        render_html(&v2.nodes)
    );
}

#[test]
fn context_missing_provider_fails_loudly() {
    let result = std::panic::catch_unwind(|| {
        let theme: Signal<String> = use_context();
        let _ = theme;
    });
    assert!(result.is_err(), "missing provider must panic");
}

#[component]
fn Card(children: Children) -> View {
    view! {
        div(class: "card") { {children} }
    }
}

#[test]
fn children_splice_from_call_site_braces() {
    let v = view! {
        Card() {
            h2 { "Hello" }
            p { "body text" }
        }
    };
    let html = render_html(&v.nodes);
    assert!(html.contains(r#"class="card""#), "card wrapper: {html}");
    assert!(
        html.contains("Hello") && html.contains("body text"),
        "children spliced inside the wrapper: {html}"
    );
}

#[test]
fn set_if_changed_skips_no_op_writes() {
    let s = use_signal(|| String::from("a"));
    let runs = Rc::new(std::cell::Cell::new(0u32));
    let r2 = runs.clone();
    let _fx = use_effect(move || {
        r2.set(r2.get() + 1);
        s.get();
    });
    assert_eq!(runs.get(), 1, "initial run");
    s.set_if_changed("a".into());
    assert_eq!(runs.get(), 1, "equal value must not rerun the effect");
    s.set_if_changed("b".into());
    assert_eq!(runs.get(), 2, "changed value reruns the effect");
}

#[test]
fn set_still_fires_on_equal_values() {
    let s = use_signal(|| 0u32);
    let runs = Rc::new(std::cell::Cell::new(0u32));
    let r2 = runs.clone();
    let _fx = use_effect(move || {
        r2.set(r2.get() + 1);
        s.get();
    });
    s.set(0);
    assert_eq!(runs.get(), 2, "plain set is unconditional");
}

#[test]
fn dispose_frees_slot_and_stale_handle_panics() {
    let s = use_signal(|| 1u32);
    assert_eq!(s.get(), 1);
    s.dispose();
    // Handle is stale: reads and writes panic (slab slot freed).
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.get()));
    assert!(r.is_err(), "get on disposed signal must panic");
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.set(2)));
    assert!(r.is_err(), "set on disposed signal must panic");
    // dispose is idempotent — a second call must not panic or corrupt.
    s.dispose();
    // A fresh signal may reuse the id under a bumped generation.
    let t = use_signal(|| 5u32);
    assert_eq!(t.get(), 5);
}

#[test]
fn panic_in_effect_does_not_wedge_the_world() {
    let trigger = use_signal(|| 0u32);
    let first = Rc::new(std::cell::Cell::new(true));
    let f2 = first.clone();
    // The seed run of use_effect panics; the TrackGuard must restore the
    // flush counters during the unwind (caught here) — otherwise every
    // later flush on this thread sees DEPTH > 0 and silently bails.
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        use_effect(move || {
            trigger.get();
            if f2.get() {
                f2.set(false);
                panic!("effect explodes once");
            }
        })
    }));
    assert!(r.is_err(), "the seed run must actually panic");
    // The world still works: writes flush dependents.
    let echo = use_signal(|| 0u32);
    let _fx2 = use_effect(move || {
        echo.set(trigger.get());
    });
    trigger.set(41);
    assert_eq!(echo.get(), 41, "reactivity must survive an effect panic");
}

#[test]
fn hook_slot_type_mismatch_resets_instead_of_panicking() {
    // Rules-of-hooks violation: the same position hosts different hook
    // types across builds (a conditional swapped the component shape).
    // Must warn and reset the slot — not panic the render loop.
    fn build(which: u32) -> View {
        if which == 0 {
            let a = use_signal(|| String::from("signal-slot"));
            view! { p { {a.get()} } }
        } else {
            let b = use_ref(|| 7u32);
            view! { p { {(*b.borrow()).to_string()} } }
        }
    }
    let which = Rc::new(std::cell::Cell::new(0u32));
    let w2 = which.clone();
    let render = wrap_render(Rc::new(move || build(w2.get())));
    let v1 = render();
    assert!(
        render_html(&v1.nodes).contains("signal-slot"),
        "first build ran the signal branch"
    );
    which.set(1);
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let v2 = render();
        render_html(&v2.nodes)
    }));
    let html = r.unwrap_or_else(|_| "<panic>".to_string());
    assert!(
        html.contains("7"),
        "mismatched slot must reset and re-run, not panic: {html}"
    );
}

#[test]
fn style_declarations_survive_semicolons_in_values() {
    use kitae_ui::vnode::parse_declarations;
    let decls = parse_declarations(
        "color: red; background: url(data:image/png;base64,AAA/BB+C==); content: \"a;b\"; margin: 0",
    );
    assert_eq!(
        decls,
        vec![
            ("color".to_string(), "red".to_string()),
            (
                "background".to_string(),
                "url(data:image/png;base64,AAA/BB+C==)".to_string()
            ),
            ("content".to_string(), "\"a;b\"".to_string()),
            ("margin".to_string(), "0".to_string()),
        ],
        "embedded ; in url() and quotes must not split the block: {decls:?}"
    );
    // Unbalanced-paren tolerance: a stray `(` can't swallow the rest.
    let decls = parse_declarations("width: calc(100% - 10px; height: 5px");
    assert_eq!(
        decls,
        vec![
            ("width".to_string(), "calc(100% - 10px".to_string()),
            ("height".to_string(), "5px".to_string()),
        ],
        "unclosed paren falls back to treating ; as the separator"
    );
}
