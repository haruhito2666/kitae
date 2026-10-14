# Kitae Security & Memory Audit

> **Original audit of the engine (`crates/`, Rust) and app (`app/Sources/`, Swift),
> performed 2026-09-06; status refreshed 2026-09-25 against the current tree.**
> Covers HTTPS/TLS, settings/privacy policy, security vulnerabilities, memory
> behavior, and (appendix) power efficiency. The finding IDs below preserve the
> original audit trail; current dispositions are called out in each section.
> Companion docs: `ENGINE_CORE_STATUS.md` (progress record),
> `BROWSER_PARITY_PLAN.md` (item I6 owns the fuzzing/hardening long tail).

## 0. Method & scope

- **Engine**: `kitae_net` (TLS/client/cookies/cache), `kitae_core` (FFI facade,
  browser loop, history, QTSQ password/signing), `kitae_qtsq` (vendored C FFI),
  `kitae_live` (server/ws/http/sha1), `kitae_render_gpu` (FFI), `kitae_shape`
  (leaked-slice pattern), `kitae_cli` dev tooling.
- **App:** the Swift app sources, including `EngineController`,
  `EngineFrame`, `TabManager`, `PageCanvasView`, `SettingsStore`,
  `SettingsWindow`, `DeepLink`, `PageInfoPopover`, `HistoryManager`, and the
  QTSQ players.
- **Method**: manual review of every `unsafe` block, every FFI alloc/free pair,
  every timer/display-link/loop, the wire decoder, the cookie/redirect/URL
  paths, the live-server request path, and the Swift retain graph.

**Current disposition (2026-09-25):** the original S-1 … S-7 and M-1 … M-4
findings are fixed; S-8 remains a product/privacy decision and P-4 remains
intentionally deferred. No critical/high open finding is recorded; remaining
risks and validation gates are listed in §5.5–§5.6. The current checkpoint is
1149 workspace tests passed (3 ignored), 9 native contracts, and an arm64
macOS 15.0 bundle; manual GUI and external release validation remain open.
The latest A11/B3/AX changes have automated and contract coverage, but this
refresh is not a new exhaustive manual review of those diffs.

| ID | Area | Original severity | Original one-liner |
|---|---|---|---|
| S-1 | Security | **Medium** | `file://` fetchable from any page origin (local-file read via a link/`@import`) |
| S-2 | Security | **Medium** | `kitae:open-external:` prefix is page-reachable → arbitrary scheme to NSWorkspace |
| S-3 | Security | Medium-low | kitae_live session tokens: SHA-1 of clock+counter, not an RNG |
| S-4 | Security | Medium-low | kitae_live: no Origin/Host check → localhost CSRF from any website |
| S-5 | Security | Low | No `catch_unwind` at the FFI / engine-thread boundary |
| S-6 | Security | Low | Lottie WKWebViews enable `allowFileAccess/UniversalAccessFromFileURLs` private prefs |
| S-7 | Security | Low | QTSQ password kept as plain `String`, never zeroized |
| S-8 | Security | Info | Browsing history persisted to disk unencrypted |
| S-9 | Security | Info | HTTPS/HSTS, mixed-content, privacy settings, and dependency-audit status (see current disposition below) |
| M-1 | Memory | **Leak (confirmed)** | `WKScriptMessageHandler` retain cycle in both Lottie views; no deinit |
| M-2 | Memory | Growth (documented) | Cookie jar has no eviction cap |
| M-3 | Memory | Bounded note | Per-tab frame buffer never shrinks; duplicate `lastFrameBytes` copy |
| M-4 | Memory | Bounded note | Per-tab history `Vec<String>` unbounded (URL strings only) |

> **Progress (2026-09-25):** the original security/memory fixes remain in
> place; Settings/Privacy, HSTS, v20, deep-link allowlisting, and the shared
> QTSQ/web raster-cache purge path are now implemented and covered by
> automated/native tests. Final release validation and the intentionally
> deferred **P-4** remain open.

---

## 1. Security vulnerabilities, one by one

### S-1 — `file://` is fetchable from any page origin ⚠️ fix first

- **Location:** `crates/kitae_net/src/lib.rs:704-712` (`fetch_with` serves
  `file://` straight from disk); `crates/kitae_core/src/lib.rs:693-709` and
  `2455-2474` (`click_link` resolves *any* href then navigates);
  `crates/kitae_net/src/lib.rs:663` (`normalize_input` passes `file://` through).
- **Description:** The engine happily fetches `file://` URLs regardless of where
  the request originated. Link navigation, `<link rel=stylesheet>`, `@import`,
  and `<img src>` all flow through the same fetch path with no origin check.
  A remote page can therefore reference local files.
- **Impact / scenario:** A malicious page renders
  `<a href="file:///Users/victim/Documents/notes.html">continue</a>`, or pulls a
  local stylesheet/image with `<link href="file:///…">` — on click (or
  automatically, for subresources) local bytes enter the engine and can be
  rendered or re-exfiltrated through further links (a local HTML page can embed
  file content into its own links). Real browsers block file access from
  network origins entirely.
