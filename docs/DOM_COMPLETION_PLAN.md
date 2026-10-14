# Kitae DOM Completion Plan — bucket A (HTML & DOM, 12 items)

> **Execution plan for Phase 1 bucket A of `BROWSER_PARITY_PLAN.md`.**
> Status markers: ✅ done + tested · 🚧 partial (documented scope cut) · ⏳ pending.
> Ground rules unchanged: no JS/WASM, pure Rust, display list is the contract.
>
> **Current refresh (2026-09-25):** all 12 bucket-A implementation rows are
> shipped. A11's final range-selection/IME pass is covered by engine and FFI
> tests and the AppKit input bridge; visual selection, composition, and
> `contenteditable` behavior still require manual GUI sign-off. The current full
> workspace checkpoint is 1149 passed, 3 ignored, plus 9 native contract tests.

**Batch 1 shipped 2026-09-06** — A2/A3/A4/A5/A6/A8/A9 all landed, 442 tests
green workspace-wide, app rebuilt. A6 shipped in its full 🚧 scope (the plan's
cut stands: no `::backdrop`, no modal focus/top-layer).

**Batch 2 shipped 2026-09-06** — A7/A10/A12 landed, 502 tests green
workspace-wide, app rebuilt.

**A1 + A11a shipped 2026-09-06** — full-spec HTML5 tree construction
(adoption agency, active-formatting reconstruction, foreign-content breakout;
html5lib battery green) and the A11a caret/typing engine for form controls
(insert-at-caret, backspace/delete-forward, arrow/home/end, click-to-place,
caret painted as a 1px display-list rect — no wire bump; FFI codes 31–35;
Swift `keyDown`/`insertText` + click forwarding). 600 tests green
workspace-wide (excl. `kitae_render_gpu`). At that checkpoint,
`contenteditable`, text selection, and IME remained.

| # | Item | Status | Notes |
|---|---|---|---|
| A1 | Full-spec HTML5 tree construction | ✅ | Adoption agency for misnested formatting, active-formatting reconstruction across structure, button-scope paragraph closing, foreign-content breakout, parse recovery — html5lib-exact (30-case battery green, permanent `kitae_html` tests). |
| A2 | `<template>` inert content | ✅ | No-JS approximation of inert fragments: children stay in the DOM (serialized/dumped) but never activate — `<style>` inside is not collected, images never load, layout/AX/find never see it. Spec separates content into a fragment; behaviorally equivalent here. |
| A3 | `<picture>` + `srcset`/`sizes` | ✅ | Source selection at image layout: `type` (supported formats only) + `media` (the same media-query evaluator the stylesheets use, against the layout viewport **and** the page's `EnvironmentState` — so `media="print"` and `(prefers-color-scheme: dark)` follow the print switch and the appearance setting; the `<img>`'s own `media` attribute gates the whole element); srcset w/x descriptors on the img **and** on `<source>`; density assumed 1, `sizes` deferred. |
| A4 | `<colgroup>`/`<col>` column sizing | ✅ | col `width` (CSS or the attribute — px/%/em, bare number = px per HTML) + `span` fix columns; a spanned col with a width splits evenly; remaining auto columns split the rest (min 20px). Columns past the occupied grid still create empty columns. |
| A5 | `<details>`/`<summary>` toggle | ✅ | UA styles flip the `+`/`-` marker via `summary::before` + `details[open] > summary::before`; layout of a closed `<details>` renders only the summary; click on a summary toggles `open` + relayout + republish (engine-internal toggle hit-areas `DisplayList.toggles`, no wire change; clipped/toggled sub-buffers carry them). |
| A6 | `<dialog>[open]` rendering | ✅ | Closed dialogs are `display: none` (UA); `[open]` renders a fixed-position card centered via `left:50%` + `translateX(-50%)` + `top:10%`, white bg, border, radius, shadow — all through the UA sheet, zero layout code. **Finish shipped 2026-09-08**: `::backdrop` dims the page under the card, modal dialogs autofocus and trap Tab inside, Esc closes the topmost open dialog first, and the backdrop gates clicks until escape (`dialog_modal_autofocuses_and_traps_tab`, `escape_closes_topmost_dialog_then_next`, `modal_backdrop_gates_click_until_escape`, `dialog_backdrop_dims_page_under_card`). |
| A7 | Form control UIs (date/color/range/file) | ✅ | Value-display v1: range paints a slider (gray track, blue fill, knob at the value fraction, step-quantized); color paints the value swatch (default black); file renders the "Choose file" pill or the basename; datetime inputs show their ISO value verbatim. Type-aware default sizes (range 129×14, color 49×27, file ≥150px). **Interaction shipped 2026-09-08**: clicking the range track sets the value from the track position (step-quantized), and arrow keys on a focused range/number/select step the value (`click_sets_range_value_from_track_position`, `step_focused_adjusts_range_number_select`). |
| A8 | `<progress>`/`<meter>` | ✅ | Replaced-element widgets from the layout-static widget arm: pill track + fill from `value`/`max` (clamped; no value = indeterminate gray half-fill); meter green inside `[low,high]`, yellow outside, red at extremes. Inline-block widgets lay out through a scratch-buffer pass in `compose_node` (own band row, like form controls). |
| A9 | `<output>`/`<datalist>` | ✅ | `output` renders as an inline container (its text flows); `datalist` stays hidden (a popup source is meaningless without script). |
| A10 | Constraint validation UI | ✅ | `kitae_style::control_invalid` computes validity live (required+empty, per-type checks for email/url/number/range/date/time/datetime-local/color; submit/button/reset/image/hidden/file and disabled controls never invalid). `:invalid` matches through the full cascade (UA + author); `:valid` parses to `:not(:invalid)`. UA rule `input:invalid {border:1px solid #d93025}` and the control chrome read the computed border color, so invalid controls paint the error red. `submit_form` gates on `form_failures` — first failure takes focus, relayout repaints it, submit returns None; filling the field re-opens the gate. **Finish shipped 2026-09-08**: `pattern` validates through a regex-subset engine (`kitae_style::pattern_matches` — literals, `.`, character classes with `\d \w \s` shorthands, quantifiers; spec-anchored full-string; unparseable pattern ignored, not a failure), and a validation bubble paints near the first failing control (`pattern_blocks_submit_and_bubble_reports`). |
| A11 | Editing (`contenteditable`, caret, IME) | ✅ | **A11a shipped 2026-09-06**: caret model for form controls — insert-at-caret typing, backspace/delete-forward, arrow/home/end movement, click-to-place caret, caret painted as a 1px `DisplayList` rect at publish (clean list stays caret-free; no wire bump); engine Commands + FFI codes 31–35; Swift `keyDown`/`insertText` + click forwarding + first-responder. **A11b shipped 2026-09-08**: `contenteditable` hosts edit as plain text through the A11a value abstraction — the attribute is read live (`"false"` inert, runtime flips respected), edits flatten the host to one text child, hosts join tab order, clicking anywhere on the host places the caret, and the caret paints subtree-aware. **A11c shipped 2026-09-25**: anchor/head selection can be set by offset or page point, extends with drag or Shift+caret keys, reports the selected string, paints measured highlight rects, and replacement-aware typing/backspace/delete collapse the range. `PageCanvasView` implements `NSTextInputClient`; marked-text updates and commit/cancel (including explicit commit text) route through FFI 43–51. The clean display list remains decoration-free, so republishes do not accumulate carets/highlights. `contenteditable` remains the documented plain-text/single-space-offset v1 model, and manual GUI validation is still pending. |
| A12 | MathML | ✅ | v1 linearized: `math` is an inline-block container, tokens (`mi`/`mn`/`mo`/`mtext`) flow inline in reading order; `mi` italic, `mn` upright, `mo` gets 3px side margins. All through the UA sheet — zero layout code. **2D layout shipped 2026-09-08**: a layout-engine `math_walk` renders `mfrac` (stacked numerator/denominator around a bar), `msqrt` (radical) and `mover`/`munder` (over/underbar, centered), `msub`/`msup`/`msubsup` (shifted + shrunk scripts), `mrow` grouping, and `<semantics>` skips non-first children — 5 tests (`mathml_mfrac_stacks_around_bar`, `mathml_msqrt_radical_and_overbar`, `mathml_msubsup_shifts_and_shrinks`, `mathml_mover_centers_above`, `mathml_semantics_skips_annotation`). |

