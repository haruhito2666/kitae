# Kitae — Scripting Decision (QuickJS scope)

_Written 2026-10-14. Owner decision: add QuickJS. Amended the same day by
owner decision: a **page JavaScript tier** ships too. This document records
exactly what that means, so the boundaries survive every future
conversation._

## Part 1 — the closed-API automation runtime (`kitae_script`, shipped)

Kitae ships a script engine as `kitae_script`, an embedded **QuickJS**
runtime that executes *automation scripts* against **Kitae's own closed
API surface**. Consumers:

1. **User scripts** — per-site/per-document automation maintained by the
   user (the stronger sibling of the planned userstyle system).
2. **Agent scripts** — the same API exposed to the `kitae_mcp` /
   `kitae_server` tooling, so an agent can script extraction and
   interaction without a browser stack.
3. **Server-side kitae_ui / kitae_live** — component logic that wants a
   scripting layer inside a server the author controls.

## Part 2 — the page JavaScript tier (`page-js`, shipped 2026-10-14,
opt-in)

**Owner decision, second revision:** pages may ALSO execute their own
scripts through a feature-gated tier in `kitae_core` (`page-js`),
implemented on the same QuickJS runtime:

- **Off by default, feature-gated (`--features page-js`).** Without the
  cargo feature the binary cannot run page scripts at all; with it, the
  host still opts in per engine (`Browser::set_page_js`, FFI command 53,
  `Engine::builder().page_js(true)`). The default build and every existing
  test/consumer keep the strictly script-free behavior.
- **The render path is legitimized, with fallbacks intact.** Page scripts
  run in document order after parse and before styling (inline text;
  `src` fetched through the engine cache + origin gate like stylesheets,
  with per-script/total byte caps). A page whose scripts all fail while
  the body stays text-thin still falls back to the honest interstitial —
  the same policy as scripts declined.
- **DOM through the real arena.** The shim (`pagejs.rs`) operates on the
  live `kitae_dom` tree: mutations are first-class DOM mutations, so
  restyle/relayout/republish, print, a11y, and find all see script
  output. Event-loop slices (timers, settled `fetch()` promises, queued
  navigations, click events) drain from the hosts' existing 2 ms idle
  tick; `fetch()` rides the shared background pool origin-scoped (S-1).
- **Budgets, always.** Per page: 64 MiB heap, 8 MiB stack, 10 s wall
  clock per task (interrupt-enforced), script-count/size caps. Errors are
  values; the runtime survives every script.
