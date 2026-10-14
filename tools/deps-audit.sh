#!/bin/sh
# Kitae dependency audit (S-9, SECURITY_AUDIT.md).
#
# Runs license + advisory checks over the workspace dependency tree so the
# "no vulnerable/unknown-license dependency" property is checkable in one
# command (the repo has no CI yet; run this before releases).
#
# Requires: cargo-deny (licenses + advisories). Optional: cargo-audit.
set -e
cd "$(dirname "$0")/.."

if ! command -v cargo-deny >/dev/null 2>&1; then
    echo "!! cargo-deny not installed — run: cargo install cargo-deny" >&2
    exit 1
fi

echo "== cargo deny: licenses =="
cargo deny check licenses

echo "== cargo deny: advisories =="
cargo deny check advisories

if command -v cargo-audit >/dev/null 2>&1; then
    echo "== cargo audit: vulnerabilities =="
    cargo audit --db "$HOME/.cargo/cargo-audit-db" \
        --ignore RUSTSEC-2024-0436 \
        --ignore RUSTSEC-2025-0141 \
        --ignore RUSTSEC-2026-0206 \
        --ignore RUSTSEC-2026-0192
else
    echo "!! cargo-audit not installed (cargo install cargo-audit) —" >&2
    echo "!! advisory scan already covered by cargo-deny above." >&2
fi

echo "OK: dependency audit clean"