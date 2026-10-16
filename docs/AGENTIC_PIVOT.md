# Owner Decision — The Agentic Browser Pivot

_Written 2026-10-15. This document records the owner's third and current
revision of the Kitae vision. The prior revisions (the no-JS document
browser, 2026-10-11; the `kitae_script` automation runtime, 2026-10-14; the
opt-in `page-js` tier with tiers JS-1/JS-2, 2026-10-15) remain in force
where this document does not amend them. Supersedes the phase ordering in
[`ROADMAP.md`](ROADMAP.md) and the market verdicts in
[`PRODUCT_STRATEGY.md`](PRODUCT_STRATEGY.md) §3 as described below._

---

## 1. The decision, in the owner's terms

> "The Rust-only vision is good for long term, but for now we can't do
> this. We continue to develop the first vision but we will not focus on
> it. We will now focus on making a standard-JS browser like Chrome with
> QuickJS — but focused on very low RAM consumption above all, very fast,
> and with all agentic features. We're making a complete agentic browser."

Decoded into the project's vocabulary:

- **The first vision (pure-Rust document browser, display-list endpoint,
  no-JS moat) is demoted, not deleted.** It keeps building — the engine is
  the renderer of the agentic browser — but no longer gets feature phases.
- **The page-JS tier becomes the product core.** What shipped as opt-in
  tiers (JS-1: modules/style/preventDefault/selectors; JS-2:
  rAF/MessageChannel/geometry/History/propagation/forms) is now the main
  compatibility program: the goal is standard-JS pages run like Chrome
  runs them.
- **The differentiator is resource honesty + agency**, not "we render it
  too": smallest RAM footprint in its class (QuickJS/quickjs-ng's no-JIT
  interpreter and the Rust engine, versus V8's JIT arenas), fast, and a
  browser that agents can drive completely — that is, the 2026 trend
  (AI-native browsers) entered deliberately rather than by accident.

## 2. What is promoted

1. **Compatibility track (formerly "scripting"):** the tier ladder
   continues as the current focus — JS-3 through JS-6 (below), plus a
   conformance discipline so "standard" is measured, not vibes.
2. **The agentic surface (former Phase 5, partial):** `kitae_mcp`
   (read/links/find/screenshot/pdf/click/fill/submit/back), headless
   `kitae_server`, and the closed-API action layer are promoted from "B2B
   after Phase 1" to a consumer-track pillar: the browser ships with an
   agent runtime in the product.
3. **The performance-harness Phase 0 item is repurposed and promoted:**
   RSS-per-tab, cold start, and pages/sec are measured against Chrome and
   Safari on a fixed page set and published — the RAM number is the
   product's headline claim, so it is CI-measured from now on, not
   anecdotal.

## 3. What is demoted (continues, unfocused)

- Phase 1 (feeds/reading/library), Phase 2 (native app suite), Phase 3
  (no-JS app platform), Phase 4 (delegation/CLI/userstyles): kept as the
  "first vision", developed only where the agentic browser needs the same
  work (readability/extraction feeds agents as much as readers; print and
  QTSQ remain engine strengths). No new first-vision feature phases start
  while the compatibility/agentic tracks run.
- The no-JS *strategy* claims (READER positioning, "the no-JS web as a
  moat") move to the backburner chapters of `PRODUCT_STRATEGY.md`; the
  product framing becomes "the complete agentic browser."
- QTSQ, Bridge, mobile-first: unchanged (backburner / very hard / later).

## 4. Where prior verdicts now stand (explicit, so no document argues
with itself)

- ~~"Compete on site coverage" — never~~ → **now the core program**,
  bounded by the conformance panel (§5), not by specmaximalism.
- ~~"WPT conformance race — abandon"~~ → **now kept, but scoped**: WPT
  subsets (DOM/events/promises/storage) score each tier's exit; the
  engine-side full-tree-construction long tail continues opportunistically.
- ~~"Daily-driver general browser — abandon"~~ → **now the destination**,
  reached via the agent-first audience rather than the consumer-default
  browser market.
- **WASM — still a full "never" today.** A true Chrome-class standard-JS
  browser will eventually need it, so this is flagged for an explicit
  owner decision on the compatibility track. Until the owner overturns it,
  no WASM is fetched or executed, unchanged from the founding rule.
- **Page-JS gating:** the cargo feature and per-engine enable stay for
  library/embedded builds (auditability, budgets), but the shipped app
  already enables page JS for every tab (since 2026-10-14) — under this
  pivot the browser product is, by definition, a JS browser.

## 5. The compatibility ladder (each tier exits on tests + a WPT subset)

Already shipped (2026-10-15): classic scripts + ES modules + dynamic
`import()`; timers/rAF/`performance.now`/microtasks/idle/`MessageChannel`;
layout geometry (`getBoundingClientRect`, `offsetWidth/Height`,
`innerWidth/Height`); History API + `popstate` + `location.hash`; full
capture/bubble propagation with `stop(Immediate)Propagation`,
`currentTarget`; form hooks (`keydown`/`input`/`keyup`/`change`/`submit`
with preventDefault semantics); `checked`/select-aware `value`; selectors
through the common grammar; text-run click hit-testing.

- **JS-3 — Standard constructors and read paths.** `Event`/
  `CustomEvent`/`MouseEvent`/`KeyboardEvent` constructors +
  `dispatchEvent`; `URL`, `URLSearchParams`, `FormData` (engine-encoded
  bodies over the existing pool), `Headers`/`Request`/`Response` for
  fetch ergonomic parity; `getComputedStyle` (read path off the style
  engine); `structuredClone`; listener options (`once`, `passive`,
  `signal`); `dataset`; scroll/selection APIs pages actually call.
