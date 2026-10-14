# Kitae

**The web, without the noise.**

Kitae is a typography-first browser *engine* for the static web, written in
pure Rust, with a native macOS frontend written in Swift (AppKit + Core Text).
There is **no JavaScript engine** — no V8, no QuickJS, no scripting of any
kind. Kitae reads the web the way a reader app does: text, structure, links.
That's it.

The name is intentional and stays: **Kitae**.

## Architecture

```
┌──────────────────────────── crates/ (Rust) ─────────────────────────┐
│  kitae_core     the embeddable engine: browser loop, history,      │
│                 content policy + C FFI facade (engine thread,      │
│                 frame serialization)  ← the library embedders use  │
│  kitae          thin SDL2 shell (dev/CI harness for the engine)    │
│  kitae_layout   block/inline layout → display list (+ links)       │
│  kitae_style    CSS cascade over the DOM, UA stylesheet            │
│  kitae_css      CSS tokenizer + parser                             │
│  kitae_html     HTML5-lite tokenizer, entities, tree builder       │
│  kitae_dom      arena-based DOM (NodeId = u32 index)               │
│  kitae_net      URL normalization, HTTP fetch, link resolution     │
└─────────────────────────────────────────────────────────────────────┘
             │ stable C ABI (commands in, frames out)
┌────────────▼───────────── app/ (Swift + AppKit) ────────────────────┐
│  Kitae.app: window, menu bar, chrome (top-bar & sidebar layouts),  │
│  multi-tab TabManager, Core Text page painting, animations.        │
│  No scripting, no HTML — Swift only decodes frames and draws.      │
└─────────────────────────────────────────────────────────────────────┘
```

- **Rust is the engine**: networking, parsing, styling, layout, history, and
  content policy all live in `kitae_core` — a pure-Rust library with no
  AppKit, no SDL, no windowing. Text measurement is *injected* by the host
  through a vtable, so layout is exact on any platform.
- **Swift is the face**: the app owns the window, runs a 60 Hz poll on the
  engine, decodes a compact binary frame (rects / texts / links / scroll /
  title / URL / nav flags), paints text with Core Text, and routes clicks
  back at page-space coordinates.
- **The contract** is `crates/kitae_core/include/kitae_core.h`:
  `kitae_engine_command` (navigate / back / forward / reload / click / scroll
  / resize) in, `kitae_engine_take_frame` out. Little-endian, versioned,
  plain data. The `kitae` SDL binary is itself just another embedder — proof
  the facade works without Swift.
- `native/` (the old C++ shell) is retired from the main path; see
  [docs/CPP_ROLE.md](docs/CPP_ROLE.md) for where C++ returns later (media
  codecs + HarfBuzz shaping, as libraries only).

## The macOS app

- **Two chrome layouts**, switchable live in Settings (⌘,): a classic
  **top bar** (tab strip + toolbar) and an Arc-inspired **sidebar** rail with
  its own nav cluster. The choice persists.
- **Multi-tab from day one** — one engine (one OS thread) per tab; ⌘T new
  tab, ⌘⌥W close, full menu bar (App / File / Edit / View / History / Window).
- **Appearance: system / light / dark** — every chrome color is a dynamic
  light/dark pair that updates live when the system flips (including
  layer-backed views via `viewDidChangeEffectiveAppearance`); Settings can
  also pin Light or Dark explicitly.
- **Command palette (⌘K)** — fuzzy-filtered launcher listing open tabs plus
  commands (new tab, reload, back/forward, layout switch, settings), in a
  floating panel with dimmed backdrop, spring entrance, and full keyboard
  navigation.
- **Settings window** — icon sidebar (Appearance / Tabs / Advanced / About)
  with crossfading panes: appearance radios, live layout preview cards you
  click to switch chrome, and a reduce-motion toggle.
- **Share sheet (⌘I)** — the current page URL via `NSSharingServicePicker`.
- **Print (⌘P)** — writes the page to PDF (Letter/A4) through `NSPrintOperation`,
  paginated by AppKit. The printed frame is laid out by the engine in *print
  media* (`kitae_engine_command(KITAE_CMD_SET_MEDIA) = 52`): the page is
  re-styled and re-laid-out at the paper's printable width, `@media print`
  rules apply while `@media screen` ones stop, `prefers-color-scheme` /
  `prefers-reduced-motion` answer as paper, and the engine flags the frame it
  publishes (wire v20 `flags` bit2) so the host knows the frame is the print
  one. Screen media and the window viewport are restored right after.
