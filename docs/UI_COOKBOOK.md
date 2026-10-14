# kitae_ui Cookbook

Working recipes for the `kitae_ui` layer-3 UI library. Every recipe here is
compiled and asserted in `crates/kitae_ui/tests/cookbook_tests.rs` — if a
pattern breaks, that file fails. Test names match recipe numbers.

Imports: everything in the first snippet comes from `kitae_ui::prelude::*`;
recipes 5–7 additionally use `use_cached_resource`, `use_resource_with_deps`,
`suspense_view` (`kitae_ui::resource`), `route_param`/`query_param`
(`kitae_ui::kit`), and `Slots` (`kitae_ui::vnode`).

```rust
use kitae_ui::prelude::*;

#[component]
fn Hello(name: String) -> View {
    view! { p { "Hello, {name}!" } }
}
```

A component is a plain function with `#[component]`: arguments are props
(usually `String`, `u64`, `Signal<T>`, or `Rc<dyn Fn(..)>`), the body returns
a `View` built by `view!`. Inside the body, hook calls (`use_signal`,
`use_resource`, …) are slot-stable — one per call site, however often the
body re-runs.

---

## Recipe 1 · the counter — signals and events

State is a `Signal<T>`; handlers are `move` closures; text interpolates a
signal directly and re-renders when it changes.

```rust
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
```

Rules of thumb:

- `use_signal(|| init)` creates state; read with `sig.get()`, write with
  `sig.set(v)` or `sig.update(|v| …)`.
- `{count}` in text is sugar for the current value, refreshed on change.
- An event handler is `on:click = move |e| …`; `e.value` carries input
  payloads (`Some(String)` on `input` events).

## Recipe 2 · lists with keys

Render a list with `for` and give each row a `key` so the differ can move
instead of rebuild:

```rust
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
```

Keys may be integers or strings. Prefer a stable identity (an id field) over
the loop index when items can reorder.

## Recipe 3 · conditional branches

`view!` supports `if` / `else if` / `else` inline. Branches swap in place —
the diff patches rather than remounts the subtree.

```rust
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
```

## Recipe 4 · forms: two-way binding, errors on submit, reset

`use_form()` returns a `Form` handle (cheap to clone — give each event
closure its own). `field_value` registers a field and returns its live
`Signal<String>`; binding it to `input(value: …)` makes the field two-way
with no handler. Errors are plain strings the author sets/clears; the view
recomputes on the re-render that follows.

```rust
#[component]
fn SignupForm() -> View {
    let form = use_form();
    let email = form.field_value("email", "");
    let submit_form = form.clone();
    let reset_form = form.clone();

    // Computed before the view: the error shows after the re-render that
    // follows a submit attempt.
    let err = form.error("email").unwrap_or_default();

    view! {
        div(class: "form") {
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
```

Notes:

- `Form::submit()` bumps a reactive submit counter (`form.submits()`) — gate
  error display on a real attempt if you like.
- `reset()` restores initials, clears touched flags and errors, zeroes the
  counter; signals keep identity, so bindings stay live.
- `is_dirty(name)` compares against the initial; `touch(name)` marks blurred.

## Recipe 5 · resources: background fetch with deps + suspense

`use_resource_with_deps(deps, fetch)` runs `fetch` on a background thread;
the value lands when the runtime drains. `deps` is a `u64` — bump it (a
version counter, a hash) to refetch. `suspense_view` renders a fallback
until the value arrives.

```rust
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
```

Rules:

- **Fetch closures must be `Fn() -> T + Clone + Send + 'static`** — they run
  on another thread, so they must NOT capture `Signal`s (not `Send`).
  Capture plain values (`u64`, `Arc<T>`) instead.
- Same deps on re-render → no refetch; changed deps → fresh fetch while the
  old value stays visible until the new one lands.
- `use_resource(fetch)` is the no-deps variant; `Resource::refetch()`
  re-runs on demand; `res.loading.get()` / `res.error.get()` expose status.

## Recipe 6 · shared cached data across components

