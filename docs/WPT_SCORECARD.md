# Kitae WPT conformance scorecard (parity item I5)

> Measured conformance against vendored subsets of the web-platform-tests
> corpora. The harnesses **measure**, they do not claim: each test asserts
> only that the rate never falls below the baseline recorded here, so a
> regression turns the sweep red. Raising a number requires re-measuring and
> updating the baseline constant in the same commit.
>
> **Current checkpoint:** all six harnesses pass — page-JS added
> 2026-10-16 (tier JS-3) and re-measured at the same rate after tiers
> JS-4–JS-6 (observers, XHR + workers, storage + canvas; tier tests in
> `crates/kitae_core/src/lib.rs` and `pagejs.rs`). These vendored subsets
> are not the full WPT suite, and the tree score does not imply general
> web-platform conformance.

## Harnesses

| Harness | Corpus | Measured | Baseline const |
|---|---|---|---|
| `crates/kitae_html/tests/wpt_tokenizer.rs` | html5lib/WPT tokenizer `.test` files under `crates/kitae_html/tests/data/tokenizer/` | **6256/6256 — 100.0%** (116 skipped; 2026-09-25) | `BASELINE_PASS_RATE` = 1.000 |
| `crates/kitae_html/tests/wpt_tree_construction.rs` | html5lib tree-construction `*.dat` under `crates/kitae_html/tests/data/tree-construction/` | **1072/1784 — 60.1%** (8 skipped; 2026-09-25) | `BASELINE_PASS_RATE` = 0.600 |
| `crates/kitae_css/tests/wpt_css_data.rs` (hexadecimal) | css-parsing-tests `color_hexadecimal_3.json` | **81/81 — 100.0%** | `COLOR_HEX_BASELINE` = 1.000 |
| `crates/kitae_css/tests/wpt_css_data.rs` (keyword) | css-parsing-tests `color_keywords_3.json` | **160/160 — 100.0%** | `COLOR_KEYWORD_BASELINE` = 1.000 |
| `crates/kitae_css/tests/wpt_css_data.rs` (An+B) | css-parsing-tests `An+B.json` | **128/128 — 100.0%** | `AN_PLUS_B_BASELINE` = 1.000 |
| `crates/kitae_core/tests/wpt_page_js.rs` | WPT DOM/events/URL/MutationObserver subset under `crates/kitae_core/tests/data/wpt_pagejs/` | **1213/1349 — 89.9%** (2026-10-16, re-measured after tiers JS-4–JS-6; unchanged) | `BASELINE_PASS_RATE` = 0.898 |

All six run inside the workspace's test discipline. The page-JS harness
needs the tier's feature switched on, so its line is
`cargo test -p kitae_core --features page-js --test wpt_page_js` — without
`page-js` the harness reports a visible skip, never a 0/0 rate.
`KITAE_WPT_DIFF=1` (HTML), `KITAE_CSS_DIFF=1` (CSS), and
`KITAE_WPT_DIFF=1`/`KITAE_WPT_ONLY=<label>` (page-JS) print divergences
and select single files for triage.

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

Page-JS (`wpt_page_js.rs`) — the Track-A conformance spine, first slice:

- Each corpus file is spliced into a document with WPT's own
  `testharness.js` inlined (the real bootstrap every browser passes these
  tests with), loaded through `Browser::load_html` with page JS on, and
  driven to completion by the poll loop a host would run.
- `<script src=...>` tags whose target is under `/resources/` are dropped;
  the harness inlines `testharness.js` itself and appends a reporter that
  emits one `KITAE_WPT|<status>|<name>|<message>` console line per test
  plus `KITAE_WPT_DONE|<pass>|<fail>|<total>`.
- `setup({ output: false })` disables testharness's DOM results table; the
  console reporter above replaces it. (Kitae's shim cannot render the full
  results document; measuring is independent of it.)
- `.any.js` META lines (including `script=` helpers) are stripped; the one
  helper these files use, `subsetTestByKey`, is defined as "run
  everything": the harness exercises the whole corpus rather than a
  `?include=` variant.
- The URL corpora are data-driven (`fetch("resources/urltestdata*.json")`);
  those two files load from a loopback HTTP origin the harness serves
  (`urltestdata.json` + `urltestdata-javascript-only.json` vendored), so
  the fetches resolve same-origin exactly as on a WPT server.
- Statuses: 0 pass, 1 fail, 2 timeout, 3 not-run, 4 precondition.
  Not-run/timeout count as not-passed.

Page-JS known gaps (each suppresses whole clusters of the 10.0% residual):

1. **Multi-document variants** — `document.cloneNode(true)`, `new
   Document()`, `DOMImplementation.createHTMLDocument()` are absent (one
   `Dom` per page; no multi-document model), which fails 3 of 5 cases in
   both `Event-dispatch-bubbles-*` files.
2. **`Node-textContent.html` runs 0 tests**, and `MutationObserver-textContent.html`'s
   last case (CDATASection) is not-run — both need
   `document.implementation.createDocument*` and
   `createDocumentFragment`/`createComment`/`createProcessingInstruction`,
   `DOMParser`/CDATA/multi-document support. Both files stay in the
   corpus and start scoring as that lands; `Node-textContent.html`
   contributes nothing to numerator or denominator today.
3. **URL path canonicalization depth** — file-scheme drive letters
   (`file://host/C:/`), the pipe→colon mapping, and the full path
   percent-encode set (`^ { } |` and friends) diverge; our path writer is
   the engine's resolver, not the WHATWG parser.
4. **Opaque-path/search edge semantics** — `non-spec:/.//`, `<?>`, `<:#>`
   and similar opaque-path parser-state corners.
5. **Lone surrogates** — the URL constructor replaces them with U+FFFD
   (spec behavior) before crossing into Rust; other string surfaces that
   cross the boundary (e.g. attribute writes) still reject lone surrogates
   with a conversion error rather than lossy-replacing.
6. **Host IDNA** — only ASCII-lowercasing of special-scheme hosts is
   implemented; full IDNA/UTS-46 mapping (unicode hosts, punycode
   validation) is not.

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
