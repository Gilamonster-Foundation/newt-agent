#!/usr/bin/env bash
# file-regressions.sh — open ONE deduplicated GitHub issue per eval "rung" whose
# behavioral pass-rate REGRESSED against the last green baseline. Idempotent: a
# persistent regression re-uses its already-open issue instead of spamming a new
# one every night.
#
# This is the A2 half of the nightly-eval automation (#790). It is driven by
# .github/workflows/nightly-eval.yml (#789), which runs the EXISTING
# scripts/eval/ratchet.sh over a small (task x mode x model) tier and captures
# its output here.
#
# WRAPS ratchet.sh — it does NOT run any model. It parses the tab-separated
# RATCHET lines ratchet.sh emits on stdout:
#
#     RATCHET<TAB>task<TAB>mode<TAB>model<TAB>behavioral<TAB>details
#
# A "rung" is keyed by task/mode (model is a CONTROL, per RATCHET.md — the
# single-vs-crew staircase is what we watch). A trial "passes" when its
# behavioral column matches ^PASS (covers PASS and PASS?gameable). A rung
# REGRESSED when its current pass-rate (pass/total across the run's trials) is
# strictly lower than the same rung's pass-rate in the baseline.
#
# Dedup mirrors brush-watch.yml:52-72 —
#   gh issue list --search 'in:title "<stable rung-keyed title>" state:open'
# then gh issue create only if none is open.
#
# SECURITY: no hosts/keys/GPUs here — model NAMES only, resolved from the
# operator's local ~/.newt config by ratchet.sh (never committed).
#
# Usage:
#   file-regressions.sh --current <run.tsv> [--baseline <base.tsv>] [--dry-run]
#   file-regressions.sh --self-test          # synthetic fixtures; no gh, no models
#
# --dry-run prints the file-or-skip decision for every rung and creates nothing.
# When no --baseline is given, the newest OTHER nightly-*.tsv in the results dir
# is used. For testing the idempotent skip path without network, set
# $FR_STUB_OPEN_TITLES (newline-separated issue titles) — those titles are then
# treated as "already open" instead of calling gh.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RESULTS_DIR="${FR_RESULTS_DIR:-$HERE/results}"

CURRENT="" BASELINE="" DRY_RUN=false SELF_TEST=false
while [ $# -gt 0 ]; do
  case "$1" in
    --current)  CURRENT="$2"; shift 2;;
    --baseline) BASELINE="$2"; shift 2;;
    --results-dir) RESULTS_DIR="$2"; shift 2;;
    --dry-run)  DRY_RUN=true; shift;;
    --self-test) SELF_TEST=true; DRY_RUN=true; shift;;
    -h|--help) sed -n '2,40p' "${BASH_SOURCE[0]}"; exit 0;;
    *) echo "file-regressions: unknown arg '$1'" >&2; exit 2;;
  esac
done

# --- aggregation ------------------------------------------------------------
# Emit "<task>/<mode>\t<pass>\t<total>" for each rung in a RATCHET tsv file.
aggregate() {
  awk -F'\t' '
    $1=="RATCHET" {
      key=$2"/"$3
      total[key]++
      if ($5 ~ /^PASS/) pass[key]++
    }
    END { for (k in total) printf "%s\t%d\t%d\n", k, (pass[k]+0), total[k] }
  ' "$1" | sort
}

# Raw RATCHET rows for one rung (task, mode) from a file — for the issue body.
rung_rows() {
  local file="$1" task="$2" mode="$3"
  awk -F'\t' -v t="$task" -v m="$mode" \
    '$1=="RATCHET" && $2==t && $3==m {print}' "$file"
}

# --- issue plumbing ---------------------------------------------------------
# True if an open issue with exactly this title exists. Honors the test stub.
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
  # Idempotent — labels may already exist. || true so a re-run never errors.
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

run_url() {
  if [ -n "${GITHUB_SERVER_URL:-}" ] && [ -n "${GITHUB_REPOSITORY:-}" ] && [ -n "${GITHUB_RUN_ID:-}" ]; then
    echo "${GITHUB_SERVER_URL}/${GITHUB_REPOSITORY}/actions/runs/${GITHUB_RUN_ID}"
  else
    echo "(local run — no GITHUB_RUN_ID)"
  fi
}

