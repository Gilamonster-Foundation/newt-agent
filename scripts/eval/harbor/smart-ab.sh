#!/usr/bin/env bash
# Two-arm smart-harness experiment on terminal-bench (#2260/#2263).
# Same binary, same tasks, same Harbor verifier; only the arm differs.
# Metric: FALSE COMPLETIONS (newt says completed, Harbor says unresolved),
# never pass rate alone — see board card 2026-09-11_terminal-bench-false-completion-rate.
# Usage: NEWT_BENCH_BIN=<branch release newt> smart-ab.sh <job.json> <run-label>
set -euo pipefail
JOB="${1:?job json}"; LABEL="${2:?run label}"
# P3 (review 5175154929): report from the SAME jobs_dir the job was executed with.
JOBS_DIR="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["jobs_dir"])' "$JOB")"
: "${NEWT_BENCH_BIN:?path to the branch-built newt binary}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# Harbor imports the adapter by bare module name (newt_agent:NewtAgent); the
# README's own invocation sets this. Without it the run dies in five seconds
# with "Failed to import module 'newt_agent'" before any inference.
export PYTHONPATH="$HERE${PYTHONPATH:+:$PYTHONPATH}"
"$NEWT_BENCH_BIN" --version
# A binary that cannot exec inside a task image turns every trial into a silent
# reward-0 with no events -- a run that "completes" having measured nothing.
# Observed 2026-09-11: a host-built newt needed GLIBC_2.39; three of eight
# task images are Debian bookworm at 2.36 and refused it at exec. Refuse here
# instead, before any inference. Override the floor only if the task set's
# oldest base image really is newer (NEWT_BENCH_GLIBC_FLOOR).
FLOOR="${NEWT_BENCH_GLIBC_FLOOR:-2.36}"
NEED="$(objdump -T "$NEWT_BENCH_BIN" 2>/dev/null | grep -oE 'GLIBC_[0-9.]+' | sed 's/GLIBC_//' | sort -V | tail -1)"
if [ -n "$NEED" ] && [ "$(printf '%s\n%s\n' "$FLOOR" "$NEED" | sort -V | tail -1)" != "$FLOOR" ]; then
  echo "refusing: $NEWT_BENCH_BIN requires GLIBC_$NEED but the task-image floor is $FLOOR;" >&2
  echo "          build it against the oldest base image (e.g. rust:<msrv>-bookworm) or raise NEWT_BENCH_GLIBC_FLOOR." >&2
  exit 2
fi
for arm in off on; do
  if [ "$arm" = on ]; then profile=~/.newt/bench/qwen-smart.toml; smart=1; else profile=~/.newt/bench/qwen-plain.toml; smart=""; fi
  echo "=== arm: smart=$arm  profile=$(basename "$profile") ==="
  # harbor 0.20: --no-delete keeps each task environment after completion (the
  # cross-tab reads newt-events.jsonl + the frame from it); not forcing a build is
  # already the default, so warm image layers are reused across arms.
  # P1 (review 5175154929): the env prefix MUST stay on the same logical line as
  # `harbor run`. A comment between a trailing backslash and the command turns the
  # assignments into a standalone statement and Harbor sees neither variable.
  NEWT_BENCH_PROFILE="$profile" NEWT_BENCH_SMART="$smart" \
    harbor run --config "$JOB" --no-delete --job-name "smart-${arm}-${LABEL}" 2>&1 | tail -3
done
echo "=== cross-tab ==="
python3 "$HERE/cross-tab.py" "$JOBS_DIR"/smart-off-"$LABEL"* "$JOBS_DIR"/smart-on-"$LABEL"*
