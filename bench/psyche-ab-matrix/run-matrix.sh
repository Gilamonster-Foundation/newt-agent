#!/usr/bin/env bash
# Psyche × OCAP A/B matrix runner — portable, reproducible on any host that can
# reach the ornith endpoint (dgx1). Sweeps {4 curated postures} × {OCAP off/on}
# over a set of self-verifying tasks, driving `newt solve` per cell, and emits a
# matrix report (matrix.md + results.csv).
#
# Everything is parameterised by env so gnuc (or any box) reproduces it:
#   NEWT             path to the newt binary            (default: repo target/release/newt, then $PATH)
#   ORNITH_ENDPOINT  OpenAI-compatible base URL         (default: dgx1 via Tailscale)
#   ORNITH_MODEL     served model id                    (default: ornith-1.0-35b-q8)
#   TASKS_DIR        task set                           (default: ./tasks)
#   MAX_ROUNDS       solve tool-round cap               (default: 15)
#   TASK_TIMEOUT     per-cell wall cap (seconds)        (default: 600)
#   OUT              output dir                         (default: ./runs/<stamp>)
#   POSTURES         space-sep subset of posture names  (default: all 4)
#   OCAP_MODES       space-sep of: off on               (default: both)
#
# Design notes:
#  - OCAP off  = `solve --non-interactive true` (yolo / full-access, no prompts).
#    OCAP on   = `NEWT_BENCH_OCAP=on` (confined, workspace-fenced writes).
#  - Postures are set with global flags / env only (no config edits per cell):
#      baseline   : (nothing)                      cognition off · tenacity standard · no crew
#      tenacity   : --tenacity relentless
#      crew       : NEWT_TEAM=1
#      obsessive  : --obsessive                    (contemplating + relentless + crew)
#  - ornith is a chat_completions backend, so the cognition dial (reasoning.effort,
#    a Responses-API concept) does NOT wire through — the axes that actually move
#    ornith are tenacity + crew. The matrix still records all four postures.
set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"

NEWT="${NEWT:-}"
if [ -z "$NEWT" ]; then
  for cand in "$REPO/target/release/newt" "$REPO/target/release/newt.exe" \
              "$REPO/target/debug/newt" "$REPO/target/debug/newt.exe" "$(command -v newt 2>/dev/null)"; do
    [ -n "$cand" ] && [ -x "$cand" ] && { NEWT="$cand"; break; }
  done
fi
[ -x "$NEWT" ] || { echo "FATAL: no newt binary (set NEWT=...); tried repo target/ + PATH" >&2; exit 2; }

# Portable python: prefer python3 (Linux/gnuc), fall back to python. Verify it
# actually RUNS (a Windows Store `python3` shim resolves via `command -v` but
# errors at runtime), so pick the first interpreter that executes.
if [ -z "${PY:-}" ]; then
  for c in python3 python; do
    if command -v "$c" >/dev/null 2>&1 && "$c" -c 'import sys' >/dev/null 2>&1; then PY="$c"; break; fi
  done
fi
[ -n "${PY:-}" ] || { echo "FATAL: need a working python3/python on PATH for metric parsing" >&2; exit 2; }

ORNITH_ENDPOINT="${ORNITH_ENDPOINT:-http://100.113.207.102:8080}"
ORNITH_MODEL="${ORNITH_MODEL:-ornith-1.0-35b-q8}"
TASKS_DIR="${TASKS_DIR:-$HERE/tasks}"
MAX_ROUNDS="${MAX_ROUNDS:-15}"
TASK_TIMEOUT="${TASK_TIMEOUT:-600}"
STAMP="$(date +%Y%m%d-%H%M%S)"
OUT="${OUT:-$HERE/runs/$STAMP}"
POSTURES="${POSTURES:-baseline tenacity crew obsessive}"
OCAP_MODES="${OCAP_MODES:-off on}"

mkdir -p "$OUT"
CSV="$OUT/results.csv"
MD="$OUT/matrix.md"
CFG="$OUT/ornith.toml"

# Render the backend config from the endpoint/model (portable — no ~/.newt needed).
cat > "$CFG" <<EOF
default_backend = "ornith"
[[backends]]
name = "ornith"
endpoint = "$ORNITH_ENDPOINT"
model = "$ORNITH_MODEL"
kind = "openai"
api = "chat_completions"
tiers = ["FAST", "STANDARD", "COMPLEX", "REVIEW"]
EOF

echo "posture,ocap,task,verify,status,tool_calls,write_calls,total_tokens,wall_secs,solve_rc" > "$CSV"

# Posture → global flags applied to `newt`.
posture_flags() {
  case "$1" in
    baseline)  echo "" ;;
    tenacity)  echo "--tenacity relentless" ;;
    crew)      echo "" ;;                 # crew is an env gate (below)
    obsessive) echo "--obsessive" ;;
    *) echo "" ;;
  esac
}
posture_env() {  # extra env exports for a posture (crew gate)
  case "$1" in
    crew)      echo "NEWT_TEAM=1" ;;
    *) echo "" ;;
  esac
}

# Reachability pre-check — fail fast rather than burn a whole matrix on a dead endpoint.
if ! curl -s --max-time 10 -o /dev/null -w '%{http_code}' "$ORNITH_ENDPOINT/v1/models" | grep -q 200; then
  echo "FATAL: $ORNITH_ENDPOINT/v1/models did not return 200 — is dgx1/ornith up + reachable from here?" >&2
  exit 3