# --- the report -------------------------------------------------------------
# Compare current vs baseline, decide file/skip/ok per rung, act (or print).
# Returns the count of NEW issues it filed (0 in dry-run).
report() {
  local current="$1" baseline="$2"
  local labels_ensured=false filed=0

  declare -A CUR_P CUR_T BASE_P BASE_T
  local k p t
  while IFS=$'\t' read -r k p t; do
    [ -n "$k" ] || continue
    CUR_P["$k"]="$p"; CUR_T["$k"]="$t"
  done < <(aggregate "$current")

  if [ -n "$baseline" ] && [ -f "$baseline" ]; then
    while IFS=$'\t' read -r k p t; do
      [ -n "$k" ] || continue
      BASE_P["$k"]="$p"; BASE_T["$k"]="$t"
    done < <(aggregate "$baseline")
  else
    echo "no baseline available — recording current run as the first baseline; nothing to compare."
    return 0
  fi

  local rung task mode cp ct bp bt
  for rung in $(printf '%s\n' "${!CUR_T[@]}" | sort); do
    task="${rung%%/*}"; mode="${rung##*/}"
    cp="${CUR_P[$rung]}"; ct="${CUR_T[$rung]}"
    bp="${BASE_P[$rung]:-}"; bt="${BASE_T[$rung]:-}"

    if [ -z "$bt" ]; then
      echo "NEW RUNG (no baseline): $rung  current=${cp}/${ct}"
      continue
    fi

    # Regression iff current rate < baseline rate (float compare via awk).
    if awk "BEGIN{exit !((${cp}/${ct}) < (${bp}/${bt}))}"; then
      local title body rows
      title="nightly-eval regression: ${rung}"
      if is_open "$title"; then
        echo "SKIP (issue already open): $title  [${bp}/${bt} -> ${cp}/${ct}]"
        continue
      fi
      rows="$(rung_rows "$current" "$task" "$mode")"
      body="Behavioral pass-rate for rung **\`${rung}\`** regressed against the last green baseline.

| | pass / total |
|---|---|
| baseline | ${bp} / ${bt} |
| current  | ${cp} / ${ct} |

Current run rows (from \`ratchet.sh\`):

\`\`\`
${rows}
\`\`\`

- Run: $(run_url)
- Baseline: \`${baseline}\`

_Opened automatically by the \`nightly-eval\` workflow (\`scripts/eval/file-regressions.sh\`)._"
      if ! $labels_ensured; then ensure_labels; labels_ensured=true; fi
      file_issue "$title" "$body"
      filed=$((filed + 1))
    else
      echo "OK (no regression): $rung  [${bp}/${bt} -> ${cp}/${ct}]"
    fi
  done

  echo "filed ${filed} new issue(s)."
  return 0
}

# --- self-test --------------------------------------------------------------
# Synthetic fixtures exercise all three decision paths with NO gh and NO models:
#   * a regressed rung with no open issue      -> WOULD FILE
#   * that same rung, but already open (stub)   -> SKIP
#   * a stable rung                             -> OK (no regression)
#   * a rung that only exists this run          -> NEW RUNG
self_test() {
  local tmp; tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN
  local base="$tmp/baseline.tsv" cur="$tmp/current.tsv"

  # baseline: T0/single 3/3, T1/crew 3/3, T2/single 0/3 (already failing)
  {
    printf 'RATCHET\tT0-fix-add\tsingle\tqwen2.5-coder:7b\tPASS\ttests_pass=ok\n'
    printf 'RATCHET\tT0-fix-add\tsingle\tqwen2.5-coder:7b\tPASS\ttests_pass=ok\n'
    printf 'RATCHET\tT0-fix-add\tsingle\tqwen2.5-coder:7b\tPASS\ttests_pass=ok\n'
    printf 'RATCHET\tT1-parse-port\tcrew\tqwen2.5-coder:7b\tPASS\tleaves=1\n'
    printf 'RATCHET\tT1-parse-port\tcrew\tqwen2.5-coder:7b\tPASS\tleaves=1\n'
    printf 'RATCHET\tT1-parse-port\tcrew\tqwen2.5-coder:7b\tPASS\tleaves=2\n'
    printf 'RATCHET\tT2-humanize-duration\tsingle\tqwen2.5-coder:7b\tFAIL\ttests_pass=fail\n'
    printf 'RATCHET\tT2-humanize-duration\tsingle\tqwen2.5-coder:7b\tFAIL\ttests_pass=fail\n'
    printf 'RATCHET\tT2-humanize-duration\tsingle\tqwen2.5-coder:7b\tFAIL\ttests_pass=fail\n'
  } > "$base"

  # current: T0/single regresses to 1/3; T1/crew holds 3/3; T2/single stays 0/3;
  #          T3/single is brand new (no baseline rung).
  {
    printf 'RATCHET\tT0-fix-add\tsingle\tqwen2.5-coder:7b\tPASS\ttests_pass=ok\n'
    printf 'RATCHET\tT0-fix-add\tsingle\tqwen2.5-coder:7b\tFAIL\ttests_pass=fail\n'
    printf 'RATCHET\tT0-fix-add\tsingle\tqwen2.5-coder:7b\tFAIL\ttests_pass=fail\n'
    printf 'RATCHET\tT1-parse-port\tcrew\tqwen2.5-coder:7b\tPASS\tleaves=1\n'
    printf 'RATCHET\tT1-parse-port\tcrew\tqwen2.5-coder:7b\tPASS\tleaves=1\n'
    printf 'RATCHET\tT1-parse-port\tcrew\tqwen2.5-coder:7b\tPASS?gameable\tleaves=2\n'
    printf 'RATCHET\tT2-humanize-duration\tsingle\tqwen2.5-coder:7b\tFAIL\ttests_pass=fail\n'
    printf 'RATCHET\tT2-humanize-duration\tsingle\tqwen2.5-coder:7b\tFAIL\ttests_pass=fail\n'
    printf 'RATCHET\tT2-humanize-duration\tsingle\tqwen2.5-coder:7b\tFAIL\ttests_pass=fail\n'
    printf 'RATCHET\tT3-format-temperature\tsingle\tqwen2.5-coder:7b\tPASS\ttests_pass=ok\n'
  } > "$cur"

  local fails=0

  echo "### self-test A: regressed rung, no open issue -> WOULD FILE"
  local outA
  outA="$(FR_STUB_OPEN_TITLES="" report "$cur" "$base")"
  echo "$outA"
  grep -q "WOULD FILE: nightly-eval regression: T0-fix-add/single" <<<"$outA" || { echo "  !! expected WOULD FILE for T0-fix-add/single"; fails=$((fails+1)); }
  grep -q "OK (no regression): T1-parse-port/crew" <<<"$outA" || { echo "  !! expected OK for T1-parse-port/crew"; fails=$((fails+1)); }
  grep -q "OK (no regression): T2-humanize-duration/single" <<<"$outA" || { echo "  !! expected OK (still-failing) for T2-humanize-duration/single"; fails=$((fails+1)); }
  grep -q "NEW RUNG (no baseline): T3-format-temperature/single" <<<"$outA" || { echo "  !! expected NEW RUNG for T3-format-temperature/single"; fails=$((fails+1)); }
  grep -q "filed 1 new issue" <<<"$outA" || { echo "  !! expected 1 filed"; fails=$((fails+1)); }

  echo
  echo "### self-test B: same regression, issue ALREADY OPEN (stub) -> SKIP (idempotent)"
  local outB
  outB="$(FR_STUB_OPEN_TITLES="nightly-eval regression: T0-fix-add/single" report "$cur" "$base")"
  echo "$outB"
  grep -q "SKIP (issue already open): nightly-eval regression: T0-fix-add/single" <<<"$outB" || { echo "  !! expected SKIP for already-open T0-fix-add/single"; fails=$((fails+1)); }
  grep -q "filed 0 new issue" <<<"$outB" || { echo "  !! expected 0 filed on the idempotent pass"; fails=$((fails+1)); }

  echo
  if [ "$fails" -eq 0 ]; then
    echo "SELF-TEST PASS"
    return 0
  fi
  echo "SELF-TEST FAIL ($fails assertion(s))"
  return 1
}

# --- main -------------------------------------------------------------------
if $SELF_TEST; then
  self_test
  exit $?
fi

[ -n "$CURRENT" ] || { echo "usage: file-regressions.sh --current <run.tsv> [--baseline <f>] [--dry-run]" >&2; exit 2; }
[ -f "$CURRENT" ] || { echo "file-regressions: current results not found: $CURRENT" >&2; exit 2; }

# Default baseline = newest OTHER nightly-*.tsv in the results dir. Normalize the
# current path to absolute so it is excluded even when passed in relative.
if [ -z "$BASELINE" ]; then
  cur_abs="$(cd "$(dirname "$CURRENT")" && pwd)/$(basename "$CURRENT")"
  BASELINE="$(ls -1t "$RESULTS_DIR"/nightly-*.tsv 2>/dev/null | grep -vFx "$cur_abs" | head -1 || true)"
  [ -n "$BASELINE" ] && echo "using baseline: $BASELINE"
fi

report "$CURRENT" "$BASELINE"
