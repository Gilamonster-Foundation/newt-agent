#!/usr/bin/env bash
# Model survey sweep (#75) — run the ground-truth rig across a list of models on
# ONE endpoint, checkpointed, and emit a results matrix. The repeatable core of
# the nightly model-survey / cross-family-tuning program.
#
# Usage:
#   NEWT_BIN=<release/newt> NEWT_EVAL_BIN=<release/newt-eval> \
#   survey_models.sh --endpoint URL --hardware "gnuc 4060 Ti" \
#       --corpus DIR --surface JSON --out DIR \
#       --models "m1 m2 ..." [--timeout 1200]
#
# - Checkpointed: a model whose results/<safe>.json already exists is skipped
#   (resume after interruption).
# - Per-model timeout (default 1200s): a slower run is recorded as `timeout` —
#   itself a result (the model is too slow for the harness budget).
# - Scoring is the 0.6.8 verify oracle via rig_pyo3_examples.sh. The newt binary
#   under test is whatever NEWT_BIN points at.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

ENDPOINT="" HARDWARE="" CORPUS="" SURFACE="" OUT="" MODELS="" TIMEOUT=1200
while [ $# -gt 0 ]; do
  case "$1" in
    --endpoint) ENDPOINT=$2; shift 2 ;;
    --hardware) HARDWARE=$2; shift 2 ;;
    --corpus)   CORPUS=$2; shift 2 ;;
    --surface)  SURFACE=$2; shift 2 ;;
    --out)      OUT=$2; shift 2 ;;
    --models)   MODELS=$2; shift 2 ;;
    --timeout)  TIMEOUT=$2; shift 2 ;;
    *) echo "survey: unknown arg $1" >&2; exit 1 ;;
  esac
done
for v in ENDPOINT HARDWARE CORPUS SURFACE OUT MODELS; do
  [ -n "${!v}" ] || { echo "survey: --${v,,} is required" >&2; exit 1; }
done

RESULTS="$OUT/results"
mkdir -p "$RESULTS"

for model in $MODELS; do
  safe=$(echo "$model" | tr ':/' '__')
  card="$RESULTS/$safe.json"
  if [ -f "$card" ]; then echo "survey: skip $model (have result)" >&2; continue; fi
  echo "survey: [$(date +%H:%M:%S)] $model (timeout ${TIMEOUT}s) ..." >&2
  rundir="$OUT/run-$safe"
  if timeout -k 30 "$TIMEOUT" bash "$SCRIPT_DIR/rig_pyo3_examples.sh" live \
        --out "$rundir" --corpus "$CORPUS" --surface "$SURFACE" \
        --model "$model" --url "$ENDPOINT" > "$card.tmp" 2>"$rundir.err"; then
    if jq -e . "$card.tmp" >/dev/null 2>&1; then
      jq --arg m "$model" --arg hw "$HARDWARE" '. + {model:$m, hardware:$hw}' "$card.tmp" > "$card"
    else
      jq -n --arg m "$model" --arg hw "$HARDWARE" '{model:$m, hardware:$hw, error:"no scorecard"}' > "$card"
    fi
  else
    rc=$?
    note=$([ "$rc" = 124 ] && echo "timeout" || echo "error(rc=$rc)")
    jq -n --arg m "$model" --arg hw "$HARDWARE" --arg n "$note" '{model:$m, hardware:$hw, error:$n}' > "$card"
  fi
  rm -f "$card.tmp"
  # Scoped cleanup: kill any newt left holding the GPU for THIS run (matched by
  # the run dir in its argv), never touching unrelated newt sessions.
  pkill -f "$rundir" 2>/dev/null || true
  sleep 2
done

# ── emit the matrix ─────────────────────────────────────────────────
MATRIX="$OUT/matrix.md"
{
  echo "| model | result | score | py | tool events | tokens in/out | capped | detail |"
  echo "|---|---|---|---|---|---|---|---|"
  for card in "$RESULTS"/*.json; do
    [ -f "$card" ] || continue
    jq -r '
      if .error then
        "| `\(.model)` | ⚠ \(.error) | — | — | — | — | — | — |"
      elif (.python_files // 0) == 0 then
        # Wrote no .py files: did NOT complete the task. A vacuous import score
        # of 1.0 is not a pass — record it as no-output (its own data point).
        "| `\(.model)` | ∅ no-output | — | 0 | \(.forensics.max_turn_tool_events // "—") | \(.forensics.tokens_in // "—")/\(.forensics.tokens_out // "—") | \(.forensics.likely_capped // "—") | wrote no .py files |"
      elif ((.score.details // "") | test("no imports")) then
        # Wrote .py file(s) but with NO imports — not a real PyO3 example. A
        # vacuous pass; distinct from both a real pass and a fabrication.
        "| `\(.model)` | ◐ vacuous | — | \(.python_files) | \(.forensics.max_turn_tool_events // "—") | \(.forensics.tokens_in // "—")/\(.forensics.tokens_out // "—") | \(.forensics.likely_capped // "—") | wrote .py but no imports |"
      else
        "| `\(.model)` | \(if .score.passed then "✅ PASS" else "❌ FAIL" end) | \(.score.score*1000|floor/1000) | \(.python_files) | \(.forensics.max_turn_tool_events // "—") | \(.forensics.tokens_in // "—")/\(.forensics.tokens_out // "—") | \(.forensics.likely_capped // "—") | \((.score.details // "")[0:70]) |"
      end' "$card"
  done
} > "$MATRIX"
echo "survey: matrix → $MATRIX" >&2
cat "$MATRIX"
