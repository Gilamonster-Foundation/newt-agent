#!/usr/bin/env bash
# Two-arm smart-harness experiment on terminal-bench (#2260/#2263).
# Same binary, same tasks, same Harbor verifier; only the arm differs.
# Metric: FALSE COMPLETIONS (newt says completed, Harbor says unresolved),
# never pass rate alone — see board card 2026-09-11_terminal-bench-false-completion-rate.
# Usage: NEWT_BENCH_BIN=<branch release newt> smart-ab.sh <job.json> <run-label>
set -euo pipefail
JOB="${1:?job json}"; LABEL="${2:?run label}"
: "${NEWT_BENCH_BIN:?path to the branch-built newt binary}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
"$NEWT_BENCH_BIN" --version
for arm in off on; do
  if [ "$arm" = on ]; then profile=~/.newt/bench/qwen-smart.toml; smart=1; else profile=~/.newt/bench/qwen-plain.toml; smart=""; fi
  echo "=== arm: smart=$arm  profile=$(basename "$profile") ==="
  NEWT_BENCH_PROFILE="$profile" NEWT_BENCH_SMART="$smart" \
    harbor run --config "$JOB" --no-rebuild --no-cleanup --job-name "smart-${arm}-${LABEL}" 2>&1 | tail -3
done
echo "=== cross-tab ==="
python3 "$HERE/cross-tab.py" /var/tmp/tbench-harbor/smart-off-"$LABEL"* /var/tmp/tbench-harbor/smart-on-"$LABEL"*
