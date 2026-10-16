# Kitae Engine Core — Status Document

_What the engine core contains today, and exactly how much is left. Last refreshed: 2026-10-16._

> **2026-10-16 — tiers JS-4/JS-5/JS-6 land: the Track-A ladder's feature
> work is complete** (inventory and the recorded gaps in
> `SCRIPTING_DECISION.md`; the measured conformance spine in
> `WPT_SCORECARD.md`). **JS-4 observers:** `MutationObserver` (records
> journaled by the DOM wrappers — attributes, characterData, childList,
> with `subtree`/`attributeFilter`/old-value flags — delivered on the
> microtask queue with the engine tick as the safety net),
> `IntersectionObserver` (threshold crossings against the published
> display-list geometry and scroll state), `ResizeObserver` (content-box
> sizes from the same publish). **JS-5 glue:** async `XMLHttpRequest`
> over the same fetch pool `fetch` uses (custom headers/bodies take the
> pool's Raw path; sync XHR refused by design) and `Worker` as a
> documented same-thread emulation (pool-fetched script evaluated with
> `self`/`postMessage`/`addEventListener` bound to a port, messages
> `structuredClone`d, `terminate()` stops delivery — no real
> parallelism). **JS-6 truth:** spec-shaped `Storage` for
> `localStorage`/`sessionStorage` (methods + Proxy named properties;
> `localStorage` process-wide per origin and in-memory, `sessionStorage`
> scoped per `Browser` and surviving navigation inside it, opaque origins
> throw `SecurityError`) and a software canvas 2D context (paths with
> both fill rules, rects, arcs, curves, the affine transform set with
> save/restore/globalAlpha, `getImageData`/`putImageData`, `drawImage`
> from canvas/`ImageData`/`<img>`, `toDataURL()` → PNG encoded by
> `kitae_media`). Engine-side changes this tier pulled in: the UA sheet
> no longer hides `<canvas>` (it is a replaced element now), layout emits
> `\0canvas:{node}` sentinels whose registered raster the publish pass
> stamps on every relayout, and `ensure_web_raster` decodes PNG/JPEG/GIF/
> WebP `data:` URLs (which is what makes `toDataURL` output usable as an
> image source). Suite: kitae_core page-js 46/46 (271 total with the
> feature, 224 without), WPT page-JS subset re-measured at 1213/1349
> (89.9%, unchanged — the harness now skips visibly instead of failing
> when the `page-js` feature is off), and the full workspace sweep is
> green (55 suites, 1171 passed, 0 failed, 3 ignored). The ladder's one remaining exit — page-JS default-on for
> library builds — is an owner gate: the app already runs the tier on
> every tab and `crates/kitae_core/Cargo.toml` records the flip.

> **2026-10-15 — tier JS-3: the standard constructor set and read paths
> land** (Track A's first step under the agentic pivot; inventory amended
> in `SCRIPTING_DECISION.md`). `Event`/`CustomEvent`/`MouseEvent`/
> `KeyboardEvent` constructors + `dispatchEvent`; `AbortController`/
> `AbortSignal` with `once`/`passive`/`signal` listener options;
> `URL`/`URLSearchParams` (base-resolved through `kitae_net::resolve`,
> live `searchParams` linkage); `Headers`/`Request`/`Response` with
> standard fetch semantics (any HTTP status resolves; transport failure
> rejects); `FormData` — including `new FormData(form)` and multipart
> POST bodies riding the pool's uncached Raw path (a recorded-loopback
> test asserts the wire bytes); `structuredClone` (cycles, Date, Map,
> Set, typed arrays); `element.dataset`; `getComputedStyle` (a
> computed-subset serializer published per relayout, width/height
> answered as used px from painted geometry); and the scroll/selection/
> focus surface — `scrollTo`/`scrollBy`/`scrollY`/`pageYOffset`/
> `scrollIntoView`/`scrollTop`, `focus()`/`blur()` (queued through the
> poll, applied like any engine focus), `document.activeElement`,
> `getSelection().toString()` (engine selection text). Engine-side: the
> publish path now carries view state (scroll/active/selection) on every
> publish and the computed-style map on relayout; script focus reuses the
> same body as the handle-based `focus_node`. Suite: kitae_core page-js
> 28/28 (258 total with the feature, 224 without), workspace sweep 851/0.
> JS-3's recorded gaps (response headers = content-type/length only,
> fetch abort does not cancel the in-flight job, computed-subset rather
> than full CSSStyleDeclaration, document-only scrolling) are listed in
> `SCRIPTING_DECISION.md`.

> **2026-10-15 — owner pivot: the agentic QuickJS browser is the focus**
> (`AGENTIC_PIVOT.md`). The page-JS tier is promoted from opt-in add-on to
> the product's core compatibility program (tiers JS-3… follow: standard
> constructors, observers, workers, canvas, storage — each exiting on a
> WPT subset + site-panel score), with very low RAM per tab as the
> headline metric (CI-measured, not anecdotal) and the agent surface
> (`kitae_mcp`, headless server, in-app agent runtime) promoted to a
> first-class track. The engine/display-list architecture continues
> unchanged as the renderer; the first-vision phases (reading/app suite)
> continue but get no new feature starts while the compatibility and
> agentic tracks run. WASM remains a full "never" pending an explicit
> owner decision. Operative plan: `ROADMAP.md` §The current plan
> (Tracks A–E).

> **2026-10-15 — tier JS-2: the page-JS tier runs framework-era scripts**
> (`SCRIPTING_DECISION.md` amended; all four planned fronts landed in one
> pass, each test-guarded). Animation: `requestAnimationFrame`/
> `cancelAnimationFrame` drain from the idle tick with `performance.now()`
> timestamps, `queueMicrotask`, `requestIdleCallback`/`cancelIdleCallback`.
> Messaging: `MessageChannel`/`MessagePort` (ports wired pairwise; a post
> round-trips through the loop). Geometry: the relayout path publishes
> node boxes + viewport into the page (`set_geometry`), so
> `getBoundingClientRect`/`offsetWidth`/`offsetHeight`/`innerWidth`/
> `innerHeight` answer from the rendered truth (painted boxes, sub-tree
> union fallback; reads before first layout see zeros — documented gap).
> History: `pushState`/`replaceState` move the address bar without a
> reload (pending-history hand-off into the engine's nav loop),
> `back`/`forward`/`go` route through the engine's own history,
> `popstate` fires on the loaded document, `location.hash` get/set.
> Events: full propagation — capture root→target, both listener lists at
> the target, bubble target→root, into window/document-delegated
> listeners — with `stopPropagation`/`stopImmediatePropagation`/
> `currentTarget`. Form/key hooks: the engine now dispatches
> `keydown`/`input`/`keyup` on typing and `input`+`change` when a
> control's value commits (checkbox/radio toggle, range pointer or arrow
> step, select choice via `set_input_value`); `submit` reaches page
> listeners and `preventDefault()` keeps the engine from submitting.
> Element `value` follows select semantics (get = selected option, set =
> move the marker) and `checked` reads/writes checkedness. Two engine-side
> fixes fell out: click hit-testing now scores text runs as candidates
> (width via the engine measurer, line box from `metrics_family`), so a
> click on an anchor's *label* targets the anchor instead of the body;
> and the JS dispatch path converts display-list NodeIds through
> `token_for_node` (JS element tokens and DOM NodeIds are different
> spaces — both small colliding integers; the old cast only worked by
> accident). Suite: kitae_core page-js 252/0, workspace sweep 851/0,
> Kitae.app rebuilt.

