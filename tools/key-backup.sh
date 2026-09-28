#!/usr/bin/env bash
# Encrypt the Refledger signing seed for offline backup (docs/KEY-BACKUP.md).
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: tools/key-backup.sh <key-file> <output.age> [age -r recipient...]

Encrypts a 64-char hex Ed25519 seed with age. Pass recipients via AGE_RECIPIENT
or extra -r arguments. Never prints the plaintext seed.
EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" || $# -lt 2 ]]; then
  usage
  exit 1
fi

key_file=$1
out=$2
shift 2

if [[ ! -f "$key_file" ]]; then
  echo "key file not found: $key_file" >&2
  exit 1
fi

seed=$(tr -d ' \n\r\t' < "$key_file")
if [[ ! "$seed" =~ ^[0-9a-f]{64}$ ]]; then
  echo "key file must be 64 lowercase hex chars" >&2
  exit 1
fi

if ! command -v age >/dev/null 2>&1; then
  echo "age is required (https://age-encryption.org/)" >&2
  exit 1
fi

recipients=()
if [[ -n "${AGE_RECIPIENT:-}" ]]; then
  recipients+=(-r "$AGE_RECIPIENT")
fi
recipients+=("$@")
if [[ ${#recipients[@]} -eq 0 ]]; then
  echo "pass AGE_RECIPIENT or age -r arguments" >&2
  exit 1
fi

printf '%s\n' "$seed" | age "${recipients[@]}" -o "$out"
# Best-effort scrub of the shell variable in this process.
seed=""
echo "wrote encrypted backup: $out"
