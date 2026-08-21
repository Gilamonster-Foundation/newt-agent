#!/usr/bin/env bash
# Model survey sweep (#75) — run the ground-truth rig across a list of models on
# ONE endpoint, checkpointed, and emit a results matrix. The repeatable core of
# the nightly model-survey / cross-family-tuning program.
#
# Usage:
#   NEWT_BIN=<release/newt> NEWT_EVAL_BIN=<release/newt-eval> \
#   survey_models.sh --endpoint URL --hardware "gpu-runner 4060 Ti" \
#       --corpus DIR --surface JSON --out DIR \
#       --models "m1 m2 ..."|auto [--require-tools] [--repeats K] [--timeout 1200]
#
# - --models auto: discover every model on the endpoint (/api/tags).
# - --require-tools: skip models lacking the `tools` capability (cheap /api/show
#   probe) — no-tool models are kept on disk for the swarm planning tier, not
#   surveyed as coders.
# - --repeats K (default 1): run each model K times and report a pass-RATE.
#   Borderline models are stochastic (the 0.6.8 variance finding — nemotron3:33b
#   and gemma4:e4b flipped on reruns), so a single cell is indicative, not
#   reliable; K runs turn it into pass/K. Per-run scorecards land in
#   results/<safe>/rN.json; the aggregate pass-rate in results/<safe>.json.
# - Checkpointed: a model whose results/<safe>.json already exists is skipped,
#   and within a model a run whose rN.json exists is skipped (resume after
#   interruption mid-sweep or mid-repeat).
# - Per-model timeout (default 1200s): a slower run is recorded as `timeout` —
#   itself a result (the model is too slow for the harness budget).
# - Scoring is the 0.6.8 verify oracle via rig_pyo3_examples.sh. The newt binary
#   under test is whatever NEWT_BIN points at.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

ENDPOINT="" HARDWARE="" CORPUS="" SURFACE="" OUT="" MODELS="" TIMEOUT=1200 REQUIRE_TOOLS=0 REPEATS=1
while [ $# -gt 0 ]; do
  case "$1" in
    --endpoint) ENDPOINT=$2; shift 2 ;;
    --hardware) HARDWARE=$2; shift 2 ;;
    --corpus)   CORPUS=$2; shift 2 ;;
    --surface)  SURFACE=$2; shift 2 ;;
    --out)      OUT=$2; shift 2 ;;
    --models)   MODELS=$2; shift 2 ;;   # space-separated list, or `auto` to discover
    --repeats)  REPEATS=$2; shift 2 ;;  # K runs/model → a pass-rate (stochastic failures)
    --timeout)  TIMEOUT=$2; shift 2 ;;
    --require-tools) REQUIRE_TOOLS=1; shift ;;  # skip models lacking the tools capability
    *) echo "survey: unknown arg $1" >&2; exit 1 ;;
  esac
done
for v in ENDPOINT HARDWARE CORPUS SURFACE OUT MODELS; do
  [ -n "${!v}" ] || { echo "survey: --${v,,} is required (use --models auto to discover)" >&2; exit 1; }
done

# True iff the endpoint reports the `tools` capability for $1 — a cheap
# /api/show metadata probe (no model load). Reasoning / no-tool models lack it;
# we keep them on disk for the swarm *planning* tier but don't survey them as
# coders (#75 — tool-supporting focus).
tool_supported() {
  curl -sk "$ENDPOINT/api/show" -d "{\"model\":\"$1\"}" 2>/dev/null \
    | jq -e '(.capabilities // []) | index("tools")' >/dev/null 2>&1
}

# `--models auto`: discover every model on the endpoint (then --require-tools
# filters to the tool-capable ones). This is how new models join the survey —
# pull a model, re-run, it's picked up.
if [ "$MODELS" = "auto" ]; then
  MODELS=$(curl -sk "$ENDPOINT/api/tags" 2>/dev/null | jq -r '.models[].name' | sort)
  echo "survey: discovered $(echo "$MODELS" | wc -w) models on $ENDPOINT" >&2
fi

RESULTS="$OUT/results"
mkdir -p "$RESULTS"