- **v0 honesty.** Supported surface is inventory-listed below — amended
  2026-10-15 (tier JS-1): `style.prop =` writes (Proxy-backed style
  object), `preventDefault()` on engine default actions (a fired listener
  no longer owns the click), ES modules (static chains + dynamic
  `import()` over the module budget), and the full common selector
  grammar (combinators `>` `+` `~`, attribute operators, structural
  pseudos, `:not`, comma lists, element-scoped `querySelector(All)` +
  `closest`) moved from gap to inventory. Amended again 2026-10-15 (tier
  JS-2): animation frames (`requestAnimationFrame`/`cancelAnimationFrame`
  on the idle tick), `performance.now()`, `queueMicrotask`,
  `requestIdleCallback`, `MessageChannel`, `getBoundingClientRect` +
  `offsetWidth/Height` + `innerWidth/Height` from the published layout,
  the History API (`pushState`/`replaceState`/`back`/`forward`/`go`,
  `popstate`, `location.hash`), full event propagation (capture →
  target → bubble with `stopPropagation`/`stopImmediatePropagation`/
  `currentTarget`), and form/key engine hooks (`keydown`/`input`/`keyup`
  on typing; `input`/`change` when a control's value commits — checkbox/
  radio toggle, range drag or step, select choice; `submit`, where
  `preventDefault()` keeps the engine from submitting). Element `value`
  now follows select semantics (selected option) and `checked` reads/
  writes the checkedness attribute. Still not supported (documented
  gaps, each a future row): bare module specifiers (`import "react"`),
  import maps, `data:`/`blob:` module URLs, external-script
  async/defer scheduling, geometry reads before first layout (load-time
  `getBoundingClientRect` sees zeros), `change` on text fields at commit
  (blur/Enter) — text `input` fires per edit, `change` is wired for the
  discrete controls only — and the JS-3 gap list recorded at the top of
  this section.
  Dynamic `import()` resolves synchronously on the JS thread and is
  bounded by the module fetch deadline (12 s shared, 96 modules, 6 MiB).
  Page-script clicks now hit-test text runs as well as painted boxes, so
  a click on an anchor's label resolves to the anchor. Amended a third
  time the same day (tier JS-3, first step of the agentic pivot's Track A
  — `ROADMAP.md` §The current plan): the standard constructor set lands
  (`Event`/`CustomEvent`/`MouseEvent`/`KeyboardEvent` + `dispatchEvent`,
  `AbortController`/`AbortSignal`, `DOMException`, `URL`/
  `URLSearchParams` resolved through the engine's own URL handling,
  `Headers`/`Request`/`Response`, `FormData` with engine-carried
  multipart POST bodies over the shared pool), `structuredClone`
  (objects/arrays/Date/Map/Set/typed arrays/cycles), listener options
  (`once`/`passive`/`signal`), `element.dataset` (camelCase ↔ `data-*`),
  `getComputedStyle` (a computed-subset `getPropertyValue`, with
  width/height answered as *used* px from the painted geometry), and the
  read/act paths pages call — `window.scrollTo`/`scrollBy`/`scrollX`/
  `scrollY`/`pageYOffset`, `element.scrollIntoView`,
  `documentElement.scrollTop`, `element.focus()`/`blur()`,
  `document.activeElement`, `window.getSelection()` (engine selection
  text). JS-3's recorded gaps: response headers expose content-type and
  content-length only (the net layer keeps no generic header list);
  `fetch` abort rejects the promise but the in-flight pool job finishes;
  `getComputedStyle` is the subset above, not a full
  CSSStyleDeclaration; only the document scrolls (no nested scroll
  containers); `activeElement` tracks engine focus, not programmatic
  focus trees.

  Amended 2026-10-16 (tiers JS-4/JS-5/JS-6 — the last feature work of the
  Track-A ladder; the WPT-subset spine that measures it lives in
  `crates/kitae_core/tests/wpt_page_js.rs`, see `WPT_SCORECARD.md`):
  **observers** — `MutationObserver` (records are journaled by the DOM
  wrappers themselves: attributes, characterData, childList, with
  `subtree`/`attributeFilter`/`attributeOldValue`/`characterDataOldValue`;
  callbacks run on the microtask queue, as the spec has it, with the
  engine tick as the safety net), `IntersectionObserver` (entries when a
  threshold is first crossed, against the published display-list geometry
  and the engine's scroll state), `ResizeObserver` (content-box sizes from
  the same published geometry); **`XMLHttpRequest`** (async only —
  synchronous XHR is refused by design; `open`/`send`/`abort`,
  `setRequestHeader`, the OPENED→DONE readyState ladder, status/
  statusText/responseText/response/responseURL, `getResponseHeader`/
  `getAllResponseHeaders`, `onload`/`onerror`/`onreadystatechange`/
  `onloadend` plus `addEventListener`; rides the same fetch pool as
  `fetch`, so custom headers and bodies take the pool's Raw path);
  **`Worker`** (documented same-thread emulation: the script is fetched
  through the pool and evaluated with `self`, `postMessage` and
  `addEventListener` bound to a port; messages are `structuredClone`d and
  queued until the script is ready; `terminate()` stops delivery — there
  is no real parallelism and the doc says so); **Web storage**
  (`localStorage`/`sessionStorage` as spec-shaped `Storage` objects:
  `getItem`/`setItem`/`removeItem`/`clear`/`key(i)`/`length` with
  Proxy-backed named-property get/set/delete/`in`/ownKeys; `localStorage`
  is process-wide per origin and in-memory — no disk persistence,
  documented; `sessionStorage` is scoped to one `Browser` and survives
  navigation inside it; opaque origins — `file:`, non-special schemes —
  throw `SecurityError` on every operation, as the spec has it; no
  cross-window `storage` events, documented); **canvas 2D** (`<canvas>`
  is a replaced element with the spec's 300×150 default — the UA sheet no
  longer hides it; `getContext('2d')` returns a context with fill/stroke
  styles (hex, rgb()/rgba(), a named subset), paths (`moveTo`/`lineTo`/
  `closePath`/`rect`/`arc`/`ellipse`/`quadraticCurveTo`/`bezierCurveTo`),
  `fill`/`stroke` under both rules, `fillRect`/`strokeRect`/`clearRect`,
  the affine transform set (`translate`/`scale`/`rotate`/`transform`/
  `setTransform`/`getTransform`) with `save`/`restore`/`globalAlpha`,
  `getImageData`/`putImageData`/`createImageData`, `drawImage` from
  another canvas, an `ImageData` or an `<img>` element (whose pixels come
  across from the engine's own decode cache), and `toDataURL()` → PNG
  encoded by `kitae_media`; the JS-side buffer reaches the engine once per
  poll tick through the `\0canvas:{node}` sentinel and paints as an
  ordinary display-list image); `btoa`/`atob`; and PNG/JPEG/GIF/WebP
  `data:` image URLs now decode (`ensure_web_raster`), which is what makes
  the canvas `toDataURL` output usable as an image source at all.

  JS-4/JS-5/JS-6 recorded gaps: worker threads are an emulation (no
  parallelism, no `importScripts`), synchronous XHR refused, no
  `WebSocket`/`EventSource`, no cross-window storage events, no canvas
  text drawing (`fillText`/`strokeText` are no-ops and `measureText`
  estimates), no canvas `clip`/gradients/patterns/shadows/filters, no
  anti-aliasing (hard-edged spans), canvas `lineCap`/`lineJoin`/
  `miterLimit`/dash fixed, `globalCompositeOperation` accepted but always
  source-over, and `toBlob`/`transferControlToOffscreen` absent.

The earlier analysis stands as **the boundary of this tier**: no claim is
made that this closes the compatibility race with Chromium's script
surface — the tier serves pages whose scripting is modest; heavy
SPA-era pages keep their outs (below). The not-do list (`ROADMAP.md`,
`ENGINE_CORE_STATUS.md` rule #1) binds the ENGINE's identity: the display
list remains the endpoint, page scripts are an opt-in layer on it, no
engine-grade DOM/event investment races Chromium, and **WASM stays a full
"never"**.

Remaining outs for script-heavy pages: the interstitial with one-key
external open, per-domain delegation to the system browser (Phase 4), and
the delegated-render tier (host-side WebKit, results re-enter Kitae as a
document).

## Part 3 — the page tier is the product core (2026-10-15, owner decision
[`AGENTIC_PIVOT.md`])

Owner pivot: Kitae's focus is now a **standard-JS browser on QuickJS —
lowest RAM in its class, fast, fully agentic**. The `page-js` tier moves
from opt-in add-on to the main compatibility program: the tier ladder
continues (JS-3 standard constructors/read paths, JS-4 observers, JS-5
workers + XHR, JS-6 canvas + storage truth — full ladder and exit
criteria in `AGENTIC_PIVOT.md` §5; the dependency-ordered plan lives in
`ROADMAP.md` §The current plan, Tracks A–E), with a WPT-subset + site-panel
conformance discipline. The cargo feature gate and per-engine enable
remain for library/embedded builds; the shipped browser app already
enables page JS for every tab, and under this pivot that is the product,
not an exception. WASM stays a full "never" pending an explicit owner
decision (flagged, not made, in the pivot doc). Budgets and the display
list as the render endpoint carry forward unchanged.

## Why the closed API matters (design contract)

The automation API is **ours to define and finish** — it is the shape that
makes "optimize QuickJS and add features bit by bit" a converging project
instead of the unbounded web-DOM treadmill:

- The API surface is an explicit inventory in `kitae_script::api` — every
  callable exists because we listed it, with a document saying why. There
  is no gradual growth toward "everything the web has"; when the surface
  covers reading/navigation/forms/extraction well, it is done.
- Scripts run against Kitae's document state (display list, AX tree,
  navigation, forms) — not against a web-DOM emulator. Compatibility
  pressure from real websites therefore has no lever into this engine.
- **Who may act, and who may not (B4, 2026-10-16).** The act verbs
  (`kitae.navigate`, `click`, `fill`, `submit`, `watch`, `dispatch`, …)
  are reachable from three places only: a Track-A automation script
  (through `SharedSession`, behind the script host's own policy), the
  transports (`kitae_mcp`, `kitae_server`), and the app's agent panel
  (FFI commands 54–58, behind a per-verb grant the user answers). They are
  **not exposed to page scripts**: `kitae_script::agent::install_agent_api`
  is called with an `AgentApi` that a page runtime never receives, so a
  hostile page cannot drive its own browser — the page tier's inventory
  below is the whole of what fetched script can reach. The default policy
  on any new session is read-only, and a refusal names the verb rather
  than failing silently.

## Page-JS API inventory (implemented — amended 2026-10-15, tier JS-3)

document: getElementById, body/documentElement/head, createElement,
createTextNode, querySelector/querySelectorAll (full supported grammar:
tag, #id, .class, `*`, `[attr]` with `=` `~=` `|=` `^=` `$=` `*=`,
compound parts, descendant/`>`/`+`/`~` combinators, `:first-child`,
`:last-child`, `:nth-child(odd|even|N|an+b)`, `:not(compound)`, comma
lists — unknown syntax matches nothing), getElementsByTagName,
getElementsByClassName, title (get/set), write/writeln (appends to body),
readyState ("complete"), location (href get/set, reload) + global
location alias. Elements: attributes (get/set/has/remove), className +
classList, textContent, innerHTML (graft via fragment parser),
appendChild/insertBefore/removeChild/remove, style: `style.prop =`
(Proxy; camelCase compiled to kebab), cssText, setProperty/
getPropertyValue/removeProperty, value (select-tag aware:
get = selected option's value, set = move the `selected` marker),
checked (get/set the checkedness), tagName/id, children, parentElement,
element.querySelector(All) (document-matched, scope-filtered) +
closest(), getBoundingClientRect (painted boxes; sub-tree union
fallback) + offsetWidth/offsetHeight, addEventListener (capture or
bubble; `stopPropagation`/`stopImmediatePropagation`/`currentTarget`
honored; window/document-delegated listeners reach element events) +
onclick, setTimeout/Interval + clears (string bodies compiled),
queueMicrotask, requestAnimationFrame/cancelAnimationFrame (idle-tick
driven, `performance.now()` timestamps), requestIdleCallback/
cancelIdleCallback, MessageChannel/MessagePort (structured round-trip
between ports), performance.now/timeOrigin, fetch(url) GET →
{ok,status,text(),json()} over the shared pool, console.log/warn/error →
host notifications, localStorage (per-page, session),
navigator.userAgent. History: pushState/replaceState (same-document, the
address bar follows, no reload), back/forward/go, popstate on the loaded
document, location.hash get/set. ES modules: `<script type="module">`
inline + `src`, static import chains resolved against the importing
module's URL, dynamic `import()`, top-level code sees the same globals
(strict mode always), one budget ledger per page (96 modules / 1 MiB
each / 6 MiB total / 12 s). Events dispatched: DOMContentLoaded, load,
click (host hit-test over painted boxes AND text runs — an anchor's
label is a target — then capture/bubble through the tree),
keydown/input/keyup (engine typing), input+change on value commits
(checkbox/radio toggle, range pointer or step, select choice), submit
(preventDefault suppresses the engine's submission), popstate. JS-3
additions to the same inventory: the constructor classes above plus
script-constructed and script-dispatched events (`dispatchEvent`),
listener records with `once`/`capture`/`passive`/`signal` (AbortSignal
removal), `FormData` (incl. `new FormData(form)` reading named fields),
`fetch` with `method`/`headers` body/`signal` — POST/PUT/PATCH/DELETE
over the pool's uncached Raw path with auto content-type for FormData
(multipart) and URLSearchParams (urlencoded), Response objects with
ok/status/statusText/headers/url/text/json/arrayBuffer/clone,
`structuredClone`, `URL`/`URLSearchParams` (live `searchParams` linkage),
`element.dataset`, `getComputedStyle`, and the scroll/selection/focus
reads listed in the Part 2 amendment above (scroll/focus requests are
queued and applied by the host's poll; `window.scrollX` is always 0).
JS-4/JS-5/JS-6 additions to the same inventory: `MutationObserver`,
`IntersectionObserver` (`observe`/`unobserve`/`disconnect`/`takeRecords`,
thresholds + root geometry), `ResizeObserver`, `XMLHttpRequest`,
`Worker` (`postMessage`/`onmessage`/`onerror`/`terminate`), `localStorage`
and `sessionStorage` (`Storage` methods + Proxy property access),
`<canvas>` with `getContext('2d')` and the drawing surface above,
`ImageData`, `btoa`/`atob`. The whole page surface lives in one file:
`crates/kitae_core/src/pagejs.rs` (`BOOTSTRAP` is the reviewable shape,
the `__kitae*` Rust globals are its data plane).

## Runtime guarantees (non-negotiable, enforced in the crate)

- **Memory budget** — every context gets a hard memory limit
  (`RuntimeConfig.memory_bytes`); an allocation over budget aborts the
  script, never the host.
- **Time budget** — an interrupt deadline aborts runaway loops
  (`RuntimeConfig.deadline`); a script cannot hold the host thread.
- **No panic path** — script errors, syntax errors, and budget aborts are
  values (`ScriptError`); the runtime outlives every script.
- **Closed surface** — new bindings land through review against the
  inventory below, never as ad-hoc globals.

## API inventory (v0 — `kitae` global)

| Member | Purpose |
|---|---|
| `kitae.log(…)` | Host-side structured logging from scripts |

Growth follows the consumers (user scripts need navigation + read APIs
first; agents need selection + form APIs). Each addition: one row here or
in the crate's inventory, one review.

### The act surface (B1/B4) — inventory frozen at 15 verbs

`kitae_agent::Verb` is the whole list, and `Verb::inventory()` is the
machine-readable form the transports publish: `navigate`, `click`, `fill`,
`submit`, `back`, `read`, `links`, `find`, `extract`, `watch`, `dispatch`,
`screenshot`, `pdf`, `archive`, `verify`. Seven of them are read-only
(`read`, `links`, `find`, `extract`, `screenshot`, `pdf`, `verify`) and run
under the default policy; the other eight need a grant, an allow-list entry
or an owner's answer. Adding a sixteenth verb means a row here, a row in
`kitae_mcp`'s `TOOLS`, and a policy flag — which is exactly the friction
the "closed API" contract above is for.
