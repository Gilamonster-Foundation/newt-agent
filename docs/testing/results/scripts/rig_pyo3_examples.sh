#!/usr/bin/env bash
# Ground-truth stress rig (#75) — replay the "one Python example per PyO3 crate"
# incident against the real stack and score whether the output is usable.
#
# This ties the rig together end-to-end:
#   snapshot corpus -> drive `newt code` headless -> score -> forensics -> scorecard
#
# Usage:
#   rig_pyo3_examples.sh dry-run --out DIR [--surface JSON]
#   rig_pyo3_examples.sh live    --out DIR --corpus DIR --surface JSON \
#                                --model MODEL --url OLLAMA_URL [--num-ctx N]
#
#   dry-run : seed CANNED incident output (fabricated imports) instead of
#             running a model — exercises the score+scorecard pipeline with zero
#             DGX time. This is how the rig is validated without inference.
#   live    : snapshot CORPUS into a fresh workspace, drive `newt --no-splash
#             code` headless (via run_newt_session.sh) against MODEL, then score
#             and collect forensics from the session's sandboxed ~/.newt.
#
# Scoring is the Rust-tested verify oracle (`newt-eval score`, #339/#340/#342).
# This script is the LIVE driver + glue; like the b56 gauntlet it is not a CI
# gate (the logic worth testing lives in Rust). Override the binaries with
# NEWT_BIN (the worker, used by run_newt_session.sh) and NEWT_EVAL_BIN.
#
# Output in --out DIR: workspace/, prompts.txt, score.json, scorecard.json,
#   and (live) session.log + forensics from the run's conversations.db.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NEWT_EVAL_BIN="${NEWT_EVAL_BIN:-$HOME/.cache/newt-bench-target/release/newt-eval}"

INCIDENT_PROMPT="create an examples folder and write one python script as an example for each and every PyO3 crate in this repository."

die() { echo "rig: $*" >&2; exit 1; }

# ── parse args ──────────────────────────────────────────────────────
[ $# -ge 1 ] || die "usage: $0 dry-run|live --out DIR [...]; see header"
MODE=$1; shift
OUT="" CORPUS="" SURFACE="" MODEL="" URL="" NUM_CTX=""
while [ $# -gt 0 ]; do
  case "$1" in
    --out)     OUT=$2; shift 2 ;;
    --corpus)  CORPUS=$2; shift 2 ;;
    --surface) SURFACE=$2; shift 2 ;;
    --model)   MODEL=$2; shift 2 ;;
    --url)     URL=$2; shift 2 ;;
    --num-ctx) NUM_CTX=$2; shift 2 ;;
    *) die "unknown arg: $1" ;;
  esac
done
[ -n "$OUT" ] || die "--out DIR is required"

WS="$OUT/workspace"
PROMPTS="$OUT/prompts.txt"
SCORE_JSON="$OUT/score.json"
SCORECARD="$OUT/scorecard.json"
mkdir -p "$WS"

# ── drive ───────────────────────────────────────────────────────────
DRIVE="dry-run"
SANDBOX=""
case "$MODE" in
  dry-run)
    # Seed canned incident output: fabricated module imports the model "wrote".
    mkdir -p "$WS/examples"
    cat > "$WS/examples/newt_core_example.py" <<'PY'
from newt_core import classify, Caveats, CountBound
caveats = Caveats.new()
PY
    cat > "$WS/examples/newt_data_example.py" <<'PY'
from newt_data import DataStore
ds = DataStore()
ds.load_csv("x.csv")
PY
    cat > "$WS/examples/newt_agent_core_example.py" <<'PY'
from newt_agent.core import Router, Tier
import os
PY
    # A surface for the dry-run: the REAL umbrella module the model should have
    # used. Caller may override with --surface.
    if [ -n "$SURFACE" ]; then
      cp "$SURFACE" "$OUT/python_surface.json"
    else
      cat > "$OUT/python_surface.json" <<'JSON'
{"modules": ["newt_agent.core", "newt_agent.data", "newt_agent.tools"]}
JSON
    fi
    ;;
  live)
    [ -n "$CORPUS" ]  || die "live mode needs --corpus DIR"
    [ -n "$SURFACE" ] || die "live mode needs --surface JSON"
    [ -n "$MODEL" ]   || die "live mode needs --model MODEL"
    [ -n "$URL" ]     || die "live mode needs --url OLLAMA_URL"
    DRIVE="live:$MODEL"
    cp -r "$CORPUS"/. "$WS"/
    cp "$SURFACE" "$OUT/python_surface.json"
    printf '%s\nexit\n' "$INCIDENT_PROMPT" > "$PROMPTS"
    echo "rig: driving newt against $MODEL @ $URL ..." >&2
    # Explicit sandbox under --out (7th arg) so we don't depend on a pre-seeded
    # /tmp/newt-bench/ existing (run_newt_session.sh's mktemp default).
    mkdir -p "$OUT/sandbox"
    SANDBOX=$(NEWT_BIN="${NEWT_BIN:-}" "$SCRIPT_DIR/run_newt_session.sh" \
      "$MODEL" "$URL" "$WS" "$PROMPTS" "$OUT/session.log" "$NUM_CTX" "$OUT/sandbox")
    echo "rig: session sandbox = $SANDBOX" >&2
    ;;
  *) die "mode must be dry-run or live (got: $MODE)" ;;
esac

# ── score ───────────────────────────────────────────────────────────
[ -x "$NEWT_EVAL_BIN" ] || die "newt-eval binary not found at $NEWT_EVAL_BIN (set NEWT_EVAL_BIN)"
"$NEWT_EVAL_BIN" score --workspace "$WS" --surface-dir "$OUT" --json > "$SCORE_JSON" || true

# ── forensics (live only — read the run's sandboxed conversations.db) ──
FORENSICS='{"available":false}'
if [ "$MODE" = "live" ] && [ -n "$SANDBOX" ]; then
  DB="$SANDBOX/.newt/conversations.db"
  if [ -f "$DB" ]; then
    # Largest turn's tool-event count; cap-hit inferred when it reaches the
    # default max_tool_rounds (25) — end_reason is not yet persisted (#75).
    EVENTS=$(sqlite3 "$DB" "SELECT COALESCE(MAX(json_array_length(events)),0) FROM turns;" 2>/dev/null || echo 0)
    TOK_IN=$(sqlite3 "$DB" "SELECT COALESCE(SUM(tokens_in),0) FROM turns;" 2>/dev/null || echo 0)
    TOK_OUT=$(sqlite3 "$DB" "SELECT COALESCE(SUM(tokens_out),0) FROM turns;" 2>/dev/null || echo 0)
    CAPPED=$([ "${EVENTS:-0}" -ge 25 ] && echo true || echo false)
    FORENSICS=$(jq -n --argjson ev "${EVENTS:-0}" --argjson ti "${TOK_IN:-0}" \
      --argjson to "${TOK_OUT:-0}" --argjson cap "$CAPPED" \
      '{available:true, max_turn_tool_events:$ev, tokens_in:$ti, tokens_out:$to, likely_capped:$cap}')
  fi
fi

# ── scorecard ───────────────────────────────────────────────────────
PY_COUNT=$(find "$WS" -name '*.py' -not -path '*/.venv/*' -not -path '*/__pycache__/*' | wc -l | tr -d ' ')
jq -n \
  --arg drive "$DRIVE" \
  --argjson py "$PY_COUNT" \
  --slurpfile score "$SCORE_JSON" \
  --argjson forensics "$FORENSICS" \
  '{drive:$drive, python_files:$py, score:$score[0], forensics:$forensics}' \
  | tee "$SCORECARD"