- **Fix:** Thread the current page's scheme through the fetch path (a field on
  `Browser`/`Page`, e.g. `page_origin_is_local`) and reject in `fetch_with`:
  `file://` requests are only served when the *current page* is itself local
  (or the URL came from the address bar / open panel). Subresource and link
  paths both go through this gate.
- **Tests:** remote page with a `file://` stylesheet → not fetched; local page
  with `file://` subresource → fetched; address-bar `file://` → always allowed.
- **Status:** ✅ **fixed 2026-09-06** — `kitae_net::local_origin_allowed` +
  `fetch_for_origin` gate (`fetch`, `fetch_with`, `fetch_cached` all enforce
  it via a per-engine-thread page-origin record; kitae_core records the
  origin on every load and renders the block error page for link clicks);
  tests `local_origin_blocks_network_pages_and_cached_files`,
  `network_page_cannot_navigate_to_local_files`,
  `network_page_cannot_pull_local_subresources` all pass.

### S-2 — `kitae:open-external:` prefix is page-reachable ⚠️ fix first

- **Location:** `crates/kitae_core/src/lib.rs:698-701` and `2464-2471` — both
  link-click paths do `abs.strip_prefix("kitae:open-external:")` on the *page
  supplied* href and hand the remainder to `host.open_external`;
  `app/Sources/TabManager.swift:342-347` then calls `NSWorkspace.shared.open`.
- **Description:** The `kitae:open-external:` scheme was designed for
  engine-generated interstitials (the JS-gate page, `lib.rs:3693`), but the
  gate keys on the *prefix of any resolved href*, not on who generated the
  page. Any site can write `href="kitae:open-external:<anything>"`.