- **Micro-animations everywhere** — hover fills, press-bounce springs, layout
  cross-fades, loading sweep, palette/pane entrances — all gated by
  System Settings → Accessibility → Reduce Motion *and* an in-app
  "Prefer reduced motion" toggle (`Theme.motionEnabled` collapses them to
  instant changes, honored live).
- External handles: `mailto:` and other non-http schemes open via
  `NSWorkspace`; plain words in the address bar search via DuckDuckGo's
  no-JS endpoint.

## Content policy (the point of Kitae)

The modern web is scripted; the readable web is not. Kitae handles that in
three tiers (engine-side, in `kitae_core`):

1. **Normal page** — rendered as-is (scripts ignored; only markup matters).
2. **`<noscript>` preference** — if the `<body>` has less than 200 characters of
   real text but the page carries a substantial `<noscript>` fallback, the
   noscript content *replaces* the body (static-site generators rely on this).
3. **Interstitial** — if the swapped body still has less than 100 characters of
   text and the page has ≥ 2 `<script>` tags, Kitae shows an interstitial
   explaining the page is JavaScript-dependent, with a
   `kitae:open-external:{url}` link that opens the URL in the system browser.

Other niceties: DuckDuckGo HTML results are decoded (`uddg=` → real target),
plain-text responses render in `<pre>`, `file://` URLs work, and HTTP errors
get a clean error page.

## Build & run

Requirements: Rust toolchain + Swift (Xcode CLT). No CocoaPods, no
.xcodeproj, no JS toolchain.

```sh
# The macOS app (builds the Rust staticlib, then swiftc → Kitae.app):
cd app && make          # or: make run

# The SDL2 dev harness (optional; brew install sdl2 sdl2_ttf):
cargo build --release
cargo run -p kitae -- https://text.npr.org
cargo run -p kitae -- --dump URL    # headless: text + links, CI-friendly
```

## Testing

```sh
cargo test --workspace          # 1149 passed, 3 ignored (serialized or parallel)
cd app && make native-test      # 9 AppKit/C-ABI contract tests
cd app && make ffi-smoke        # one-shot C consumer: frame bytes + version 20
```

## Known limits (current scope)

- Fetch is synchronous on the engine thread (per-tab; the UI never blocks),
  and an oversized stylesheet is rejected *after* the body is fetched.
- CSS covers a broad practical subset (selectors, cascade/layers/scope,
  flex/grid, multi-column, tables, backgrounds incl. multi-layer, transforms,
  filters, transitions/animations, `@media`/`@container`, `@font-face`), not
  the whole spec: `::backdrop` + `visibility: hidden` still paint, and
  `text-overflow` does not interact with `visibility`.
- Layout is a single-column flow plus the print frame; fragmentation
  properties are carried but paged media is AppKit's job (see below).
- Print: the media switch covers stylesheet `@media` (including
  `<style media>`/`<link media>`) and every `media` attribute on a resource
  — `<img media>`, `<picture><source media>`, `<video>/<audio><source media>`
  — all read the page's live environment, so a print run selects its print
  sources. Paged media (`@page`, running headers, forced breaks landing on a
  page boundary) is parsed and carried but not paginated by the engine —
  AppKit paginates the single-column frame.

## Roadmap

- **Product strategy & full roadmap** — where Kitae goes next: the document
  browser thesis, the market board (possible / very hard / nearly impossible),
  and the phased plan. See [docs/PRODUCT_STRATEGY.md](docs/PRODUCT_STRATEGY.md)
  and [docs/ROADMAP.md](docs/ROADMAP.md).
- **Embeddable engine** — Kitae's ecosystem role: the small, pure-Rust,
  JS-free web engine that apps and other browsers embed, the way the JS
  world embeds V8/QuickJS. See [docs/EMBEDDING_VISION.md](docs/EMBEDDING_VISION.md).
- **C++ as libraries** — JPEG/WebP decode and HarfBuzz shaping behind the
  same C ABI style, once images land. See [docs/CPP_ROLE.md](docs/CPP_ROLE.md).
- **kitae_ui** — a pure-Rust reactive UI library (React-like, no WASM, no
  script): components run on the author's server or in-process, the engine
  receives patch frames. See [docs/UI_LIBRARY_PLAN.md](docs/UI_LIBRARY_PLAN.md).
- **Kitae Bridge** — future *browser-layer* feature: a server-side execution
  proxy so the scripted old web works in Kitae with zero script in the
  engine, the app, or the user's machine. See [docs/BRIDGE_FEATURE.md](docs/BRIDGE_FEATURE.md).
