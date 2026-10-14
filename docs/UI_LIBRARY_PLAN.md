# kitae_ui: The Pure-Rust UI Library Plan

> Companion docs: [EMBEDDING_VISION.md](EMBEDDING_VISION.md) (ecosystem
> role) and [CPP_ROLE.md](CPP_ROLE.md) (where C++ fits). This doc is the
> plan for the developer-facing UI layer — the piece that makes Kitae not
> just an engine you embed, but a platform people *build on*.

## The idea in one line

**React made web pages interactive with JavaScript; kitae_ui makes them
interactive with Rust — the browser speaks it natively, so nothing is
shipped, compiled to, or executed as script.**

## Why a UI library at all

Kitae pages have no JS engine — that is the identity, not a gap. But a
"reading engine" alone caps what the platform is for. The question every
web author will ask is: *"how do I make it interactive?"*

Every existing answer is JS-shaped: ship JS, or compile to WASM and glue
it with JS. Kitae rejects both (WASM is JS-glue by construction — a wasm
module cannot touch the page except through a script engine). So Kitae's
answer must be its own: a reactive UI library whose runtime is **the
engine itself**, with one of two homes for application state:

- **Server-held (websites):** components run on the server; the browser
  receives tree patches and sends event messages. The proven pattern here
  is Phoenix LiveView — full interactivity with zero client scripting.
- **Embedder-held (apps):** components run in-process next to the engine;
  the embedder's Rust app is the "server". Kitae.app's own start page
  becomes the first user of this mode.

One reactive core, two runtimes, one syntax. The author learns it once.

## Design principles (in priority order)

1. **Easy beats expressive.** The user-facing rule for every API decision:
   a web developer who knows HTML/CSS and a little Rust should be
   productive in one afternoon. If two designs are equal, the simpler
   signature wins — every time.
2. **One macro, tiny prelude.** `view!` for markup, a dozen functions in
   `kitae_ui::prelude` total. If a concept needs its own macro, it had
   better earn it.
3. **HTML-shaped, CSS-styled.** Elements are HTML elements; styling is the
   engine's CSS. Website compatibility (static HTML still renders) is a
   hard requirement — `kitae_ui` is progressive enhancement, never a
   lock-in.
4. **No hidden execution.** There is no script anywhere in this story —
   client or server output. Closures run in-process (local mode) or on
   the author's server (live mode); the browser only ever exchanges
   patches and events.
5. **Rust-idiomatic, not Rust-archaeological.** No lifetime plumbing in
   user code, no trait-bound soup in error messages, no async juggling
   for a click handler.

## The syntax (target API)

A component is a plain function. State is a signal. Markup is `view!`.

### Hello page (server, live mode)

```rust
use kitae_ui::prelude::*;

#[component]
fn Home() -> View {
    view! {
        html {
            head { title { "Hello Kitae" } }
            body {
                h1(class: "hero") { "Hello, Kitae!" }
                p { "This page has no JavaScript. The server speaks Rust." }
                a(href: "/about") { "About" }
            }
        }
    }
}
```

### Counter (the "is interactivity easy?" test)

```rust
#[component]
fn Counter() -> View {
    let count = use_signal(|| 0);

    view! {
        section(class: "card") {
            h2 { "Count: {count}" }
            button(on:click = move |_| count.update(|c| *c += 1)) { "+1" }
            if count.get() > 10 {
                p(class: "warn") { "That's a lot of clicking." }
            }
        }
    }
}
```

Reading it: `use_signal` makes one piece of state; `{count}` interpolates
it into text; `count.update(..)` changes it and the framework re-renders
exactly what changed. The `on:click` closure runs **on the server** in
live mode (the browser just reports "button clicked"); it runs in-process
in local mode. The author writes identical code for both.

### List with keys (diffing made explicit)

```rust
#[component]
fn Todos() -> View {
    let todos = use_signal(Vec::new);
    let draft = use_signal(String::new);

    let add = move |_| {
        let text = draft.get().trim().to_string();
        if !text.is_empty() {
            todos.update(|t| t.push(Todo { text, done: false }));
            draft.set(String::new());
        }
    };

    view! {
        form(on:submit = add) {
            input(value: draft, placeholder: "What needs doing?")
            button(type: "submit") { "Add" }
        }
        ul {
            for todos.iter().enumerate() -> (i, todo) {
                li(key: i) {
                    checkbox(checked: todo.done,
                             on:change = move |_| todos.update(|t| t[i].done = !t[i].done))
                    span { "{todo.text}" }
                }
            }
        }
    }
}
```

### Embedder (local mode — the whole app)

```rust
fn main() -> kitae::Result<()> {
    let engine = Engine::builder().viewport(800, 600).build()?;
    engine.mount(Counter)?;   // a kitae_ui component IS the page
    engine.run()
}
```

### React → kitae_ui concordance (the learning path)

