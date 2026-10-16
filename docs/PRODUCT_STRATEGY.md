# Kitae — Product Strategy

_What Kitae is for, which markets it can win, which it cannot, and how it gets
distributed. Written 2026-10-11 against the 2026-09-26 engine checkpoint
(`ENGINE_CORE_STATUS.md`). Companion document: [`ROADMAP.md`](ROADMAP.md)._

> **Amended 2026-10-15 by owner decision (see
> [`AGENTIC_PIVOT.md`](AGENTIC_PIVOT.md)):** the market verdicts below are
> revised in place meaning — the "general daily-driver browser" and
> "WPT-conformance race" verdicts change from *abandon* to *bounded core
> program*, re-entered through a different door: a standard-JS browser on
> QuickJS, differentiated on very low RAM, speed, and complete agentic
> capability, aimed first at the agent audience. The document-browser and
> reading-beachhead theses become the secondary "first vision" track. The
> headline product claim becomes: the pages agents need, at a fraction of
> Chrome's footprint, driven end-to-end by agents.

## 0. The current strategy (2026-10-15 — supersedes §2 where they differ)

**Build the complete agentic browser: a standard-JS browser running its
pages on QuickJS — the smallest RAM footprint in its class, very fast,
and driveable end-to-end by agents.**

- The differentiation is not "we render it too": it is resource honesty
  (per-page RAM is CI-measured against Chrome/Safari and published — the
  scoreboard is the claim) plus agency (a closed, permissioned action
  surface the browser itself exposes, `docs/ROADMAP.md` Tracks B).
- The entry market is agents, not consumers: agent fleets pay for RAM
  economics directly (pages-per-dollar), and agents accept a bounded,
  scored compatibility panel. The consumer agentic app (B4) is the habit
  that grows behind the headless surface (B2/B3) — this ordering is the
  recorded recommendation, not yet a shipped reality.
- The agent audience reframes the old impossibility verdicts: the
  compatibility race is bounded by the panel + WPT-subset discipline
  (`docs/ROADMAP.md`, Track A), not by the spec-maximalist human web.
- The engine's security posture becomes a product feature for agents:
  every page's JS runs under hard memory/time budgets with an
  origin-gated network — a materially smaller blast radius for
  prompt-injection-by-webpage than a hosts-V8-in-process architecture
  can honestly claim.
- Sections 1–8 below now read as the first-vision analysis that remains
  true wherever the pivot does not amend them (the diagnosis of the
  document-shaped web, the reader/a11y/print strengths, the distribution
  calculus); §3's board rows flip exactly as the amendment above states.

---

## 1. The honest diagnosis

Kitae today is a complete engine (17 crates, 1149 tests, print, a11y, media,
GPU renderer, QTSQ) with almost no product loop around it. Nobody installs an
engine; they install a habit. The missing work is not rendering — it is the
loop that makes someone open Kitae every morning.

The no-JavaScript constraint is fatal as a browser and decisive as a reader:

- **As a general browser** (industry definition: renders what Chrome renders),
  Kitae is a worse Safari and always will be. The entire point of a general
  browser is that any site works; without a JS engine this is unfixable, and
  adding one is a permanent, non-negotiable no.
- **As a reader/working environment**, Kitae is the best possible one: no
  tracker scripts, no JS payloads, instant startup, real print, durable pages.
  The constraint that disqualifies it as a browser is the moat as everything
  else.

## 2. The strategy in one line

**Stop shipping a browser. Ship the definitive document browser + native app
suite for the no-JS web — and monetize the same engine headlessly where
nobody needs JavaScript at all.**

"Document browser" means: pages are documents (read, find, print, archive,
quote), and daily tasks that the JS web buries inside SPAs are handled by
first-party native apps rendered by the engine itself. "General purpose" is a
destination earned in phases — never a launch claim.

The one bet underneath the whole strategy:

> Enough of daily browser life is document-shaped rather than SaaS-shaped —
> reading, mail, news, research, shopping-lite, learning — that a document
> browser plus first-party apps covers it. The holdouts (Slack, Notion, Figma,
> Google Docs) are handled by honest delegation, not apology.

## 3. The market board

