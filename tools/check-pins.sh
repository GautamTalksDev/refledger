#!/usr/bin/env bash
# Fail if any GitHub Actions workflow (or composite action) references an
# action that is not pinned to a full 40-character lowercase commit SHA.
#
# Allow-list:
#   - local composites: uses: ./...
#   - docker images with digest: uses: docker://image@sha256:<64 hex>
#
# Warn (exit 0 for the warn itself; still exit 1 on hard violations) when a
# pinned SHA has no trailing version comment — Dependabot needs the comment
# to know which version line to bump.

set -euo pipefail

# Prefer an explicit root (tests), then the caller's cwd (CI runs from repo root).
root="${CHECK_PINS_ROOT:-$PWD}"
cd "$root"

sha40='[0-9a-f]{40}'
violations=0
warnings=0

collect_files() {
  # Workflows
  if [[ -d .github/workflows ]]; then
    find .github/workflows -type f \( -name '*.yml' -o -name '*.yaml' \) -print
  fi
  # Composite actions anywhere in the repo
  find . -type f \( -name 'action.yml' -o -name 'action.yaml' \) \
    ! -path './target/*' ! -path './.git/*' -print 2>/dev/null || true
}

is_allowed_local() {
  local ref="$1"
  [[ "$ref" == ./* ]] || [[ "$ref" == ./. ]]
}

is_allowed_docker_digest() {
  local ref="$1"
  # docker://name@sha256:<64 lowercase hex>
  [[ "$ref" =~ ^docker://.+@sha256:[0-9a-f]{64}$ ]]
}

is_pinned_sha() {
  local ref="$1"
  # owner/repo/path@<40 hex>  OR  owner/repo@<40 hex>
  [[ "$ref" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+(/[A-Za-z0-9_./-]+)?@${sha40}$ ]]
}

# Extract the uses: value from a line (handles quotes and trailing comments).
extract_uses() {
  local line="$1"
  # Strip leading whitespace and optional '- '
  line="${line#"${line%%[![:space:]]*}"}"
  if [[ "$line" == -* ]]; then
    line="${line#-}"
    line="${line#"${line%%[![:space:]]*}"}"
  fi
  case "$line" in
    uses:*)
      local rest="${line#uses:}"
      rest="${rest#"${rest%%[![:space:]]*}"}"
      # Drop trailing comment
      rest="${rest%%#*}"
      # Trim trailing whitespace
      rest="${rest%"${rest##*[![:space:]]}"}"
      # Strip quotes
      rest="${rest#\"}"
      rest="${rest%\"}"
      rest="${rest#\'}"
      rest="${rest%\'}"
      printf '%s' "$rest"
      ;;
    *)
      return 1
      ;;
  esac
}

has_version_comment() {
  local line="$1"
  # After the SHA, expect whitespace then # then a version-ish comment
  [[ "$line" =~ @${sha40}[[:space:]]+#[[:space:]]*.+ ]]
}

while IFS= read -r file; do
  [[ -f "$file" ]] || continue
  lineno=0
  while IFS= read -r line || [[ -n "$line" ]]; do
    lineno=$((lineno + 1))
    # Only consider lines that introduce a uses: key (not nested strings).
    trimmed="${line#"${line%%[![:space:]]*}"}"
    case "$trimmed" in
      uses:*|-[[:space:]]uses:*) ;;
      *) continue ;;
    esac

    # Normalise "- uses:" to "uses:"
    if [[ "$trimmed" =~ ^-[[:space:]]+uses:(.*)$ ]]; then
      trimmed="uses:${BASH_REMATCH[1]}"
    fi

    ref="$(extract_uses "$trimmed" || true)"
    if [[ -z "${ref:-}" ]]; then
      continue
    fi

    if is_allowed_local "$ref"; then
      continue
    fi
    if is_allowed_docker_digest "$ref"; then
      continue
    fi

    if is_pinned_sha "$ref"; then
      # Soft warning: missing version comment for Dependabot.
      if ! has_version_comment "$line"; then
        echo "WARN  ${file}:${lineno}: pinned SHA has no trailing version comment: ${ref}"
        warnings=$((warnings + 1))
      fi
      continue
    fi

    echo "ERROR ${file}:${lineno}: unpinned or invalid uses: ${ref}"
    violations=$((violations + 1))
  done < "$file"
done < <(collect_files | sort -u)

if [[ "$warnings" -gt 0 ]]; then
  echo "pin-check: ${warnings} warning(s) (missing version comments)"
fi

if [[ "$violations" -gt 0 ]]; then
  echo "pin-check: ${violations} violation(s) — all external uses: must be 40-char lowercase hex SHAs"
  exit 1
fi

echo "pin-check: OK"
exit 0