`use_cached_resource(key, fetch)` backs a resource with a process-wide
identity cache keyed by `key`: first caller fetches, every later caller with
the same key gets the cached value instantly (no thread, no loading window).
The cache never invalidates; `refetch()` refreshes it.

```rust
#[component]
fn ReadsCache(tag: &'static str) -> View {
    let res = use_cached_resource("cookbook-config", || "v1".to_string());
    let v = res.get().unwrap_or_else(|| "…".to_string());
    view! { p { "{tag}={v}" } }
}
```

Caveat: on the very first render, two same-key callers both miss (the cache
fills only when a value lands), so both fetch and both land the same value.
From the next render on, callers hit the cache immediately.

## Recipe 7 · routing: params, query, history

`Router::new(initial)` holds the current path plus back/forward stacks. It
is slot-stable inside a component; the live runtime (`kitae_live`) seeds the
initial path from the request URL so deep links work.

```rust
use kitae_ui::kit::{query_param, route_param};

#[component]
fn DocsPage() -> View {
    let router = Router::new(String::new());
    let path = router.path.get();
    let section = route_param(&path, "/docs/:section", "section")
        .unwrap_or_else(|| "index".to_string());
    let lang = query_param(&path, "lang").unwrap_or_else(|| "en".to_string());
    view! {
        nav {
            button(on:click = move |_| router.navigate("/docs/css")) { "Docs" }
        }
        article {
            h1 { "Section: {section}" }
            p { "Lang: {lang}" }
        }
    }
}
```

Helpers and history:

- `route_param(path, "/docs/:section", "section")` → `Some("css")`;
  `query_param(path, "lang")` → `Some("de")`. Both percent-decode and turn
  `+` into a space.
- `Router::param` / `Router::query` read the same helpers against the
  current path.
- `navigate(to)` pushes onto the back stack; a fresh navigation clears the
  forward stack (like a browser). `back()` / `forward()` step;
  `can_back()` / `can_forward()` guard.

## Recipe 8 · tabs

`Tabs` renders the strip and the ACTIVE panel only (inactive content is not
in the tree — good for heavy panels).

```rust
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
```

## Recipe 9 · data table with sort + paging

`Table` sorts (numeric-aware) when a header is clicked and pages with the
◀ / ▶ buttons; sorting never resets the page. You own the two signals.

```rust
#[component]
fn UsersTable() -> View {
    let sort = use_signal(|| None::<(usize, bool)>); // (col, ascending)
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
```

`page_size == 0` shows everything; the pager prints "Page N of M"; `on_row`
receives the absolute row index on click.

## Recipe 10 · toasts

Render one `ToastHost` per page; `push_toast` appends; clicking a toast
dismisses it. `kind` picks the accent (`"info"`, `"success"`, `"error"`).

```rust
#[component]
fn Toasty() -> View {
    let toasts = use_signal(|| Vec::<ToastMsg>::new());
    let toasts2 = toasts.clone();
    view! {
        div {
            ToastHost(toasts)
            button(on:click = move |_| push_toast(&toasts2, "success", "Saved!")) {
                "Notify"
            }
        }
    }
}
```

## Recipe 11 · theming

`ThemeProvider` scopes a subtree: it provides the theme token class
(`kit-theme-dark` / `kit-theme-light`) that `Button` and friends append to
their class names. Wrap your page root once; the token cascades.

```rust
#[component]
fn ThemedPage() -> View {
    view! {
        ThemeProvider("dark".to_string()) {
            Button("Save".into(), Rc::new(|_| {}), "primary".into(), false)
        }
    }
}
```

Button variants: `"primary"`, `"secondary"`, `"danger"`, `"ghost"`;
`disabled: true` removes the click handler entirely.

## Recipe 12 · named slots

A component whose last prop is `slots: Slots` receives the call site's
braces split by `slot(name: "…")` directives. Leftover tokens become the
`"children"` slot.

```rust
use kitae_ui::vnode::Slots;

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

// Call site:
// PageShell("Docs".to_string()) {
//     slot(name: "sidebar") { p { "Sidebar content" } }
//     slot(name: "children") { p { "Main content" } }
// }
```

