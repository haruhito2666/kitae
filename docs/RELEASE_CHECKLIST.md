# Kitae 0.1 release checklist (RP-C7)

Publishing, tagging, registry changes, and any `cargo login` are deliberate
human/external **user-gated** actions. This file is the map. Updated
2026-09-25:

- Publish chain is `kitae_ui_macros → kitae_ui → kitae_live → kitae_cli`
  (kitae_ui is deliberately engine-free; nothing else needs publishing).
- All four manifests carry `description`, MIT license, and
  `version = "0.1.0"` (workspace-inherited).
- Intra-chain deps are **version+path dual deps**: `{ version = "0.1.0",
  path = … }` — path wins in the workspace, versions resolve once
  published. No manifest edits are needed at publish time.
- `cargo package -p kitae_ui_macros --no-verify` succeeds. Preparing
  downstream packages currently stops because `kitae_ui_macros` is not yet
  in the crates.io index; do not claim the four-package chain is publish-ready.
- `kitae new --registry` selects the published version line
  (`kitae_ui = "0.1"` + `kitae_live = "0.1"`); path dependencies remain the
  default until the manual post-publish flip below.
- The app bundle is version `0.1.0` build `1`, registers the `kitae` deep-link
  scheme, and the v20/native-contract path is built and tested for arm64 macOS
  15.0.

## Before publishing

1. **LICENSE file** — the manifest declares `license = "MIT"` (enough for
   cargo), but crates.io displays and links a `LICENSE` file. Add the MIT
   text at the repo root.
2. **git init + commit** — the project is not yet a git repository. Tags
   and reproducibility (and the `repository` manifest field) all start
   here.
3. **Manifest URLs** — add `repository` (and optionally `homepage` /
   `documentation`) to `[workspace.package]`; every crate inherits it.
   This silences the remaining `cargo package` warning.

## Current validation (2026-09-26)

- **Pass:** serialized and default-parallel full workspace runs (1149 passed,
  3 ignored), `make native-test` (9/9), `make` (app bundle up to date, ad-hoc
  signed), `make ffi-smoke` (frame bytes 462464, version 20), `cargo check
  --workspace --all-targets`, `cargo clippy --workspace --all-targets`,
  `cargo fmt --all -- --check`, `cargo audit` (0 vulnerabilities; 4 unmaintained
  transitive-crate warnings: `bincode`, `paste`, `rustybuzz`, `ttf-parser`),
  and `./tools/deps-audit.sh`.
- **Print:** the engine's print-media path and the host's
  print choreography are covered headlessly (native contract tests); running
  the actual `NSPrintOperation` needs a GUI session and a printer/PDF
  destination, so the paper round trip is still a manual check.
- **Manual GUI gate:** verify A11 range selection/IME, nested scrollbars/wheel
  routing/transforms, AX roles/actions/live announcements,
  Settings/Privacy/HSTS persistence, `kitae://` deep links, SPA
  query/deep-link rendering, v20 `<video controls>`, QTSQ cache purge/reload,
  Markdown typography, and the supported QTSQ video transport in Kitae.app.
- **External/manual blockers:** add the root `LICENSE`; initialize and
  commit git; add manifest repository metadata; publish the four crates in
  order; flip the CLI registry default; tag/push; and perform the
  stranger-shell check. None of those actions is completed by this checklist.
- **Deployment status:** `app/Makefile` targets
  `arm64-apple-macos15.0`, `LSMinimumSystemVersion` is `15.0`, and
  `vtool` reports `minos 15.0` for the current arm64 executable. This is a
  local-target check, not clean-machine installation, distribution signing,
  notarization, or broader architecture validation.

## Publishing (order matters — deps first; not run here)

```sh
cargo login          # paste the token in the terminal; never in a file
cargo publish -p kitae_ui_macros
cargo publish -p kitae_ui
cargo publish -p kitae_live
cargo publish -p kitae_cli
```

Between steps, wait for the index to accept each crate (a minute or two),
or the next `cargo publish` rejects the version dep as "not found".

## Tag the 0.1 line

```sh
git tag v0.1.0 && git push --tags
```

## Flip the default (still manual)

After all four crates are published, make registry deps the default scaffold:
in `crates/kitae_cli/src/main.rs`, `cmd_new` currently computes
`let registry = args.iter().any(|a| a == "--registry");` — flip it to
`true` and rename the flag to `--path` (opting back into checkout deps).
Release as kitae_cli `0.1.1` so `cargo install kitae_cli` scaffolds against
crates.io out of the box. This change has not been made.

## Verify from a stranger's shell (post-publish only)

```sh
cargo install kitae_cli
kitae new my-site --registry && cd my-site && kitae dev
# open the printed URL in Kitae; edit src/lib.rs, watch it hot-swap
kitae build && open dist/index.html   # plain static HTML
```

This check is blocked until the four crates are published and the registry
default is flipped.

Then: `git tag v0.1.1`.