> **2026-10-15 — tier JS-1: the page-JS tier grows real-web joints**
> (`SCRIPTING_DECISION.md` amended). Four upgrades, each test-guarded:
> ES modules land (`kitae_script::ModuleFetcher` + `ScriptRuntime::run_module`;
> quickjs-ng resolves static chains and dynamic `import()` through the
> fetcher; `page-js` collects `type="module"` blocks — inline and external —
> behind a per-page budget of 96 modules / 1 MiB each / 6 MiB total / 12 s
> wall clock; bare specifiers, import maps, and `data:` URLs reject
> cleanly); `style.prop =` writes land (Proxy-backed style object over
> the inline style attribute, camelCase→kebab, cssText, removeProperty);
> `preventDefault()` is honored (JS-built click events carry
> `defaultPrevented` back to the engine, which stands its default actions
> down — and fired listeners no longer own the click, so one delegated
> analytics handler cannot break every link); selectors reach the common
> grammar (combinators `>` `+` `~`, attribute operators `~=` `|= `^=`
> `$=` `*=`, `:first-child`/`:last-child`/`:nth-child(an+b)`/`:not`,
> comma lists, element-scoped `querySelector(All)` + `closest`). Suite:
> kitae_core page-js 244/0, workspace sweep green.

> **2026-10-14 (second revision) — page JavaScript ships: the `page-js`
> tier.** Owner decision, amending the scope again (`SCRIPTING_DECISION.md`
> Part 2): pages may execute their own scripts when the host enables it.
> `kitae_core` gains the cargo feature `page-js` (off by default; a build
> without it cannot run page scripts at all), `Browser::set_page_js`, FFI
> command 53 `KITAE_CMD_SET_PAGE_JS`, and `Engine::builder().page_js(true)`.
> `crates/kitae_core/src/pagejs.rs` implements the shim: page scripts run
> in document order after parse and before styling (inline + external via
> the engine cache/origin gate with byte caps); the DOM surface is a shim
> over the LIVE `kitae_dom` arena (tokens bridge JS objects to NodeIds, so
> `innerHTML`/`querySelector`/mutations are first-class tree edits that the
> ordinary restyle/relayout/republish path renders). Event loop: timers,
> settled `fetch()` promises (via the shared pool, origin-scoped), queued
> location navigations, and click dispatch (rect hit-test, topmost node)
> drain from the hosts' 2 ms idle tick. Budgets per page: 64 MiB heap,
> 8 MiB stack, 10 s interrupt deadline; errors are values; the runtime
> survives every script. Fallback intact: all-scripts-failed + text-thin
> body → the honest interstitial. v0 gaps documented in the decision
> inventory (no Proxy style props, no modules/workers/canvas, preventDefault
> not honored on engine defaults, rect-only hit-testing). Debugging lessons
> recorded: rquickjs evaluates STRICT by default (`EvalOptions.strict=false`
> for web semantics), and JS callbacks must run inside the `with_dom` TLS
> scope or their DOM ops silently no-op.

> **2026-10-14 (later) — QuickJS lands, scoped: `kitae_script`.** Owner
> decision recorded in `SCRIPTING_DECISION.md` (which also amends the
> not-do-entry below and README): QuickJS ships as the **closed-API
> automation runtime** for user/agent scripts and server-side `kitae_ui`
> — it never touches the page pipeline, and no web JavaScript is fetched
> or executed. The crate embeds QuickJS via `rquickjs` (quickjs-ng, MIT;
> licenses audit clean under the S-9 allowlist) with the guarantees as
> first-class tests: hard memory budget, wall-clock interrupt deadline
> (note rquickjs 0.9 semantics — the handler returning `true` *aborts*),
> errors-as-values (`ScriptError`: Syntax/Thrown/DeadlineExceeded/
> MemoryExceeded), a surviving-runtime contract, and a closed surface
> (`kitae.log` v0; no `fetch`/XHR/timers). API growth follows the
> inventory in the decision doc, one reviewed row at a time. 7 tests.

> **2026-10-14 — Phase 0 begins (roadmap `ROADMAP.md`), item 1 shipped:
> fetch off the engine thread.** A shared background fetch pool lives in
> `kitae_net::pool`: fixed worker threads, an unbounded job queue, and a
> claim-by-id completion store (`submit` / `take_results`) so several
> engines — and future non-engine consumers like feed refresh — share one
> pool without stealing each other's work. Jobs carry the page origin
> explicitly and run the *same* origin-scoped fetch internals as the
> engine thread, so the S-1 gate, cookies, HSTS and the response cache
> behave byte-identically; unclaimed completions cap out oldest-first so
> abandoned loads self-clean. `Browser` gains `set_background_fetch`:
> with it on, top-level **http(s)** loads push history + flag loading
> synchronously, submit to the pool, and return; `poll_fetches()` (driven
> from the hosts' 2 ms idle tick — FFI and the Rust facade now share the
> same tick shape) replays the exact post-fetch sequence and republishes.
> `about:`/`file:`/`data:` pages and POST submits stay inline; the
> displayed page (and its origin, for mid-flight link-click gating) stays
> live until the completion lands. Direct-API/test callers keep the
> synchronous load contract (mode defaults off). The new tick timing
> also exposed a latent probe/consume race in the FFI tests' shared
> `wait_for_frame_inner` helper (the frame mailbox holds only the latest
> frame, so a publish between the size probe and the consuming call
> breaks a `got == len` assert; worse, retrying naively discards the
> frame the consume just delivered and the URL-filtering loops hang) —
> the helper now returns whatever was fully delivered and reallocs only
> on a refused copy. Fixed.
> kitae_core lib 219 → 224
> tests and kitae_net 79 → 84; workspace totals settle at the sweep.
> Subresource fetches and POST submits remain engine-thread work; that is
> the next-layer question for the feed/mail phases, not this one.


> **Current checkpoint (2026-09-26):** serialized and default-parallel
> workspace sweeps pass 1149 tests (3 ignored), the app bundle is current, nine
> native contract tests pass, workspace check/formatting/dependency checks pass,
> and HSTS is persisted through an Application Support snapshot. All 74 A–I
> implementation rows in the parity census are shipped. Remaining work is
> manual GUI/AX/QTSQ validation, the explicitly unlisted conformance long
> tail, and external release work; none is claimed complete by this checkpoint.

> **2026-09-26 additions on top of that checkpoint:**
> - **Media surface:** APNG animation end to end (still raster + frame
>   sequence in the animation store), safe in-crate **BMP/ICO** decoders, the
>   `<video>` attribute set (`poster`, `muted`/`autoplay`, `loop`, `preload`,
>   `playsinline`, `crossorigin`) and `<video>/<audio><source>` candidate
>   selection (first decodable, `media`-gated, element `src` last). Wire v20.
> - **CSS/layout:** `visibility` (hidden/collapse/visible), background
>   position/size/repeat, multi-layer `background` with `origin`/`clip`,
>   `aspect-ratio` (incl. `auto <ratio>`), and **block-axis media features**
>   (`orientation`, `height` ranges, MQ4 syntax) resolving against the real
>   viewport box in `@media`, `@import`, `<style/link media>`, `<source media>`
>   and `<img media>`. `background-size: auto|cover|contain` now resolves
>   because the engine hands layout the artwork's intrinsic size.
> - **Print:** `KITAE_CMD_SET_MEDIA` (52) switches the cascade to print media
>   (`@media print` applies, `@media screen` does not, `prefers-*` answer as a
>   print device) and flags the published frame (`flags` bit2); the host lays
>   out at paper width, prints via `NSPrintOperation`, then restores screen
>   media. AppKit pagination still slices the one whole-document frame.
> - **Public API:** `crates/kitae_render_gpu/include/kitae_render_gpu.h` ships
>   the renderer C ABI (the bridging header now includes it instead of
>   re-declaring it), plus `kitae_frame_version()` and
>   `kitae_engine_frame_available()` so a host can size its buffer without
>   consuming a frame.
> - **Bugs found and fixed while closing gaps:** `@import url(x.css) (media)`
>   resolved the *media list* as part of the target URL; two QTSQ navigation
>   tests skipped the shared store lock (deterministic parallel-run failure);
>   and two stale FFI assertions (a link the fixture never had, and a
>   single-byte scan that hit float payload bytes).

> **2026-09-25 current-tree refresh:**
> - **A11 finished:** selection ranges support absolute/point anchors, Shift
>   extension, selected text, measured non-accumulating highlight rects, and
>   replacement-aware typing/deletes for form controls and plain-text
>   `contenteditable`. Marked-text composition update/commit/cancel is wired
>   through `NSTextInputClient` and FFI commands 43–51. Manual IME/selection
>   GUI checks remain open.
> - **B3 finished:** the v20-compatible scroll tail carries horizontal content
>   extent, `auto`/`scroll` mode, and parent identity. The host owns x/y
>   offsets, cumulative clipping/translation, wheel chaining, horizontal and
>   vertical scrollbar dragging, transform composition, and scroll-aware AX
>   frames; nested-scroll pages bypass GPU tiles. Transformed link hit-testing
>   remains a documented approximation. Manual GUI checks remain open.
> - **AX extended:** 44 stable roles (wire ids 0–43), associated labels,
>   values/min/max, checked/expanded/selected/disabled, heading level, and
>   live politeness now ride a sparse optional `AXS1` tail on frame wire v20.
>   Swift maps them to AppKit semantics/actions and live announcements; the
>   native contract pins legacy-frame fallback and truncation rejection. This
>   is not a full platform-conformance claim.
> - **QTSQ cache bounds verified:** the shared raster store is 64 entries /
>   192 MiB with stable IDs and newest pinning; QTSQ video decoder/source state
>   is 8 entries / 64 MiB; purge clears raster, video, media, and network
>   caches. The supported QTSQ interaction and Markdown typography GUI checks
>   remain pending.
> - **Conformance remeasured:** tokenizer 6256/6256 (100.0%), tree
>   construction 1072/1784 (60.1%), CSS hex 81/81, keyword 160/160, and An+B
>   128/128. These are five vendored subset measurements, not full-WPT
>   conformance.
> - **macOS 15:** `MACOSX_DEPLOYMENT_TARGET`, `LSMinimumSystemVersion`, and
>   the built arm64 executable's `minos` are all 15.0; native contracts target
>   `arm64-apple-macos15.0`. Clean-machine and distribution validation remain
>   external release gates.

> **2026-09-10 bucket I (performance & robustness) complete:** all six
> parity-I items shipped in one pass, engine-side only (no wire bump) —
> I1 style invalidation sets (kitae_style `RuleIndex`: selectors filed
> under rightmost-compound tag/class/id keys, `:is()`/`:where()`
> arguments contribute theirs, ancestor-key demand filtered by per-key
> ancestor counts; `candidates` returns a proven superset, the cascade's
> `matches` stays the exact decision) + the hover fast path
> (`uses_interaction_state` cached on the page — hover/active changes
> skip restyle+relayout entirely on sheets that key off no interaction
> pseudo); I2 incremental layout (kitae_dom per-node `rev` counters,
> kitae_layout `FragmentCache` keyed by node rev + containing width with
> ancestor-chain invalidation, kitae_core `layout_cached` wired into the
> engine relayout path); I3 image decode-on-demand + purgeable caches
> (byte budget on decoded RGBA rasters — oldest-first eviction, newest
> pinned; `purge_image_caches()`; pressure observability); I4 fuzzing
> (`fuzz_html`/`fuzz_css`/`fuzz_urls`/`fuzz_qtsq` never-panic + round-trip tests in
> the routine sweep, docs/FUZZING.md); I5 WPT conformance harnesses with
> recorded baselines (the then-current `docs/WPT_SCORECARD.md` measured
> tokenizer 6252/6256 = 99.9%, tree-construction 1038/1784 = 58.2%, CSS hex
> 81/81, keyword 159/160, An+B 128/128) plus gap-driven engine fixes: strict
> An+B
> microgrammar in kitae_css `parse_nth`, full CSS Color 3 named-color
> table, and template-content inertness in the tree builder (content
> inside an open `<template>` no longer leaks into head/body); I6
> security hardening (mixed-content blocking — `mixed_content_allowed`
> enforced in the fetch path, URL-parse fuzz parity, sandboxing reviewed
> with the resolution recorded under H8, docs/SECURITY_AUDIT.md).
> **At that checkpoint: 1007 tests green, full workspace sweep, no
> exclusions.**
> Binary: release `.a` rebuilt + Kitae.app relinked 2026-09-10 05:20 at
> that checkpoint.

> **2026-09-10 form-control rendering fidelity pass:** showcase pages
> rendered in Kitae showed crushed/wrapped button labels, hardcoded
> control chrome and ignored author padding/radius. Root cause: form
> controls had no special-case in `layout_physical_box`, so flex-item
> controls took the generic block path (labels wrap) and
> `layout_form_control` hardcoded insets/backgrounds. Fix (no wire
> bump): UA-sheet control defaults (`padding`/`border`/`white-space:
> nowrap` on button/input/select); new `control_label_and_type` +
> `control_size` helpers give chrome-inclusive intrinsic widths so flex
> bases match the painted widget; `layout_form_control` now paints the
> author's real padding/border widths, a `border-radius` rounded ring
> (outer border-color rect + inner bg rect, per-corner inner radius),
> label inset from real chrome, and the cascade-resolved background
> (`transparent` wins → ghost buttons). `font` shorthand parsed in
> kitae_style (`font: 18px/1.5 Family`, keywords, `font: inherit` =
> no-op by design). Verified visually in the rebuilt app against the
> showcase (components/todos/home/charts all clean). One test pin
> updated (text-input width 160 → 174, UA chrome included).
> **Baseline: 898 tests green, full workspace sweep, no exclusions.**
> Binary: release `.a` rebuilt + Kitae.app relinked 2026-09-10 14:27 at
> that checkpoint.

> **2026-09-10 bucket H (browser layer) complete:** all eight parity-H
> items shipped in one pass — H1 downloads UI (Swift downloads center +
> popover over the kitae_net registry), H2 file upload
> (`KITAE_CMD_SET_FILE_FILES` = 36; host panel → real multipart submit),
> H3 print to PDF (host `PagePrinter`, ⌘P), H4 AX parity (`aria-live` →
> new `LiveRegion` role, wire value 11 in the existing u8 role field —
> no bump; pressable roles press through the engine click path;
> live-region announcements; Tab/⇧Tab roving focus via
> `focusNext`/`focusPrev`), H5 read-only inspector (⌘-click pick →
> `KITAE_CMD_INSPECT_PICK`/`INSPECT_INFO` = 38/39 → tag/attributes/
> computed style in the panel), H6 autofill + passwords (engine
> Basic/Digest sign-in pages + login-form detection,
> `KITAE_CMD_SET_AUTH` = 37; host Keychain fill/save), H7 session
> restore (`SessionStore` session.json, restore on launch / save on
> terminate; Reopen Closed Tab ⌘⇧T), H8 per-tab crash isolation (FFI
> `catch_unwind` guards + per-tab recovery page — the reload-on-panic
> option). Wire stays v20.
> **Baseline: 925 tests green, full workspace sweep, no exclusions.**
> Binary: release `.a` rebuilt + Kitae.app relinked 2026-09-10 22:33 at
> that checkpoint.

> **2026-09-10 bucket G (network) complete:** all eight parity-G items
> shipped in one pass, entirely inside `kitae_net` (no wire bump) —
> G1 HTTP/2 (reqwest `http2`, h2c prior knowledge, `Response.protocol`),
> G2 HTTP/3 + QUIC via quinn+h3 on the **ring** provider (`fetch_h3`;
> endpoints must be constructed inside a tokio runtime — quinn's
> `runtime-tokio` feature is required even for manual `block_on`),
> G4 Basic/Digest auth + embedder credential provider, G5 range/resumable
> downloads with progress hooks (`.part`/`.meta` resume, cancel keeps the
> part), G6 RFC 6455 WebSockets (ws+wss, fragmentation, auto-Pong;
> handshake reader now returns frame data coalesced with the 101),
> G7 SSE (data/id/event/retry + Last-Event-ID reconnect), G8
> preconnect/prefetch + RFC 9218 priorities. kitae_net: 51 tests.
> **Baseline: 893 tests green, full workspace sweep, no exclusions.**
> zstd decompression stays deferred (C dep); the downloads UI is H1.

> **2026-09-09 bucket F (media) complete:** all five parity-F items shipped
> in one pass — F1 `<audio>` decode via symphonia → CAF handoff, F2/F3 MP4
> + WebM/MKV demux (pure Rust, `kitae_media::video`) with host-side
> VideoToolbox decode + `WebVideoDriver` clock per the approved Option 2
> (`MEDIA_CODECS_DECISION.md`), F4 in-page `<video controls>` chrome
> (engine paints + hit-tests), F5 WebVTT captions (parse + fetch + cue
> overlay). Wire v20 (per-item `controls` flag + 8 web-video C exports).
> **At that checkpoint: 859 tests green, full workspace sweep, no exclusions.**
> Binary: release `.a` rebuilt + Kitae.app relinked 2026-09-09 22:52.

> **2026-09-07/08 completion (historical checkpoint; §1–2 below now describe
> the current tree, and the live tally lives in
> [`BROWSER_PARITY_PLAN.md`](BROWSER_PARITY_PLAN.md) §0.5):** Track B
> (RP-B1..B8) shipped — transitions, `:has()`, `@container`, filters
> (wire v16), scroll containers (wire v17), grid completion, ch/ex, 3D
> audit; A11a/A11b form-control + `contenteditable` editing; the 2026-09-08
> finish pass closed A6/A7/A10/A12 (`::backdrop`+modal focus, range
> interaction, `pattern`+validation bubbles, 2D MathML `math_walk`); G3
> response decompression (brotli+deflate+gzip; zstd deferred — C dep).
> **2026-09-08 CSS pass:** parity bucket B was implemented except B3's then
> tail — B4 `writing-mode` + logical flow (rigid-rotation stamp, no wire bump),
> B5 multi-column, B7 `clip-path`/`mask` + B8 blend/`isolation` (wire v18),
> B9 ch/ex measurer tail (`with_metrics`), B11 unicode-range/font-display,
> B12 full text-decoration, B13 line-break properties + ellipsis,
> B14 counters + `list-style`, B15 long-tail at-rules/values. B3 had cumulative
> nested offsets/clipping and wheel chaining then; its remaining metadata and
> interaction semantics shipped 2026-09-25. At that checkpoint the baseline was
> **735 tests green** (`--exclude kitae_render_gpu`) and the release binary was
> rebuilt 2026-09-08 22:50.

> **The original second-tier engine roadmap, including the wgpu/Metal
> renderer, is preserved in [`ENGINE_CORE_ROADMAP.md`](ENGINE_CORE_ROADMAP.md)
> (Phases J–P). Current work outside the shipped A–I census is validation and
> the explicitly unlisted conformance/performance tail.**
>
> **QTSQ (Phase J) has shipped:** viewer pages for images/text/audio/video,
> real-pixel rendering, audio playback, Finder open — those historical
> surfaces were visually checked 2026-09-04. The legacy compatibility corpus
> reached 49/58 decodes; the remaining files retain the dispositions in
> `QTSQ_STATUS.md`. The text typography pass (Markdown viewer, `pre-wrap`,
> kerning) is code-complete but its current-tree GUI check remains pending;
> all four follow-on features — password prompt, Save-Page-as-QTSQ (GUI-verified
> end-to-end), tensor viz pages, ML-DSA-65 signature verify (frame wire
> v11) — shipped and tested 2026-09-04. Live status:
> **[`QTSQ_STATUS.md`](QTSQ_STATUS.md)**.

The engine core is Kitae's V8-equivalent: the embeddable, JS-free web engine for
the Rust ecosystem (as V8/QuickJS are for JS). It takes HTML + CSS in and emits a
display list out — that display list is the engine's contract with the world.
The Rust crates implement the engine. Vendored C/C++ is limited to existing
policy-bounded native components (currently the QTSQ decoder); platform media
decode is supplied by VideoToolbox and shaping by `rustybuzz`/Core Text.
There is **no JS engine and no WASM** — permanently out by design.

---

## 1. The pipeline (all wired end-to-end)

```
URL ──kitae_net──▶ Response(bytes, mime, cache)
        │
        ▼
   kitae_core::navigate → fetch_step/fetch_result (sync engine thread)
        │                  direct image navigations synthesize an <img> page
        ▼
   kitae_html::parse ──▶ (Dom, body NodeId)
        │                tree construction: implied tbody/tr wrappers,
        │                foster parenting, void elements, table context,
        │                pre whitespace, noscript interstitial handling
        ▼
   kitae_core::collect_styles → StyleSource list (inline style="" attrs,
        │                       <style> blocks, <link rel=stylesheet> URLs
        │                       fetched through kitae_net cache)
        ▼
   kitae_css::parse_stylesheet ──▶ Stylesheet (rules, at-rules, media)
        │
        ▼
   kitae_style::StyleEngine::compute(dom) ──▶ Vec<(NodeId, ComputedStyle)>
        │                                    cascade, inheritance, UA sheet,
        │                                    specificity, custom props, var()
        ▼
   kitae_layout::layout(dom, styles, width, &TextMeasurer) ──▶ DisplayList
        │   tables · grid · images · form controls · floats · flex · positioning
        ▼
     DisplayList v8 (page-space paint/transform/filter/clip/media +
                    sticky/scroll side-lists + content_height)
          │
          ├─▶ kitae_render (SDL shell renderer — dev viewer, rounded/gradient/shadow paint)
          ├─▶ kitae_render_gpu (wgpu/Metal offscreen renderer, CPU fallback parity tests)
          └─▶ kitae_core C FFI staticlib, frame wire v20 ──▶ Swift Kitae.app
              (paint + focus/selection + media/find + AX semantics +
               optional scroll/AX tails)
```

**Current workspace test status (2026-09-26): 1149 passed, 3 ignored** in
both serialized and default-parallel full sweeps. The original core census at
this section's writing was 297 tests; current per-crate headings below were
refreshed where a stable test count was available.

---

## 2. What is built, crate by crate

### kitae_dom — DOM arena (4 tests)
- `Dom` arena of `NodeId`s; `Node {kind: Element|Text, parent, children, text, attrs}`.
- Accessors: `node()`, `element()`, `children()`, `tag()`, `attr()`.
- Mutation for incremental editing and forms: `set_text`, `set_attr`,
  `remove_attr` (plus existing `append`, `insert_before`, `detach`).

### kitae_html — tokenizer + tree builder + streaming parse (57 tests)
- Full tokenizer + tree builder (`TreeBuilder::build`, `parse`, `parse_fragment`).
- WHATWG-flavoured construction: implied `<tbody>` when `<tr>` appears directly
  in `<table>`, implied `<tr>` wrappers, foster parenting of stray content,
  void-element handling, `<pre>` newline/whitespace rules, raw-text elements.
- Table context stack (`TableCtx`) driving the implied-element rules.
- **Full entity table (N3)**: the complete WHATWG named-entity set (2231
  entries, longest-first matching) — the `&#…;` numeric path predates it.
- **Streaming parse (O4)**: `Tokenizer::new_streaming/feed/finish` with
  `TagOutcome::Ready/Junk/NeedMore`, entity-boundary lookahead (32 chars),
  raw-text rewind; `TreeBuilder::feed/finish/drain_tokens` +
  `parse_streaming` — chunk-boundary whitespace holds one pending space so
  every chunking parses identically to batch (property-style test).
- `parse_fragment(html, context_tag)` for fragments.

### kitae_css — parser + at-rules (58 tests)
- `parse_stylesheet(css)` / `parse_stylesheet_viewport(css, viewport_width)` →
  `Stylesheet` of rules with selectors + declarations.
- Selector support: type, class, id, descendant, child `>`, sibling `+`/`~`,
  attribute selectors, pseudo-classes that are statically matchable, plus
  (N6) dynamic state pseudo-classes (`:hover`, `:active`, `:focus`,
  `:checked`, `:disabled`, `:nth-child(An+B)` via `parse_nth`).
- At-rules: `@media` (+ `eval_media_condition`), `@supports`
  (N7, `eval_supports_condition` with `not/and/or` over a supported-property
  table), `@font-face` (N5, `FontFace`/`FontSrc`), `@keyframes` (N8,
  `Keyframes`/`KeyframeBlock`), `@import` at the style-engine level.
- Declarations: `parse_declarations`, `parse_color` (hex/rgb/named),
  `parse_length`, `parse_number`; custom properties `--x` + `var()`
  resolution; shorthand expansion for the common box/font/border shorthands.

### kitae_style — cascade + ComputedStyle (114 tests)
- `ComputedStyle` covers the full layout surface (display/position/float/
  overflow/flex/grid/object-fit/box/font/color/text/white-space/z-index/
  top-right-bottom-left), plus (K–N additions): `Direction`, `VerticalAlign`
  (M3/M4), `BackgroundPaint` — `Color`/`LinearGradient`/`RadialGradient`/
  `Image` (N4), `BorderRadius` (L4), `font_faces()` collection (N5),
  `animations` (`Timing` + parsed `@keyframes` refs, N8), box `shadows`
  (L4).
- `StyleEngine::compute(dom)` cascades UA sheet + author sheets + inline
  styles by specificity, inherits inherited properties down the tree;
  `compute_with_state(dom, &InteractionState)` (N6) re-evaluates
  `:hover`/`:active`/`:focus`-carrying rules for the engine's
  hover/active/focus nodes.
- `sample_animation(anim, frames, t)` (N8): keyframe interpolation over
  transform/opacity/color/background for the embedder's present loop.

### kitae_net — POST, cookies, conditional requests (Phase O)
- `fetch(url) -> Response`, `fetch_cached`, `fetch_with(method, url, headers,
  body)` (Phase O1: POST bodies, extra headers; POST responses never cached),
  in-memory response cache (`cached_len`, `clear_cache`), `configure(FetchConfig)`
  — the config now gates the cookie jar (`cookies: bool`).
- Cookie jar (Phase O2, in-memory, dies with the process): `Set-Cookie`
  parsing with name/value, Path, Domain (suffix-scope + forgery rejection),
  Secure (never over http), HttpOnly, Max-Age (0 deletes; `Expires` alone
  stays session-scoped — documented); matching cookies sent on requests;
  `cookie_len`/`clear_cookies` for inspection.
- Conditional requests (Phase O3): cache entries store `ETag`/`Last-Modified`;
  stale-but-validated entries revalidate with `If-None-Match`/
  `If-Modified-Since` — a 304 refreshes the timestamp and serves the cached
  body without re-downloading (loopback-server test proves the wire traffic).
- URL handling: `normalize_input`, `resolve`, content-type gating, gzip,
  redirects, error taxonomy; loopback test server for POST/304 tests.
- **Security hardening 2026-09-06 (SECURITY_AUDIT.md S-1/M-2):** the
  local-origin gate — `local_origin_allowed(page_url)` + `fetch_for_origin`
  refuse `file://` requests coming from a network page (`ForbiddenLocalOrigin`
  error, rendered as a clear error page); `set_page_origin` records the
  current page so subresources, links, `@import` and `<img>` all pass through
  the same gate. The cookie jar now enforces caps (S-2/M-2): 50 per domain,
  3000 total, 4 KiB per cookie value, evicting expired-then-LRU — a hostile
  `Set-Cookie` flood cannot grow engine memory without bound. 16 tests green.

### kitae_core — orchestrator + interaction + find + a11y (174 tests, frame wire v20)
- The embedder's orchestrator (this is what the Swift app drives).
- **C FFI staticlib, frame wire v20**: one engine thread per tab;
  `commands in / frames out`. Commands 1–58 cover navigation, interaction,
  find, media, password/environment/privacy, HSTS confirmation, editing,
  selection/composition, file upload, auth, inspection, find, media type,
  page-JS, and the agent plane (54–58, below). Frames carry
  paint items + transforms/filters/clips + focus/selection + media + find +
  sticky/scroll side-lists + the AX tree and sparse optional `AXS1` semantics
  tail.
- Forms: `submit_form` honors GET, urlencoded POST, and host-supplied
  `multipart/form-data` file parts; POST responses never enter the cache.
- Find-in-page (Phase P1): `find_in_page(query) -> Vec<FindMatch>` —
  case-folded substring match over display-list text runs, adjacent runs on
  one baseline merge so matches span word/style boundaries; `set_find_query`
  / `find_next` / `find_prev` / `clear_find` with republish (no relayout);
  the query survives navigation and re-resolves per layout; capped at 1000.
- Accessibility tree: document-rooted `AxNode` tree with 44 stable roles,
  associated labels, value/min/max, checked/expanded/selected/disabled,
  heading level, and live politeness. Bounds come from the display list
  (identity or word claim; zero rect remains unresolved), `ax_focused` follows
  engine focus, and the current tree is published per frame.
- Interaction contract: focus cycling, form values/submission, checkbox/radio
  activation, page and editable selection, caret movement, visual selection,
  marked-text composition, publish-without-relayout where safe, and hover/
  active restyle through `set_hover`/`set_active`.
- Declarative animations (N8): `sample_animations(t)` for embedder-side
  interpolation.
- **Security hardening 2026-09-06 (SECURITY_AUDIT.md):** dependency policy
  (S-9) — `deny.toml` (permissive allowlist + rustsec advisory DB, three
  justified ignores for unmaintained shaping/test crates) and
  `tools/deps-audit.sh` as the one-command release gate; also fixed four
  crates missing `license.workspace = true` (`kitae_dom`, `kitae_html`,
  `kitae_net`, `kitae_qtsq`) — both `cargo deny` license + advisory checks
  pass clean. S-2/S-5/S-7 landed alongside: `kitae:open-external:` links
  are restricted to http/https targets (`open_external_permitted` — arbitrary
  schemes can no longer reach `NSWorkspace` from a web page); the engine-thread
  command loop, `parse_command` and every `extern "C"` entry point now run
  under `catch_unwind`, so a panic surfaces as a `crash\t` notification + error
  frame and the thread keeps serving instead of aborting the process; the
  QTSQ unlock password is held in a `zeroize::Zeroizing<String>` and overwritten
  on drop (no plaintext credential idling in freed heap). Navigation history is
  capped at `HISTORY_MAX = 500` entries, evicting the oldest from the front
  (M-4, SECURITY_AUDIT.md) — back/forward semantics are position-relative, so
  the cap is invisible; `history_caps_at_max_evicting_oldest` proves it.

#### The agent plane inside the engine (B4, 2026-10-16)

- `kitae_core::agent::SessionState` holds everything a session carries
  *besides* a browser — policy, rasterizer, limits, journal, grants,
  transition clock — and every verb is a method on it taking `&mut Browser`.
  `AgentSession` is just those two fields bundled, which is what lets the
  app run the verbs over the browser painting its window instead of a
  second, invisible one. One implementation, four transports
  (`kitae_mcp`, `kitae_server`, the app's FFI, and the Track-A script host
  through `SharedSession`).
- **Permissions**: the installed policy is consulted first; the session's
  default is a read-only `AllowList`; a *grant* (what the app records when
  the user answers a prompt) outranks the policy for that verb, and nothing
  but a host that asked can record one. Refusals carry a fixed prefix
  (`PERMISSION_PREFIX`) so a host tells "ask the user" from "the verb
  failed" without parsing prose.
- **FFI commands 54–58**: `AGENT_ACT` (verb JSON in, reply out),
  `AGENT_JOURNAL`, `AGENT_GRANT`, `AGENT_REVOKE_GRANTS`, `AGENT_DECLARE`.
  Answers arrive on the notifications queue as `agent-result` /
  `agent-journal` / `agent-policy` / `agent-raster`; the reply JSON is
  `{ok, verb, result, journal}` or `{ok:false, verb, kind, message,
  pending}`. `kitae_engine_notification_available` exists because
  `take_notification` cannot put an oversized message back and a
  screenshot's base64 is large.
- **Rasterizers are injected, not compiled in.** The pure-Rust rasterizer
  paints with `kitae_render_gpu::cpu`, which depends on `kitae_core` — so
  the engine cannot call it without a cycle. `PageRasterVtable` +
  `HostRaster` are the seam: a host hands over a context and three C
  function pointers (`kitae_engine_set_raster`), and `kitae_headless`
  exposes one call — `kitae_headless_install_raster(engine, scale)` — that
  the app makes at startup. The same shape the app already uses for Core
  Text measurement.
- Behaviour is pinned by tests at three levels: `kitae_core`'s agent tests
  (verbs over a real page), two FFI tests driving a live engine thread
  (permission flow end to end; a host rasterizer reading the engine's own
  browser), and `make -C app ffi-smoke`, which does the app's exact C calls
  (install raster → `read` → `navigate` prompts → `screenshot` returns a
  real PNG).

### kitae_layout — the heart (232 tests)
`layout(dom, styles, width, tm) -> DisplayList`.

- **DisplayList contract v8** (`DISPLAY_LIST_VERSION = 8`, `#[non_exhaustive]`):
  page-space coordinates; `shadows` → `Rect`s → `DisplayImage`s → `TextRun`s →
  `LinkArea`s (+ `DisplayMedia` video items); `content_height`;
  `page_background`. `Rect` carries `radius` (per-corner) + `paint`
  (`RectPaint`: Solid / LinearGradient / RadialGradient / Image) — L4.
  `DisplayImage` carries `payload: Option<u64>` (K3 raster reference).
  `TextMeasurer` decouples layout from real fonts.
- **Block flow**: margins/padding/width/centering, min/max clamps, explicit
  height, `overflow: hidden` clip, background spliced under the subtree.
- **Inline runs**: anonymous block composition, word-wrapping, line boxes
  with ascent/descent/line-height, `text-align` offsets, `<br>`, `<pre>`,
  `display:none` pruning, link tracking.
- **Positioning (E1)**: relative/absolute/fixed, z-order via
  captured-buffer merge, top/right/bottom/left.
- **Flexbox (E2)**: single-line flex runs, two-pass measure→paint, grow/
  shrink/gap, justify-content, align-items, row-reverse, auto-height columns.
- **Floats (E3) + clear (N1)**: `FloatBox` registry, band-shaped lines,
  stacking, containment; `clear: left/right/both` consults the registry and
  pushes the flow cursor below matching floats (`Clear` enum + `clear_y`).
- **Tables (E4 + N2 + C5)**: row groups, rows, cells, captions, and occupancy-
  based rowspans; borders honor `border-spacing` and `border-collapse`, and
  caption sides are painted below/above as resolved.
- **Grid (E5 + C6)**: fixed/flex/auto/intrinsic track sizing, `repeat`,
  `minmax`, named lines/areas, sparse and dense auto-placement, gaps, spans,
  and v1 subgrid; column-flow/row-subgrid values are carried but masonry and
  full alternate-flow placement remain documented deviations.
- **Images (F + K)**: attr/CSS sizing with aspect preservation, `object-fit`;
  `image_dimensions` delegates to `kitae_media::dimensions` (real header
  decode); intrinsic size defaults 300×150.
- **Form controls (H)**: button/input/select/textarea boxes, labels from
  value/placeholder/options, `[x]`/`[ ]` check markers.
- **Sticky/scroll metadata (Q5/B2/B3):** painted-item ranges for sticky boxes
  and viewport/content extents, overflow mode, and nearest parent for nested
  scroll containers; the host owns offsets and interaction.
- **Incremental (G)**: `DirtySet`, `dirty_chain`, `layout_subtree` splicing.
- **Bidi + vertical-align (M3/M4)**: `measure_run` shaping seam,
  `PendingWord.dy/rtl`, `Line.rtl`, UBA visual-run reorder at `flush_line`;
  `direction` and `vertical-align` style properties flow end-to-end.

### kitae_media — real image decode (Phase K) + web media (parity F, 2026-09-09)
- `decode_image(bytes)` → RGBA8 (PNG via `png`; JPEG via `zune-jpeg`; GIF via
  `gif`; WebP via `image-webp`; static SVG via `resvg`), with dimension
  fallbacks, CMYK JPEG header fallback, `object_fit_rect`, format sniffing,
  and decode-size guards.
- **Web media (parity bucket F):** `audio::decode_audio` — symphonia
  probe/decode of every supported container (mp3/wav/flac/ogg-vorbis/m4a)
  → PCM + duration; `video` — MP4 (stts/stss/stsz/stsc/stco/co64, avcC/
  hvcC/vpcC/av1C extradata) + WebM/MKV (EBML SimpleBlock + lacing,
  CodecPrivate) demux → compressed samples keyed by frame; `vtt::parse_vtt`
  — cues with timestamps/voice tags for `<track>` captions.

### kitae_shape — shaping, fonts, bidi (Phase M, 24 tests)
- `Shaper` seam: `FallbackShaper` (per-char math, tests) + `RustybuzzShaper`
  (`rustybuzz` — pure-Rust HarfBuzz port) behind one trait; `WidthCache` so
  repeated measurement of unchanged runs stays cheap.
- `font`: CoreText family enumeration, `Script` classification + per-script
  fallback chains (Arabic → Geeza Pro, Devanagari → Devanagari MT, CJK →
  Hiragino, emoji → Apple Color Emoji), web-font registry for `@font-face`
  (N5) with `CTFontManagerRegisterFontsForURL` registration.
- `bidi::visual_runs`: UBA reordering via the `unicode-bidi` crate.

### kitae_render_gpu — wgpu/Metal renderer (Phase L, 38 tests)
- `GpuScene` built from a `DisplayList` (or a serialized frame): instanced
  rounded-rect SDF quad pipeline, linear/radial gradient stops, image
  textures keyed by raster ids (K3 cache), box-shadow quads, page clear.
- `GlyphAtlas` (256px pages, r8, LRU) for text; `cpu::render` software path
  (`hash_pixels`) as the permanent fallback + parity gate
  (`gpu_matches_cpu_reference_where_available`).
- `backend::GpuContext`: wgpu offscreen surface + readback; `ffi::kru_gpu_*`
  C ABI (tile render with scale/ox/oy) for the Swift present loop.

### kitae_render — SDL dev viewer
- Renders a `DisplayList` (rects incl. rounded/gradient paint + shadows,
  image placeholders/borders, texts, links) into an SDL window;
  `FfiMeasurer` bridges real font metrics back into layout. Developer
  shell — the Swift Kitae.app is the product face.

### Swift Kitae.app (browser layer implemented; current GUI gate open)
- All native: tabs, ⌘K palette, share sheet, Settings (appearance + layout
  preview + reduce-motion + **GPU render toggle**), both layouts, floating
  content card, QTSQ audio/video players. Talks to the engine purely through
  the C FFI — **frame wire v20 decoder** (paint/transform/filter/clip items,
  images with raster payloads, media controls, find matches, sticky/scroll
  metadata, AX tree plus optional `AXS1` semantics), raster pull via
  `kitae_qtsq_raster`, `MetalCanvasView`
  GPU present loop with CPU fallback, find highlight overlay, and
  NSAccessibility bridging of the engine's AX tree.
- **Security hardening 2026-09-06 (SECURITY_AUDIT.md S-6/M-1):** the
  Lottie chrome icons no longer set the `allowFileAccessFromFileURLs` /
  `allowUniversalAccessFromFileURLs` private WebKit preferences (S-6), and
  both Lottie views were rebuilt on a weak inner `ReadyHandler` pattern —
  `WKUserContentController` strongly retains its handlers, so conforming the
  view itself was a retain cycle (viewport → webview → controller → view) and
  the entire WebContent process leaked for the app lifetime (M-1). `deinit`
  added to both views. The external-open deck (`kitae:open-external:`) is
  additionally validated to http/https at the app boundary (S-2). The per-tab
  frame buffer now shrinks back to ~1 frame after a heavy page passes
  (M-3, SECURITY_AUDIT.md) — no more retaining MBs of largest-frame-ever in
  every tab's chrome memory. Power (P-1/P-2): the cloud backdrop caps at
  30 fps and idles when the window isn't key; the poll timer backs off to
  10 Hz after ~1 s of quiescence. P-3: the two WKWebView-based Lottie icons
  are replaced by native `CAShapeLayer` views (`SecurityIconView.swift`,
  `NativeReloadButton.swift`) — no WebKit, no JS engine, no WebContent
  process; `Built Kitae.app`.
  30 fps and pauses entirely while the window isn't key (1 Hz heartbeat;
  NSView has no key callback); the tab poll timer self-adapts — 60 Hz while
  any engine is busy (frame or loading), 10 Hz after ~1 s idle. App rebuilds
  clean.

