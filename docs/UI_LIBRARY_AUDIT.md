# UI Library Code Audit — kitae_ui / kitae_ui_macros / kitae_live / kitae_cli

**Date:** 2026-09-09
**Scope:** the four UI-library crates only (per request). Engine crates were opened solely to verify contracts that cross the boundary (`SetStyle{""}` semantics in kitae_core `ui.rs`; the two-way-binding wire path).
**Method:** 100% of the four crates read line-by-line; every doc claim checked against the code; cross-module contracts traced to ground truth (grep across crates); `cargo clippy` and the full test suite re-run as baseline. Assessment only — no code was changed.

**Baseline:** clippy exits 0 (warnings only). `cargo test -p kitae_ui -p kitae_ui_macros -p kitae_live -p kitae_cli`: 16/16 suites `test result: ok` — 0 failures (~137 tests, 2 ignored).

## Verdict

The protocol layer (wire decode, WS framing, sessions, resume, static serving) is solid: hostile-input-safe decode, real CSPRNG session tokens, canonicalize-confined static dirs, correct RFC 6455 framing rules. The reactive core is architecturally sound but carries one reliability class to fix before long-running use (panic-unsafe flush guards) plus several leaks. There is exactly **one genuine security hole**, and it is small and cheaply fixed: the localhost CSRF origin gate uses prefix matching and is bypassable. Nothing found invalidates the architecture; the fix list below is ~a dozen focused changes.

---

## High — fix before any exposure beyond localhost dev

### H1. Localhost CSRF gate has a prefix-match hole — `kitae_live/src/server.rs` (`origin_permitted`, ~755–761)

```rust
o == "null" || o.starts_with("http://127.0.0.1") || o.starts_with("http://localhost")
```

`http://127.0.0.1.evil.com` and `http://localhost.attacker.tld` pass. A malicious page open in any browser can complete the WebSocket handshake against a developer's `kitae dev` server (session creation, event dispatch, live patch stream). The code's own S-4 comment claims this exact attack is prevented.

**Fix shape:** parse the Origin URL and require the host component to be exactly `127.0.0.1`, `localhost`, or `[::1]` (optionally pin the port). ~10 lines, easily tested.

### H2. Reactive-flush guards are not panic-safe — `kitae_ui/src/signals.rs` (`DEPTH`, `EFFECT_DEPTH`, `ACTIVE`)

`ScopeGuard` in hooks.rs is Drop-based and panic-safe, but the three flush counters in signals.rs are incremented/decremented inline. A panic inside an effect/handler during a reactive flush leaves the thread-local counters elevated forever: every later flush on that thread sees depth > 0 / ACTIVE set and bails, so `PENDING` signals never flush — **reactivity silently freezes thread-wide**, with no error surfaced.

Reachable from real code: a user handler that panics; `ErrorBoundary` (kit.rs) deliberately catches render panics and keeps the app alive — exactly the case that then runs on a wedged signal world; the testing fuzz harness catches unwinds, leaving counters corrupted for later tests on the same thread.

**Fix shape:** wrap each counter in a Drop guard (or run flush bodies under `catch_unwind` that resets the counters), so a panic degrades to "this update was lost", not "reactivity is dead".

### H3. `ErrorBoundary` documentation over-promises — `kitae_ui/src/kit.rs`

The doc says the scope stack is panic-safe and "later renders keep working". True for hook state (Drop-based guards); false for signal state (H2) — after a caught panic, components render but never update again. Until H2 is fixed, the doc describes a guarantee the crate does not provide, which is precisely how H2-class bugs ship unnoticed.

---

## Medium

### M1. Keyed reconcile panics on duplicate keys — `kitae_ui/src/diff.rs` (`keyed_reconcile`)
Duplicate `key:` values hit a duplicate-map insert and an `expect("kept key must be live")`. React warns and recovers; here it is a hard crash of the site process. Realistic trigger: rendering user/DB data keyed by an id that contains duplicates. **Fix:** warn + fall back to index reconcile for the offending list.

### M2. Hook slot type mismatch is a hard panic — `kitae_ui/src/hooks.rs`
A conditional swap of component type at the same position hits `expect("hook slot type mismatch")`. The classic rules-of-hooks violation, but a diagnostic + slot reset matches the framework's maturity elsewhere.

### M3. Handlers are never unregistered on removal — `kitae_ui/src/diff.rs`
`RemoveNode` prunes the tree but not the session's handler map; closures of removed subtrees live for the whole session. Every dynamic list (tabs, todos, search results) accumulates them. Memory leak, plus stale-handler hazards if an id is reused.

### M4. Signal slab never frees — `kitae_ui/src/signals.rs`
The slab's free-list is dead code: dropped signal cells are never returned, so signal memory grows monotonically. `Form` (components.rs) allocates fresh signals per keystroke, compounding it. Long-lived dashboards — the showcase use case — feel this first.

