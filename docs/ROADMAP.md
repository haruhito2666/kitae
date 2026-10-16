# Kitae — Full Roadmap

_Ordered by dependency, deliberately without timelines — start each phase when
the previous exit criteria are met, not when a calendar says so. Written
2026-10-11 against the 2026-09-26 engine checkpoint
(`ENGINE_CORE_STATUS.md`). Strategy context and market analysis:
[`PRODUCT_STRATEGY.md`](PRODUCT_STRATEGY.md)._

---

> **Owner pivot, 2026-10-15 — the agentic browser is now the focus.** The
> owner has re-ordered this roadmap: the focus moves from the no-JS
> document browser to a **standard-JS browser on QuickJS — very low RAM
> first, fast, and complete agentic features**. The phases below keep
> their text but not their order: the page-JS compatibility ladder (tiers
> JS-3…"JS-6"), the agent surface (former Phase 5: `kitae_mcp`,
> `kitae_server`, in-app agent runtime), and the perf/RAM scoreboard
> (Phase 0, reskinned) are the current track; Phases 1–4 continue as the
> "first vision", developed only where the agentic browser needs the same
> work. The not-do list below was amended accordingly: site coverage and
> WPT-conformance competition, formerly "never", are the core program
> (bounded by the conformance panel in `AGENTIC_PIVOT.md` §5); **WASM
> remains a full "never"** pending an explicit owner decision. Full
> record: [`AGENTIC_PIVOT.md`](AGENTIC_PIVOT.md).

---

## The current plan (2026-10-15 — the agentic QuickJS browser; deliberately
no timelines)