---

## 3. How much is left — engine-core roadmap

Every planned slice D–P is executed and green:

| Slice | Scope | Status |
|---|---|---|
| D | Selector completeness, at-rules, custom props, expanded ComputedStyle, `<style>`/`<link>` collection into StyleEngine | ✅ done |
| E1 | Positioning rel/abs/fixed, z-order, overflow clip, min/max, height | ✅ done |
| E2 | Flexbox engine | ✅ done |
| E3 | Floats (band-shaped lines, stacking, containment) | ✅ done |
| E4 | Tables: row groups, rows, cells, caption (`layout_table_children`, colspan, caption-above) | ✅ done |
| E5 | Grid: fixed/flex/auto/intrinsic track sizing, `repeat`/`minmax`, named lines/areas, sparse/dense placement, spans, v1 subgrid | ✅ done |
| F | Images in the pipeline: header decode, `object-fit`, real `DisplayImage` items, direct-image pages, SDL/Swift placeholders, FFI v2 | ✅ done |
| G | Incremental layout: `DirtySet` invalidation, `dirty_chain`, `layout_subtree` relayout, repaint counter + clean-resize skip | ✅ done |
| H | Interaction contract: roving focus, form values/GET/urlencoded/multipart submit, checkbox/radio activation, caret/selection/composition, focus/selection frames | ✅ done |
| I | Full `cargo test --workspace` green + summary | ✅ done |
| J | QTSQ native format (own plan/status) | ✅ done |
| K | Real image decode: PNG/JPEG/GIF/WebP/SVG → RGBA, decoded-raster cache + payload ids (FFI v4), object-fit crop math | ✅ done |
| L | wgpu/Metal renderer (`kitae_render_gpu`): quad pipeline, glyph atlas, image textures, gradients/radius/shadows, Swift MetalCanvasView + Settings toggle, CPU parity gate | ✅ done |
| M | Shaping + bidi: `kitae_shape` (rustybuzz, width cache, CoreText enumeration, per-script fallback), `unicode-bidi` visual runs, `direction`, `vertical-align` | ✅ done |
| N | CSS/layout completeness: clear, rowspan, full entity table, gradient paint model, `@font-face`, dynamic pseudo-classes + `:nth-child`, `@supports`, `@keyframes` + animation sampling | ✅ done |
| O | Network hardening: POST (`fetch_with` + form POST), cookie jar, conditional requests (ETag/304), streaming parse (tokenizer feed/finish, batch-parity test) | ✅ done |
| P | Find-in-page + accessibility tree, current FFI v20 + Swift overlay/highlight + NSAccessibility bridge | ✅ done |