| Path | Verdict | Why |
|---|---|---|
| Reader for the static/authored web (feeds, extraction, library) | **Beachhead — do first** | Uses everything already built; a daily habit; a pre-assembled audience (RSS / HN / indie web) |
| Headless Kitae rendering API + MCP server (HTML → md/pdf/links/shot) | **Parallel B2B — do second** | Monetizes the engine with zero consumer adoption needed; content is authored, the engine's sweet spot |
| kitae_ui / kitae_live as a "no-JS web framework" | Very hard — defer | Devs must learn a framework *and* abandon React. Dogfood via the first-party suite first |
| QTSQ as a flagship feature | Deprioritize | Own format, no ecosystem. Keep as an engineering showcase; zero marketing spend |
| Kitae Bridge (server-side JS execution proxy) | Nearly impossible — abandon | Operating a headless-Chrome fleet: cost, legal grayness, ops — and it destroys the zero-attack-surface purity that differentiates |
| Daily-driver general browser (industry definition) | **The destination (2026-10-15, §0)** | The ceiling argument died with the page-JS tier; the path is agents first, consumers behind them, on one bounded panel at a time |
| iOS / Android browser | Very hard — later | iOS mandates WKWebView for browsers; Android is Chromium in practice. macOS-only is honest, not sad |
| WPT conformance race vs Chromium | **Bounded core program (2026-10-15, §0)** | Or: it was. The race is re-entered bounded — agent-relevant panel + WPT subsets, scored; the unbounded human tail still stays out of scope |

### Nearly impossible (do not spend a year here)

1. Replacing Chrome/Safari as anyone's *only* browser on the industry's terms.
2. Winning over the general web — publishers monetize JS; they will never
   optimize for Kitae. You are an ad blocker with better typography as far as
   they're concerned.
3. Spec conformance parity with Chromium/Gecko. The finish line moves monthly.
4. Kitae Bridge as a core engine feature (see table).
5. A foundation/consortium-style institutional adoption story — that requires
   corporate sponsors a solo engine project does not attract.

### Very hard (possible, but brutal)

1. Embedding Kitae as a WebView replacement inside third-party products —
   procurement, trust, security audits. The realistic slice is *server-side*
   rendering, where procurement is a single developer.
2. kitae_ui framework adoption — framework graveyards are deep; only viable
   after you've shipped several products on it.
3. Staying secure as a solo dev: the parser bug class is yours alone (the V8
   bug class doesn't exist for you — that's the trade). Fuzzing exists; keep it
   a release gate.
4. Multi-platform GUIs — Windows/Linux ports are projects, not ports.

## 4. The market pivot worth taking seriously: agents