| React | kitae_ui | Notes |
|---|---|---|
| `useState` | `use_signal` | same job, no setter pair — one handle |
| JSX | `view!` | braces + attribute syntax instead of XML |
| props | `#[component] fn` args | plain typed arguments |
| `useEffect` | `use_effect` | same mental model |
| data fetching | `use_resource` | cancellation handled by the runtime |
| lists + `key` | `for .. -> (i, x)` + `key:` | same diffing semantics |
| conditional render | `if` inside `view!` | plain Rust `if` |

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│                     your components                      │
│        (#[component] fns + view! + use_signal)           │
├─────────────────────────────────────────────────────────┤
│              kitae_ui  (the reactive core)               │
│  signals · effects · virtual tree · diff → patch stream  │
│            (no engine dependency, pure lib)              │
├──────────────────────────┬──────────────────────────────┤
│   local runtime          │      live runtime            │
│  patches applied         │  patches serialized over     │
│  in-process onto the     │  WebSocket ("Live v1");      │
│  engine's style/layout   │  events routed back to the   │
│  tree directly           │  author's server handlers    │
├──────────────────────────┴──────────────────────────────┤
│        engine (net → html → css → style → layout)        │
│        display list → wgpu/Metal (CPU raster fallback)   │
└─────────────────────────────────────────────────────────┘
```

Crates this adds to the workspace:

- **`kitae_ui`** — reactive core + local runtime. Depends on nothing but
  std in phase 0; gains an optional engine feature for the local runtime.
- **`kitae_live`** — the server crate: route mounting, session handling,
  patch/event serialization, static fallback rendering. A website dev's
  `Cargo.toml` gets exactly two Kitae crates: `kitae_ui` + `kitae_live`.
- **engine side** — the Live *client* (patch applier + event sender) lives
  in the engine behind a feature, exposed through both the Rust API and
  the C FFI (two new commands + one event stream), so the Swift app and
  every other embedder get it for free.

## The Live wire protocol (v1)

Deliberately mirrors the engine's existing FFI philosophy: versioned,
explicit, binary-first.

- Transport: WebSocket, one session per mounted component tree.
- Frames: tagged, little-endian, `MAGIC "KTLV" + u8 version + u16 kind`.
- Kinds: `Init` (static HTML + hydration tokens) · `Patch` (op list) ·
  `Event` (element id + event name + payload) · `Ping/Pong` ·
  `Redirect` · `Error`.
- Patch ops (v1 set): `InsertNode`, `RemoveNode`, `MoveNode`,
  `SetText`, `SetAttr`, `RemoveAttr`, `SetStyle`, `SetFocus`,
  `ScrollIntoView`. The op set IS the DOM-mutation surface — nothing
  else can mutate a live page, which keeps the no-script guarantee
  auditable in one enum.
- Progressive enhancement: `Init` is a complete static page. Without the
  socket (old browser, bot, curl), everything still renders; with it,
  patches flow. A kitae_live site is a working static site by default.
- Reconnect: session id + monotonic patch seq; on reconnect the server
  resends from the client's last acked seq. State lives server-side, so
  recovery is cheap.

## Phases

### Phase 0 — reactive core (no engine, pure library)
Signals with dependency tracking, effects, the virtual element tree,
`view!` macro, keyed diff producing patch streams. Acceptance: the Hello,
Counter, and Todos examples compile as doctests; diff correctness has a
property-test suite (random trees, random mutations, minimal-diff
assertions); zero engine dependencies.

> **SHIPPED 2026-09-05.** `kitae_ui` crate + `kitae_ui_macros` (`view!`):
> signals with dependency tracking, `use_signal`/`use_effect`, VNode
> trees, keyed + positional diff producing v1 `PatchOp` streams
> (`InsertNode`…`ScrollIntoView`), `render_html` for the static path,
> component-call syntax in the macro. Signals are thread-local; state is
> hoisted to the closure owner and passed into components as `Signal<T>`.

### Phase 1 — local runtime (engine integration)
Mount components onto the engine's style/layout tree; patches drive
incremental re-style/re-layout of only the dirty subtree; input events
route back into `on:*` closures. Acceptance: **the Kitae start page is
rebuilt as kitae_ui components** — the app dogfoods its own library —
with scroll, hover, and click behaviors intact, and a repaint counter
proving incremental updates (no full-page relayout on a signal change).

> **SHIPPED 2026-09-05.** `kitae_core/src/ui.rs` `LocalApp` (mount onto a
> `html`>`body` DOM, apply ops, ref maps, handler table); `about:start`
> mounted from `start_page.rs` components; `Browser::click` routes into
> `on:click`; incremental publish via the splice path (ops-driven
> top-level dirty subtree, height-delta shift, full-relayout fallback);
> repaint-counter tests prove mount = 1 layout and signal changes splice
> without a relayout.

### Phase 2 — live runtime (websites)
`kitae_live` server crate + engine-side Live client; wire protocol v1 as
specified above; reconnect/resume; static fallback. Acceptance: Counter
and Todos run over a real WebSocket from the Kitae browser against a
kitae_live server, survive a forced disconnect mid-interaction, and the
same pages render correctly with the socket disabled.

> **SHIPPED 2026-09-06.** `kitae_live` (KTLV wire v1 over hand-rolled RFC
> 6455 — SHA-1 accept token, client-side masking, per-socket non-blocking
> I/O, single-threaded poll loop because kitae_ui signals are
> thread-local): routes, sessions surviving disconnects keyed by id,
> resume via `Sec-WebSocket-Protocol: kitae, resume <id>`, static
> fallback pages stamped `<meta name="kitae-live">`, seq numbers where 0
> is the hydration marker and updates start at 1, history window 4096.
> Engine side (`kitae_core` behind the `live` cargo feature, optional
> `kitae_live` dep): `live::LiveApp` — a non-blocking state machine
> (connect → open → dead → resume-with-backoff) that hydrates by replacing
> the static body's children with the hydration ops (refs address only
> that tree), applies server patches through the same apply machinery as
> the local runtime, acks the highest applied seq, and sends `Event`
> frames for clicks (hit-test → handler-bearing ref from
> `NodeData.handler` on the wire) and input values (two-way bindings via
> `set_input_value` → `Event{name:"input"}`). The FFI loop polls the
> socket between commands (`recv_timeout`); commands 27 `LIVE_POLL` and
> 28 `LIVE_STATE` (replies `live\t1/0` through the notifications queue);
> the engine loop + splice were generalized for both mounts (ops-driven
> splice root from dirty refs, text-aware splice bounds, removals fall
> back to full relayout). Master switch `set_live_enabled(false)` +
> `live_disconnect()`. Acceptance tests in `kitae_core/src/live.rs`:
> Counter and Todos (type/Add/toggle/delete) over a real in-process
> WebSocket, forced disconnect mid-interaction resuming the SAME session
> (state intact, post-resume click counts on), and static render with the
> socket disabled. kitae_live: 26 tests + doctest; kitae_core: 101 green
> with the feature, 97 without. Kitae.app rebuilt `--features live`
> (header also gained the long-missing raster-frame + vtable `family`
> declarations). wss/TLS deferred — dev servers are plain http.

### Phase 3 — developer experience
`kitae` CLI: `kitae new` (scaffold), `kitae dev` (hot reload — save to
patch-on-screen in under 100 ms), `kitae build` (static export). The
tutorial site is itself a kitae_live site, viewable in Kitae. Templates:
landing page, docs site, dashboard, form.

> **SHIPPED 2026-09-06.** `kitae_cli` crate → the `kitae` binary (hand-rolled
> args, no new deps). `kitae new <name> --template hello|landing|docs|
> dashboard|form` scaffolds a complete compiling site (path deps resolved
> from the CLI's own repo until the crates are published). `kitae dev` runs
> `cargo run` with `KITAE_DEV=1`: src/ edits restart the site; the
> generated main's dev loop watches `content/` and calls the new
> `World::refresh_all()` — a fresh Init + hydration pushed to every
> connected socket, patch-on-screen in milliseconds (unit-tested with two
> live sockets). `kitae build` runs the site with `--build-static dist`;
> `kitae_live::write_static` renders every route to plain HTML. The
> tutorial site is `kitae_cli/examples/tutorial.rs` — a three-page
> kitae_live site (intro, live counter, server-state list) viewable in
> Kitae. Tests: template invariants, scaffold layout, static-export
> roundtrip, and a full scaffold→`cargo check` compile proof; end-to-end
> smoke of `kitae new`/`kitae build`/`kitae dev` content hot-reload on a
> real scaffolded project.

### Phase 4 — ecosystem polish
Form components with validation, `use_resource` data fetching, client-side
routing (`<a>` handling + history), accessibility semantics (every
component maps to correct roles/labels the engine exposes to macOS
accessibility), and a public examples gallery ("Built with Kitae").

> **SHIPPED 2026-09-06.** `use_resource` (`kitae_ui/src/resource.rs`): a
> fetch closure runs on a background OS thread and lands its value in a
> signal via a type-erased channel; the drain runs on the UI thread
> (signals are thread-local), `World::step` calls it every iteration and
> `broadcast_updates()` re-renders sessions whose trees changed — also the
> general server-push primitive. Form components + validation: two-way
> `input` bindings (engine) with server-side validation handlers — the
> form template and the gallery's validation card are the reference.
> Client-side routing: `<a>` navigation through the engine's link path
> (real browser history, per-route sessions) plus server-initiated
> `Redirect` frames; SPA-style single-mount routing noted as future work.
> Accessibility: satisfied by construction — live pages are ordinary
> engine pages, so the engine's AX tree (roles/labels from tags) covers
> them on every publish. Gallery: `kitae_cli/examples/gallery.rs` — four
> live cards (signals/events, two-way list, validated form,
> `use_resource`), verified serving over HTTP with the live marker.

## Engine work this plan pulls forward

The UI library is also the prioritization force on the engine roadmap —
each phase below is enabled by an engine feature, in order:

1. **CSS coverage (flexbox, then grid)** — live-mode pages are laid out
   by the engine's CSS; without flexbox the layout story is 1998.
2. **Incremental layout + tree invalidation** — patch application is only
   fast if re-layout touches the changed subtree.
3. **Display list as a versioned contract** — the local runtime and the
   wgpu/Metal backend both consume it; freezing it as v1 is a phase-1
   exit criterion.
4. **Input model expansion** — focus management, form events, and
   `on:*` routing through both the Rust API and the C FFI.

## Non-goals

- **No WASM, ever** — including as a kitae_ui target. Rust runs on the
  server or in-process with the embedder; it never runs *in the page*.
- **No client scripting of any kind** — the patch-op enum is the only
  mutation surface, by construction.
- **Not a React re-implementation** — concordance with React is a
  learning aid, not a compatibility promise. Where web-compat and ease
  conflict, ease wins (this is a Kitae-authored web, not a JS-web clone).
- **Not a native-toolkit replacement** — kitae_ui styles pages with CSS
  through the engine; it is not iced/egui. Embedders wanting native
  chrome keep using their toolkit *around* an engine view.

## What success looks like

A web developer with HTML/CSS knowledge and a weekend of Rust writes a
multi-page interactive site — forms, lists, live updates — in a few
hundred lines, deploys a single binary, and serves it to Kitae where it
is fully interactive with **zero bytes of script**. And the same library,
mounted in-process, is how Kitae.app builds its own interface surfaces.
That is the demo that makes the ecosystem claim real: *V8 gave the web a
language; Kitae gives the web Rust.*

## React parity — the honest gap, and the roadmap to close it

Added 2026-09-06, after the question *"is this now a fully complete UI
library like React, where people can build any type of website,
including complex ones with visual elements?"* The honest answer was no —
Phases 0–4 ship a complete, proven **core**, not full parity. This
section is the commitment list that closes the gap. It is organized in
three tracks because the gap lives in three places: the framework's
primitive set (React's features), the engine's renderer (what a page can
look like at all), and the ecosystem around both (what makes React feel
bigger than a library).

**Where we are (shipped, see phase records above):** signals with
dependency tracking, `view!` + components, keyed/positional diff → patch
ops, `use_effect`, `use_resource`, two-way input bindings, server-side
validation, keyed lists, static export + hydration, reconnect/resume,
server push (`broadcast_updates`), a11y via the engine's AX tree, real
browser history for `<a>`, `kitae` CLI with templates and hot reload.

**The gap in one paragraph:** kitae_ui's ceiling is part engine, part
framework, part ecosystem — and the engine part is *further along than it
looks*. The renderer already draws a modern page: CSS Grid (tracks,
placement, gaps), flexbox, linear/radial gradients, box-shadow, animated
`@keyframes`, 2D transforms, `position: fixed/sticky`, borders,
`::before/::after`, `@font-face`, var(), calc(). The remaining engine
gaps are narrower: **CSS transitions** (state changes snap — nothing
interpolates between styles), **scrollable panes inside a page**, **:has()
+ container queries**, **filters**, and **video codecs**. The framework
itself is missing React's composition primitives (refs, context,
portals, error boundaries) and, like any young library, has no component
ecosystem. Closing all three tracks is what "build ANY website" requires.

### Track A — framework core (React feature parity)

> **Progress 2026-09-06:** the first three slices are SHIPPED —
> **RP-A1 (hooks core)** `use_signal`/`use_ref` now work *inside*
> `#[component]` bodies: position-stable via a scope tree
> (`kitae_ui/src/hooks.rs` — parent path + sibling index identity,
> per-build hook-cursor reset, slots persisted across builds; the rule of
> hooks applies exactly as in React). `#[component]` injects the scope
> enter/exit; render closures registered through `LocalApp::new`,
> `Session::new`, and `Route::new` are wrapped to start each build.
> **RP-A2 (context)** `provide_context`/`use_context` — values live on
> scopes and walk up the chain; reactivity comes from providing a Signal.
> **RP-A3 (children)** `Card() { … }` passes the braces as a lazy
> `Children` (a deferred builder — it runs inside the component's scope,
> which is what makes context+children compose); a component declares
> `children: Children` last and splices it with `{children}`. 6 tests in
> `kitae_ui/tests/reactive_core.rs`; workspace 486 green.
>
> **TRACK A COMPLETE 2026-09-06 — all nine slices shipped.** The remaining
> six landed the same day: **RP-A4 (error boundaries)** — render panics
> are caught at the nearest `ErrorBoundary` (`std::panic::catch_unwind`
> around `children.render()`), the fallback view renders, siblings stay
> alive, and recovery is automatic: fix the state that panicked and the
> real child returns on the next render (the panic-safe scope guard
> keeps the hook stack consistent). **RP-A5 (resources v2)** —
> `use_resource` is slot-stable inside a component (initial fetch runs
> exactly once across re-renders), gains `is_loading()` and
> `refetch()` (loading flips on, the stale value keeps rendering until
> the fresh one lands), and a panicking fetch flips loading off via the
> task's failure path. **RP-A6 (memoization)** — `use_memo_view(deps,
> build)` caches the built `View` per scope slot; unchanged deps skip
> the rebuild entirely (proven by a build counter that only advances
> when the closure runs). **RP-A7 (forms)** — `use_form()` /
> `form.field(name)` / `touch` / `is_touched` / `submit` / `submits`:
> per-field touched signals + submit lifecycle as crate state, zero
> ad-hoc glue. **RP-A8 (router)** — `Router::new(path)` wraps a route
> signal (`current()` / `navigate(to)`), `Link(router, to) { … }`
> dispatches navigation, and the content area patches in place (SPA
> core; history/back-forward + params are follow-ups). **RP-A9
> (strictness)** — `kit::warn` always collects into a global warnings
> log (echoed to stderr under `KITAE_STRICT=1`), `take_warnings()`
> drains; the first warning ships: mixed keyed/unkeyed lists detected
> in `diff_list`. 6 tests in `kitae_ui/tests/track_a.rs`; workspace 504
> green (kitae_render_gpu excluded; one kitae_html test failing is the
> author's in-progress adoption-agency edit, untouched here).

- **RP-A1 · refs & imperative handles (S).** `use_ref` for per-component
  mutable state that survives re-render — SHIPPED above; the node-handle
  half is now **SHIPPED too (2026-09-06)**: `use_node_ref()` + the
  `node_ref: handle` attribute mark an element; `NodeData.node_ref` rides
  the wire (trailing-optional, same pattern as `handler`); the runtime
  records handle → engine node at insert time. The host resolves it
  through `Browser::node_rect(handle)` (measured on-screen rect — the
  union of everything the element paints), `Browser::focus_node(handle)`
  (keyboard focus when focusable), and
  `Browser::scroll_node_into_view(handle)` (minimal scroll into the
  viewport). Plus `Browser::load_component(render)` — mount any component
  tree as a full page (the embedder-held app mode) and the natural home
  for handle-driven UIs. Acceptance met: focus an input on demand, read
  its rect, scroll a below-the-fold block into view (tested with
  viewport-containment assertions).
- **RP-A2 · context (S).** Provider/consume for theme, locale, session
  user — propagate without prop drilling. Scoped to the mounted tree;
  thread-local slab makes this straightforward. Acceptance: theme switch
  re-renders a deep subtree with zero prop changes.
- **RP-A3 · children & slots (S/M).** Components currently take data
  params only. Add `Children` (a captured `View`) and named slots so
  `<Card>`-style wrappers, layouts, and dialogs compose. Acceptance: the
  docs template's sidebar layout rewritten as a `Layout` component taking
  children. — **SHIPPED:** `Card() { … }` braces arrive as
  a lazy `Children` (declared last, spliced with `{children}`, core
  2026-09-06); named slots landed in the follow-up pass — a component's
  last prop `slots: Slots` renders call-site fragments by name
  (`slots.has("header")` / `{slots.render("header")}`; cookbook recipe
  12, track_a tests).

- **RP-A-server · runtime hardening (SHIPPED 2026-09-06).** The server
  runtime grew its production surface: **graceful shutdown**
  (`Server::shutdown_handle()` → `World::run_until_stopped()` returns;
  the handle is the only cross-thread surface), **unforgeable session
  tokens** (session ids are resume capabilities — SHA-1 of clock+counter
  replaces the guessable `s-1` counter), **idle-session TTL eviction**
  (`set_session_ttl`; the session object — diff shadow, history, handler
  table — drops; the author's signal state persists by design),
  **static-file serving** (`mount_dir("/static", "./assets")` —
  content-typed, canonicalized, traversal-refused; routes win on
  overlap), **`World::push_session(id)`** (targeted server push beside
  the global `broadcast_updates`), and **HEAD** (headers-only responses).
  6 new tests; kitae_live 34 + doctest green.
- **RP-A4 · error boundaries (M) — SHIPPED 2026-09-06.** A component's
  render panic is caught at the nearest boundary: render a fallback, keep
  the rest of the page alive. `ErrorBoundary(fallback) { … }` wraps
  `children.render()` in `catch_unwind`; the panic-safe scope guard keeps
  the hook stack consistent, so removing the panic source brings the real
  child back on the next render. Acceptance met (deliberately panicking
  card degrades to the fallback and recovers).
- **RP-A5 · resources v2 (M) — core SHIPPED 2026-09-06; follow-ups SHIPPED 2026-09-08.** `use_resource`
  is slot-stable (initial fetch exactly once across re-renders), exposes
  `is_loading()` and `refetch()` (stale-while-revalidate: the old value
  keeps rendering until the fresh one lands), and a panicking fetch flips
  loading off. **Follow-ups now in:** `use_resource_with_deps` (refetch
  only when the dep tuple changes — same deps skip the thread entirely),
  `Resource::error()` (a panicking fetch surfaces `Some(msg)` via
  catch_unwind, loading flips off), `suspense_view(res, fallback, ready)`
  (the classic match re-implemented once), and `use_cached_resource(key,
  fetch)` — an identity-keyed process-global cache: the first call with a
  key fetches on a thread, every later call (re-render, other component)
  returns the cached value immediately, and a value landing after first
  mount is seeded quietly on the next render without a refetch. 12 tests
  in `tests/track_a2.rs` cover all four.
- **RP-A6 · subtree memoization (S) — core SHIPPED 2026-09-06; skip_eq SHIPPED 2026-09-08.**
  `use_memo_view(deps, build)` caches the built `View` per scope slot;
  unchanged deps skip the rebuild (build counter only advances when the
  closure runs). **Follow-up now in:** `#[component(skip_eq)]` — the
  macro packs every top-level binding into a props tuple compared through
  a dedicated hook slot; equal props reuse the cached view and the body
  never runs (React.memo semantics, the 5,000-row case without explicit
  memo calls).