**E4–H specifics (all landed):** table column alignment across rows with
measured x-offsets, td 3/8 padding honored, caption above the grid, th bold,
row-group flattening, tallest-cell row sizing; grid equal/fixed+flex tracks,
gaps, wrapping, column spans; image attr/CSS sizing with aspect preservation,
shrink-to-fit, `object-fit: cover` carried, linked-image hit areas, PNG/GIF/
JPEG dimension decode; dirty-chain leaf→root, subtree node-count savings,
mutation-reflecting subtree relayout; focus cycling by id/tag, input/textarea/
select round-trips, typing appends, checkbox toggle + form-data collection,
GET submit URL building, select-all/clear with publish-without-relayout.

**Remaining effort:** none on the original K–P plan — every phase of
`ENGINE_FEATURES_PLAN.md` is implemented and green (see the table above).
Current follow-up is validation and the documented deviations, not another
K–P implementation row. Multipart file upload is now implemented; ICC/HDR,
`Expires`-date cookie parsing (session fallback), and unresolved zero-rect AX
cases remain documented limitations.

### HTML/CSS completion (R-slices, 2026-09-05 — `ENGINE_HTML_CSS_COMPLETION_PLAN.md`)
- **R1 borders + outline**: per-side width/style/color + all shorthands;
  solid exact, dashed/dotted as segment runs, 3D styles paint solid;
  borders inset the content box (background stays at the border-box edge);
  invisible sides resolve to width 0 (the initial `medium` must not eat
  layout space); outline rings paint `outline-offset` outside the border
  edge and cost no flow height. No wire changes (rects only).