fi

echo "== psyche A/B matrix =="
echo "newt:      $NEWT"
echo "endpoint:  $ORNITH_ENDPOINT   model: $ORNITH_MODEL"
echo "postures:  $POSTURES"
echo "ocap:      $OCAP_MODES"
echo "tasks:     $(ls "$TASKS_DIR" 2>/dev/null | tr '\n' ' ')"
echo "out:       $OUT"
echo

for posture in $POSTURES; do
  pflags="$(posture_flags "$posture")"
  penv="$(posture_env "$posture")"
  for ocap in $OCAP_MODES; do
    if [ "$ocap" = "on" ]; then ocap_env="NEWT_BENCH_OCAP=on"; ni="--non-interactive true"
    else ocap_env=""; ni="--non-interactive true"; fi
    for taskdir in "$TASKS_DIR"/*/; do
      task="$(basename "$taskdir")"
      ws="$OUT/ws/$posture-$ocap-$task"; rm -rf "$ws"; mkdir -p "$ws"
      [ -x "$taskdir/setup.sh" ] && ( cd "$ws" && "$taskdir/setup.sh" ) >/dev/null 2>&1
      events="$OUT/$posture-$ocap-$task.jsonl"

      # Run the cell. All psyche/OCAP knobs are flags + env; --config pins ornith.
      # shellcheck disable=SC2086
      env $ocap_env $penv NEWT_NO_MODEL_PULL=1 \
        timeout "$TASK_TIMEOUT" "$NEWT" $pflags --config "$CFG" solve \
          --instruction-file "$taskdir/instruction.txt" --cwd "$ws" \
          --events "$events" $ni --max-rounds "$MAX_ROUNDS" --plain \
          > "$OUT/$posture-$ocap-$task.trace" 2>&1
      rc=$?

      # Metrics from the solve_result JSON (last JSON object emitted).
      read -r status tcalls wcalls toks wall < <("$PY" - "$OUT/$posture-$ocap-$task.trace" <<'PY'
import sys, json, re
txt = open(sys.argv[1], encoding="utf-8", errors="replace").read()
obj = {}
for m in re.finditer(r'\{"[^\n]*"solve_result"[^\n]*\}', txt):
    try: obj = json.loads(m.group(0))
    except Exception: pass
print(obj.get("status","?"), obj.get("tool_calls",0), obj.get("write_calls",0),
      obj.get("usage_total_tokens",0), round(float(obj.get("wall_secs",0)),1))
PY
)
      # Task verification (harness-independent).
      verify="skip"
      if [ -x "$taskdir/verify.sh" ]; then
        if ( cd "$ws" && "$taskdir/verify.sh" ) >/dev/null 2>&1; then verify="pass"; else verify="fail"; fi
      fi
      echo "$posture,$ocap,$task,$verify,${status:-?},${tcalls:-0},${wcalls:-0},${toks:-0},${wall:-0},$rc" >> "$CSV"
      printf "  %-9s ocap=%-3s %-14s -> %-4s (%s, tools=%s writes=%s tok=%s %ss)\n" \
        "$posture" "$ocap" "$task" "$verify" "${status:-?}" "${tcalls:-0}" "${wcalls:-0}" "${toks:-0}" "${wall:-0}"
    done
  done
done

# Render matrix.md (pass-rate grid + per-cell rollup) from the CSV.
"$PY" - "$CSV" "$MD" "$ORNITH_MODEL" "$ORNITH_ENDPOINT" <<'PY'
import sys, csv, collections
csvp, mdp, model, endpoint = sys.argv[1:5]
rows = list(csv.DictReader(open(csvp, encoding="utf-8")))
postures = ["baseline","tenacity","crew","obsessive"]
ocaps = ["off","on"]
cell = collections.defaultdict(list)
for r in rows: cell[(r["posture"], r["ocap"])].append(r)
def summ(rs):
    if not rs: return "—"
    npass = sum(1 for r in rs if r["verify"]=="pass")
    tok = sum(int(r["total_tokens"] or 0) for r in rs)//max(len(rs),1)
    wall = sum(float(r["wall_secs"] or 0) for r in rs)/max(len(rs),1)
    return f"{npass}/{len(rs)} pass · {tok} tok · {wall:.0f}s"
with open(mdp,"w",encoding="utf-8") as f:
    f.write(f"# Psyche × OCAP A/B matrix — {model}\n\n")
    f.write(f"Endpoint `{endpoint}` · each cell = pass-rate over the task set + avg tokens/wall per task.\n\n")
    f.write("| posture \\ OCAP | " + " | ".join(ocaps) + " |\n")
    f.write("|---|" + "|".join(["---"]*len(ocaps)) + "|\n")
    for p in postures:
        f.write(f"| **{p}** | " + " | ".join(summ(cell[(p,o)]) for o in ocaps) + " |\n")
    f.write("\n## Per-cell task detail\n\n| posture | ocap | task | verify | status | tools | writes | tokens | wall |\n|---|---|---|---|---|---|---|---|---|\n")
    cols = ["posture","ocap","task","verify","status","tool_calls","write_calls","total_tokens","wall_secs"]
    for r in rows:
        if not r.get("task"): continue  # skip any blank/partial row
        f.write("| " + " | ".join(str(r.get(c) or "") for c in cols) + " |\n")
print(f"\nwrote {mdp}")
PY

echo
echo "matrix:  $MD"
echo "csv:     $CSV"