### M5. `use_resource` fetch races + unbounded threads — `kitae_ui/src/resource.rs`
No request versioning: rapid dependency changes let older fetches resolve last and overwrite fresh data. Every refetch spawns a new thread with no coalescing or cap — a debounce-less input can spawn hundreds.

### M6. Effects outlive their component — `kitae_ui/src/effects.rs`
No per-component teardown: an effect registered under a scope keeps re-running on its trigger after the component is gone from the tree. Same leak family as M3.

### M7. Two-way binding is test-harness-only — expectation gap
`VElement.bindings` is consumed only by `testing.rs::fire()`; the wire's `NodeData` has no bindings field, so live pages must hand-write `on:input = move |e| …` (confirmed by the Todos acceptance test in kitae_core `live.rs`, ~340–436). But `kitae_ui/src/lib.rs`'s Todos doc example reads as if `value: draft.get()` write-back works on live sites. Either ship bindings on the wire or fix the doc — the current state invites bugs.

### M8. Hostile request heads are never rejected or reaped — `kitae_live/src/server.rs` (`poll_heads`, ~354–395)
The 32 KiB head cap `break`s out of the read loop, but a buffer that broke without `head_end` then falls through to the "Partial head: keep the bytes and the socket" path — so oversize heads are retained, not rejected. `heads` sockets are never TTL-reaped, and the Ok(0)/error paths keep the socket too: slowloris-grade unbounded retention. Dev/local severity only (the supported deployment), but the cap should actually cap. **Fix:** close the socket when a head exceeds the cap; reap heads idle beyond N seconds.

---

## Low-Medium

### L1. Silent truncation in wire encode — `kitae_live/src/wire.rs`
`put_u16(ops.len() as u16)` on Patch and `put_u16(*index as u16)` for insert/remove/move silently truncate above 65535 — a >64K-node hydration or >65K child index corrupts the frame rather than erroring. In-place fix without a wire bump: assert/error. Proper fix: u32 (+ KTLV version bump).

### L2. Two-way binding only for bare identifiers — `kitae_ui_macros`
`value: draft` binds; `value: self.draft` is silently display-only (the macro only checks for a single Ident). No diagnostic — a foot-gun.

---

## Low

- **`parse_declarations` naive `;` split** — `kitae_ui/src/vnode.rs`: breaks `url(data:…;base64,…)` style values.
- **Macro parse errors are unspanned panics** — `kitae_ui_macros`: compile-time only, but diagnostics are the point of a DSL macro.
- **Void tags with children render invalid HTML** — `kitae_ui/src/render.rs`: `<br>…</br>` is emitted instead of dropping the children.
- **`CACHE` never invalidated** — documented trade; still a correctness trap when content/ files change identity.
- **`Signal::set` has no equality skip** — every set reruns dependents; re-entrant `update`/`rerun()` can RefCell-panic. Cheap win: a `PartialEq` short-circuit behind a flag.
- **Warnings are process-global; `ToastHost` keys by index; `DatePicker` falls back to hardcoded (2026, 1)** — `kit.rs` / `components.rs` quality nits.
- **`LiveClient` sends the RFC sample key on every connection** — `kitae_live/src/client.rs`: fingerprintable and noncompliant (the key must be fresh per connection); no security impact locally.
- **`file_stamp` is mtime-only** — `kitae_cli/src/main.rs`: an edit that restores the same mtime with a different size is missed; hotreload.rs got this right (mtime+size).
- **Resume parsing accepts any `resume` substring** — `server.rs` `split("resume")`: cosmetic; the capability model (unforgeable 80-bit tokens) holds regardless.

---

## Verified clean (worth recording)