- **R2 text-decoration**: `overline` (ascent top) + `line-through` (≈0.3em
  above baseline) join `underline`.
- **R3 ::before/::after**: parser keeps `::before`/`::after` (and legacy
  single-colon) as trailing selector markers routed to a pseudo cascade that
  inherits from the element's own computed style; `content` supports quoted
  strings with CSS escapes and `attr(name)`; generated text feeds the
  element's own line flow (block containers and inline elements; flex/grid/
  table children skip it — documented). ::after may start a fresh line when
  the children's trailing flush fired first (documented approximation).
- **R4 @import**: recursive expansion at collect time (`expand_imports`) —
  relative targets resolve against the page base (inline blocks) or the
  importing sheet's URL; cycle guard via a seen-list, depth cap 8, sheet
  cap 32; media-qualified imports expand only for absent/`all`/`screen`.
- **R5 viewport units**: `vw`/`vh`/`vmin`/`vmax` parse in kitae_css and fold
  at style time (`parse_len_px_v`) against the engine's viewport (Browser
  passes its real size; headless prepare uses 600 height, documented).
  Without a viewport they reject rather than fold to zero.
- **R6 inline <svg>**: kitae_dom gains `outer_html` (XML-safe serializer,
  self-closing childless elements, xmlns injection); the HTML tree builder
  now honors the `/` self-closing flag inside svg/math foreign content;
  layout treats `<svg>` as a replaced element (sized like `<img>`: CSS
  width/height → attributes → 300×150) whose subtree serializes to a base64
  data URL; the engine decodes data URLs through resvg into the shared
  raster cache (`ensure_svg_raster` + `b64_decode`) and stamps the payload.
