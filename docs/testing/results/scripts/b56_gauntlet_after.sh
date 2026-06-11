#!/usr/bin/env bash
# AFTER SCRATCH HARNESS — B5 (gauntlet probe) + B6 (overflow incidence)
# drivers re-run against the post-Phase-17/18/19 loop (issues #245/#247).
# NOT wired into CI or the workspace build.
#
# Same protocol as the baseline `b56_gauntlet.sh` — same model, same
# num_ctx=4096, same marker, same prompts, same per-run-fresh sandbox HOME
# for B6 — with two mechanical differences:
#   - outputs land in /tmp/newt-bench-after/ so the baseline's pinned
#     /tmp/newt-bench/ artifacts (the B3 replay corpus) are never clobbered;
#   - analysis runs through b56_analyze_after.py (the trim debug lines the
#     baseline analyzer grepped for were removed by 18.4; visibility is now
#     the compression/overflow/anti-thrash notices).
#
# Workspace fixtures: reuses the baseline's seeded /tmp/newt-bench/ws-b5
# (10 × ~10KB data_NN.md) and /tmp/newt-bench/ws-b6 (3 × ~50KB data_big_N.md)
# so the request shapes match the baseline runs byte-for-byte (minus the
# nondeterministic model behavior).
#
# Usage:
#   b56_gauntlet_after.sh b5
#   b56_gauntlet_after.sh b6 <runs>
set -euo pipefail

MODE=$1
RUNS=${2:-10}
SCRIPTS_DIR=$(cd "$(dirname "$0")" && pwd)
MODEL=llama3.1:8b
NUM_CTX=4096
MARKER=GAUNTLET-7f3d9c
OUT=/tmp/newt-bench-after
mkdir -p "$OUT"

start_proxy() { # $1 = log path
  python3 "$SCRIPTS_DIR/ollama_capture_proxy.py" --listen 18434 \
    --upstream https://REDACTED-HOST --log "$1" \
    > "$OUT/proxy.log" 2>&1 &
  PROXY_PID=$!
  sleep 1
}
stop_proxy() { kill "$PROXY_PID" 2>/dev/null || true; sleep 0.5; }

if [ "$MODE" = b5 ]; then
  cat > "$OUT/prompts-b5.txt" <<EOF
ACTIVE TASK $MARKER: read every data file in this directory one at a time (data_00.md, data_01.md, data_02.md, data_03.md, data_04.md, data_05.md, data_06.md, data_07.md, data_08.md, data_09.md — one read_file call each), and after reading ALL of them, write a file named result.txt whose only content is the exact marker string from this ACTIVE TASK followed by the word done.
without reading any files, restate the exact ACTIVE TASK marker string from my original task in this session
exit
EOF
  rm -f /tmp/newt-bench/ws-b5/result.txt
  start_proxy "$OUT/capture-b5.jsonl"
  bash "$SCRIPTS_DIR/run_newt_session.sh" "$MODEL" http://127.0.0.1:18434 \
    /tmp/newt-bench/ws-b5 "$OUT/prompts-b5.txt" \
    "$OUT/b5-session.log" "$NUM_CTX"
  stop_proxy
  python3 "$SCRIPTS_DIR/b56_analyze_after.py" --capture "$OUT/capture-b5.jsonl" \
    --session "$OUT/b5-session.log" --marker "$MARKER"
  if [ -f /tmp/newt-bench/ws-b5/result.txt ]; then
    echo "result.txt: $(cat /tmp/newt-bench/ws-b5/result.txt)"
  else
    echo "result.txt: NOT WRITTEN"
  fi
elif [ "$MODE" = b6 ]; then
  cat > "$OUT/prompts-b6.txt" <<EOF
ACTIVE TASK $MARKER: read data_big_1.md, then data_big_2.md, then data_big_3.md (one read_file call each), and then answer: what was the exact marker string in this ACTIVE TASK?
exit
EOF
  for i in $(seq 1 "$RUNS"); do
    start_proxy "$OUT/capture-b6-run$i.jsonl"
    bash "$SCRIPTS_DIR/run_newt_session.sh" "$MODEL" http://127.0.0.1:18434 \
      /tmp/newt-bench/ws-b6 "$OUT/prompts-b6.txt" \
      "$OUT/b6-run$i.log" "$NUM_CTX"
    stop_proxy
    python3 "$SCRIPTS_DIR/b56_analyze_after.py" --capture "$OUT/capture-b6-run$i.jsonl" \
      --session "$OUT/b6-run$i.log" --marker "$MARKER"
  done
else
  echo "usage: $0 b5 | b6 [runs]" >&2
  exit 1
fi
