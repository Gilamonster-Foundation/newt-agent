#!/usr/bin/env bash
# Verify-gated revert-RETRY loop (#73, R2) — the measurement instrument for
# "does retry move the needle?". Wraps the REAL gate (`newt-eval verify`) around
# `newt code`: run the task → gate the output → revert the fabricated files →
# re-prompt the model to recreate ONLY them, grounded → re-gate, up to N times.
#
# This is the rig-level proof before the in-loop agentic integration: if retry
# lifts the grounded-coverage of a fabrication-prone model, the deep wiring is
# justified; if not, that is itself the finding.
#
# Usage:
#   NEWT_BIN=<release/newt> NEWT_EVAL_BIN=<release/newt-eval> \
#   rig_retry_loop.sh --out DIR --corpus DIR --surface JSON \
#       --model MODEL --url OLLAMA_URL [--max-retries 2] [--num-ctx N]
#
# Output: workspace/, manifest.txt, prompts-r*.txt, session-r*.log, and
# scorecard.json {score, python_files, retries_used, final_accept, history[]}.
#
# NOTE: deliberately NOT `set -e` — `newt-eval verify`/`score` exit non-zero as
# an honest GATE SIGNAL (fabrications found), which is normal control flow here,
# not a fatal error.
set -uo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NEWT_EVAL_BIN="${NEWT_EVAL_BIN:?set NEWT_EVAL_BIN}"

TASK="create an examples folder and write one python script as an example for each and every PyO3 crate in this repository."
die() { echo "retry: $*" >&2; exit 1; }

OUT="" CORPUS="" SURFACE="" MODEL="" URL="" NUM_CTX="" MAX_RETRIES=2
while [ $# -gt 0 ]; do
  case "$1" in
    --out)         OUT=$2; shift 2 ;;
    --corpus)      CORPUS=$2; shift 2 ;;
    --surface)     SURFACE=$2; shift 2 ;;
    --model)       MODEL=$2; shift 2 ;;
    --url)         URL=$2; shift 2 ;;
    --num-ctx)     NUM_CTX=$2; shift 2 ;;
    --max-retries) MAX_RETRIES=$2; shift 2 ;;
    *) die "unknown arg: $1" ;;
  esac
done
for v in OUT CORPUS SURFACE MODEL URL; do [ -n "${!v}" ] || die "--${v,,} is required"; done

WS="$OUT/workspace"
mkdir -p "$WS" "$OUT/sandbox"
cp -r "$CORPUS"/. "$WS"/
cp "$SURFACE" "$OUT/python_surface.json"

# The knowledge-base fact (R1) — leads every prompt as the authoritative surface.
MANIFEST="$OUT/manifest.txt"
"$NEWT_EVAL_BIN" manifest --workspace "$CORPUS" > "$MANIFEST"

# Drive one `newt code` turn on the shared workspace with a given prompt file.
drive() {  # $1 = prompts file, $2 = session log
  NEWT_BIN="${NEWT_BIN:-}" "$SCRIPT_DIR/run_newt_session.sh" \
    "$MODEL" "$URL" "$WS" "$1" "$2" "$NUM_CTX" "$OUT/sandbox" >/dev/null
}

# ── turn 0: the task, with the manifest injected ────────────────────
printf '%s\n\n%s\nexit\n' "$(cat "$MANIFEST")" "$TASK" > "$OUT/prompts-r0.txt"
echo "retry: [$(date +%H:%M:%S)] turn 0 (task) ..." >&2
drive "$OUT/prompts-r0.txt" "$OUT/session-r0.log"

HISTORY="[]"
RETRIES_USED=0
FINAL_ACCEPT=false
# ── revert-retry loop ───────────────────────────────────────────────
for r in $(seq 1 "$MAX_RETRIES"); do
  # Gate the current workspace with the REAL R2 gate (exit 2 = fabrications, a
  # signal, captured not aborted).
  verify_out=$("$NEWT_EVAL_BIN" verify --workspace "$WS" --manifest-from "$CORPUS" 2>/dev/null)
  gate_rc=$?
  # Per-iteration score for the history — two steps so the scorer's non-zero exit
  # can't corrupt the captured JSON.
  score_raw=$("$NEWT_EVAL_BIN" score --workspace "$WS" --surface-dir "$OUT" --json 2>/dev/null)
  iter_score=$(printf '%s' "$score_raw" | jq -c '{score:.score, passed:.passed}' 2>/dev/null)
  [ -n "$iter_score" ] || iter_score='{}'
  # The gate indents each reverted file with two spaces: "  <path>  [mods]".
  bad_files=$(printf '%s\n' "$verify_out" | grep -E '^  ' | awk '{print $1}')
  HISTORY=$(echo "$HISTORY" | jq -c --argjson s "$iter_score" --arg n "$(echo "$bad_files" | grep -c . || true)" \
    '. + [{iter:'"$((r-1))"', score:$s.score, passed:$s.passed, fabricating_files:($n|tonumber)}]')

  if [ "$gate_rc" -eq 0 ]; then
    echo "retry: gate ACCEPTS after turn $((r-1)) — done" >&2
    FINAL_ACCEPT=true
    break
  fi

  # Revert: remove exactly the gate's revert set.
  mods=$(echo "$verify_out" | grep -oE '\[[^]]+\]' | tr -d '[]' | tr ',' ' ' | tr -s ' ')
  for f in $bad_files; do rm -f "$WS/$f"; done
  echo "retry: [$(date +%H:%M:%S)] turn $r — reverted $(echo "$bad_files" | grep -c .) file(s), regenerating ..." >&2

  # Re-prompt: recreate ONLY the reverted examples, grounded in the surface.
  {
    cat "$MANIFEST"
    printf '\n%s\n' "Some example files were removed because they imported modules that do not exist (${mods}). Recreate one correct example for each removed crate, importing ONLY the authoritative paths listed in the PYO3 IMPORT SURFACE above — exactly as the example files still present already do. Do not import the crate name directly."
    printf 'exit\n'
  } > "$OUT/prompts-r$r.txt"
  drive "$OUT/prompts-r$r.txt" "$OUT/session-r$r.log"
  RETRIES_USED=$r
done

# ── final score + scorecard ─────────────────────────────────────────
final_raw=$("$NEWT_EVAL_BIN" score --workspace "$WS" --surface-dir "$OUT" --json 2>/dev/null)
FINAL=$(printf '%s' "$final_raw" | jq -c '.' 2>/dev/null)
[ -n "$FINAL" ] || FINAL='{}'
PY=$(find "$WS" -name '*.py' | wc -l | tr -d ' ')
jq -n --argjson final "$FINAL" --argjson hist "$HISTORY" \
  --arg model "$MODEL" --argjson py "$PY" --argjson retries "$RETRIES_USED" \
  --argjson accept "$FINAL_ACCEPT" \
  '{model:$model, python_files:$py, retries_used:$retries, final_accept:$accept,
    score:$final.score, passed:$final.passed, details:$final.details, history:$hist}' \
  > "$OUT/scorecard.json"
echo "retry: scorecard → $OUT/scorecard.json" >&2
jq -c '{model, score, passed, python_files, retries_used, final_accept}' "$OUT/scorecard.json"