- **R7 @layer**: statement + block forms, nested paths join with dots,
  anonymous layers get generated names; cascade sort key gains a layer rank
  (unlayered wins normally; `!important` inverts — layered beats
  unlayered).
- **R8 text-shadow**: inherited; painted as duplicate runs behind the text
  (last shadow deepest). The initially deferred blur shipped through sparse
  wire-v19 per-run blur and CPU/GPU/host painters.
- **Historical 2026-09-05 deferrals:** full adoption-agency recovery,
  `:has()`, container queries, scrollable overflow, `writing-mode`,
  multi-column, filters/clip-path, H.264 policy, `ch`/`ex`, and transitions
  subsequently shipped through the A–I parity buckets. AVIF and canvas remain
  outside the shipped media/rendering scope noted in the parity plan.
- **Next roadmap (historical 2026-09-05 wording):** the gap to V8/Chrome/
  Chromium was itemized in `BROWSER_PARITY_PLAN.md`. The original "75" was
  corrected to 74 by census; the live 2026-09-25 status is 74/74 implementation
  rows shipped, with manual/release gates still open.
  **Bucket A batch 1 SHIPPED 2026-09-06** (`DOM_COMPLETION_PLAN.md`):
  A2 template inertness (styles/images inside never activate),
  A3 picture/srcset (type/media/w+xD selection at image resolve, `sizes`
  deferred), A4 colgroup/col column pinning (CSS width or attribute,
  span-aware), A5 details/summary toggle (UA `+`/`-` marker flips with
  `[open]`; closed details render only the summary; click toggles via the
  engine-internal `DisplayList.toggles` hit-areas, no wire change), A6
  dialog[open] (fixed centered card through the UA sheet alone —
  `left:50%` + `translateX(-50%)`; `::backdrop`/modal focus deferred), A8
  progress/meter widgets (pill track + fill, meter low/high zones,
  indeterminate half-fill), A9 output inline + datalist hidden. Batch also
  fixed `parse_attr`'s glued fn line, verified negative-% length parsing,
  and repaired the kitae_ui splice (form controls now re-stamp v7
  `cur_node`; `splice_try` derives the splice root's box as a rect-union
  fallback when the root itself paints nothing). 442 tests green
  workspace-wide; app rebuilt. Remaining in bucket A: A1/A7/A10/A11/A12
  (all L-size, follow-up batch).
  **Bucket A batch 2 SHIPPED 2026-09-06**: A7 form-control widget chrome
  (range slider track/fill/knob step-quantized from min/max/value; color
  value swatch; file "Choose file" pill or basename; datetime values shown
  verbatim; type-aware default sizes), A10 constraint validation
  (`kitae_style::control_invalid` computes validity live — required+empty +
  per-type email/url/number/range/date/time/datetime-local/color, buttons
  and hidden/file/disabled never invalid, `pattern` deferred; `:invalid`
  matches through the full cascade, `:valid` = `:not(:invalid)`; UA
  `input:invalid {border:1px solid #d93025}` reaches the control chrome
  read; `submit_form` gates on `form_failures` — blocked submit focuses the
  first failure, relayouts, returns None, and the same predicate drives
  both UI and gate), A12 MathML v1 linearized (UA sheet alone: `math`
  inline-block, tokens inline, `mi` italic, `mo` 3px side margins; 2D
  layout deferred). Batch note: `parse_color` packs alpha in the LOW byte
  (RGBA); forcing opacity is `| 0xFF` — the original `| 0xFF000000` chrome
  read corrupted the red channel while the UA rule was applying all along.
  502 tests green workspace-wide; app rebuilt. Remaining in bucket A:
  A1 (full-spec tree construction) + A11 (editing) — both L-size.

