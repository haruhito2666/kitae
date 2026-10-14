# QTSQ in Kitae — Status

> Living status doc for QTSQ support in Kitae (plan: `QTSQ_INTEGRATION_PLAN.md`).
> Updated 2026-09-25. Shipped items are separated from partial/deferred work;
> final app/GUI validation remains a release gate. Engine-wide status lives in
> `ENGINE_CORE_STATUS.md`.

---

## 1. Shipped & verified

### Plan stages J1–J4 + J6 (shipped 2026-09-04, visually verified)

| Stage | Delivered |
|---|---|
| J1 | `crates/kitae_qtsq` — vendored `libqtsq.a` + hand-written FFI; pure-Rust header probe; `decode_image`/`decode_text`/audio/video APIs |
| J2 | `.qtsq` navigation → typed viewer pages (image / text / audio / video / info); title = filename |
| J3 | Real pixels: `<img>` pages + shared URL-keyed decoded-raster cache (`kitae_qtsq_raster` C ABI) painted by the shell; stable payload IDs, LRU/byte-budget eviction, and purge/clear are wired; `qtsq_fit_dims` caps 1024 |
| J4 | Audio: decode on engine thread → temp CAF → system player (`qtsq_prepare_audio` + media registration) |
| J6 | Finder/Open-panel file open in Kitae.app; SDL shell `kitae file.qtsq`; share-sheet PNG export |

- **J5 (video)**: engine-side decode, frame stepping, transport state, tab-scoped
  player ownership, and the native play/pause/scrub bar are implemented.
  Dedicated GUI playback verification remains a release gate.
- Tests: serialized and default-parallel full workspace sweeps on 2026-09-25
  are green (1149 passed, 3 ignored).
- Example verified end-to-end: `~/Downloads/1368659.qtsq` renders in both SDL
  and Swift app.

### Decode-gap pass (2026-09-04, "Kitae should render any qtsq file")

Legacy v1/v2 files from before the current writer failed closed. Fixes, all
in `crates/kitae_qtsq`:

- **Legacy Dictionary text** (`native/qtsq/shim.c` `qtsq_compat_normalize`):
  old writers emit `[u32 inflated_size][zlib stream]` with **32-bit** dict
  counts, and (critically) DO set the ZLIB flag — so flag gating cannot
  discriminate; the shim content-sniffs the layout (0x78 0xda at offset 4 is
  unambiguous: a modern u64 raw_size that big would exceed the decoder's own
  1e13 cap), inflates, rewrites u32 counts → u64 counts, re-compresses, and
  rewraps as `[u64][zlib]`, setting ZLIB / clearing CHECKSUMMED. The three
  legacy texts now decode (DTCN_architecture 35,822 chars; implementation_plan
  8,541; xandeum-nodes 245,936).
- **Legacy NONE images**: `original_size` vs `singularity_size` mismatch is
  normalized at open; a ≤1-row pixel shortfall is padded (black) in
  `decode_raw_pixels`.
- **`probe()` hardening**: bounds-checked before reading version byte; stale
  v2-rejection test replaced (v2 is supported now) with an unknown-version
  test. Truncated files fail closed, never panic.

**Historical corpus snapshot (2026-09-04): 49/58 files decoded** before the
password-prompt feature. The table records that run's nine non-decodes; it is
preserved as history, not claimed as the current support matrix. The three
password-locked entries now have an implemented unlock path, but no refreshed
whole-corpus rate is claimed here.

| Group | Count | Disposition |
|---|---|---|
| Password-encrypted (start, glenn-carstens-peters, Kokode) | 3 | Unlocks with **feature ①** (password prompt) below |
| Old Temporal videos, IDX1 "malformed" | 2 | Unsupported legacy writer/decoder; fail closed |
| Raw (strategy 1, dt 0) | 2 | Info-page-only **by design** |
| Old Fractal / Layered-flux images (−4) | 5 | Unfixable here: vendored `libqtsq.a` is byte-identical to the upstream build; QTSQStudio cannot read them either |

### Text typography pass (2026-09-04, code complete — visual verification pending)