- **RP-A7 · forms crate-level (M) — core SHIPPED 2026-09-06; dirty/error/reset SHIPPED 2026-09-08.**
  `use_form()` gives per-field touched signals, `touch`/`is_touched`,
  `submit` lifecycle and `submits` count as crate state. **Follow-ups now
  in:** `field_value`/`is_dirty`/`is_dirty_any` (initial value recorded
  once per field; auto-registered on first `set_value`), `set_error`/
  `clear_error`/`error` per field, and `reset()` restoring the recorded
  initials (auto-registered fields reset to `""`), clearing errors and
  zeroing submits.
- **RP-A8 · SPA router (M/L) — core SHIPPED 2026-09-06; history/params SHIPPED 2026-09-08.**
  `Router::new` (route signal, `current()`/`navigate`), `Link(router, to)
  { … }` dispatches navigation, content patches in place. **Follow-ups
  now in:** back/forward stacks (fresh navigation clears the forward
  stack; navigating to the same path is a no-op), `route_param(path,
  pattern, name)` + `Router::param` (`:name` captures with literal
  segments matched), and `query_param(path, key)` + `Router::query`
  (percent-decoding, `+` → space). Deep links via Redirect frames remain
  open (host-layer).
- **RP-A9 · dev-mode strictness (S) — core SHIPPED 2026-09-06; purity detection SHIPPED 2026-09-08.**
  `kit::warn` collects into a global log (echoed to stderr under
  `KITAE_STRICT=1`), `take_warnings()` drains; first warning = mixed
  keyed/unkeyed lists. **Follow-ups now in:** signal writes during render
  warn once per signal ("renders must be pure; move the write into an
  effect or event handler") — suppressed for effect bodies running under
  first-mount `use_effect`; and `wrap_render_strict` double-renders the
  closure and compares the HTML, warning "impure render" when two passes
  disagree.

### Track B — engine visual parity (the remaining renderer gaps)

These are engine slices (kitae_style/kitae_layout/kitae_render), tracked
cross-reference in `BROWSER_PARITY_PLAN.md`; enumerated here because each
one gates concrete UI-library use cases. **Verified against the tree
2026-09-06** (grep + audit reconciliation): Grid, gradients, box-shadow,
keyframes, fixed/sticky, and transforms are already in — the list below
is what is actually left. Ordered by payoff.

- **RP-B1 · CSS transitions (M).** The one motion gap: interpolate
  computed-style changes over time (transform/opacity/color first —
  layout-safe properties), stepping in the engine's paint loop.
  `@keyframes` already animate; transitions make every hover/press
  state feel alive. Unblocks: perceived polish everywhere.
- **RP-B2 · scrollable overflow regions (M/L).** `overflow:
  auto/scroll` panes inside a page: nested scroll extents, scrollbar
  painting + hit-testing, wheel routing between nested scrollers.
  Unblocks: mail UIs, tables with frozen headers, chat panes, code
  viewers.
- **RP-B3 · :has() + container queries (M).** Deferred-by-decision items
  now promoted: component libraries need parent-state selectors and
  container-responsive widgets. Unblocks: reusable components that adapt
  to where they're placed.
- **RP-B4 · filters & backdrop-filter (M).** blur/drop-shadow/
  brightness; mix-blend-mode later. Unblocks: image treatments, glassy
  surfaces.
- **RP-B5 · grid completion (S/M).** Grid works (tracks, placement,
  gaps); add `grid-template-areas`, named lines, `conic-gradient` while
  in the file, subgrid later. Unblocks: expressiveness for app shells.
- **RP-B6 · text engine completion (M).** `ch`/`ex` units,
  `writing-mode`, multicol, hanging punctuation — the remaining text
  track from the parity plan. Unblocks: editorial/magazine layouts.
- **RP-B7 · media codecs decision point (C policy).** mp4/webm playback
  (H.264 policy per `CPP_ROLE.md`), AVIF decode. Unblocks: any
  video-first site.
- **RP-B8 · 3D transform rendering audit (S).** `perspective`/
  `rotate3d` parse; confirm the wire → paint path renders true 3D, or
  scope what's missing. Unblocks: confident card-flip/depth effects.
- **RP-B9 · canvas / WebGL page APIs — stays OUT.** Restated because
  React parity invites the question: a canvas *page* API is a JS-shaped
  escape hatch (non-goals). Chart/diagram needs are met by SVG (inline
  SVG landed in R6) — see RP-C2.

#### Track B status (2026-09-07)

All eight of B1–B8 are shipped end-to-end at the engine level. The host
side (Swift / CIFilter / scrollbar / CoreMedia decode) remains a small
follow-up on each — the engine exposes the per-item data the host needs
through the wire; the host now applies it. Status by row:

- **B1 transitions — SHIPPED.** Wire v9, parse + apply + tick driver.
  See `ENGINE_CORE_STATUS.md` for the diff list.
- **B2 scrollable overflow — SHIPPED.** `Overflow::Auto | Scroll` parsed
  and recognized during layout. Wire v17 carries `(node, x, y, w, h,
  content_h)` per scroll container. Host paints the scrollbar and
  routes wheel events (host concern).
- **B3 :has() + container queries — SHIPPED.** Engine two-pass for
  `@container` (record boxes on first layout, fold widths into the
  second style pass; no fixpoint). cqw/cqh/cqi/cqb/cqmin/cqmax fold at
  style time.
- **B4 filters & backdrop-filter — SHIPPED.** `parse_filter_list` covers
  the standard functions; `drop-shadow` color defaults to currentColor.
  Wire v16 carries a sparse per-item filter list. Host applies via
  `CALayer.filters` (CIFilter). `backdrop-filter` parses + stores; the
  real backdrop layer is a host concern (no public macOS API).
- **B5 grid completion — SHIPPED.** `grid-template-areas`, named lines,
  `conic-gradient`. Verified by trace.
- **B6 text — partial.** `ch`/`ex` parse and fold against the engine's
  measurer-fallback (0.5em). Full measurer integration is future work
  (the engine would need a `FontMetrics` field on `ComputedStyle`
  populated by the host's `TextMeasurer`). `writing-mode` and
  multi-column are documented in `BROWSER_PARITY_PLAN.md` as remaining
  slices; not yet implemented.
- **B7 media codecs — DECIDED.** `docs/MEDIA_CODECS_DECISION.md` is the
  decision doc. Recommended path: royalty-free decoders (dav1d + libvpx)
  + macOS VideoToolbox for H.264. Awaiting user approval before
  implementation begins.
- **B8 3D transform — SHIPPED.** `perspective`/`rotate3d` parsed, wire
  carries the orthographic-projection matrix, host renders true 3D.

### Track C — ecosystem & DX (what makes React feel big)

- **RP-C1 · KitaeUI component kit — SHIPPED (2026-09-08).** `kitae_ui::components`
  (prelude-exported): ThemeProvider (`kit-theme-{light,dark}` context +
  `theme_class()` fallback), Button (variant/disabled), Modal (backdrop
  click-close, panel swallows bubbled clicks), Drawer (left/right anchor),
  Tabs, ToastHost + `push_toast` (click-dismiss; engine has no timers),
  Tooltip (click-toggle; engine has no hover), Table (sortable — numeric-
  aware cmp — and paged), VirtualList (pager windowing), DatePicker
  (always-open calendar, month browsing). All built only on Track A
  primitives. Deviations recorded in the module doc (engine event
  vocabulary = click/input only). 11 integration tests in
  `tests/components_tests.rs` incl. a 150-round fuzz of a composed app.
  Fix that fell out: `hooks::enter_scope` now resets **both** `hook_cursor`
  AND `child_cursor` at a scope's own re-entry — depth-2+ components were
  getting a fresh scope per render and silently dropping internal signal
  state (Tooltip/DatePicker). Workspace 638 green.
- **RP-C2 · charts starter — SHIPPED (2026-09-08).** `kitae_ui::charts`
  (prelude-exported): `LineChart`, `AreaChart`, `BarChart`, `Sparkline` —
  dependency-free SVG charts that take a `Signal<Vec<…>>` and recompute
  geometry per render (the whole "driven by signals" story). Plain `svg`
  subtrees so any host can paint them. Starter scope: min/mid/max y
  gridlines + labels, first/last x labels, one accent color, zero-pinned
  bar/area baselines (red bars + zero line for negatives), chrome-free
  sparkline with last-sample dot. Empty/single-point data renders empty —
  never NaN. 7 integration tests incl. a 60-round churn property run;
  module-doc doctest proves the signal redraw.
- **RP-C3 · Kitae DevTools — SHIPPED (2026-09-08).** `kitae_ui::devtools`
  (prelude-exported `DevTools`, `PatchLog`, `OpCounts`): a dark aside
  panel rendering four live sections — **component tree** (names, slot
  counts, scope ids, indent-per-depth), **inspected signals** (name =
  value, recomputed every render), **patch log** (the actual PatchOps the
  wire sent, `op.to_string()` rows, ring buffer capped at 256), and the
  **splice/relayout counter** (structural = Insert/Remove/Move vs
  property = SetText/SetAttr/SetStyle/…, plus totals). Plumbing: the
  `#[component]` macro now passes the fn name into
  `hooks::enter_scope(Some("Name"))`; `Scope` records it (refreshed on
  re-entry — slots are positional and reusable); `hooks::scope_tree()` +
  `signals::debug_cells()` are the public snapshots a tool can read.
  `PatchLog` is a cheap Clone handle shared with the host: the host calls
  `log.record(app.ops())` (or feeds it from the real wire) and the panel
  displays it; `clear()` drops entries only — counters keep lifetime
  totals. **Mount DevTools LAST among siblings** (it reads the tree at
  render time). Core fix that fell out: `ScopeGuard` recorded its depth
  AFTER pushing the scope, so its drop never popped the guard's own
  scope — sibling component calls nested under each other (invisible to
  hook correctness because build order is deterministic, but wrong for
  any tree display). Guard now records pre-push depth. 4 integration
  tests in `tests/devtools_tests.rs` + module doctest; workspace 651
  green. Panel-feedback testing doctrine: assert relative growth and
  content presence, never exact op counts after the panel has displayed
  a non-empty log (displaying the log changes the next diff).
- **RP-C4 · testing utilities — SHIPPED (2026-09-08).** `kitae_ui::testing`:
  `TestApp` renders a component into a queryable owned `Node` tree
  (`query`/`query_class`/`query_text`/`query_where`/`node_at`/`count`),
  fires events at elements (`fire`/`click_tag`/`click_text`/`set_value`),
  and asserts on output (`text`/`html`/`ops` — the raw PatchOp stream).
  Two-way `value:` bindings are driven by `fire`/`set_value` (the harness
  implements the documented write-back contract — see Track A follow-up
  note on bindings consumers). Plus the property harness: `fuzz(cfg)`
  fires seeded random event sequences (HTML-escape payloads included),
  catching panics (`FuzzFailure`) and draining strict-mode warnings into
  `FuzzReport`. `testing::reset()` gives a second TestApp fresh hook slots.
  14 integration tests in `tests/testing_tests.rs`; prelude re-exports
  `TestApp`. Unblocks: confidence at ecosystem scale.
- **RP-C5 · component-level hot reload (M) — SHIPPED 2026-09-08.** The
  plan's "hot-swap one component fn" is generalized to the unit Rust can
  actually swap atomically: **the routes module**. Scaffolds are now
  two-file sites — `src/lib.rs` holds the pages (`pub fn routes()` +
  the `kitae_live::export_routes!()` C export), `src/main.rs` is a host
  the user rarely edits. kitae_live gained `DylibWatch` (mtime+len
  baseline → pure `changed()` → `load_routes()` dlopens a fresh
  versioned copy `lib-hot-{gen}.dylib` next to the real dylib, dlsyms
  `kitae_routes`, never dlcloses the old image; failed loads keep the
  stamp pending), `target_dylib`, `preload_dylib`, and `export_routes!`.
  `kitae dev` split into two loops: lib.rs edits rebuild WITHOUT
  restarting — the running site swaps its own routes dylib, clients stay
  connected (a failed page build only prints; the last good routes keep
  serving); main.rs/Cargo.toml edits restart the host. Sites build
  `prefer-dynamic` with kitae_ui/kitae_live as dylibs so host + swapped
  dylib share ONE image of each (one thread_local signal universe) —
  lab-verified linkage. `kitae new` writes `.cargo/config.toml`
  (`@loader_path` / `$ORIGIN` rpath) and the lib crate-type pair
  `["dylib", "rlib"]`. Non-unix keeps content-only hot reload. Tests:
  37 kitae_live (incl. real-dlopen hotreload tests) + 5 CLI tests incl.
  a full scaffold → build → dlopen → edit → rebuild → re-fire e2e
  (`scaffolded_dylib_hot_loads`). Sub-second inner loop: edit → save →
  swap, no disconnect, no lost state.
- **RP-C6 · docs & cookbook (M) — SHIPPED 2026-09-08.** `docs/UI_COOKBOOK.md`
  (16 recipes: counter, keyed lists, conditionals, forms, resources +
  suspense, identity cache, routing, tabs, table, toasts, theming, named
  slots, error boundaries, charts, skip_eq memo, patch-op observability,
  plus a wire-shape cheatsheet and the documented deviations list), and
  `crates/kitae_ui/tests/cookbook_tests.rs` — 17 integration tests, one per
  recipe, so documented patterns cannot silently rot. Two API gaps closed
  while making the recipes honest: `Form` is now `#[derive(Clone)]` (event
  handlers each take a handle), and the if-branch splicing limitation is
  documented (a bare component call as an entire branch body needs an
  element wrapper). kitae_ui suite 100 tests green.
- **RP-C7 · publishing (S) — PREP COMPLETE 2026-09-08; the act of
  publishing is user-gated.** Publish chain `kitae_ui_macros → kitae_ui
  → kitae_live → kitae_cli` is package-ready: all four `cargo package`
  clean at 0.1.0, version+path dual deps everywhere (no manifest edits
  at publish time), CLI/template carry descriptions, and `kitae new
  --registry` already scaffolds against the published version line.
  `docs/RELEASE_CHECKLIST.md` holds the exact order (deps first), the
  LICENSE/git-init prerequisites, and the post-publish default flip.
  Publishing, `cargo login`, and tagging wait on the user.

### What we deliberately do NOT copy from React

- **Concurrent rendering / time-slicing** — our diff runs server-side or
  in-process, synchronously; there is no main starved by rendering.
  (RP-A6 memoization is the perf slice that matters.)
- **Class components, legacy lifecycles** — functions and signals only.
- **DOM escape hatches** (`dangerouslySetInnerHTML` and friends) — the
  patch-op enum remains the only mutation surface; that auditability is
  the product.
- **Hydration-reconciliation quirks** — our Init/hydration contract is
  explicit (static HTML + the mount's ops), not a best-effort match.

### Sizing, order, and the definition of done

Rough sizing: S ≈ a day, M ≈ a few days, L ≈ 1–2 weeks at the current
cadence. Recommended order (each step is independently shippable):

1. **Framework core quick wins:** RP-A1 refs, RP-A2 context, RP-A3
   children — the composition primitives everything else leans on.
2. **Motion:** RP-B1 transitions (the engine already keyframe-animates;
   this is the last motion gap).
3. **App shells:** RP-B2 overflow panes + RP-B5 grid completion, then
   RP-A8 SPA router — together they make Gmail-style UIs expressible.
4. **Component-library enablers:** RP-A4 error boundaries, RP-A5
   resources v2, RP-A6 memo, RP-A7 forms, RP-B3 :has()/container
   queries, RP-B4 filters.
5. **Ecosystem:** RP-C1 kit (starting with Modal + VirtualList), RP-C2
   charts, RP-C3 devtools, RP-C4 testing, RP-C6 docs, RP-C5 hot reload,
   RP-C7 publish. RP-B7 codecs and RP-B6 text completion interleave
   wherever a concrete site demands them.

**Definition of done for "like React":** a developer can rebuild, without
hitting a missing primitive, (a) the GitHub issues page (lists, forms,
routing, state), (b) a Gmail-style three-pane app (Grid, overflow panes,
fixed toolbars, SPA router), (c) an Apple-style product page (gradients,
shadows, transitions, keyframes, video), and (d) a Figma-*lite* canvas
viewer (SVG, virtualization) — each in a few hundred lines, each fully
interactive in Kitae with zero bytes of script, and each a complete
static site for every other browser.