### Later add-ons (planned, outside the current goal)
- **Kitae Bridge** browser-layer feature (`BRIDGE_FEATURE.md`).
- **kitae_ui** pure-Rust React-like UI library (`UI_LIBRARY_PLAN.md`) —
  **ALL 4 PHASES SHIPPED 2026-09-05/06** (plan doc carries the full
  record):
  - Phase 0: `kitae_ui` + `kitae_ui_macros` — signals, effects, `view!`,
    keyed/positional diff → v1 PatchOp streams.
  - Phase 1: `ui::LocalApp` mounts the start page as components; clicks
    route into `on:click`; incremental splice publish (repaint-counter
    proven).
  - Phase 2: `kitae_live` server crate (KTLV wire v1 over hand-rolled RFC
    6455; sessions survive disconnects, resume by session id, static
    fallback pages stamped `<meta name="kitae-live">`) + engine Live
    client behind the `live` cargo feature (`live::LiveApp` non-blocking
    connect→open→dead→resume state machine; hydration swaps the static
    body for the ops-built ref tree; clicks/inputs become Event frames;
    patches splice or fall back; backoff reconnect; FFI commands 27
    `LIVE_POLL` / 28 `LIVE_STATE`). Acceptance tests: Counter + Todos
    (two-way input binding) over a real WebSocket, forced disconnect
    mid-interaction resuming the SAME session, socket-disabled static
    render.
    **Security hardening 2026-09-06 (SECURITY_AUDIT.md S-3/S-4):** resume
    session tokens now come from the OS RNG (`getrandom`, no more SHA-1 of
    clock+counter) and both the HTTP and WebSocket-upgrade paths validate
    `Origin` against loopback — a website in another browser can no longer
    drive a local dev session (localhost-CSRF). 34 tests green.
  - Phase 3: the `kitae` CLI (`kitae_cli` crate) — `kitae new` (5
    templates), `kitae dev` (src watch → restart; content watch →
    `World::refresh_all()` pushes fresh state to connected pages in
    milliseconds), `kitae build` (`kitae_live::write_static` static
    export); tutorial site as a kitae_live example.
  - Phase 4: `use_resource` (background-thread fetch → channel → drain on
    the UI thread → `World::broadcast_updates()` server push), form
    validation story, gallery example. `<a>`/history routing and a11y
    come free from the engine (link navigation, AX tree on every
    publish).
  - **2026-09-06 — honest parity assessment + roadmap:** Phases 0–4 ship
    a complete, proven core, NOT React parity. The plan doc gained a
    "React parity" section: the gap analysis and the RP-A/B/C slice list
    (framework primitives — refs/context/children/error boundaries/
    router; engine gaps verified against the tree — transitions, overflow
    panes, :has()/container queries, filters, codec decision, text
    completion; ecosystem — component kit/devtools/testing) with sizing,
    ordering, and a concrete definition of done. Note: Grid, gradients,
    box-shadow, @keyframes, and fixed/sticky were verified PRESENT in the
    engine on 2026-09-06 (an earlier conversational assessment overstated
    those gaps; the doc reflects the verified tree).
    **UI_LIBRARY_PLAN COMPLETE 2026-09-08 — ALL slices shipped:** Track A
    RP-A1–A9 (incl. follow-ups), Track B RP-B1–B8, Track C RP-C1 kit,
    C2 charts, C3 DevTools, C4 testing, C5 hot reload (routes-dylib
    hot-swap: two-file scaffold src/lib.rs pages + src/main.rs host,
    prefer-dynamic shared dep images, DylibWatch in kitae_live, `kitae
    dev` rebuilds lib.rs edits without restarting — clients stay
    connected), C6 docs/cookbook, C7 publish prep complete
    (RELEASE_CHECKLIST.md; the `cargo publish` itself is user-gated).
    736 tests workspace-wide (kitae_render_gpu excluded, user WIP).
    Remaining open items are the plan's own deferred/known-limits notes
    plus the publish/tag user gate.