# Classify one per-run scorecard into the four-outcome rubric (+error). Reused
# by the aggregation below and by the matrix emission.
CLASSIFY='
  if .error then "error"
  elif ((.python_files // 0) == 0) then "no_output"
  elif ((.score.details // "") | test("no imports")) then "vacuous"
  elif .score.passed then "pass"
  else "fail" end'

for model in $MODELS; do
  safe=$(echo "$model" | tr ':/' '__')
  card="$RESULTS/$safe.json"        # per-model SUMMARY = the checkpoint
  if [ -f "$card" ]; then echo "survey: skip $model (have result)" >&2; continue; fi
  if [ "$REQUIRE_TOOLS" = 1 ] && ! tool_supported "$model"; then
    echo "survey: skip $model (no tools capability — kept for the swarm planning tier)" >&2
    jq -n --arg m "$model" --arg hw "$HARDWARE" \
      '{model:$m, hardware:$hw, repeats:0, pass:0, fail:0, vacuous:0, no_output:0, error:1,
        runs:["error"],
        representative:{model:$m, hardware:$hw,
                        error:"no-tool (excluded; /api/show has no tools capability)"}}' > "$card"
    continue
  fi
  runs_dir="$RESULTS/$safe"         # per-run scorecards r1.json … rK.json
  mkdir -p "$runs_dir" "$OUT/run-$safe"   # the latter so per-run rN.err has a parent
  for r in $(seq 1 "$REPEATS"); do
    runcard="$runs_dir/r$r.json"
    [ -f "$runcard" ] && { echo "survey: skip $model run $r (have run)" >&2; continue; }
    echo "survey: [$(date +%H:%M:%S)] $model run $r/$REPEATS (timeout ${TIMEOUT}s) ..." >&2
    rundir="$OUT/run-$safe/r$r"
    if timeout -k 30 "$TIMEOUT" bash "$SCRIPT_DIR/rig_pyo3_examples.sh" live \
          --out "$rundir" --corpus "$CORPUS" --surface "$SURFACE" \
          --model "$model" --url "$ENDPOINT" > "$runcard.tmp" 2>"$rundir.err"; then
      if jq -e . "$runcard.tmp" >/dev/null 2>&1; then
        jq --arg m "$model" --arg hw "$HARDWARE" '. + {model:$m, hardware:$hw}' "$runcard.tmp" > "$runcard"
      else
        jq -n --arg m "$model" --arg hw "$HARDWARE" '{model:$m, hardware:$hw, error:"no scorecard"}' > "$runcard"
      fi
    else
      rc=$?
      note=$([ "$rc" = 124 ] && echo "timeout" || echo "error(rc=$rc)")
      jq -n --arg m "$model" --arg hw "$HARDWARE" --arg n "$note" '{model:$m, hardware:$hw, error:$n}' > "$runcard"
    fi
    rm -f "$runcard.tmp"
    # Scoped cleanup: kill any newt left holding the GPU for THIS run (matched by
    # the run dir in its argv), never touching unrelated newt sessions.
    pkill -f "$rundir" 2>/dev/null || true
    sleep 2
  done
  # Aggregate the K runs → a pass-rate summary (the checkpoint card). A
  # representative run (a pass if any, else the first) carries forensics to the
  # matrix; the counts carry the stochastic story.
  jq -s --arg m "$model" --arg hw "$HARDWARE" '
    (map(. + {outcome: ('"$CLASSIFY"')})) as $runs
    | {
        model: $m, hardware: $hw,
        repeats:   ($runs | length),
        pass:      ($runs | map(select(.outcome=="pass"))      | length),
        fail:      ($runs | map(select(.outcome=="fail"))      | length),
        vacuous:   ($runs | map(select(.outcome=="vacuous"))   | length),
        no_output: ($runs | map(select(.outcome=="no_output")) | length),
        error:     ($runs | map(select(.outcome=="error"))     | length),
        runs:      ($runs | map(.outcome)),
        representative: ( ($runs | map(select(.outcome=="pass")) | .[0]) // ($runs[0]) )
      }' "$runs_dir"/r*.json > "$card"
done

# ── emit the matrix ─────────────────────────────────────────────────
# One row per model: a pass-RATE (pass/repeats) is the headline — single-run
# cells were "indicative, not reliable" (0.6.8 variance finding), so K repeats
# turn the stochastic outcome into a rate. The outcome breakdown
# (❌ fail ◐ vacuous ∅ no-output ⚠ error) and a representative run's forensics
# follow.
MATRIX="$OUT/matrix.md"
{
  echo "| model | pass-rate | outcomes | repr score | py | tool events | tokens in/out | capped | detail |"
  echo "|---|---|---|---|---|---|---|---|---|"
  for card in "$RESULTS"/*.json; do
    [ -f "$card" ] || continue
    jq -r '
      .representative as $r
      | "| `\(.model)` "
      + "| \(if .pass>0 then "✅ " else "" end)\(.pass)/\(.repeats) "
      + "| ❌\(.fail) ◐\(.vacuous) ∅\(.no_output) ⚠\(.error) "
      + "| \(if $r.score then ($r.score.score*1000|floor/1000|tostring) else "—" end) "
      + "| \($r.python_files // "—") "
      + "| \($r.forensics.max_turn_tool_events // "—") "
      + "| \($r.forensics.tokens_in // "—")/\($r.forensics.tokens_out // "—") "
      + "| \($r.forensics.likely_capped // "—") "
      + "| \((($r.score.details // $r.error // "") | tostring)[0:70]) |"
      ' "$card"
  done
} > "$MATRIX"
echo "survey: matrix → $MATRIX" >&2
cat "$MATRIX"