- **`wire.rs` decode**: fully Option-based cursor; truncation/short-input tested. Hostile input cannot panic.
- **`ws.rs` framing**: fragmentation refused, 16 MiB cap, unmasked client frames refused, control frames answered inline, partial frames buffered correctly.
- **`sha1.rs`**: FIPS 180-1 vectors + RFC 6455 §1.3 accept-token test. (SHA-1 here is the protocol's mandated handshake token — not a security boundary.)
- **`session.rs`**: dispatch refuses unknown refs/events (tested); resume validates ack < next_seq and within the history window, else re-Init; `next_seq` monotonic across init.
- **Session tokens**: getrandom 10 bytes = 80-bit CSPRNG capability (`new_session_token`, ~769–774) — unforgeable, tested.
- **Static serving**: `serve_static_dir` canonicalize + prefix confinement (~479–506), traversal incl. `%2e%2e` tested; `path_serves` segment-aligned matching (~668–683).
- **`hotreload.rs`**: versioned copies force fresh dlopen images, a failed load consumes the stamp, handles deliberately never closed (documented, with the reason).
- **`kitae_cli/src/main.rs`**: literal `env` launcher (nothing run through a shell), canonicalize + parent + is_file gates on the site binary path, charset-validated crate name, refuses to overwrite existing dirs.
- **`charts.rs`** NaN-safe; **`devtools.rs`** bounded; **`components.rs`**, **`testing.rs`**, **hooks.rs two-phase borrow**, **lib.rs/prelude**, **templates.rs** (two-mode Cargo.toml), **http.rs**, **dev.rs** all clean.

## Resolved during audit (checked, no finding)

- **`SetStyle { value: "" }` removal convention**: honored — kitae_core `ui.rs` apply site retains the declaration on empty value, upserts otherwise.
- **Bindings on the wire**: absent by design; live two-way = manual `on:input` (see M7 for the doc gap this creates).

---

## Recommended fix order

1. **H1 Origin gate** — ~10 lines, closes the only real hole.
2. **H2/H3 guard Drop-safety + doc correction** — restores the ErrorBoundary promise.
3. **M1/M2 panics → diagnostics** (duplicate keys, hook slot mismatch).
4. **M3/M4/M6 leaks** (handlers, slab, effect teardown) — same-shape fixes, one pass.
5. **M8 poll_heads** — actually reject over-cap heads + TTL reap.
6. **M5 resource** — request versioning + in-flight coalescing.
7. **L1/L2** — encode overflow handling + macro diagnostic.
8. The Low list opportunistically.

Items 1–3 change no wire format. Only L1's proper fix would touch KTLV v1.

---

## Fix record — ALL 22 findings resolved (2026-09-09)

Every finding above is fixed and verified. Verification: `cargo test --workspace --exclude kitae_render_gpu` = **808 passed / 0 failed** across 41 test binaries; kitae_ui 111/0 (incl. 7 resource + new parse tests), kitae_live 42/42. Clippy: no new warnings from the fixes (pre-existing style lints left untouched). No wire-format changes (KTLV v1 unchanged).

**High**
- **H1** `origin_permitted` now parses the Origin URL and requires the host component to be exactly `127.0.0.1` / `localhost` / `[::1]` — prefix match gone, tested.
- **H2** All three flush counters (`DEPTH`, `EFFECT_DEPTH`, `ACTIVE`) run under Drop guards (`TrackGuard`), so a panic mid-flush resets state and reactivity survives. Tested with an unwinding handler.
- **H3** `ErrorBoundary` doc states the guarantee precisely (hook state survives; signal world resets guards) instead of over-promising.

**Medium**
- **M1** Duplicate keys: `keyed_reconcile` detects collisions, warns, and falls back to index reconcile — no panic.
- **M2** Hook slot type mismatch: warns and RESETS the slot (re-runs the hook), matching React's recovery behavior. Tested.
- **M3** `prune_handlers` runs at all three `RemoveNode` sites (single, list, keyed reconcile); removed subtrees no longer leak closures. Tested.
- **M4** Signal slab is generational: freed cells return to the free list, handles carry a generation that detects reuse, `Signal::dispose` releases explicitly. Tested.
- **M5** Resources carry a `Flight`: every fetch has a generation; a landing whose generation is stale is discarded whole; kicks that arrive mid-flight COALESCE into one follow-up (deps-change kicks additionally invalidate the in-air fetch). `drain()` restructured to collect under a short borrow and apply outside it (fixes a latent re-borrow panic). Three new tests: coalescing, stale-discard, slot stability.
- **M6** Effects register per-scope and tear down with it; `EffectHandle::stop()` for manual cleanup.
- **M7** Todos doc example no longer implies live write-back from `value: draft.get()`; documents the manual `on:input` form.
- **M8** Over-cap heads now CLOSE the socket (no retention), and head sockets are TTL-reaped.

**Low-Medium**
- **L1** Wire encode is fallible: counts/indexes beyond u16 return an encode error instead of truncating the frame.
- **L2** `value: self.draft` (single-field paths) now take the AttrRef path — `Signal<String>` fields get two-way binding, other Display types render as before. Deeper paths/method calls keep Display.

**Low**
- **L14** `parse_declarations` splits `;` quote- and paren-aware (`url(data:…;base64,…)` and `content: "a;b"` survive); a malformed block (unclosed quote/paren) falls back to a lax re-split. Tested.
- **L15** `view!` parse errors and trailing tokens, and `#[component]` without a body, emit `::core::compile_error!` diagnostics instead of panicking inside the proc macro.
- **L16** Void tags drop children instead of emitting `<br>…</br>`.
- **L17** `clear_cached(key)` / `clear_cached_all()` added and exported in the prelude.
- **L18** `Signal::set_if_changed` (PartialEq short-circuit) + re-entrancy diagnostics instead of silent RefCell panics.
- **L19** Warnings are thread-local; `ToastHost` keys by a monotonic per-toast id and dismisses BY ID (`retain`) — removing one toast can no longer fire a neighbor's handler or re-key the list; `DatePicker`'s empty-value fallback browses the current month (civil-from-days, pure std) instead of hardcoded 2026-01.
- **L20** `LiveClient` sends a fresh random WS key per connection.
- **L21** `file_stamp` = mtime+size, mirroring hotreload.rs.
- **L22** Resume query parsing requires the exact `resume` parameter (was: any substring).
