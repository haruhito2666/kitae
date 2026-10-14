# Kitae WPT conformance scorecard (parity item I5)

> Measured conformance against vendored subsets of the web-platform-tests
> corpora. The harnesses **measure**, they do not claim: each test asserts
> only that the rate never falls below the baseline recorded here, so a
> regression turns the sweep red. Raising a number requires re-measuring and
> updating the baseline constant in the same commit.
>
> **Current checkpoint (2026-09-25):** all five harnesses pass inside the
> 1149-test/3-ignored full workspace sweep. These vendored subsets are not
> the full WPT suite, and the tree score does not imply general web-platform
> conformance.

## Harnesses

| Harness | Corpus | Measured (2026-09-25) | Baseline const |
|---|---|---|---|
| `crates/kitae_html/tests/wpt_tokenizer.rs` | html5lib/WPT tokenizer `.test` files under `crates/kitae_html/tests/data/tokenizer/` | **6256/6256 — 100.0%** (116 skipped) | `BASELINE_PASS_RATE` = 1.000 |
| `crates/kitae_html/tests/wpt_tree_construction.rs` | html5lib tree-construction `*.dat` under `crates/kitae_html/tests/data/tree-construction/` | **1072/1784 — 60.1%** (8 skipped) | `BASELINE_PASS_RATE` = 0.600 |
| `crates/kitae_css/tests/wpt_css_data.rs` (hexadecimal) | css-parsing-tests `color_hexadecimal_3.json` | **81/81 — 100.0%** | `COLOR_HEX_BASELINE` = 1.000 |
| `crates/kitae_css/tests/wpt_css_data.rs` (keyword) | css-parsing-tests `color_keywords_3.json` | **160/160 — 100.0%** | `COLOR_KEYWORD_BASELINE` = 1.000 |
| `crates/kitae_css/tests/wpt_css_data.rs` (An+B) | css-parsing-tests `An+B.json` | **128/128 — 100.0%** | `AN_PLUS_B_BASELINE` = 1.000 |

All five run inside `cargo test --workspace`. `KITAE_WPT_DIFF=1` (HTML) and
`KITAE_CSS_DIFF=1` (CSS) print divergences for triage.

## Why the rate can be low — what is and is not measured

The corpora exercise the full HTML parsing pipeline; kitae's parser is
deliberately scoped and deviates in documented places. The tree number is
dominated by those gaps plus tree-builder depth (adoption agency, template
modes), not by tokenization: the tokenizer itself is at 100.0%.

## Skip rules (documented, per harness)

Tokenizer (`wpt_tokenizer.rs`):

- Cases with `initialStates` other than Data are skipped — kitae's batch
  entry point always starts in Data.
- Cases with `lastStartTag` are skipped — fragment-context states.
- `xmlViolation.test` is skipped whole — it drives fragment/context states
  the batch entry point cannot reach. These skips total **116**.
- `doubleEscaped` cases are unescaped (including surrogate pairs) and
  measured, not skipped.
- Error arrays are never compared — kitae has no error channel.
- Normalization: consecutive character tokens merge (both sides), attribute
  dicts compare order-insensitively.

Tree construction (`wpt_tree_construction.rs`):

- `#script-on` variants are skipped (kitae has no scripting) — **8 skips**.
- `#document-fragment` cases **are measured**, through
  `kitae_html::parse_fragment` with the case's context element.
- Normalizations applied to the expected side (deliberate kitae
  divergences, so the score measures *unintended* divergence only):
  DOCTYPE/comment nodes dropped (no such node kinds — gap below), empty
  `<head>` stripped, whitespace model (collapse outside raw-text elements;
  verbatim inside script/style/pre/textarea/xmp/iframe/noembed/noframes
  with the spec's swallowed leading newline), attribute lines sorted.
- Error codes are never compared — kitae has no error channel.

CSS (`wpt_css_data.rs`): everything is measured; `null` expectations mean
the input must be rejected.

## Known gaps (each suppresses whole clusters)

1. **In-template insertion modes** — `<template>` content stays inside the
   template element (inert; fixed 2026-09-10 — it used to leak into body),
   but the template insertion modes themselves are not modelled, so cases
   that require spec-exact template bookkeeping still diverge.
2. **Adoption-agency scope nuance** — misnested formatting elements match
   the common cases; the full scope/search bookkeeping is simplified.
3. **Rawtext end-tag-open buffering** — `</x` inside rawtext at a
   truncation point can buffer differently from the spec.
4. **Plaintext integration contexts** — the tokenizer now has a distinct
   plaintext state for Data and plaintext fragment contexts, but residual
   frameset, head/table, select, and foreign-content edge cases diverge.
5. **Select-scope formatting reconstruction** — formatting-element
   reconstruction inside `select` is simplified.
6. **`<p><hr></p>`** — the trailing implied empty `<p>` shape differs.
7. **Foreign-content depth and CDATA** — SVG/MathML parsing is shallow;
   deep nesting, integration-point casing, and foreign CDATA handling
   remain incomplete.
8. **DOCTYPE/comment not in the DOM** — no node kinds for them (tree
   normalizations compensate in the score).
9. **No error codes** — the parse-error channel is unimplemented, so
   error expectations are never compared anywhere.
10. **currentColor** — the CSS keyword is contextual and is not resolved by
    the static `parse_color` entry point.
11. **CSS component-value mapping gap** — the harness measures the
    `parse_color`/`parse_nth` entry points, not full declaration-level
    component-value parsing.


## How to update

1. Fix or intentionally diverge; re-run the harness with `-- --nocapture`.
2. Move the printed rate into the table above (with the date) and set the
   baseline constant to just **below** the exact ratio (1072/1784 is
   0.6008977… — a baseline of `0.601` can fail on float comparison).
3. Add any newly-identified systematic gap to the list above.
