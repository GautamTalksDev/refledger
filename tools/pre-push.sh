#!/usr/bin/env bash
# Pre-push gate matching CI: fmt, clippy -D warnings, tests, pin-check.
# Run from the repository root (or any subdirectory; we locate the root).
set -euo pipefail

root="$(git rev-parse --show-toplevel 2>/dev/null || true)"
if [[ -z "${root}" ]]; then
  echo "pre-push: not inside a git work tree" >&2
  exit 1
fi
cd "$root"

echo "==> cargo fmt --all -- --check"
cargo fmt --all -- --check

echo "==> cargo clippy --workspace --all-targets -- -D warnings"
cargo clippy --workspace --all-targets -- -D warnings

echo "==> cargo test --workspace --all-features"
cargo test --workspace --all-features

echo "==> tools/check-pins.sh"
bash tools/check-pins.sh

echo "pre-push: OK"
