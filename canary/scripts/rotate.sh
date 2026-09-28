#!/usr/bin/env bash
# Rotate through §6.2 tag-movement patterns. Ground-truth ledger lines are
# staged without performed_at — the workflow stamps that after git push returns
# so we time detection against the push, not GitHub Actions queue delay.
set -euo pipefail
cd "$(dirname "$0")/.."

LEDGER=canary/ledger.jsonl
PENDING=.canary-pending.jsonl
STATE=.canary-state
mkdir -p canary
touch "$LEDGER"
: > "$PENDING"
[[ -f "$STATE" ]] || echo 0 > "$STATE"

PATTERNS=(
  floating_major_forward
  exact_content_change
  commit_metadata_only
  lightweight_annotated_roundtrip
  delete_recreate
  batch_exact_to_one
)

stage_action() {
  local pattern="$1" tag="$2" from="$3" to="$4"
  printf '{"pattern":"%s","tag":"%s","from":"%s","to":"%s"}\n' \
    "$pattern" "$tag" "$from" "$to" >> "$PENDING"
}

ensure_blob() {
  local content="$1" path="$2"
  mkdir -p "$(dirname "$path")"
  printf '%s\n' "$content" > "$path"
  git add "$path"
  git commit -m "canary: material $path" >/dev/null 2>&1 || true
}

make_commit() {
  local msg="$1" content="$2"
  ensure_blob "$content" "payload.txt"
  printf '%s\n' "$content" > payload.txt
  git add payload.txt
  git commit -m "$msg" >/dev/null
  git rev-parse HEAD
}

current() {
  local idx
  idx=$(cat "$STATE")
  if [[ -n "${FORCE_PATTERN:-}" ]]; then
    for i in "${!PATTERNS[@]}"; do
      if [[ "${PATTERNS[$i]}" == "$FORCE_PATTERN" ]]; then
        echo "$i"
        return
      fi
    done
  fi
  echo "$idx"
}

idx=$(current)
pattern="${PATTERNS[$idx]}"
echo "pattern=$pattern idx=$idx"

case "$pattern" in
  floating_major_forward)
    tree_a=$(make_commit "canary tree A" "tree-a-$RANDOM")
    git tag -f v1 "$tree_a"
    from=$tree_a
    tree_b=$(make_commit "canary tree B ahead" "tree-b-$RANDOM")
    git tag -f v1 "$tree_b"
    stage_action "$pattern" "v1" "$from" "$tree_b"
    ;;
  exact_content_change)
    c1=$(make_commit "exact before" "exact-before-$RANDOM")
    git tag -f v1.0.0 "$c1"
    c2=$(make_commit "exact after" "exact-after-$RANDOM")
    git tag -f v1.0.0 "$c2"
    stage_action "$pattern" "v1.0.0" "$c1" "$c2"
    ;;
  commit_metadata_only)
    base=$(make_commit "meta base" "same-tree-content")
    tree=$(git rev-parse HEAD^{tree})
    new=$(git commit-tree "$tree" -m "meta amended $(date -u +%s)" -p HEAD)
    git tag -f v1.0.1 "$base"
    from=$base
    git tag -f v1.0.1 "$new"
    stage_action "$pattern" "v1.0.1" "$from" "$new"
    ;;
  lightweight_annotated_roundtrip)
    c=$(make_commit "lw/ann" "lw-ann-$RANDOM")
    git tag -f -d v2 >/dev/null 2>&1 || true
    git tag v2 "$c"
    from=$(git rev-parse v2)
    git tag -f -d v2
    git tag -a v2 -m "annotated canary" "$c"
    to=$(git rev-parse v2)
    stage_action "$pattern" "v2" "$from" "$to"
    git tag -f -d v2
    git tag v2 "$c"
    stage_action "$pattern" "v2" "$to" "$(git rev-parse v2)"
    ;;
  delete_recreate)
    c1=$(make_commit "delete before" "del-$RANDOM")
    git tag -f v3.0.0 "$c1"
    stage_action "$pattern" "v3.0.0" "$c1" ""
    git tag -d v3.0.0
    sleep 5
    c2=$(make_commit "recreate after" "rec-$RANDOM")
    git tag v3.0.0 "$c2"
    stage_action "$pattern" "v3.0.0" "" "$c2"
    ;;
  batch_exact_to_one)
    t=$(make_commit "batch target" "batch-$RANDOM")
    for tag in v9.0.0 v9.0.1 v9.0.2; do
      old=$(make_commit "batch $tag old" "old-$tag-$RANDOM")
      git tag -f "$tag" "$old"
      from=$old
      git tag -f "$tag" "$t"
      stage_action "$pattern" "$tag" "$from" "$t"
    done
    ;;
esac

next=$(( (idx + 1) % ${#PATTERNS[@]} ))
echo "$next" > "$STATE"
git add "$STATE" "$PENDING" 2>/dev/null || true
git commit -m "canary: advance state to $next" >/dev/null 2>&1 || true
