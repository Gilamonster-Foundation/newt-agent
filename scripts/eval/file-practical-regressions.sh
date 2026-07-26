#!/usr/bin/env bash
# file-practical-regressions.sh — open ONE deduplicated GitHub issue per
# practical-live-regression scenario that FAILED (behavioral). INFRA rows are
# reported but do NOT open model-regression issues.
#
# Sibling of file-regressions.sh for the UAT driver
# (docs/testing/scripts/uat_tool_loop.sh). Consumes the TSV that driver emits:
#
#     case<TAB>PASS|FAIL|INFRA<TAB>details
#
# Dedup: gh issue list --search 'in:title "<stable title>" state:open'
# then create only if none is open. Titles are stable per scenario:
#   practical-regression: <case>
#
# SECURITY: no hosts/keys/GPUs here — model/provider names only.
#
# Usage:
#   file-practical-regressions.sh --current <results.tsv> [--dry-run]
#   file-practical-regressions.sh --self-test
set -uo pipefail

CURRENT=""
DRY_RUN=false
SELF_TEST=false

while [ $# -gt 0 ]; do
  case "$1" in
    --current) CURRENT="$2"; shift 2;;
    --dry-run) DRY_RUN=true; shift;;
    --self-test) SELF_TEST=true; DRY_RUN=true; shift;;
    -h|--help) sed -n '2,24p' "${BASH_SOURCE[0]}"; exit 0;;
    *) echo "file-practical-regressions: unknown arg '$1'" >&2; exit 2;;
  esac
done

run_url() {
  if [ -n "${GITHUB_SERVER_URL:-}" ] && [ -n "${GITHUB_REPOSITORY:-}" ] && [ -n "${GITHUB_RUN_ID:-}" ]; then
    echo "${GITHUB_SERVER_URL}/${GITHUB_REPOSITORY}/actions/runs/${GITHUB_RUN_ID}"
  else
    echo "(local run — no GITHUB_RUN_ID)"
  fi
}

is_open() {
  local title="$1"
  if [ -n "${FR_STUB_OPEN_TITLES:-}" ]; then
    grep -Fxq -- "$title" <<<"$FR_STUB_OPEN_TITLES"
    return
  fi
  local n
  n="$(gh issue list --search "in:title \"$title\" state:open" \
        --json number --jq 'length' 2>/dev/null || echo 0)"
  [ "${n:-0}" != "0" ]
}

ensure_labels() {
  if $DRY_RUN; then
    echo "WOULD ensure labels: regression, nightly-eval"
    return
  fi
  gh label create regression  --color B60205 \
    --description "behavioral regression caught by an eval rung" 2>/dev/null || true
  gh label create nightly-eval --color 1D76DB \
    --description "opened automatically by the nightly-eval workflow" 2>/dev/null || true
}

file_issue() {
  local title="$1" body="$2"
  if $DRY_RUN; then
    echo "WOULD FILE: $title"
    return
  fi
  gh issue create \
    --label regression --label nightly-eval \
    --title "$title" --body "$body"
}

report() {
  local current="$1"
  local labels_ensured=false filed=0 infra=0

  if [ ! -f "$current" ]; then
    echo "no results file: $current"
    return 0
  fi

  local case status details title body
  while IFS=$'\t' read -r case status details; do
    [ -n "${case:-}" ] || continue
    case "$status" in
      PASS)
        echo "OK: $case — $details"
        ;;
      INFRA)
        infra=$((infra + 1))
        echo "INFRA (no issue): $case — $details"
        ;;
      FAIL)
        title="practical-regression: ${case}"
        if is_open "$title"; then
          echo "SKIP (issue already open): $title — $details"
          continue
        fi
        body="Practical live regression scenario **\`${case}\`** FAILED.

Details: \`${details}\`

- Suite driver: \`docs/testing/scripts/uat_tool_loop.sh\`
- Run: $(run_url)
- Provider/model: \`${NEWT_PROVIDER:-"(config)"}\` / \`${NEWT_DGX_MODEL:-${NEWT_DEFAULT_MODEL:-"(config)"}}\`

This is a **behavioral** failure (not infrastructure). The harness is considered
broken when the model cannot complete this practical task.

_Opened automatically by the \`practical-regression\` workflow
(\`scripts/eval/file-practical-regressions.sh\`)._"
        if ! $labels_ensured; then ensure_labels; labels_ensured=true; fi
        file_issue "$title" "$body"
        filed=$((filed + 1))
        ;;
      *)
        echo "UNKNOWN status '$status' for $case — ignored"
        ;;
    esac
  done < "$current"

  echo "filed ${filed} new issue(s); ${infra} INFRA row(s) (not filed)."
  return 0
}

self_test() {
  local tmp; tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN
  local cur="$tmp/results.tsv"
  {
    printf 'line-count\tPASS\tok\n'
    printf 'rename\tFAIL\told name still present\n'
    printf 'bugfix\tINFRA\tconnection refused\n'
    printf 'branch\tFAIL\twrong branch\n'
  } > "$cur"

  echo "### A: FAIL with no open issue -> WOULD FILE; INFRA ignored"
  local outA
  outA="$(FR_STUB_OPEN_TITLES="" report "$cur")"
  echo "$outA"
  echo "$outA" | grep -q 'WOULD FILE: practical-regression: rename' || {
    echo "self-test A failed (rename)"; return 1; }
  echo "$outA" | grep -q 'WOULD FILE: practical-regression: branch' || {
    echo "self-test A failed (branch)"; return 1; }
  echo "$outA" | grep -q 'INFRA (no issue): bugfix' || {
    echo "self-test A failed (infra)"; return 1; }

  echo "### B: already-open rename -> SKIP"
  local outB
  outB="$(FR_STUB_OPEN_TITLES="practical-regression: rename" report "$cur")"
  echo "$outB"
  echo "$outB" | grep -q 'SKIP (issue already open): practical-regression: rename' || {
    echo "self-test B failed"; return 1; }
  echo "$outB" | grep -q 'WOULD FILE: practical-regression: branch' || {
    echo "self-test B failed (branch still fileable)"; return 1; }

  echo "self-test: OK"
  return 0
}

if $SELF_TEST; then
  self_test
  exit $?
fi

if [ -z "$CURRENT" ]; then
  echo "file-practical-regressions: --current <results.tsv> required" >&2
  exit 2
fi

report "$CURRENT"
exit $?