Ordered by dependency: a track starts when its entrance criteria (an
earlier track's exits) are met, never on a calendar. The thesis it serves:
**a standard-JS browser on QuickJS — the smallest RAM footprint in its
class, very fast, completely agentic.**

### Track A — the standard-JS compatibility ladder (the focus)

The page-JS tier is the product core; every tier below exits on its own
tests plus the conformance spine (web-platform-tests subsets for
DOM/events/promises/storage — the vendored harness discipline in
[`WPT_SCORECARD.md`](WPT_SCORECARD.md) extends to each tier's surface — and
a fixed panel of agent-relevant sites scored per build, so "standard" is
measured, not vibes).

- **JS-3 — standard constructors and read paths** (entrance: JS-2, done
  2026-10-15). **Shipped 2026-10-15** — constructors + `dispatchEvent`,
  listener options, `URL`/`URLSearchParams`, `Headers`/`Request`/
  `Response`, `FormData` + multipart POST fetch, `structuredClone`,
  `dataset`, `getComputedStyle`, scroll/selection/focus reads (inventory
  and the recorded gaps in `SCRIPTING_DECISION.md`; the conformance
  spine — WPT subsets + the agent site panel — remains the exit work).
- **JS-4 — observers.** `MutationObserver`, `IntersectionObserver`,
  `ResizeObserver` — the trio modern frameworks probe before mounting;
  their absence marks a page unsupported more than any rendering gap.
  **Shipped 2026-10-16** — all three, journaled records on the microtask
  queue, geometry from the published display list (gaps and the
  inventory in `SCRIPTING_DECISION.md`).
- **JS-5 — workers and legacy glue.** Dedicated workers (one budget per
  worker, same interrupt/memory discipline), honest
  `postMessage`/structured-clone semantics, and `XMLHttpRequest`
  implemented over the existing fetch internals (legacy analytics still
  gates on it). **Shipped 2026-10-16** — async `XMLHttpRequest` over the
  fetch pool, and `Worker` as a documented same-thread emulation (no real
  parallelism; `postMessage`/`terminate` semantics as the spec has them).
- **JS-6 — canvas and storage truth.** Canvas 2D context (software
  first, GPU-ready), storage events, and `localStorage`/`sessionStorage`
  persistence beyond the page (near-term), disk-backed later (rides the
  storage layer, Track D1). **Shipped 2026-10-16, with two recorded
  deviations**: canvas is software 2D with the documented gaps (no text,
  no gradients/clip/shadows, no anti-aliasing) and storage is in-memory
  only — same-origin `localStorage` is process-wide, `sessionStorage` is
  scoped to one browser — so disk persistence and `storage` events stay
  Track D1's work; `data:` image URLs now decode, which is what makes
  `canvas.toDataURL()` usable as an image source.
- **Ladder exit:** the panel reads and acts correctly end-to-end, and
  page-JS turns default-on for every build, library included
  (`SCRIPTING_DECISION.md` Part 3). Tiers JS-3…JS-6 are feature-complete
  (2026-10-16) and the measured conformance spine holds (46 page-JS
  library tests green, the WPT subset at its recorded baseline); the one
  remaining exit item — the default-on flip for library builds — is an
  **owner gate**: the app already runs page JS on every tab, and the
  prep is done (`crates/kitae_core/Cargo.toml` documents the flip and
  what it changes), but the flip itself waits for the owner.

### Track B — the agentic plane

Starts overlapping Track A now (the act verbs already exist as engine
hooks; JS-3's constructors and `getComputedStyle` unlock acting on real
pages reliably), deepens as the ladder climbs.

- **B1 — action verbs + permissions.** `kitae_script`'s closed API grows
  the act surface: navigate, click, fill, submit, extract, watch,
  dispatch — with a permission model (prompted in the app, declarable in
  headless).
- **B2 — `kitae_mcp`.** MCP server: `read`, `links`, `find`,
  `screenshot`, `pdf`, `click`, `fill`, `submit`, `back`, plus the B1
  verbs — full JS pages now, not the static subset it was scoped against.
- **B3 — headless `kitae_server`.** Single static Rust binary, HTTP API +
  Docker image. The agent-fleet economics: per-page RAM converts directly
  into pages-per-dollar on the same hardware.
- **B4 — the in-app agent runtime.** The same verbs, driven inside
  Kitae.app with visible agent activity and permission prompts — the
  consumer surface of the same plane.
- **B5 — page-as-data extraction.** The readability engine repurposed as
  the agent's input layer: DOM → structured JSON with provenance;
  blocked trackers and resource telemetry become agent-visible facts.
- **B6 — archive-and-verify.** QTSQ as the agent's evidence format: each
  acted-on page archived, hashed, replayable.
- **Track exit:** an external agent completes a real multi-step task
  end-to-end through `kitae_mcp` at a fraction of a Chromium fleet's RAM.
- **Explicitly deferred:** a CDP-compatible control surface — attractive
  but enormous; revisit after B2/B3 prove demand.

#### Track B status (2026-10-16): B1–B6 delivered, exit not claimed

Everything above exists, compiles and is tested. What that means, tier by
tier, and what it does *not* mean:

| Tier | Where it lives | Evidence |
| --- | --- | --- |
| B1 verbs + permissions | `kitae_agent` (shape, policy hook, 15 verbs) + `kitae_core::agent` (meaning, over a real `Browser`) | 5 + 18 library tests; default policy is read-only, refusals name the verb |
| B2 `kitae_mcp` | `crates/kitae_mcp` — stdio JSON-RPC, hand-rolled schemas, `--allow` policy | 6 protocol tests incl. a multi-step task over a local site fixture |
| B3 `kitae_server` | `crates/kitae_server` — HTTP API, sessions, batch acts, Dockerfile | 4 tests over real sockets; verified live over a socket (health + live navigate) |
| B4 in-app runtime | FFI commands 54–58 + `PageRasterVtable` + `app/Sources/AgentPanel.swift` | 2 engine-FFI tests (permission flow, host rasterizer), the C smoke test drives the same calls, 12 native XCTest cases |
| B5 extraction | `kitae_core::extract` — DOM → JSON with provenance + telemetry | 6 tests (blocks/sections, third-party + mixed-content telemetry, budgets) |
| B6 archive-and-verify | `kitae_core::evidence` — QTSQ archive, SHA-256 manifest, replay | round-trip + refusal tests; digests computed over a geometry-free projection |

Two things the table does not say, stated plainly:

1. **The Track exit is not claimed.** An external agent on *real sites*
   with a measured RAM comparison against a Chromium fleet is an owner
   decision (which sites, which fleet, which machine), not something the
   repo can assert on its own. What is verified: the verbs run on a real
   page through all three transports, example.com was driven through the
   `kitae-mcp` binary, and the servers answer over real sockets.
2. **B4's Swift panel is compiled and unit-tested, not hand-tested.** The
   engine-side path (commands → journal → permission prompt → grant →
   re-issue) is tested from Rust, and the C ABI the app links is driven
   by `make -C app ffi-smoke`; the panel's own wire handling has XCTest
   coverage. Nobody has yet clicked the buttons in a running window.

Also true and worth recording: the app's agent sessions rasterize through
the *same* pure-Rust rasterizer the headless transports use (it is injected
as a vtable — `kitae_render_gpu`'s CPU compositor sits above `kitae_core`
in the crate graph, so the engine cannot call it directly). Agent verbs are
**not** exposed to page scripts: only the panel/FFI path and the Track-A
script host can act, so a hostile page cannot drive its own browser.

### Track C — the RAM/latency scoreboard (starts immediately)

The former Phase-0 perf harness, reskinned as the headline-claim
guardrail: CI-measured RSS per tab, cold start, pages/sec, AND
interaction latency on heavy SPAs; a fixed page set with Chrome and
Safari baselines; published per build. Both numbers, always — RAM and
latency are in known tension (the no-JIT tax), and the honest pair is
the claim. The QuickJS tuning program (heap pressure, GC, startup) works
*only* under this scoreboard.

### Track D — the first vision (continues, unfocused)

The former Phases 0–4 (reading/library, native app suite, no-JS
platform, interop). Items are taken **only** where the same work serves
A–C:

- **D1 — the persistent storage layer** (Phase-0 leftover; serves JS-6
  persistence, agent state, and session history). Content-addressed
  snapshot store per the recommendation in `AGENTIC_PIVOT.md` §8.
- **D2 — extraction/readability work** (serves B5 directly).
- **D3 — print, QTSQ, a11y hardening** (serve B3/B6 and the panel).
- Parked, no new starts: the rail-shell app suite, feeds UI, mail,
  platform-scale SDK ambitions.

### Track E — boundaries and escapes

- **Graceful handoff:** on a panel miss (a page Kitae cannot yet carry),
  agent-triggered delegation to the system browser is a product surface
  of the agentic browser, not an apology — the old Phase-4 delegation,
  promoted.
- **WASM:** still a full "never" (owner standing rule). A Chrome-class
  target will likely force the question at the JS-6 gate — flagged as a
  required owner decision, not made unilaterally.
- **Unchanged not-dos:** V8, Bridge-as-core, QTSQ marketing, mobile-first.

### Dependency map

```
A (JS-3 → JS-4 → JS-5 → JS-6) ──┬── B1/B2 (verbs + MCP; overlap starts at JS-3)
A conformance spine ────────────┤── B3/B4/B5/B6 (deepen with the ladder)
C (scoreboard) ─────────────────┘── D1 storage (serves JS-6 + agent state)
E (handoff/WASM gates) productize alongside B
```

### Gating decisions (must be answered at the stated gate)

1. ~~Destination~~ — resolved 2026-10-15 by the pivot: the agentic
   browser; consumers arrive through agents (headless-first is the
   recorded recommendation).
2. **Licensing** (unchanged in nature, sooner in consequence): fully
   open engine vs. open-engine/closed-app — `kitae_mcp`/`kitae_server`
   distribution forces it before the first external release of either.
3. **WASM** — owned decision required at the JS-6 gate (Track E).
4. **CDP surface** — revisit gate: once B2/B3 external demand exists.
5. **page-JS default-on for library builds** — gate: ladder exit.

Everything below this line is the 2026-10-11 first-vision roadmap,
preserved verbatim for context; it is no longer the operative plan — the
current plan above is.

---

## The thesis this roadmap serves

Kitae is not "a worse browser." It is the document browser + native app suite
for the no-JS web: everything a browser does for you daily — read, watch,
listen, mail, organize, print, archive — with zero JavaScript, several of
those done better than Chrome can ever do them. The no-JS constraint is the
moat, not the bug.

General purpose is a destination earned in phases: dominate reading first,
ship the native app suite second, open the platform third, make delegation
seamless fourth. It is never a launch claim.

---

## Phase 0 — Foundations (blocks every later phase; mostly engine-side)

- **Fetch off the engine thread.** Fetch is currently synchronous per-tab (a
  known limit in the status doc). Introduce a shared background fetch pool
  with a job/notification channel the engine drains per frame. Required by
  feed refresh, mail polling, and preloading.
- **Persistent storage layer.** SQLite (rusqlite) or a content-addressed
  snapshot store — feeds, article library, notes, highlights, history beyond
  session restore. Everything downstream writes here.
- **App-suite navigation shell.** The Swift app becomes a rail-switchable
  shell: Web / Feeds / Library / Mail / Notes / Media / Settings. Key
  architectural move: every first-party app is a *page synthesized by the
  engine*, so built-in app UI gets layout, typography, print, a11y, and find
  through the identical display-list pipeline as the web.
- **Performance budget harness.** Cold start, per-page time, RSS per tab,
  pages/sec — measured in CI, published monthly. Engineering guardrail *and*
  marketing asset.
- **Discipline stays:** full `cargo test --workspace` sweep green, fuzzing as
  a release gate (html/css/urls/qtsq, plus each new network surface),
  cargo-deny, WPT harnesses.

## Phase 1 — Reading: master the one domain (the beachhead)

**Goal: Kitae is *my* only browser on reading days, and beats Safari Reader
Mode everywhere.**

- **Readability extraction engine** — the single most important missing
  piece. Layout-aware main-content detection: score candidate nodes by
  *geometry* (line-length consistency, column structure, link density, ink
  density) instead of DOM heuristics — the real layout engine is the unfair
  advantage. Output: title, byline, date, hero, main content, confidence.
- **Unified content policy chain:** extract → noscript-swap → honest
  interstitial → one-key external open.
- **AVIF decode** — the admitted gap in the status doc; missing images are the
  #1 reader complaint.
- **`kitae_feed` crate** — RSS 2.0 / Atom / JSON Feed parsing, OPML
  import/export, enclosures.
- **Reader UI** — three-pane (feeds | items | article), `j/k/n/p`, `o` open
  original, `s` save, `e` export, unread badges, smart "everything" stream.
- **Article library** — save-for-offline snapshot (HTML + assets + extracted
  text), SQLite index, full-text search. The library outlives the web.
- **Highlights + notes** — persist existing selection rendering; export
  Markdown with provenance frontmatter (Obsidian-friendly).
- **Sync** — CloudKit app-group container, last-writer-wins on
  content-addressed snapshots.
- **Typography suite** — themes (paper / sepia / mono / terminal), measure
  control, hyphenation, font selection.
- **Bloat counter** — per article and per month: KB served vs shown, requests
  refused, trackers blocked. Shareable; the marketing engine.
- **Exit criteria:** first public release, launch post, and Kitae replaces
  Safari for reading.

## Phase 2 — The native app suite (what makes "general purpose" true)

**Goal: Kitae is the only browser on every day that doesn't involve SaaS.**

- **Mail** — pure-Rust IMAP/SMTP (`async-imap` + `lettre`), `kitae_mail`
  crate; threads, search, compose. Kills the biggest "but I need Gmail"
  objection (Gmail is solved by IMAP).
- **Calendar** — CalDAV client; the month/week view is an engine-synthesized
  page (HTML/CSS laid out by `kitae_layout`).
- **Podcasts / audio** — the decode pipeline exists; add podcast
  subscriptions (same feed crate), queue, speed, sleep timer, Now Playing.
- **Notes** — local Markdown store (`kitae_core` already has `markdown.rs`);
  files on disk, previewed through the engine; highlights export feeds this.
- **PDF reading** — host PDFKit display, plus the existing `@media print`
  pipeline for the inverse (page → PDF).
- **Files + downloads** — polished `file://`, a real file pane; the downloads
  center already exists.
- **Exit criteria:** a full week of normal use inside Kitae minus SaaS.

## Phase 3 — The no-JS app platform (let others extend it)

**Goal: someone other than you ships a Kitae app.**

- **kitae-live productionized** — freeze the protocol versioning, stable SDK,
  docs (`UI_COOKBOOK.md` exists), reference apps.
- **kitae_ui SDK** — the framework-primitive gaps already itemized in
  `UI_LIBRARY_AUDIT.md`; component kit; testing story.
- **Dogfood the platform**: rewrite first-party mail/calendar/notes *as*
  kitae_ui apps — the platform must eat its own cooking before anyone else
  trusts it.
- **App model** — `kitae://` app scheme, app manifests, an app directory.
- **Permission console** — per-site and per-app: cookies, storage, media,
  network. "Browser as OS" made concrete and auditable.
- **Exit criteria:** a third-party app exists; docs good enough that a
  stranger could ship one.

## Phase 4 — Interop: how a no-JS browser becomes the *default* browser

**Goal: Kitae is the default browser even though the JS web exists.**

- **Per-domain delegation** — Kitae registers as default browser; a per-domain
  rule decides "render" vs. silent handoff to Chrome/Firefox with a toast.
  Learned from interstitial history. This is the practical unlock of the whole
  thesis.
- **OS integration** — share extension and "Open in Kitae", Quick Look preview
  for `.html`/`.qtsq`, Services, Shortcuts actions, Handoff, a recent-articles
  widget.
- **`kitae` CLI** — `kitae read`, `kitae fetch`, `kitae pdf`, `kitae shot`
  (the print pipeline and GPU readback already exist). A terminal app for the
  web; the dev-adoption channel that feeds the GUI.
- **Userstyle system** — per-site reader CSS overrides, sharable by URL. The
  no-JS answer to extensions, community-generated.
- **Escalation service (revisit `BRIDGE_FEATURE.md` only here, inverted)** —
  an optional paid server-side Chrome render returning static frames, as the
  fallback tier. Only worth it with Phase 5 revenue; documented as very hard
  and deliberately deferred.

## Phase 5 — The agent twin (parallel B2B track; starts after Phase 1)

**Goal: the same extraction engine monetizes to developers with no consumer
adoption required.**

- **`kitae_mcp`** — MCP server binary: `read`, `links`, `find`, `screenshot`,
  `pdf`, `click`, `fill`, `submit`, `back`. Self-host free.
- **`kitae_server`** — single static Rust binary HTTP API + Docker image;
  priced below headless-Chrome incumbents per page; won on cost,
  determinism, zero script surface, and no Chromium CVEs in the customer's
  supply chain.
- **Extraction eval harness** — private labeled URL set, quality scored per
  build, public benchmark table against Reader Mode and other readers. The
  WPT harness discipline applied where the money is.
- **Two-tier positioning** — Kitae as the cheap first pass in agent
  pipelines; JS-heavy targets escalate to a scripted browser only when needed.

## Cross-cutting tracks (run the whole way)

- **Conformance**: the tokenizer is already at 100%; push WPT
  tree-construction (currently ~60%) to ≥95% *on the static subset real pages
  use* — the "pages people read" race, not the spec race.
- **Security**: fuzz every new network surface (feeds, IMAP, CalDAV);
  cargo-deny stays the release gate; new attack-surface reviews follow the
  `SECURITY_AUDIT.md` cadence.
- **Archive standards**: save-page as a self-contained bundle, per-page
  content hash — "the archivable browser" as a category claim.
- **Documentation**: rewrite the README around the document-browser thesis
  (Section 8 of PRODUCT_STRATEGY covers framing); keep the status-doc
  discipline.
- **Distribution**: notarized DMG + Sparkle first; App Store later.

## Standing "we will NOT do this" list

- V8 / JavaScript / WASM in the **page pipeline** — permanent. This is the
  identity. (Amended 2026-10-14 by owner decision, see
  `SCRIPTING_DECISION.md`: QuickJS ships **only** as the closed-API
  automation runtime `kitae_script` — user/agent scripts against Kitae's
  own document API. Remote web JavaScript is still never fetched, executed,
  or relied on for rendering, and the display list remains the engine's
  endpoint. WASM remains a full "never".)
  (**Superseded 2026-10-15** by the agentic pivot, `AGENTIC_PIVOT.md`:
  remote page JavaScript via QuickJS is now the product core — see Part 3
  of `SCRIPTING_DECISION.md` and the JS-2 banner in
  `ENGINE_CORE_STATUS.md`. V8 stays out; the display list remains the
  render endpoint; **WASM is still a full "never"** pending an explicit
  owner decision.)
- Compete on site coverage. Every general-purpose-but-different browser
  without one dominated job is dead (Firefox OS, webOS). Coverage is Chrome's
  game; ours is depth.
- Bridge as a core engine feature — it abandons the purity that
  differentiates. Revisit only as a paid Phase-4 fallback.
- QTSQ as a marketing pillar — keep it working as an engineering showcase.
- Mobile-first. An iOS reading companion only after Phase 2, reusing the
  engine.
- Social, gaming, interactive content — never.

## Dependency map

```
Phase 0 Foundations ──┬── Phase 1 Reading ──┬── Phase 2 App suite ── Phase 3 Platform
                      │                     ├── Phase 4 Interop + CLI  (any time after 1)
                      │                     └── Phase 5 Agent API     (parallel, independent)
                      └── README/thesis rewrite, licensing decision    (any time)
```

Readability extraction (Phase 1) is the shared core of Phases 2, 4, and 5 —
build once, sell three times.

## Gating decisions (must be answered before Phase 1 build starts)

1. **Destination**: (a) "Kitae becomes *my* daily driver" — achievable and
   shippable, the foundation of everything — or (b) "Kitae becomes *other
   people's* browser plus platform" — needs Phases 3–4 and luck.
   Recommendation: build for (a), market toward (b).
2. **Licensing**: fully open engine (MIT) vs. engine open / app closed.
   Decide before the first public release; hard to reverse.
