# Kitae fuzzing program (parity item I4)

> Deterministic, seeded mutation fuzzing over Kitae's first-party HTML,
> CSS, URL/policy, and QTSQ entry points. The programs are **ordinary tests**:
> they run inside `cargo test --workspace` (256 iterations per test by default)
> so their no-panic and structural invariants are enforced on every sweep, and
> they scale via one env var for long local runs. Third-party image, font,
> audio, and shaping decoders have separate corpus/unit/fail-closed tests, but
> they do **not** yet have dedicated seeded mutation targets here. No new
> dependencies or nightly toolchain are required for the standing program.

## Why seeded mutation fuzzing (and not cargo-fuzz yet)

- The invariants we care about first — *no panic*, *structural consistency*,
  *oracle parity with the `url` crate* — are fully enforceable by seeded
  mutation over a hostile corpus; coverage guidance finds deeper inputs but
  the same assertions apply.
- House dependency discipline: no new crates, no nightly toolchain in the
  default sweep.
- Deterministic: a failure reproduces exactly from the test's fixed seed and
  mutation sequence, so a red test is a minimal reproducer.
- CI-suitable: the default 256 iterations per test add well under a second
  to a sweep.

## The programs

| Program | Target | Corpus | Invariants |
|---|---|---|---|
| `crates/kitae_html/tests/fuzz_html.rs` | `parse`, `parse_fragment` (8 contexts), `parse_streaming`, `tokenize`, streaming `Tokenizer` | 14 hostile document shapes (formatting elements, tables, `select`, SVG/MathML foreign content, `template`, `plaintext`, `textarea`, nested forms, `script`) × hostile character mutations | arena consistency (child↔parent pointers agree, ids < `dom.len()`, acyclic), root is `<html>`, fragment roots are parentless with correct child parents, **streaming == whole-parse** and **batch == streaming token streams** under random char-boundary chunking |
| `crates/kitae_css/tests/fuzz_css.rs` | stylesheet/declaration/color/length/font-face/condition evaluators, filter/clip/mask/nth/number/size parsers, `resolve_vars` (forced cycles) | 14 CSS shapes plus public value-parser seeds (media/supports/layer/font-face/container/keyframes/`:has`/var cycles/calc/gradient) | no panic anywhere; cyclic `var()` lookups terminate; valid colors and public value parsers remain fail-closed |
| `crates/kitae_net/tests/fuzz_urls.rs` | `resolve`, `mixed_content_allowed`, response classification/text, local-origin gate | 16 bases (scheme zoo, userinfo, IPv6, backslashes, bad ports, empty) × 22 hrefs × hostile response/origin mutations | no panic; **url-crate parity**; `about:`/`kitae:` pass through trimmed and unjoined; mixed-content policy is scheme-exact and pure; response/origin helpers never panic |
| `crates/kitae_qtsq/tests/fuzz_qtsq.rs` | `probe`, `is_qtsq`, audio info, text/image/audio/tensor decode, video open/dimensions/frame count | six probe fixtures (text, locked text, image, audio, video, tensor) and five decode fixtures, with byte flips, deletions, insertions, truncations, and payload-only mutations | decoder surfaces fail closed without Rust panics; no key-store writes or network access |

**Current validation (2026-09-25):** all four test binaries are green in both
serialized and default-parallel full workspace runs (1149 passed, 3 ignored).
They contribute 16 test functions in total (HTML 4, CSS 5, URL 5, QTSQ 2);
each function performs its own deterministic 256-iteration loop by default.

## Determinism & scaling

- RNG: a private XorShift64 in each file (no `rand` dependency); fixed seeds
  per test.
- Iterations: `KITAE_FUZZ_ITERS` env var, default 256.
- Long run:
  ```sh
   KITAE_FUZZ_ITERS=100000 cargo test -p kitae_html --test fuzz_html
   KITAE_FUZZ_ITERS=100000 cargo test -p kitae_css  --test fuzz_css
   KITAE_FUZZ_ITERS=100000 cargo test -p kitae_net  --test fuzz_urls
   KITAE_FUZZ_ITERS=100000 cargo test -p kitae_qtsq --test fuzz_qtsq
  ```

## Bugs it has found

- **`rgb(7 8 9 / 25%)` did not parse** — `parse_color` only spoke the legacy
  comma grammar. Fixed: `parse_rgb_channels` now accepts both grammars
  (legacy `rgb(R, G, B)` / `rgba(R, G, B, A)` and CSS Color 4
  `rgb(R G B [/ A])`, for either function name).
- **`currentColor`** is a contextual keyword (it resolves against the
  element's own `color` property), so a static `u32` parse returning `None`
  is the *correct* behavior — the declaration drops and the cascade keeps
  the previous value. Documented as a conformance gap in
  `docs/WPT_SCORECARD.md` rather than faked with a sentinel.

## Escalation path to coverage-guided fuzzing

Coverage-guided fuzzing remains deferred. If the first-party parsers stop
churning, add `cargo-fuzz` (nightly + libFuzzer, kept out of the workspace
default members) with one target per entry point, reusing these invariants as
the harness bodies:

1. `fuzz_parse_html(bytes)` — tree builder; same arena/root invariants.
2. `fuzz_parse_stylesheet(bytes)` and `fuzz_parse_declarations(bytes)`.
3. `fuzz_resolve(base, href)` — oracle: `url` crate parity.
4. `fuzz_qtsq_decode(bytes)` — the existing fail-closed check.
5. Dedicated deterministic or coverage-guided targets for web image/font/audio
   decode and `rustybuzz` input boundaries. These are explicit coverage gaps,
   not implied by the four current programs.

House rule for native/C surfaces: the vendored QTSQ decoder (including its
internal image path) stays behind its fail-closed wrapper and mutation suite.
Current web image decoding is Rust (`png`, `zune-jpeg`, `gif`, `image-webp`,
`resvg`), and shaping uses `rustybuzz`; there is no vendored stb_image or
HarfBuzz module in the current web-media path. `docs/CPP_ROLE.md` governs any
future C/C++ addition.