## Sequencing this batch

A2 → A8 → A9 → A5 → A6 → A3 → A4 — **all shipped** (batch 1).
A7 → A10 → A12 — **all shipped** (batch 2).
A1 shipped (full-spec tree construction). A11a shipped (form-control caret).

**2026-09-08 — bucket A closed to its then-last tail.** A6 finished
(::backdrop + modal focus + Esc stack + click gate), A7 finished (range
track click + focused stepping), A10 finished (`pattern` regex subset +
validation bubbles), A12 finished (2D `math_walk` layout), and A11b
contenteditable shipped (editing as plain text, click-to-place caret,
subtree-aware caret painting, tab order; 3 tests). At that checkpoint,
`kitae_core` had 120 green tests and the workspace was green excluding
`kitae_render_gpu`; A11 visual range selection + IME were the only bucket-A
implementation tail.

**2026-09-25 — A11c shipped.** Offset and point ranges, visual highlight
geometry, Shift-extension, replacement edits, and marked-text composition
update/commit/cancel are implemented through the engine, v20 FFI, and AppKit
input bridge. Bucket A is 12/12 at the implementation level. Manual GUI
verification is still required and is not implied by the passing unit/FFI
contracts.

## Shipped-but-noted deviations

- **A5 marker is `+`/`-` ASCII**, not a CSS triangle — guaranteed glyph
  coverage across host fonts; rides R3 generated content.
- **A6 dialog centering** relies on `drain_abs` resolving `left: 50%`
  against the viewport and the transform rebasing `-50%` against the card
  width; `Position::Fixed` boxes anchor to the page root. Verified by test.
- **A4** column pins resolve % against the full table width, autos get an
  equal share of what remains (min 20px) — no content-based distribution
  for autos after pinning in v1.
- **A11 editing scope:** form controls and `contenteditable` edit a flattened
  plain-text value. Selection/caret offsets follow painted words separated by
  one space; nested markup, arbitrary whitespace runs, rich-text commands,
  and script-driven composition are outside this no-JS v1 model. The AppKit
  marked-text bridge is implemented, but real IME candidate behavior and
  visual correctness still need manual GUI validation.
- **Batch fixes outside the plan scope** (found by the tests):
  `kitae_css::parse_attr` had a glued `fn` line (parse bug);
  negative `%` lengths verified parsing (`-50%` → `Percent(-50.0)`);
  `compose_node`'s form-control arm re-stamps `cur_node` (v7) so button
  rects attribute to the button, and `splice_try` now derives D's old/new
  box as a rect-union fallback when the splice root itself paints nothing —
  the kitae_ui start-page counter click splices without full relayout again.
- **Batch 2 notes**: `parse_color` is RGBA with the alpha byte in the low
  position (`#d93025` → `0xD93025FF`); the control-chrome read forces opacity
  with `| 0xFF`, not `| 0xFF000000` (the UA invalid-border rule was applying
  all along — the chrome was corrupting the red channel). `form_failures`
  and `control_invalid` are the same predicate, so the UI highlight and the
  submit gate cannot disagree.

