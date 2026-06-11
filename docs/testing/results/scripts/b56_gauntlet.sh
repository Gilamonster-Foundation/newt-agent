#!/usr/bin/env bash
# BASELINE SCRATCH HARNESS — B5 (gauntlet probe) + B6 (overflow incidence)
# drivers (issue #245). NOT wired into CI or the workspace build.
#
# B5: one scripted long-horizon session on a small num_ctx that forces the
#     trim/truncation path repeatedly (10 file reads, ~10KB each, num_ctx
#     4096), with an ACTIVE TASK marker we can grep for in the final request.
#     This is a scripted probe, not the future 017/018 eval cases.
#
# B6: N scripted sessions, each a single turn whose tool reads (~50KB × 3)
#     blow past num_ctx 4096; fresh sandbox HOME per run so the tuning-cache
#     ratchet from one run can't change the next.
#
# Usage:
#   b56_gauntlet.sh b5
#   b56_gauntlet.sh b6 <runs>
set -euo pipefail

MODE=$1
RUNS=${2:-10}
SCRIPTS_DIR=$(cd "$(dirname "$0")" && pwd)
MODEL=llama3.1:8b
NUM_CTX=4096
MARKER=GAUNTLET-7f3d9c

start_proxy() { # $1 = log path
  python3 "$SCRIPTS_DIR/ollama_capture_proxy.py" --listen 18434 \
    --upstream https://REDACTED-HOST --log "$1" \
    > /tmp/newt-bench/proxy.log 2>&1 &
  PROXY_PID=$!
  sleep 1
}
stop_proxy() { kill "$PROXY_PID" 2>/dev/null || true; sleep 0.5; }

if [ "$MODE" = b5 ]; then
  cat > /tmp/newt-bench/prompts-b5.txt <<EOF
ACTIVE TASK $MARKER: read every data file in this directory one at a time (data_00.md, data_01.md, data_02.md, data_03.md, data_04.md, data_05.md, data_06.md, data_07.md, data_08.md, data_09.md — one read_file call each), and after reading ALL of them, write a file named result.txt whose only content is the exact marker string from this ACTIVE TASK followed by the word done.
without reading any files, restate the exact ACTIVE TASK marker string from my original task in this session
exit
EOF
  rm -f /tmp/newt-bench/ws-b5/result.txt
  start_proxy /tmp/newt-bench/capture-b5.jsonl
  bash "$SCRIPTS_DIR/run_newt_session.sh" "$MODEL" http://127.0.0.1:18434 \
    /tmp/newt-bench/ws-b5 /tmp/newt-bench/prompts-b5.txt \
    /tmp/newt-bench/b5-session.log "$NUM_CTX"
  stop_proxy
  python3 "$SCRIPTS_DIR/b56_analyze.py" --capture /tmp/newt-bench/capture-b5.jsonl \
    --session /tmp/newt-bench/b5-session.log --marker "$MARKER"
  if [ -f /tmp/newt-bench/ws-b5/result.txt ]; then
    echo "result.txt: $(cat /tmp/newt-bench/ws-b5/result.txt)"
  else
    echo "result.txt: NOT WRITTEN"
  fi
elif [ "$MODE" = b6 ]; then
  cat > /tmp/newt-bench/prompts-b6.txt <<EOF
ACTIVE TASK $MARKER: read data_big_1.md, then data_big_2.md, then data_big_3.md (one read_file call each), and then answer: what was the exact marker string in this ACTIVE TASK?
exit
EOF
  for i in $(seq 1 "$RUNS"); do
    start_proxy "/tmp/newt-bench/capture-b6-run$i.jsonl"
    bash "$SCRIPTS_DIR/run_newt_session.sh" "$MODEL" http://127.0.0.1:18434 \
      /tmp/newt-bench/ws-b6 /tmp/newt-bench/prompts-b6.txt \
      "/tmp/newt-bench/b6-run$i.log" "$NUM_CTX"
    stop_proxy
    python3 "$SCRIPTS_DIR/b56_analyze.py" --capture "/tmp/newt-bench/capture-b6-run$i.jsonl" \
      --session "/tmp/newt-bench/b6-run$i.log" --marker "$MARKER"
  done
else
  echo "usage: $0 b5 | b6 [runs]" >&2
  exit 1
fi