- **JS-4 — Observers.** `MutationObserver`, `IntersectionObserver`,
  `ResizeObserver` — the trio modern frameworks probe before mounting;
  their absence marks a page as unsupported more than any rendering gap.
  **Shipped 2026-10-16** (journaled records on the microtask queue,
  geometry from the published display list).
- **JS-5 — Workers + legacy glue.** Dedicated workers (same
  budget-per-worker discipline), honest `postMessage`/structured-clone
  semantics, and `XMLHttpRequest` implemented over the fetch internals
  (legacy analytics still gates on it). **Shipped 2026-10-16** — async
  XHR over the fetch pool, and `Worker` as a documented same-thread
  emulation: no real parallelism, and the doc records it as an emulation
  rather than claiming threads.
- **JS-6 — Canvas 2D + storage truth.** Canvas 2D context (software
  first, GPU-ready), storage events, and `localStorage`/`sessionStorage`
  persistence beyond the page near-term, disk-backed later (rides the
  Phase-0 storage answer). **Shipped 2026-10-16** with three recorded
  deviations from this sketch: canvas is software 2D with the documented
  gap list (no text, no gradients/clip/shadows/anti-aliasing), storage is
  in-memory only and `sessionStorage` is scoped to one browser, and there
  are no cross-window `storage` events — all three stay on the storage
  layer's (Track D1) work. Prerequisites the tier pulled in: `<canvas>`
  is a replaced element in the UA sheet, and PNG/JPEG/GIF/WebP `data:`
  URLs decode, so `toDataURL()` output is a usable image source.

- **Ladder exit (owner gate).** The tiers JS-3…JS-6 are feature-complete
  as of 2026-10-16 and measured by the conformance spine (WPT page-JS
  subset + the tier library tests; `WPT_SCORECARD.md`). The remaining
  exit item is the flip to page-JS **default-on for library builds** —
  the app already runs the tier on every tab, and the feature flip with
  everything it changes is recorded in `crates/kitae_core/Cargo.toml`,
  but flipping it is the owner's call, not the build's. Until then the
  feature stays off by default and every tier test runs with it enabled.
- **Conformance panel** — a fixed panel of sites/agents care about
  (dashboards, editors, docs, mail-adjacent SPA patterns) scored per
  build beside the WPT subset, so regressions in the compatibility race
  are visible.

## 6. The agentic plane (what "complete agentic browser" means concretely)

1. **Agent runtime in the app**: the closed-API action layer
   (`kitae_script`) grows the act verbs (navigate/click/fill/submit/
   extract/watch/dispatch) with permission prompts — the same verbs the
   MCP server exposes, so in-app agents and external agents converge.
   *Built (B4, 2026-10-16):* the app drives the verbs over its **own**
   browser through FFI commands 54–58 — the engine thread holds the same
   `SessionState` the transports use, so there is one verb implementation,
   not two. The session starts read-only; a verb outside that rule comes
   back `pending`, the panel asks, and a grant re-issues the held request.
   Every ask, refusal and outcome lands in a journal the panel shows —
   an agent the user cannot watch is a batch job with extra steps.
2. **`kitae_mcp` + headless `kitae_server`** as first-class products,
   now justified by full JS compatibility (the old "content is authored"
   caveat dies).
3. **Extraction becomes agent infrastructure**: the readability engine
   (Phase 1's centerpiece) is repurposed as the agent's "give me the page
   as data" layer — DOM → structured JSON with provenance; tracking
   blocks and resource telemetry become agent-visible, not just
   marketing.
4. A **CDP-compatible control surface** is explicitly *deferred*
   (compatibility with Playwright-style stacks is attractive but a huge
   surface; revisit once `kitae_mcp` proves the demand).

## 7. Honest risks (recorded so they are owned, not discovered)

- **The parity race is real.** The market verdicts this pivot overturns
  were written for good reasons: Chromium's DOM/JS surface is hundreds of
  engineer-years. The bet is that the *agent audience* accepts a
  fixed-set browser if it acts reliably on a large defensible panel —
  agent workflows are narrower than human browsing, and the RAM/CVE
  footprint argument matters more to them.
- **QuickJS is slower than V8.** No-JIT means heavy interactive sites
  will not match Chrome frame-for-frame; the differentiator is footprint
  and speed-per-byte, not benchmark supremacy, and the perf harness must
  report the pair honestly (per-tab RSS AND interaction latency).
- **QuickJS correctness**: quickjs-ng is vigorous but not V8-conformant
  in corner cases (regex, date, Intl). Tier exits should include a JS
  conformance subset (test262 light) once terrain warrants.
- **Identity churn cost**: three vision revisions in five days; docs,
  memory, and the not-do lists have been amended each time. This document
  is now the tie-breaker reference; further pivots amend THIS file first.

## 8. Immediate next steps (in order)

The full dependency-ordered plan now lives in
[`ROADMAP.md`](ROADMAP.md) §The current plan (Tracks A–E, no timelines,
with dependency map and gating decisions). Summary of the first moves:

1. JS-3 (§5) — the biggest standard-JS lever with zero layout work.
2. Track C, the RAM/latency scoreboard — starts immediately; it is the
   headline claim's guardrail, so the claim is CI-measured from the
   first sprint of the pivot onward.
3. B1/B2 — the act verbs and `kitae_mcp`, overlapping Track A (real-site
   reliability deepens as JS-3+ land).
4. D1 — the persistent storage layer, taken early on the first-vision
   track because JS-6 persistence and agent state both need it.
5. Owner decisions recorded when reached: WASM (§4), page-JS default-on
   for library builds, CDP surface (§6.4).
