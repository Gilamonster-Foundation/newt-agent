#!/usr/bin/env bash
# tbench-bootstrap.sh — a fire-manually Terminal-Bench bootstrap runner.
#
# The local stand-in for the Harbor installed-agent adapter (epic #1419, WS3):
# it drives `newt solve` over a directory of tasks, runs each task's own
# verification, and tallies a pass rate + per-task result JSONL. This lets us
# start building the failure taxonomy TODAY, before full Harbor wiring, and it
# is the shape WS3 formalizes.
#
# A task dir contains:
#   instruction.md   the task prompt (fed to `newt solve --instruction-file`)
#   verify.sh        run in the solved workspace; exit 0 = PASS
#   files/           (optional) initial workspace files, copied in before solving
#
# SECURITY (RATCHET.md invariant): this script names NO host. The backend
# endpoint lives only in the `--profile` toml (local, uncommitted). See
# tbench-profile.example.toml for the shape. The workspace + harness run
# locally; only inference is remote.
#
# Usage:
#   tbench-bootstrap.sh --profile <backend.toml> [--tasks <dir>] [--out <dir>]
#                       [--max-rounds N] [--timeout SECS] [--tenacity LEVEL]
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROFILE="" ; TASKS="$HERE/tbench-tasks" ; OUT="/var/tmp/tbench"
MAX_ROUNDS=40 ; TIMEOUT=600 ; TENACITY=""

while [ $# -gt 0 ]; do
  case "$1" in
    --profile)    PROFILE="$2"; shift 2;;
    --tasks)      TASKS="$2"; shift 2;;
    --out)        OUT="$2"; shift 2;;
    --max-rounds) MAX_ROUNDS="$2"; shift 2;;
    --timeout)    TIMEOUT="$2"; shift 2;;
    --tenacity)   TENACITY="$2"; shift 2;;
    *) echo "unknown arg: $1" >&2; exit 2;;
  esac
done
[ -n "$PROFILE" ] || { echo "need --profile <backend.toml> (see tbench-profile.example.toml)" >&2; exit 2; }
[ -f "$PROFILE" ] || { echo "no profile at $PROFILE" >&2; exit 2; }
[ -d "$TASKS" ]   || { echo "no tasks dir at $TASKS" >&2; exit 2; }
command -v newt >/dev/null || { echo "newt not on PATH" >&2; exit 2; }

STAMP="$(date +%Y%m%d-%H%M%S 2>/dev/null || echo run)"
RUN="$OUT/$STAMP" ; mkdir -p "$RUN"
RESULTS="$RUN/results.jsonl"
: > "$RESULTS"

pass=0 ; total=0
for task_dir in "$TASKS"/*/; do
  [ -f "$task_dir/instruction.md" ] || continue
  name="$(basename "$task_dir")"
  total=$((total + 1))
  ws="$RUN/$name" ; mkdir -p "$ws"
  # Seed initial files, if any.
  [ -d "$task_dir/files" ] && cp -a "$task_dir/files/." "$ws/" 2>/dev/null

  # Drive newt solve headless (--non-interactive default: OCAP off + full access).
  ten_arg=(); [ -n "$TENACITY" ] && ten_arg=(--tenacity "$TENACITY")
  timeout "$TIMEOUT" newt solve \
    --cwd "$ws" \
    --instruction-file "$task_dir/instruction.md" \
    --config "$PROFILE" \
    --events "$RUN/solve-events.jsonl" \
    --max-rounds "$MAX_ROUNDS" \
    "${ten_arg[@]}" >"$ws/.solve.out" 2>&1
  solve_rc=$?

  # Verify: the task's own check, run inside the solved workspace.
  verdict="fail" ; vrc=1
  if [ -f "$task_dir/verify.sh" ]; then
    ( cd "$ws" && bash "$task_dir/verify.sh" ) >"$ws/.verify.out" 2>&1
    vrc=$?
    [ "$vrc" -eq 0 ] && { verdict="pass"; pass=$((pass + 1)); }
  else
    verdict="no-verify"
  fi

  printf '{"task":"%s","verdict":"%s","solve_rc":%d,"verify_rc":%d}\n' \
    "$name" "$verdict" "$solve_rc" "$vrc" | tee -a "$RESULTS"
done

rate=0
[ "$total" -gt 0 ] && rate=$(( pass * 100 / total ))
echo "=================================================="
echo "TBENCH: $pass/$total passed (${rate}%)   results: $RESULTS"
echo "=================================================="
# The 20% floor is the 0.8.0 gate (#1419). Exit reflects it for CI later.
[ "$rate" -ge 20 ] && exit 0 || exit 1
