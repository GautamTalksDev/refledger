#!/usr/bin/env bash
# For each SHA-pinned external action referenced from our workflows/composites,
# fetch that action's action.yml at the pinned SHA and report any `uses:` inside
# it that is NOT SHA-pinned.
#
# Report only — do not fail the build (yet). This is the exact condition that
# compromised correctly SHA-pinned workflows in the March 2026 Trivy incident.
# We measure it on ourselves before we sell it to anyone.

set -euo pipefail

root="${CHECK_TRANSITIVE_ROOT:-$PWD}"
cd "$root"

sha40='[0-9a-f]{40}'
reported=0

collect_files() {
  if [[ -d .github/workflows ]]; then
    find .github/workflows -type f \( -name '*.yml' -o -name '*.yaml' \) -print
  fi
  find . -type f \( -name 'action.yml' -o -name 'action.yaml' \) \
    ! -path './target/*' ! -path './.git/*' -print 2>/dev/null || true
}

extract_uses() {
  local line="$1"
  line="${line#"${line%%[![:space:]]*}"}"
  if [[ "$line" == -* ]]; then
    line="${line#-}"
    line="${line#"${line%%[![:space:]]*}"}"
  fi
  case "$line" in
    uses:*)
      local rest="${line#uses:}"
      rest="${rest#"${rest%%[![:space:]]*}"}"
      rest="${rest%%#*}"
      rest="${rest%"${rest##*[![:space:]]}"}"
      rest="${rest#\"}"
      rest="${rest%\"}"
      rest="${rest#\'}"
      rest="${rest%\'}"
      printf '%s' "$rest"
      ;;
    *) return 1 ;;
  esac
}

# Parse owner/repo[/subdir]@sha40 → prints: owner/repo sha [subdir]
parse_pinned() {
  local ref="$1"
  if [[ "$ref" =~ ^([A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+)(/[A-Za-z0-9_./-]+)?@(${sha40})$ ]]; then
    local repo="${BASH_REMATCH[1]}"
    local sub="${BASH_REMATCH[2]:-}"
    local sha="${BASH_REMATCH[3]}"
    sub="${sub#/}"
    printf '%s %s %s' "$repo" "$sha" "$sub"
    return 0
  fi
  return 1
}

is_pinned_sha() {
  local ref="$1"
  [[ "$ref" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+(/[A-Za-z0-9_./-]+)?@${sha40}$ ]]
}

fetch_action_yml() {
  local repo="$1"
  local sha="$2"
  local sub="$3"
  local path
  if [[ -n "$sub" ]]; then
    path="${sub}/action.yml"
  else
    path="action.yml"
  fi
  local url="https://raw.githubusercontent.com/${repo}/${sha}/${path}"
  local body
  if ! body="$(curl -fsSL --max-time 20 "$url" 2>/dev/null)"; then
    # Try action.yaml
    path="${path%.yml}.yaml"
    url="https://raw.githubusercontent.com/${repo}/${sha}/${path}"
    body="$(curl -fsSL --max-time 20 "$url" 2>/dev/null)" || return 1
  fi
  printf '%s' "$body"
}

declare -A seen=()

while IFS= read -r file; do
  [[ -f "$file" ]] || continue
  while IFS= read -r line || [[ -n "$line" ]]; do
    trimmed="${line#"${line%%[![:space:]]*}"}"
    case "$trimmed" in
      uses:*|-[[:space:]]uses:*) ;;
      *) continue ;;
    esac
    if [[ "$trimmed" =~ ^-[[:space:]]+uses:(.*)$ ]]; then
      trimmed="uses:${BASH_REMATCH[1]}"
    fi
    ref="$(extract_uses "$trimmed" || true)"
    [[ -n "${ref:-}" ]] || continue
    parsed="$(parse_pinned "$ref" || true)"
    [[ -n "${parsed:-}" ]] || continue

    # Deduplicate by full ref
    if [[ -n "${seen[$ref]+x}" ]]; then
      continue
    fi
    seen[$ref]=1

    read -r repo sha sub <<<"$parsed"
    echo "transitive: inspecting ${repo}@${sha}${sub:+/}${sub}"

    if ! yml="$(fetch_action_yml "$repo" "$sha" "$sub")"; then
      echo "  WARN  could not fetch action.yml for ${repo}@${sha}"
      continue
    fi

    lineno=0
    while IFS= read -r al || [[ -n "$al" ]]; do
      lineno=$((lineno + 1))
      at="${al#"${al%%[![:space:]]*}"}"
      case "$at" in
        uses:*|-[[:space:]]uses:*) ;;
        *) continue ;;
      esac
      if [[ "$at" =~ ^-[[:space:]]+uses:(.*)$ ]]; then
        at="uses:${BASH_REMATCH[1]}"
      fi
      inner="$(extract_uses "$at" || true)"
      [[ -n "${inner:-}" ]] || continue

      # Local and docker-digest refs inside the upstream action are fine.
      if [[ "$inner" == ./* ]] || [[ "$inner" =~ ^docker://.+@sha256:[0-9a-f]{64}$ ]]; then
        continue
      fi

      if is_pinned_sha "$inner"; then
        continue
      fi

      echo "  UNPINNED ${repo}@${sha} action.yml:${lineno}: uses: ${inner}"
      reported=$((reported + 1))
    done <<<"$yml"
  done < "$file"
done < <(collect_files | sort -u)

if [[ "$reported" -eq 0 ]]; then
  echo "transitive-check: no unpinned nested uses: found (or no fetchable action.yml)"
else
  echo "transitive-check: ${reported} unpinned nested uses: reported (non-blocking)"
fi

# Always succeed — measurement only.
exit 0
