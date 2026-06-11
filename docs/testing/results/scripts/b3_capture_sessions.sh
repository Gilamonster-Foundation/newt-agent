#!/usr/bin/env bash
# BASELINE SCRATCH HARNESS — B3 capture driver (issue #245).
# NOT wired into CI or the workspace build.
#
# Runs one scripted `newt code` session per pinned model through the capture
# proxy, producing capture-b3-<model>.jsonl logs for b3_replay_estimate.py,
# plus a 20-turn session on the weak model for the provider double-count
# drift check.
#
# Usage: b3_capture_sessions.sh
set -euo pipefail

SCRIPTS_DIR=$(cd "$(dirname "$0")" && pwd)
UPSTREAM=https://REDACTED-HOST
NUM_CTX=8192

mkdir -p /tmp/newt-bench/ws-b3
# Seed the workspace (idempotent): a real source file, the README, notes.
REPO_ROOT=$(cd "$SCRIPTS_DIR/../../../.." && pwd)
cp -f "$REPO_ROOT/newt-core/src/conversation.rs" /tmp/newt-bench/ws-b3/
cp -f "$REPO_ROOT/README.md" /tmp/newt-bench/ws-b3/
cat > /tmp/newt-bench/ws-b3/notes.md <<'EOF'
# Bench notes

This workspace exists for the newt context/memory baseline (issue #245).

## Things the agent might be asked about
- The conversation store rewrites the whole JSON record per turn.
- The token estimator is chars/4 over serialized messages, no tool schemas.
- gnuc runs an RTX 4060 Ti with 16GB of VRAM.

## Filler
The measurements compare estimated tokens against the backend's reported
prompt_eval_count across three pinned models. Numbers must be honest; a
partial baseline beats invented numbers. The store benchmark uses a tempdir
on local disk because the NFS workspace would dominate the timings.
EOF

cat > /tmp/newt-bench/prompts-b3.txt <<'EOF'
read the file conversation.rs and summarize what ConversationStore does in two sentences
list the files in this directory, then read notes.md and report its first heading
run the command `wc -l conversation.rs` and report the line count
read README.md and then conversation.rs, and tell me which file is longer
exit
EOF

# Second prompt set per model — pushes the unique-request count past the
# >=30-across-3-models bar (set 1 alone captured 8+4+12=24).
cat > /tmp/newt-bench/prompts-b3b.txt <<'EOF'
search this directory for the string "workspace_id" and report which files contain it
read notes.md and summarize its Filler section in one sentence
run the command `head -5 README.md` and quote the output exactly
create a file named bench-summary.txt containing exactly one line: the first heading of README.md
which files have you read so far in this session? answer from memory without using tools
exit
EOF

# 20 tiny turns for the double-count drift check (B3, second half).
{ for i in $(seq 1 20); do echo "reply with exactly: ok $i"; done; echo exit; } \
  > /tmp/newt-bench/prompts-drift.txt

run_one() { # $1=model  $2=slug  $3=prompts
  local log="/tmp/newt-bench/capture-$2.jsonl"
  python3 "$SCRIPTS_DIR/ollama_capture_proxy.py" --listen 18434 \
    --upstream "$UPSTREAM" --log "$log" > /tmp/newt-bench/proxy.log 2>&1 &
  local pid=$!
  sleep 1
  bash "$SCRIPTS_DIR/run_newt_session.sh" "$1" http://127.0.0.1:18434 \
    /tmp/newt-bench/ws-b3 "$3" "/tmp/newt-bench/$2.log" "$NUM_CTX"
  kill "$pid" 2>/dev/null || true
  sleep 0.5
}

run_one llama3.1:8b        b3-llama31 /tmp/newt-bench/prompts-b3.txt
run_one qwen2.5-coder:14b  b3-qwen25  /tmp/newt-bench/prompts-b3.txt
run_one qwen3-coder:30b    b3-qwen3   /tmp/newt-bench/prompts-b3.txt
run_one llama3.1:8b        b3b-llama31 /tmp/newt-bench/prompts-b3b.txt
run_one qwen2.5-coder:14b  b3b-qwen25  /tmp/newt-bench/prompts-b3b.txt
run_one qwen3-coder:30b    b3b-qwen3   /tmp/newt-bench/prompts-b3b.txt
run_one llama3.1:8b        b3-drift   /tmp/newt-bench/prompts-drift.txt

echo "now: python3 $SCRIPTS_DIR/b3_replay_estimate.py --schema-cost /tmp/newt-bench/capture-b3-{llama31,qwen25,qwen3}.jsonl"