User report: "current text rendering is very messy and bad looking". Root
cause: decoded Markdown documents were dumped raw into `<pre>` — clipped
monospace lines showing the syntax itself (`#`, `**`, `|`). Fixes, all
written this session:

- **`crates/kitae_core/src/markdown.rs` (new)**: dependency-free Markdown →
  HTML. Blocks: ATX headings, fenced code (+language class), pipe tables,
  blockquotes (recursive), ul/ol lists with indent-stack nesting and wrapped
  continuation lines, `---`/`***`/`___` rules, paragraphs. Inline (runs over
  **pre-escaped** text, so a payload can never inject markup): `` `code` ``,
  `**strong**`, `*em*`/`_em_` (word-boundary guarded — `snake_case` stays
  literal, `2 * 3 * 4` is not emphasized), `[links]`, `![images]`.
  Gates: `looks_like_markdown` (marker score, one ATX heading conclusive)
  and `looks_like_json` (whole-payload `{…}`/`[…]`).
- **Viewer dispatch** (`qtsq_viewer_page` → `qtsq_text_body`): Markdown →
  rich HTML inside a `.qtsq-text` reader column; JSON and plain prose keep
  the monospace `<pre>` view (now wrapping instead of clipping).
- **`pre-wrap` support end-to-end**: `kitae_style` gains `white_space_wrap`
  (field, default, inherit, parser `white-space: *wrap*`), UA stylesheet
  `pre` → `pre-wrap` + `.qtsq-text { max-width: 780px }` (centered reader
  column via the existing auto-center rule); `kitae_layout::feed_pre` wraps
  each physical line word-by-word against the float band in wrap mode.
- **Kerning enabled**: `.kern: 0` removed from BOTH
  `CoreTextMeasurer.measure` and `PageCanvasView.cachedLine` (attributes stay
  identical between measurement and paint, so widths still match exactly).

**Remaining for this pass:** dedicated visual verification of a decoded
Markdown doc (headings/tables/lists/code) in Kitae.app remains pending. The
serialized workspace sweep and current app/native builds are green, but the
final GUI pass has not been repeated for the current tree.

---

## 2. The four features the user picked — ALL SHIPPED 2026-09-04

Workspace status on 2026-09-25: the four features remain shipped. Both the
serialized and default-parallel full workspace sweeps are green at **1149
passed, 3 ignored**.

### ① Password prompt for password-locked QTSQ — SHIPPED
- Engine: `Browser::set_qtsq_password` + session field; FFI command 24
  (`KITAE_CMD_SET_PASSWORD`); decode path threads the password through
  `fetch_result_state`/`qtsq_viewer_page`. `qtsq_locked` rides frame wire v9.
- Swift: locked info page (from a failed decode) shows a password prompt
  (secure field + "Unlock" → SetPassword command → re-navigate). CLI:
  `kitae --password <pw>`.
- Verified: locked corpus file prompts; correct password renders content;
  wrong password returns to the info page with an error line.

### ② Save-Page-as-QTSQ — SHIPPED (flagship)
- Archive format: `<!--kitae-archive v1\nbase=<url>\ntitle=<title>\n-->` +
  page HTML, encrypted to the device key via the vendored qtsq writer
  (`kitae_qtsq::encode_text_secure` → qtc compressor, per the user's
  correction) and signed (ML-DSA-65 device key).
- Engine: `Browser::save_page_qtsq` (outcome via notifications queue
  `"1\t{path}"` / `"0\t{msg}"`), FFI command 26 (`KITAE_CMD_SAVE_PAGE`).
  Reopen: session URL stays the `.qtsq` file URL; `Page.base_url` =
  archived base so links/subresources resolve against the original page.
  The v11 sig-verdict field (`QtsqSigState`) is carried forward in the current
  v20 frame.
- Swift: share-picker item **"Save as QTSQ…"** (index 1) → NSSavePanel →
  engine save → `drainNotifications` → status hint "Saved QTSQ archive:
  {path}". Locked-file prompt keys off `qtsq_locked`; signature banner on
  every viewer page from `qtsq_sig`.