The strongest B2B slice of the "browser for AI agents" wave is *not*
computer-use agents (Operator/Claude-in-Chrome/Mariner — they need pixels and
JS; that is Chrome's turf and a loss by design). It is the **extraction and
semantic-interaction layer for text-native agents**: give any agent clean
structured content, links, find, screenshot, PDF, and click/fill/submit over
MCP or HTTP, with no Chrome involved.

Kitae is unusually suited:

| Agent need | Kitae already has |
|---|---|
| URL → clean content | HTML/CSS pipeline + noscript content policy |
| URL → links/structure | DisplayList with `LinkArea`s |
| URL → semantics for interaction | 44-role accessibility tree |
| URL → find/extract | `find_in_page` with match rects |
| URL → screenshot | wgpu offscreen renderer with readback |
| URL → PDF | `@media print` frame + PDF path |
| click / fill / submit | page-space click hit-testing + form submit |

The moat vs. the incumbents (Firecrawl, Jina, Browserless, Browserbase — all
headless-Chrome wrappers): **10–50× cheaper and faster per page** (no tab
process, no CDP session), **zero script surface** (nothing to prompt-inject
through, no Chromium CVEs in the customer's supply chain), **determinism**
(identical bytes in → identical bytes out), and a single self-hostable static
Rust binary.

The honest loss: JS-driven URLs (roughly 40–60% of arbitrary pages). The
answer is the two-tier chain — Kitae is the cheap first pass in every agent
pipeline; JS-heavy targets escalate to a scripted browser only when needed.
Even at a 50% hit rate the economics win, because the 80% of tasks
(summarize/search/extract/screenshot) land at a fraction of the cost.

Note: this inverts `BRIDGE_FEATURE.md`. The scripted browser becomes a
*fallback service* behind the engine, never inside it.

## 5. Where Kitae is structurally, permanently better

These are properties of having no script engine, not aspirations:

- **Startup** — no engine boot, no JIT warmup; pages readable before Safari's
  spinner appears.
- **Memory / battery / thermal** — no per-tab heap, no rAF loops, no background
  JS. A hundred tabs is a flex, not a crash.
- **Privacy by construction** — no canvas, no WebGL, no JS fingerprint
  surface at all; tracking reduced to auditable HTTP + cookies.
- **Security surface** — the V8/WASM CVE class does not exist for you. The
  remaining parser bug class is yours alone (mitigation: fuzzing as a release
  gate).
- **Determinism / durability** — bytes in → bytes out: bit-perfect archiving,
  diffable pages, the web as a document medium again.
- **Print and typography** — `@media print` + real pagination + the shaping
  stack; nobody beats this combination.
- **LLM-age alignment** — every page rendered is clean, structured, quotable
  with provenance. The same asset powers the agent API: two business models,
  one engine truth.

## 6. Where Kitae will never be as good (say it publicly; it builds trust)

JS-only SaaS, interactive content/games, the JS-rendered long tail.
Mitigations: honest interstitials with one-key external open (already built),
per-domain silent delegation to the system browser (to build), and the paid
escalation service (Phase 4, deferred).

## 7. Marketing plan (solo, zero budget)

**Message:** *"Read the web the way it was meant to be read. No JavaScript.
Nothing tracking. Nothing to wait for."*

**Proof assets, in priority order:**

1. The 20-second split-screen screencast: Safari vs Kitae opening the same
   article pages — stopwatch, RAM figures, clean reading view. Nobody argues
   with a stopwatch.
2. The **bloat counter**: per article and per month — KB served vs KB shown,
   requests refused, trackers blocked. In-app, screenshotable, shareable. It
   turns the no-JS constraint into a scoreboard readers post unprompted.
3. The honest-interstitial demo on a JS-heavy site — trust through candor.

**Channels, in order:**

1. Show HN: "I built a browser engine with no JavaScript — here's what
   broke." HN *is* the audience: text-first, RSS-native, anti-bloat.
2. r/programming, r/apple, r/rss, the indie web / webrings, "alternatives to"
   lists, reader-app roundups, accessibility communities (a no-JS reader is
   genuinely, marketably better for screen readers).
3. Content marketing = dogfooding: publish a weekly digest *rendered by
   Kitae* — the content is the demo.
4. Writer outreach: a "renders in 0.2s in Kitae" badge for bloggers plus RSS
   autodiscovery — offering writers extra reach for zero work is a real
   alliance, not a supplication.
5. Build in public: weekly numbers (cold-start ms, peak RSS, test count,
   pages rendered). The repo already has this discipline internally; make it
   the external credibility loop.
6. The CLI (`kitae read`, `kitae pdf`, `kitae shot`) as the dev-adoption
   wedge — `bat`-style tools spread through devs before their GUIs.

**Monetization:** $29–39 one-time, direct + founder pricing first, App Store
later; $4/mo tier once sync and audio mode exist. **Realistic expectation:
this genre tops out in the low thousands of dollars per month.** It is a craft
product with a cult audience — a studio, not a startup. If the goal is a
company, the rendering API (Section 4) is the revenue engine.

**The only metric that matters:** 7-day retention of daily readers — articles
read per user per week, feeds subscribed, highlights made. Downloads are
vanity; a habit is the product.

## 8. Gating decisions (before Phase 1 build starts)

1. **Destination**: (a) "Kitae becomes *my* daily driver" — achievable,
   shippable, the foundation of everything — or (b) "Kitae becomes *other
   people's* browser plus platform" — needs the app suite, an ecosystem,
   and luck. Recommendation: build for (a), market toward (b).
2. **Licensing**: engine fully open (MIT) vs. engine open / app closed. Decide
   before the first public release; it is hard to reverse.
3. **Framing**: keep the word "browser" (correct) with the explicit
   qualifier "document browser" everywhere in copy.
