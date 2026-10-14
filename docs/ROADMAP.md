# Kitae — Full Roadmap

_Ordered by dependency, deliberately without timelines — start each phase when
the previous exit criteria are met, not when a calendar says so. Written
2026-10-11 against the 2026-09-26 engine checkpoint
(`ENGINE_CORE_STATUS.md`). Strategy context and market analysis:
[`PRODUCT_STRATEGY.md`](PRODUCT_STRATEGY.md)._

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

- V8 / JavaScript / WASM — permanent. This is the identity.
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