- **Visually verified in the running app (GUI, no keystrokes)**: test page
  → File ▸ Share Page… → "Save as QTSQ…" → save panel → status hint
  "Saved QTSQ archive: /Users/priyanshuboruah/Documents/page.html.qtsq";
  archive probed encrypted (QTS magic, zero plaintext strings); reopen via
  `open -a Kitae` renders the archived content with the green
  "Signature verified — signed by this device." banner and provenance
  line; release-CLI dump shows "Signature verified" + correct content.
- `run_dump` now resolves archive links against the archived base (same
  rule as the engine), not the .qtsq file's directory.

### ③ Tensor visualization pages — SHIPPED
- `qtsq_viewer_page` tensor branch: shape/dtype header, one-pixel-per-value
  heat-map strip (`tensor_heatmap_dims` + `tensor_heat_color`, registered
  in the qtsq raster store, drawn via the display list) and a first-N
  values table. No compute framework, no JS.
- Verified: tensor fixture page shows shape, dtype, heat-map strip, and
  value table.

### ④ ML-DSA-65 signature verify — SHIPPED
- Vendored qtsq verify path wired through the shim
  (`qtsq_vault_verify_sig`); verdict on every viewer page and in the current
  v20 frame (`QtsqSigState`, introduced in v11: Unsigned / Valid / Invalid /
  Unverified). Green banner for a valid device-key signature; warning banner
  when tampered; unsigned pages show no banner.
- Verified: saved archive → green "Signature verified — signed by this
  device." banner; CLI dump concurs.

### Current app/frame integration (2026-09-25)

- Frame wire is **v20**: media items carry the `controls` byte used by web
  `<video>`; QTSQ viewer media keeps the docked transport path.
- Finder/Open-panel file opening and registered `kitae://` deep links are
  shipped. `kitae://history` opens history; `kitae://open` accepts only
  `http`/`https`/approved `about:` targets and rejects `file:`, `javascript:`,
  and arbitrary schemes.
- The decoded-raster cache is shared by QTSQ and web images, keyed by final
  URL, with stable IDs, 192 MiB/64-entry eviction, newest pinning, and a purge
  path that also clears web media and the network cache. QTSQ video
  decoder/source state is separately bounded to eight entries and 64 MiB;
  `qtsq_video_cache_is_bounded` covers count/byte eviction and recency.
- `kitae_qtsq` contributes 33 current automated tests (31 unit tests + 2
  dedicated mutation tests). These cover supported modern fixtures and
  fail-closed paths; they do not turn the historical 49/58 legacy corpus into
  universal support.
- HSTS policies are exported as a validated versioned snapshot and restored
  from Application Support at launch; Page Info deletion removes the effective
  parent policy when applicable.
- The app/native test and link targets are arm64 macOS 15.0; the built bundle
  and `LSMinimumSystemVersion` agree. Clean-machine/distribution validation is
  still an external release gate.

### Deferred / housekeeping
- Full legacy Temporal/IDX1 video compatibility and complete transport
  features (volume, fullscreen, PiP, captions) remain outside the current
  vendored decoder; the supported QTSQ video path is bounded and tested.
- Dedicated visual verification of the Markdown typography pass remains
  pending.
- `ENGINE_CORE_STATUS.md` is a companion status record, not the QTSQ
  validation log.

## 3. Final validation gate

- `cargo test --workspace -- --test-threads=1` and default-parallel
  `cargo test --workspace`: 1149 passed, 3 ignored.
- `make native-test`: 9 native contract tests passed; `make`: app bundle up to
  date; `cargo check --workspace`, `cargo fmt --all -- --check`, and
  `tools/deps-audit.sh`: pass.
- Still manual/pending: supported QTSQ video interaction, Markdown viewer
  typography, QTSQ cache purge/reload, and the broader release checklist's
  nested scrolling/AX, Settings/Privacy/HSTS, deep-link, and v20 video-control
  checks. Package publication, tagging, and clean-machine distribution checks
  are also still external/manual gates.

---

## 4. Commands cheat-sheet

```sh
# fast iteration
cargo build -p kitae_qtsq
cargo test --workspace --exclude kitae_render_gpu

# probe a file (FULL paths required)
cargo run -q -p kitae_qtsq --example probe /Users/priyanshuboruah/Downloads/<name>.qtsq
```