- **Web Essentials** gap-closing plan (`ENGINE_WEB_ESSENTIALS_PLAN.md`) —
  **COMPLETE 2026-09-05**, all slices landed and green (404 tests
  workspace-wide, zero failures):
  - **Q3** letter-spacing + text-transform; **Q1** calc/min/max/clamp;
    **Q6** prefers-color-scheme/reduced-motion/contrast (environment wired to
    the app settings); **Q7** :is/:where + of-type; **Q13** UAX #14 CJK line
    breaking; **Q2** CSS transforms (parse → layout bake → wire v12 → Swift/GPU
    warp) — earlier sessions, visually verified.
  - **Q8** GIF still decode + frame animation (raster frame side-table, host
    display-link stepping); **Q9** WebP static decode; **Q10** SVG static
    decode via resvg — earlier sessions, visually verified.
  - **Q11** web fonts end-to-end 2026-09-05: kitae_media woff/woff2 decoders
    + registration (33 tests); family-aware text contract — `ComputedStyle.
    font_family` (first non-generic family, inherited) → `TextRun.family` →
    family-aware `TextMeasurer` default methods (old shells compile
    unchanged) → C vtable `family` params → wire **v14** per-run family →
    Swift `CoreTextMeasurer` FontKey.family (CTFontCreateWithName;
    registered `@font-face` faces resolve) → PageCanvasView paints the same
    family. Visual verify: probe page's web-font `IIIIIIII` renders in the
    registered face (wide slab glyphs) vs the narrow Helvetica control.
  - **Q4** flex-wrap 2026-09-05: `FlexWrap`/`AlignContent` in kitae_style;
    `layout_flex_rows_wrapped` — line breaking by hypothetical main size,
    per-line grow/shrink, measure at final flexed size, per-line cross
    sizing, align-content (stretch/center/flex-end/space-between) with a
    definite container height, wrap-reverse line mirroring; column wrap
    stays single-line (documented). 5 new tests.
  - **Q5** position: sticky 2026-09-05: own `Position::Sticky`; layout
    paints in flow and records a `StickyItem` side-list on the DisplayList
    (painted-item ranges + base_y/height + nearest-scroll-ancestor bounds +
    top/bottom offsets); wire **v15** sticky section; `StickyItem::shift`
    pins top-sticky at `top_offset` below the viewport top (bottom-sticky at
    the viewport bottom), clamped inside the container. Deviation from the
    plan's "engine applies at publish time": the app scrolls its view
    locally and no frames publish on scroll, so **the host applies the shift
    at paint time** (PageCanvasView per-item translate) and the engine
    applies the same formula to link hit-testing in `Browser::click` — zero
    extra layouts while scrolling, as intended. Clip-path interaction:
    sticky ranges survive overflow clipping via survivor remapping in
    `clip_buffers`; ancestor background/shadow splices bump recorded ranges.
    Layout/core unit tests cover both clamp ends, bottom-sticky pin, range
     integrity and the wire round-trip. **GUI scroll-verification remains a
     manual gate** for the current host.
  - **Q14** iframe placeholder 2026-09-05: `iframe { display: block }` in
    the base sheet; `layout_static_box` renders a 150px bordered placeholder
    labeled "embedded frame — <src host>" and never descends (raw-text
    fallback content no longer leaks); nested browsing contexts remain out
    of scope by design.
- Historical GPU follow-ups from the 2026-09-05 checkpoint were subsequently
  addressed: GPU glyphs now carry family-aware advances and the GPU scene
  applies sticky shifts, with CPU/GPU parity tests. Link hit-testing inside
  transformed boxes remains a documented interaction approximation.
- **Start page rebuild (about:start) as a living engine test-bed
  2026-09-10**: the start page was rewritten in Dia's style — a dark
  scrolling onboarding story (full-bleed 660px hero, huge serif headline,
  italic subhead, 3 chapter cards, fixed left progress rail, completion
  stamps, footer) where every section deliberately exercises one renderer
  corner (linear/radial/conic gradients, border radius, fixed positioning,
  flex, letter-spacing); a rendering failure on this page pins the next
  engine fix by design.
  - **Root cause of the round-2 visual defects: stale release binary.**
    The app's Makefile rule for `libkitae_core.a` / `libkitae_render_gpu.a`
    only invokes cargo when the `.a` is *missing*, never when it is older
    than the crate sources — engine fixes compiled only by debug-profile
    `cargo test` never reached the app until a manual
    `cargo build --release -p kitae_core --features live -p kitae_render_gpu`
    forces the rebuild. Every "engine bug" in that round (word-space
    collapse, SVG→stadium blob, missing rail dots, 565px squished hero)
    was the old 16:56 engine still linked; the fresh frame fixed all four.
    **Rule: after touching engine sources, always force the cargo release
    build before `make`.**
  - **Abs-positioned containing block fixed (CSS 10.1)**: out-of-flow
    children resolved insets/percentages against the containing block's
    *content* box instead of its *padding* box, so `inset: 0` inside a
    padded hero landed inset by the padding (hero art at (24,118) instead
    of (0,0)). Fixed at the `drain_abs` call site in kitae_layout; the
    rail test now asserts art layers at (0,0)·660px.
  - **Paint-path parity proven 1:1**: an env-gated frame dump in
    EngineController (since removed) decoded the app-side frame and mapped
    every on-screen pixel to engine coordinates (canvas top = window 25 +
    tab strip 40 + toolbar 56 → capture y≈121; canvas ≈1900×884 at x=10;
    scrollbar-thumb extents cross-validate content_height). The Swift
    paint path is correct; geometry arrives 1:1 from the frame — GPU and
    CPU paths paint the same frame identically.
  - **Verified**: fold view judge-passed (headline word spacing, hero
    full-bleed, rail markers mid-viewport, chapter 1 badge/title); cards
    2/3 ("Drive it from the keyboard" ⌘-rows; chapter 3 stamp card) and
    the footer verified in the display list (content_height 2486.4 at
    1900×884, 95 rects / 252 texts).
  - Known gaps (accepted, non-blocking): SVG paints no vector output yet
    (the old engine's harmful fallback pill is gone; the winding dashed
    path is simply absent); border-radius on *borders* renders square
    (rail dots are hollow squares, not circles); the conic wash is a
    360-wedge approximation (thin ray fan, hard edge at its extent);
    PageCanvasView `makeGradient` builds CGGradient from straight-alpha
    NSColors where CGGradient expects premultiplied (fidelity hazard on
    translucent gradient stops); the story-column rect serializes with
    alpha 0 (intended transparent pass-through).

---

## 4. Non-negotiable design rules
1. **No JavaScript in the page pipeline — ever.** The engine's endpoint is the display list. (Amended 2026-10-14, `SCRIPTING_DECISION.md`: QuickJS ships **only** as `kitae_script`, the closed-API automation runtime for user/agent scripts and server-side `kitae_ui` — it never touches fetched pages, and no web script executes in the render path.)
2. **No WASM — ever**, out by design.
3. Pure Rust engine first; vendored C/C++ stays behind the existing policy
   (currently the QTSQ decoder), while shaping uses `rustybuzz`.
4. macOS-only native frontend (Swift Kitae.app), arm64 deployment target 15.0;
   Makefile + swiftc, no .xcodeproj.
5. The name **Kitae** never changes.