- **Impact / scenario:** A page link like
  `kitae:open-external:x-apple://…` (or any installed app's URL scheme) opens
  an arbitrary scheme handler on the user's machine without leaving the page —
  classic NSWorkspace abuse (deep-link exploitation of other apps).
- **Fix (choose one; first is smallest):** (a) restrict the open-external
  target to `http`/`https` URLs (validate before `open_external`); (b) tag the
  interstitial link at generation time (engine-internal link id) so only
  engine-authored links trigger the hatch; (c) a per-page
  `allows_open_external` flag set only when rendering the interstitial.
- **Tests:** remote page with `kitae:open-external:file:///…` and a non-http(s)
  scheme → nothing queued to the host; interstitial page → hatch still works.
- **Status:** ✅ **fixed 2026-09-06** — `open_external_permitted` in
  kitae_core (both `click_link` and `app_click` paths): engine-queued
  externals are restricted to `http`/`https`/`mailto`, anything else is
  refused with an about:blank error page; `TabManager.engineWantsExternalOpen`
  mirrors the allowlist before `NSWorkspace.open`. Tests
  `page_cannot_trigger_open_external_with_arbitrary_schemes` and
  `mailto_and_https_still_open_externally` pass.
- **Current app deep-link boundary (2026-09-25):** `Info.plist` registers the
  `kitae` scheme. `kitae://history` and `kitae://open` are handled locally;
  `open` accepts only HTTP(S) and approved `about:` targets, rejecting
  `file:`, `javascript:`, and arbitrary schemes.

### S-3 — kitae_live session tokens: SHA-1 of clock+counter, not an RNG

- **Location:** `crates/kitae_live/src/server.rs:711-728`
  (`new_session_token` = `sha1("{nanos}:{counter}")`, truncated to 20 hex).
- **Description:** The doc comment claims "unforgeable"; in fact the entropy is
  only the wall-clock nanosecond (plus a monotonic counter). An attacker who
  can estimate the server's clock (HTTP `Date` responses) can brute-force a
  narrow window of seeds offline and hijack a session by resuming with the
  guessed id. SHA-1 itself is fine here (a uniqueness hash) — the *inputs* are
  the problem.
- **Impact / scenario:** another local user (or a CSRF'd page, see S-4) resumes
  someone else's Live session and receives its patch stream. The server binds
  loopback only, which contains the blast radius — but the token is the
  capability and should be genuinely unguessable.
- **Fix:** replace with the OS RNG: `getrandom::getrandom` (pure-Rust, tiny) or
  `rand::random::<[u8; 16]>()` → hex/base64. Keep the `s-` prefix and length.
- **Tests:** existing `session_ids_are_unforgeable_tokens` (`server.rs:1064`)
  still passes; add a uniqueness test over 10k tokens.
- **Status:** ✅ **fixed 2026-09-06** — `new_session_token` now draws from
  the OS CSPRNG via the `getrandom` crate (workspace dependency, pure Rust,
  no API change, `s-` prefix preserved).

### S-4 — kitae_live: no Origin/Host validation (localhost CSRF)

- **Location:** `crates/kitae_live/src/server.rs` — `parse_head` (~line 730)
  and the WS-upgrade path accept requests with no `Origin`/`Host` check
  (grep for `Origin` in the crate: zero hits).
- **Description:** The dev server binds `127.0.0.1` (good —
  `crates/kitae_cli/src/templates.rs:106`), but any *website* the developer
  visits in any browser can script a WebSocket to `ws://127.0.0.1:8787/`.
  Same-origin policy stops other origins from *reading* responses, not from
  *making* the connection — without an Origin check the server happily
  upgrades and starts streaming patches and accepting events.
- **Impact / scenario:** a malicious page drives the developer's live-preview
  session: reads rendered state, injects click events (a nuisance today; a
  real vector once kitae_live hosts anything sensitive).
- **Fix:** on WS upgrade (and state-fetch GETs), require `Origin` to be
  `http://127.0.0.1[:port]` / `http://localhost[:port]`; reject others 403.
  Cheap, and standard practice for local dev servers.
- **Tests:** upgrade with a foreign `Origin` → 403; with a loopback Origin → 101.
- **Status:** ✅ **fixed 2026-09-06** — `origin_permitted` on the HTTP + WS
  request paths: any request carrying an `Origin` must be this server's own
  loopback origin, otherwise 403. Origin-less clients (curl, the engine,
  the tests) are unaffected.

### S-5 — No `catch_unwind` at the FFI / engine-thread boundary

- **Location:** `crates/kitae_core/src/ffi.rs` — the engine-thread command
  loop (~lines 130-218) and every `pub extern "C" fn` (none wrap their bodies
  in `catch_unwind`); `crates/kitae_qtsq/src/lib.rs` calls vendored C
  (`libqtsq.a`) on the same path.
- **Description:** Pure-Rust paths are panic-free by test (QTSQ decode is
  fuzz-lite tested to fail closed, `qtsq/lib.rs:1425-1450`), but a panic in
  the engine-thread closure silently kills that tab's loop (frozen tab, no
  error page), and a panic unwinding *through* an `extern "C"` entry point is
  UB (Rust aborts, taking the whole app down).
- **Impact / scenario:** one malformed future page construct, or one C-level
  bug in `libqtsq.a`, converts a tab-level failure into an app-level crash or
  a silent hang.
- **Fix:** wrap the engine-thread loop body in
  `std::panic::catch_unwind(AssertUnwindSafe(…))`; on panic, publish an error
  frame ("this tab crashed — reload") and continue the loop. For `extern "C"`
  fns, wrap bodies likewise and return the null/0 error convention. Report
  panics through the existing notifications queue.
- **Tests:** a debug command that panics → engine publishes the error frame
  and still answers the next command.
- **Status:** ✅ **fixed 2026-09-06** — the engine-thread command loop and
  every `extern "C"` entry point route through `run_caught` /
  `try_caught`: a panic now publishes a `panic\t{message}` notification
  ("this tab crashed — reload") and the thread keeps serving commands.

### S-6 — Lottie WKWebViews enable file-access private preferences

- **Location:** `app/Sources/LottieIconView.swift:51-52`,
  `app/Sources/LottieReloadButton.swift:44-45` —
  `config.preferences.setValue(true, forKey: "allowFileAccessFromFileURLs")`,
  `config.setValue(true, forKey: "allowUniversalAccessFromFileURLs")`.
- **Description:** Both chrome-icon WebViews use undocumented WebKit
  preferences that weaken file-URL sandboxing. The loaded content is only the
  bundled `.lottie` player (`loadFileURL(…, allowingReadAccessTo: resourceURL)`,
  `LottieIconView.swift:65`), so today the trust boundary holds — but these
  flags are exactly what App Review / security reviewers look for, and any
  future mistake about what loads into these WebViews inherits the relaxed
  policy.
- **Impact:** latent hardening issue; not currently exploitable (content is
  bundled, no network, `hitTest` returns nil so the view is click-transparent).
- **Fix:** the flags are unnecessary for lottie-web's same-file fetches —
  remove them and verify both icons still animate. Longer term, replace the
  WebViews with native CAAnimation (also P-3 below).
- **Status:** ✅ **fixed 2026-09-06** — both private preferences removed;
  P-3 later replaced the WebViews entirely with native CAShapeLayer icons,
  so the WebKit dependency is gone.

### S-7 — QTSQ password kept as plain `String`, never zeroized

- **Location:** `crates/kitae_core/src/lib.rs:328` (`qtsq_password:
  Option<String>`), `:739-741` (`set_qtsq_password` → `password.to_string()`),
  `:714` (cloned per decode). Crosses the FFI as a C string
  (`ffi.rs` `KITAE_CMD_SET_PASSWORD`, line 697).
- **Description:** The unlock password for encrypted `.qtsq` files lives in
  ordinary heap strings for the process lifetime and is never overwritten;
  copies also exist in the Swift→C call buffer.
- **Impact:** low for a local reader app (requires memory inspection of the
  user's own process), but it's the one credential the engine holds — treat it
  accordingly.
- **Fix:** adopt `zeroize` (or a hand-rolled `Drop` that volatile-writes
  zeros) for the stored password; accept the FFI copy as short-lived.
- **Status:** ✅ **fixed 2026-09-06** — `qtsq_password` is now
  `Option<zeroize::Zeroizing<String>>` (zeroized on replace and drop); the
  `zeroize` crate (pure Rust) was added to kitae_core.

### S-8 — Browsing history persisted unencrypted (informational)

- **Location:** `app/Sources/HistoryManager.swift:82-89` — JSON snapshot
  written atomically to disk 0.5 s after any change.
- **Description:** Same as Safari's default (plaintext history file in the
  user profile), so not a defect vs. platform norms — recorded so it's a
  *decision*, not an accident. Contents: full URL + title list, no retention
  cap in the file.
- **Options (if ever wanted):** file protection via `NSFileProtectionComplete`,
  a retention cap, or a "clear on quit" setting.
- **Status:** ✅ **2026-09-06** — the "no retention cap" claim was stale:
  `HistoryManager.add` already caps at **2000 entries** (oldest dropped).
  **2026-09-25:** Settings now provides clear-history and clear-on-quit
  controls. At-rest encryption (`NSFileProtectionComplete`/Keychain) remains
  a product decision, not an implemented security fix.

### S-9 — HTTPS, privacy, and dependency policy (informational)

- **Current implementation (2026-09-25):** `kitae_net` records valid
  `Strict-Transport-Security` responses from HTTPS origins in a capped
  in-memory store with expiry, `includeSubDomains`, and effective-subdomain
  matching. HTTP requests covered by HSTS upgrade before dispatch; the
  top-level HTTPS-only setting upgrades navigation, while the insecure-
  navigation setting emits an FFI confirmation request.
- **Privacy policy:** Settings → Privacy is pushed to every engine through
  `KITAE_CMD_SET_PRIVACY_POLICY` / `KITAE_CMD_SET_SEARCH_PROVIDER`. App
  defaults are HTTPS-only off, insecure-HTTP warning on, third-party-cookie
  blocking on, and DNT on. DNT headers, same-site third-party-cookie
  blocking, and selectable search providers are covered by `kitae_net` tests.
  Page Info exposes the active HSTS policy and can delete it.
- Password input values are masked in the display list and excluded from AX
  labels; the engine retains the value only for form submission.
- **Already shipped in the same area:** mixed-content blocking is enforced at
  all seven core subresource fetch sites (§5.2); `deny.toml` and
  `tools/deps-audit.sh` are the local dependency gate.
- **Remaining risks:** HSTS has no preload list, the stable UA remains a
  fingerprint, there is still no OS sandbox/site isolation, and this repository
  has no CI. HSTS snapshots are persisted atomically in Application Support;
  they are not a substitute for a preload list. These are tracked limitations,
  not claims that HTTPS/HSTS is absent.

## 2. Memory leaks & growth, one by one

### M-1 — `WKScriptMessageHandler` retain cycle in the Lottie views (confirmed leak) ✅ fixed

- **Location (as found):** `app/Sources/LottieIconView.swift:50` and
  `app/Sources/LottieReloadButton.swift:43` —
  `config.userContentController.add(self, name: "onReady")`. Neither file had
  a `deinit` or any `removeScriptMessageHandler` call.
- **Mechanism:** `WKUserContentController` **strongly** retains its message
  handlers. The view owns the webview → the webview's configuration → the
  userContentController → back to the view. That cycle meant neither the view
  nor its WKWebView (and its WebContent process) could ever deallocate.
- **Fix (shipped):** the standard inner-handler pattern, in both files: the
  class no longer conforms to `WKScriptMessageHandler`; a private
  `ReadyHandler: NSObject, WKScriptMessageHandler` holds the owner `weak` and
  relays the single `onReady` message to a private `didBecomeReady()`. A
  `deinit` was added to both views (stops any in-flight load so the
  WebContent process tears down promptly). The cycle is broken even if a
  teardown hook were missed, because nothing strong points back at the view.
- **Test:** put the app through tab open/close cycles and check
  `view.didMoveToWindow == nil` deallocations (or a unit target with
  `objc_retainCount`); WebContent process count stays flat in Activity Monitor.
- **Status:** ✅ **fixed 2026-09-06** — both Lottie views rebuilt on the
  weak-inner-handler pattern; `deinit` added; app builds clean.

### M-2 — Cookie jar has no eviction cap (documented, unbounded growth)

- **Location:** `crates/kitae_net/src/lib.rs:418-426` — the jar's own doc
  comment says it: *"no eviction cap — sites that stuff hundreds of cookies
  just grow it until process exit."* `CookieJar { cookies: Vec<Cookie> }`,
  inserted on every `Set-Cookie`, deleted only by Max-Age=0.
- **Impact:** a hostile or broken site can grow engine memory at will via
  repeated `Set-Cookie` headers (each ~100-500 B, so 100k cookies ≈ tens of
  MB) — a slow leak in practice, bounded only by process exit. No cap means
  the "small by reader-browser standards" assumption is attacker-controlled.
- **Fix:** enforce RFC 6265 §6.3 style limits, smallest first: cap per-domain
  (50) and total (3000), evict oldest-accessed; also cap name+value length
  (4 KiB per cookie). ~20 lines in `CookieJar::store` plus tests.
- **Tests:** set 60 cookies on one domain → 50 kept, oldest evicted; 4 KiB
  value cap; Max-Age=0 still deletes before eviction.
- **Status:** ✅ **fixed 2026-09-06** — `COOKIE_MAX_PER_DOMAIN = 50`,
  `COOKIE_MAX_TOTAL = 3000` and a 4 KiB per-cookie budget are enforced by
  `CookieJar::enforce_caps` (expired-then-oldest-first eviction, plus a
  `last_access` LRU stamp); test `cookie_caps_evict_oldest_and_per_domain`.

### M-3 — Per-tab frame buffer never shrinks; duplicate frame copy ✅ fixed

- **Location:** `app/Sources/EngineController.swift` — `frameBuffer: [UInt8]`
  (started at 1 MiB, grew to each tab's high-water frame size and was only
  ever reused) plus the separate `lastFrameBytes` copy kept for the GPU tile
  renderer (it re-parses the raw wire frame).
- **Fix (shipped):** shrink-with-hysteresis in `poll()` — after a frame lands,
  if the buffer is above the 1 MiB floor *and* more than 2× the current
  frame's size, it is reallocated to `length + 4096`. A tab that once
  rendered one heavy page no longer holds MBs of frame buffer at rest: the
  retained buffer converges to ~1 frame + slack instead of
  largest-frame-ever, and the 2× threshold prevents thrash on alternating
  sizes. `lastFrameBytes` stays (the GPU renderer needs the raw wire bytes),
  but it is inherently bounded to the current frame.
- **Status:** ✅ **fixed 2026-09-06** — `EngineController.poll()` shrinks the
  buffer; app builds clean (`Built Kitae.app`).

### M-4 — Per-tab history `Vec<String>` is unbounded ✅ fixed

- **Location:** `crates/kitae_core/src/lib.rs` — `Browser.history: Vec<String>`
  and the three push sites (`navigate`, `load_html`, form submit).
- **Fix (shipped):** new `pub const HISTORY_MAX: usize = 500` and a private
  `Browser::push_history` that truncates the forward tail (unchanged browser
  semantics), pushes, then evicts oldest entries from the front past the cap.
  All three call sites route through it; `pos` lands on the newest entry so
  back/forward behavior is identical to before the cap.
- **Status:** ✅ **fixed 2026-09-06** — test
  `history_caps_at_max_evicting_oldest` proves the cap (506 pushes → 500
  entries, oldest six evicted, back/forward still navigate correctly);
  kitae_core 104 tests green.

### Verified clean (no findings) — engine & app

- **FFI ownership pairs are complete and Swift honors them:** 
  `kitae_engine_new` → `kitae_engine_destroy` (`Box::into_raw`/`from_raw`;
  `EngineController.deinit` calls destroy — fires on tab close because the
  tab→delegate references are `weak`, so no retain cycle there);
  `kru_gpu_init` → `kru_gpu_shutdown`; every `kru_gpu_render_tile` result is
  released via `kru_gpu_pixels_free` including the copy path;
  every `kitae_qtsq_*_decode` result is released via `kitae_qtsq_free_buffer`
  and the Swift holders implement `Drop`.
- **`kitae_shape`'s leaked `'static` slice** is reconstructed in `Drop` exactly
  once (the documented leaked-slice pattern; no double-free).
- **Engine frame mailbox** is a single-slot `Option` — replaced on publish,
  consumed on read; no queue growth.
- **Wire decoding (`EngineFrame.swift`)** — every `read UInt32` length field
  goes through `guard offset + n <= data.count` bounds checks before slicing;
  no `Data` precondition crashes were found on truncated v20 frames, including
  the media-item `controls` byte, extended scroll metadata, and the optional
  length-delimited `AXS1` semantics tail. The native contract also pins the
  legacy-frame fallback and rejects a truncated semantics tail.
- **HTTP cache** is capped (64 entries / 300 s TTL); **POST responses are
  never cached** (correct).
- **Swift lifecycle:** all `Timer` closures capture `[weak self]` and are
  invalidated; `TabManager`/`EngineController` delegate refs are `weak`;
  `PageCanvasView`'s `CADisplayLink` is invalidated when animations end and
  caches are cleared; video/audio players invalidate their tick timers on
  pause; QTSQ decode paths fail closed and free their buffers on every error
  branch.

## 3. How Kitae handles HTTPS / TLS (verified clean)

- **Stack:** one shared blocking `reqwest::blocking::Client` (0.12.28) per
  `HttpFetch` in `crates/kitae_net/src/lib.rs`, built with
  **`rustls-tls`** — the crate docs say it explicitly: *"rustls — no OpenSSL,
  per the security brief."* Pure-Rust TLS 1.2/1.3 with modern defaults; no
  `native-tls`, no OpenSSL linkage.
- **Verification is never weakened:** `danger_accept_invalid_certs` and
  `danger_accept_invalid_hostnames` appear **zero** times in the tree.
  Self-signed, expired, and hostname-mismatched certificates fail the normal
  way → `FetchError::Transport` → the standard error page.
- **Timeouts:** 30 s total per request, 10 s connect (covers TCP + TLS
  handshake) — no hung sockets, no stuck tabs.
- **Scheme discipline:** `normalize_input` (line 663) upgrades bare hosts to
  `https://` (only `localhost`/IPs get `http://`); unknown/dangerous schemes
  are rejected by fetch → error page, never executed (there is no script
  engine to target anyway).
- **Cookies over TLS:** Secure-flagged cookies are never accepted or sent
  over plain http (Phase O2); the jar enforces RFC 6265 path matching and
  rejects `Domain=` forgery that doesn't suffix-match the host.
- **Redirects:** reqwest default policy (≤10 hops); `final_url` after the hop
  chain is the base for relative-link resolution, so a redirect can't spoof
  the origin of subsequent fetches.
- **Historical gaps at the audit snapshot:** no HTTPS-only/HSTS, no
  mixed-content block, UA fingerprint. Their current disposition is recorded
  in S-9 and §5.2; HSTS is now persisted but not preloaded.

## 4. Recommended fix order (smallest blast radius first)

| Order | Item | Size | Why first |
|---|---|---|---|
| 1 | **S-1** file:// origin gate | M | remote-page → local-file read; the only path where page content touches the user's disk |
| 2 | **S-2** gate `kitae:open-external:` | S | page-reachable OS scheme launch; 5-line validation once S-1's plumbing exists |
| 3 | **M-1** Lottie handler retain cycle | S | the one confirmed leak; pure app-side, no engine risk |
| 4 | **S-3** RNG session tokens + **S-4** Origin check | S | both kitae_live, dev-time only, ~1 hour combined |
| 5 | **M-2** cookie jar caps | S | attacker-controlled growth; small, well-tested area |
| 6 | **S-6** drop private WebKit prefs | S | trivial once M-1's handler refactor lands (same two files) |
| 7 | **S-5** `catch_unwind` boundaries | M | robustness, not exploitable; do with parity item I6 |
| 8 | **S-9** dependency audit in CI | S | local `tools/deps-audit.sh` is clean; CI remains absent |
| — | S-7 zeroize · S-8/M-3/M-4 recorded | S | discretionary |

Every fix ships with the tests listed in its section. Current serialized and
default-parallel full workspace baselines are both green; §5.5–§5.6 retain
the explicit sandbox, platform, manual-GUI, and release gates.

## 5. I6 hardening pass — fetch policies & untrusted-input fuzzing (refreshed 2026-09-25)

Parity item **I6** ("security hardening: strict fetch policies, URL-parse
fuzz parity, sandboxing review") closes the mixed-content half of S-9 and
adds a standing mutation program over Kitae's first-party HTML, CSS, URL/
policy, and QTSQ entry points. Third-party media/font decoders retain their
own corpus/fail-closed tests but do not yet have dedicated seeded mutation
targets. Current Settings/Privacy and HSTS controls are part of the shipped
browser layer; companion doc: `docs/FUZZING.md` (parity item I4).

### 5.1 Threat model (no-JS engine)

Kitae executes no script, so the classic web exploit classes (script
injection, drive-by exploitation, script-driven CSRF) do not exist. What a
hostile page *can* do is feed bytes to the engine's parsers and steer
fetches:

- **Parsers fed page bytes:** the HTML tokenizer/tree builder, CSS tokenizer
  and property parsers, URL resolver, cookie parser, Rust image decoders
  (`png`, `zune-jpeg`, `gif`, `image-webp`, `resvg`), audio/media parsers,
  web-font container decoders and `rustybuzz` shaping inputs, plus the
  vendored QTSQ C decoder.
- **Fetch decisions the page influences:** every subresource URL (`<img>`,
  `<link>`, `@import`, `@font-face`), link clicks, and form posts.

The hardening target is therefore fail-closed behavior: the first-party paths
covered by the standing mutation suites preserve their no-panic/structural
invariants, media/font decoders retain their separate fail-closed tests, and
the fetch-policy surface is scheme-exact. The decoder coverage gaps in §5.4
remain explicit.

### 5.2 Mixed-content blocking (S-9 remainder, closed)

- **Policy:** `kitae_net::mixed_content_allowed(page_base, sub_url)` — a page
  loaded over `https` must not pull subresources over plaintext `http`,
  where a network attacker could rewrite content the secure page will trust.
  Scheme-exact in both directions; `data:`/`about:` subresources and
  unparseable bases are unaffected (the policy only tightens the
  https→http edge). Loopback hosts (`localhost`, `*.localhost`, `127.0.0.0/8`,
  `::1`) are exempt — the secure-contexts spec treats them as potentially
  trustworthy and they never leave the machine, which keeps local dev
  servers working on https pages.
- **Enforcement — all seven subresource fetch sites in kitae_core:** the
  image decode paths (`raster_for` for the embedder path, the relayout image
  loop, the incremental-splice image loop, and the `publish_current`
  republish stamp), external stylesheets and `@import` chains
  (`load_stylesheets`/`expand_imports`, which now thread `page_base`
  through), and web fonts (`ensure_web_font_src` — fonts are active
  content: a network attacker who rewrites a font reshapes every glyph on
  the page). Refusals are silent-by-design: the image/declaration is
  dropped and the cascade keeps the previous value, matching browser
  treatment of mixed content in a no-JS pipeline.
- **Tests:** unit truth tables in kitae_net
  (`mixed_content_blocks_https_to_http`,
  `mixed_content_exempts_loopback_origins` — including spoof shapes like
  `localhost.evil.example` and `127.0.0.1.evil.example`), plus four engine
  tests using a real loopback HTTP server: stylesheets and `@import` are
  refused on https pages, loopback stylesheets on https pages still load
  (positive control), a mixed-content image is refused (payload `None`, no
  raster), and a mixed-content `@font-face` never registers.

### 5.3 URL-resolution fuzz parity

`kitae_net::resolve` is the single choke point every page-supplied href
flows through. `crates/kitae_net/tests/fuzz_urls.rs` holds it to two
properties over mutated corpora (scheme zoo, userinfo, IPv6, backslashes,
bad ports, `%` soup, control characters):

- **No panic, ever** (the primary fuzz invariant), and
- **url-crate parity:** whenever `resolve` returns `Some` for a joinable
  href, the result equals `Url::parse(base).join(trimmed)` — the engine's
  idea of a URL can never diverge from the audited `url` crate, which is
  what reqwest/rustls will actually dial.

`about:`/`kitae:` passthrough (trimmed, unjoined) and the mixed-content
policy's scheme-exactness are pinned by their own fuzz tests.

### 5.4 Untrusted-input surfaces × fuzz coverage

| Surface | Home | Fuzz coverage | Invariant |
|---|---|---|---|
| HTML tokenizer + tree builder | kitae_html | `tests/fuzz_html.rs` (seeded mutations) | no panic; arena consistency (parent pointers, ids < len, acyclic); root is `<html>` |
| Fragment parsing | kitae_html | `tests/fuzz_html.rs`, 8 contexts + tokenizer streaming parity | no panic; fragment-root discipline; batch/stream token equivalence |
| Streaming parse | kitae_html | `tests/fuzz_html.rs`, random chunking | chunked == whole-parse (the O4 contract) |
| CSS parser + evaluators | kitae_css | `tests/fuzz_css.rs` | no panic across stylesheet/declaration/color/length/font-face/condition/public value parsers, incl. cyclic `var()` |
| URL resolver + policy | kitae_net | `tests/fuzz_urls.rs` | no panic; url-crate parity; policy scheme-exact |
| Cookies | kitae_net | byte-flip + cap tests (M-2) | fail-closed attribute parsing; caps enforced |
| Images | `kitae_media` Rust decoders | corpus/unit, bomb, truncation, and fail-closed tests; **no dedicated seeded mutation target yet** | decode failure becomes payload-`None`; dimension/decode guards apply |
| Audio/video containers | `kitae_media` + Symphonia + MP4/WebM demux | fixture/garbage/unit coverage; **no dedicated seeded mutation target yet** | demux/decode fails closed; unsupported Opus is a documented gap |
| QTSQ | vendored C | byte-flip suite in-kit + `crates/kitae_qtsq/tests/fuzz_qtsq.rs` across probe/decode/video surfaces | signature verified before trust; unlock gate; fail-closed mutation coverage |
| Text shaping | `kitae_shape` / `rustybuzz` | script/cluster/font-feature unit coverage; **no dedicated byte-mutation target yet** | only registered/gated font bytes reach the shaper |
| Web-font containers | `kitae_media` / `kitae_core` | WOFF/WOFF2 round-trip/malformed tests; container sniffers reject non-sfnt/woff shapes; no seeded mutation target yet | registration fails closed |

The deterministic seed corpora make the first-party no-panic properties a
**blocking test** rather than an occasional exercise. The image/audio/font/
shaping rows above remain honest coverage gaps, not implicit fuzz coverage.
`docs/FUZZING.md` records the escalation path.

### 5.5 Sandboxing and policy review — remaining risks (honest list)

- **HSTS/HTTPS-only is shipped, with limits:** HSTS policies are persisted
  atomically in Application Support and restored at launch, but there is no
  preload list. HTTPS-only applies to top-level navigation; the
  insecure-navigation warning is a host confirmation, not a network-level
  guarantee.
- **Single process, no site isolation:** all tabs share one engine process;
  there is no per-origin isolation. Rust memory safety and the C decoders'
  fail-closed behavior are the actual boundary — a parser bug costs
  "rendered garbage", not code execution, but the isolation claim is
  process-wide, not per-site.
- **No OS-level sandbox yet:** the app is not App-Store-sandboxed
  (`com.apple.security.app-sandbox`) and the engine thread has no seatbelt
  profile. Tracked as future work; with no JS engine the urgency is
  bounded, but it is the right end-state.
- **`file://` access stays gated by S-1's per-engine-thread origin record**
  (network origins cannot read local files; address-bar and local pages
  can) — unchanged by I6.
- **The UA string is a stable fingerprint** (privacy, not integrity).
- **Nested scroll/AX state is not a security boundary:** per-container x/y
  offsets, scroll-aware AX frames, role/value/state semantics, live-region
  announcements, and press/increment synthesis are host-local UI behavior.
  The shared QTSQ/web raster cache is bounded and purgeable; payload IDs are
  lookup handles, not authority.
- **Deep links remain allowlisted:** registered `kitae://` targets are
  restricted in `DeepLinkParser`; page-authored external links are governed
  separately by S-2.

### 5.6 Final validation gate (2026-09-25)

- Serialized and default-parallel `cargo test --workspace`: 1149 passed, 3
  ignored; native contract tests: 6/6; `make`: app bundle up to date.
- `cargo check --workspace`, `cargo fmt --all -- --check`, and
  `./tools/deps-audit.sh` pass. The built arm64 executable reports
  `minos 15.0`, matching the bundle and native-test target.
- Manual GUI checks for A11 selection/IME, HSTS/settings, deep links,
  nested scroll/transforms, AX roles/actions/live regions, v20 media, QTSQ
  cache clearing, Markdown typography, and supported QTSQ video interaction
  remain. Clean-machine/distribution validation and package publication/tagging
  remain external release gates.

## Appendix A — Power efficiency (from the same audit)

The engine design is power-friendly by construction: no JS VM, no GC, no
async runtime; fetches block a per-tab thread so the main thread never spins;
animation clocks run only when the host provides them. Findings, ranked:

| ID | Finding | Impact | Fix |
|---|---|---|---|
| P-1 | `CloudBackdropView` rendered its Metal shader at a **continuous 60 fps** (`preferredFramesPerSecond = 60`, `isPaused = false`, `CloudBackdropView.swift`); it paused only on window removal — GPU/CPU burn even while idle in the background | **Biggest single power item in the app** | ✅ **fixed 2026-09-06** — frame rate halved to **30 fps**; animation runs only while the window is attached **and** key (a 1 Hz heartbeat re-checks `window?.isKeyWindow` and sets `mtkView.isPaused`; NSView exposes no key-window callback in this dialect), so a background/unfocused app idles the shader entirely |
| P-2 | The fixed **60 Hz poll timer** (`TabManager.start()` → `pollAll()` over every tab) never backed off; each tick locked every tab's mailbox mutex even when nothing changed — constant main-runloop wakeups that fight App Nap | Moderate; scales with tab count | ✅ **fixed 2026-09-06** — `TabManager` now self-schedules a one-shot per tick at **60 Hz while any engine is busy** (frame produced or loading) **and backs off to 10 Hz** after ~1 s of quiescence. The consume-on-read frame protocol makes the slow path lossless; loading flags catch new navigations within one idle tick |
| P-3 | Lottie icons run **one WKWebView + WebContent process + JS engine each** (`LottieIconView`, `LottieReloadButton`) to draw two toolbar glyphs; load-time cost mostly, but heavyweight | Low-moderate | ✅ **fixed 2026-09-06** — both views replaced with native `CAShapeLayer` implementations (`SecurityIconView.swift`, `NativeReloadButton.swift`) that draw a spinner/checkmark/triangle and a rotating circular arrow with Core Animation. No WebKit, no JS engine, no WebContent process, no script-message-handler retain cycle. Same class names and `setState`/`setLoading` interfaces, so `ChromeView.swift` needed zero changes; `Built Kitae.app` |
| P-4 | kitae_live client/server poll loops use `WouldBlock → sleep(1–2 ms)` (~1000 wakeups/s while waiting) | Dev-time only | ⏳ **deferred by design** (audit's own recommendation) — a dev-loop refactor to `poll()` risks churn in hand-rolled WebSocket code for zero user-facing gain; re-evaluate if kitae_live ever runs outside `kitae dev` |
| ✓ | Correct today: `CADisplayLink` invalidated when GIF/animation players end; video tick timer exists only while playing; history save + resize are debounced one-shots; `MTKView` pauses on window removal; engine fetches never spin the main thread | — | keep |

## Appendix B — What was checked and found clean (security)

- **No script attack surface at all** — no JS engine, no WASM; `javascript:`-style
  links can't resolve and never execute. Engine-generated pages (error,
  plain-text, interstitials) consistently escape text via `escape_html`
  (`kitae_core/src/lib.rs`), so static-HTML XSS is the only web-content class,
  and it renders as text.
- **QTSQ decode fails closed**: in-crate byte-flip coverage plus
  `crates/kitae_qtsq/tests/fuzz_qtsq.rs` exercise probe, text/image/audio/
  tensor/video mutations; ML-DSA-65 signatures are verified before content
  trust; password unlock gates every decode path. Decoded QTSQ/web rasters
  are URL-keyed, bounded, and purgeable; QTSQ video decoder/source state is
  separately bounded. Cache eviction is a memory policy, not a trust decision.
- **Path traversal is handled** in kitae_live static serving — canonicalize +
  prefix-confine, with a regression test (`server.rs` traversal test).
- **URL allow-list for `file://` in the CLI templates** and loopback-only
  binding (`127.0.0.1`, `kitae_cli/src/templates.rs:106`).
- **Bounds-checked FFI string reads** (`read_cstr` in `ffi.rs`); host-supplied
  strings are length-limited (`externalBufferLimit = 4096`,
  `EngineController.swift:196`).
- **No secrets in logs**; notifications/panics don't echo cookie or password
  material.

---

*Original audit performed by Cline on 2026-09-06; disposition and validation
refreshed 2026-09-25. The original finding locations refer to that snapshot
and may drift as fixes land; each status change should update its item here.*








