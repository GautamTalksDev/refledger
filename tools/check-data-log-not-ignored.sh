#!/usr/bin/env bash
# Guard: sealed ledger paths under data/log must never be gitignored.
# A top-level /data/ rule once blocked the publisher's `git add data/log`.
set -euo pipefail

root="$(git rev-parse --show-toplevel 2>/dev/null || true)"
if [[ -z "${root}" ]]; then
  echo "check-data-log-not-ignored: not inside a git work tree" >&2
  exit 1
fi
cd "$root"

sample="data/log/2026/09/30.jsonl"
if git check-ignore -q -- "${sample}"; then
  echo "FAIL: ${sample} is ignored by .gitignore (publisher cannot git add data/log)" >&2
  git check-ignore -v -- "${sample}" >&2 || true
  exit 1
fi

echo "OK: ${sample} is not ignored"
