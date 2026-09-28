#!/usr/bin/env bash
# Stamp performed_at after git push returns, then commit + push the ledger.
# Called only from the workflow, after tags are on the remote.
set -euo pipefail
cd "$(dirname "$0")/.."

LEDGER=canary/ledger.jsonl
PENDING=.canary-pending.jsonl

if [[ ! -s "$PENDING" ]]; then
  echo "no pending canary actions"
  exit 0
fi

performed="$(date -u +"%Y-%m-%dT%H:%M:%S.000Z")"

mkdir -p canary
touch "$LEDGER"
python3 - "$PENDING" "$LEDGER" "$performed" <<'PY'
import json, sys
pending_path, ledger_path, performed = sys.argv[1:4]
with open(pending_path) as f:
    lines = [ln.strip() for ln in f if ln.strip()]
with open(ledger_path, "a") as out:
    for ln in lines:
        row = json.loads(ln)
        row["performed_at"] = performed
        out.write(json.dumps(row, separators=(",", ":")) + "\n")
PY

: > "$PENDING"
git add "$LEDGER" "$PENDING"
git commit -m "canary: ledger performed_at=${performed}" || true
git push origin HEAD:main