A slot not passed at the call site is simply absent — check `slots.has(..)`
and render nothing (no built-in fallback).

## Recipe 13 · error boundaries

`ErrorBoundary(fallback) { children }` catches panics from the children's
build and renders the fallback instead; siblings and later renders keep
working. Put one around risky third-party widgets.

```rust
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
```

Keep the wrapped child inside an element (`div { {Boom()} }`) — a bare
component call as the whole branch body is the one shape the macro cannot
splice into an `if`.

## Recipe 14 · charts

`LineChart`, `AreaChart`, `BarChart`, `Sparkline` are components taking a
signal of points plus width/height; they render SVG-like shapes as plain
elements (no JS). Update the signal, the chart redraws.

```rust
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
```

## Recipe 15 · memoization with skip_eq

`#[component(skip_eq)]` turns the component into React.memo: on re-render
with equal props (`PartialEq`), the body is skipped entirely and the cached
view is reused. The diff still runs, so patch ops stay minimal.

```rust
#[component(skip_eq)]
fn ExpensiveRow(n: u32) -> View {
    view! { li { "row {n}" } }
}
```

For subtrees with a single cheap dependency, `use_memo_view(deps, build)`
caches the built view against a `u64` version instead.

## Recipe 16 · patch ops are the only mutation surface

Every state change becomes a `PatchOp` stream (`InsertNode`, `RemoveNode`,
`MoveNode`, `SetText`, `SetAttr`, `SetStyle`, …) — the same stream the live
runtime ships over the wire and DevTools displays. `TestApp::ops()` hands
you the last stream, so tests can assert on the wire format itself.

```rust
use kitae_ui::patch::PatchOp;
use kitae_ui::testing::TestApp;

#[test]
fn ops_stream_is_observable() {
    let mut app = TestApp::new(|| OpCounter());
    let _ = app.ops(); // the initial mount stream
    assert!(app.click_text("0"));
    let ops = app.ops(); // the click's diff stream
    assert!(ops.iter().any(|op| matches!(op, PatchOp::SetText { .. })));
}
```

`TestApp` (the C4 harness) also offers: `text()`, `html()`, `query(tag)`,
`count(tag)`, `exists(tag)`, `click_text(s)` / `click_tag(t)` (first match in
document order), `set_value(tag, v)` (types into the first bound input),
`fire(node, event, value)` (fires handlers AND writes `value:` bindings),
and `refresh()`. Call `kitae_ui::testing::reset()` between tests that use
hook scopes or caches.

---

## Known deviations (documented, tested)

- **Tooltip toggles on click** — the engine has no hover events.
- **VirtualList pages instead of scrolling** — no scroll events; page-size
  windows with a pager.
- **`Form` errors are non-reactive** — they appear on the re-render that
  follows the write (submit/set_error), which is the normal flow.
- **A bare component call as the entire body of an `if` branch** needs an
  element wrapper (`div { {Boom()} }`) — macro splicing limitation.

## Wire-shape cheatsheet

| Pattern | Shape |
|---|---|
| State | `let s = use_signal(\|\| init)` · `s.get()` · `s.set(v)` · `s.update(\|v\| …)` |
| Events | `on:click = move \|e\| …` (`e.value: Option<String>`) |
| Two-way input | `input(value: sig)` — no handler needed |
| List | `for xs.iter().enumerate() -> (i, x) { li(key: i) { … } }` |
| Branch | `if cond { … } else if c2 { … } else { … }` inside `view!` |
| Fetch | `use_resource_with_deps(version, \|\| val)` (no signals in the closure!) |
| Suspense | `{suspense_view(&res, fallback_view, \|v\| view! { … })}` |
| Cache | `use_cached_resource("key", \|\| val)` |
| Route | `route_param(&path, "/docs/:s", "s")` · `query_param(&path, "k")` |
| Slots | `slots: Slots` + `slots.has("name")` + `{slots.render("name")}` |
| Boundary | `ErrorBoundary(fallback_view) { children }` |
| Memo | `#[component(skip_eq)]` or `use_memo_view(deps, build)` |
