#!/usr/bin/env bash
# BASELINE SCRATCH HARNESS (issue #245) — scripted `newt code` session in a
# sandbox HOME. NOT wired into CI or the workspace build.
#
# Usage:
#   run_newt_session.sh <model> <ollama_url> <workspace> <prompts_file> <out_log> [num_ctx] [sandbox_home]
#
# - Never touches the real ~/.newt: HOME is a throwaway sandbox dir.
# - Pipes <prompts_file> (one prompt per line, last line `exit`) into the TUI.
# - NEWT_DEBUG=1 so per-round diagnostics (token usage, trim events) land in
#   the captured output.
set -euo pipefail

MODEL=$1
URL=$2
WS=$3
PROMPTS=$4
OUT=$5
NUM_CTX=${6:-}
SANDBOX=${7:-$(mktemp -d /tmp/newt-bench/home.XXXXXX)}

NEWT_BIN=${NEWT_BIN:-$HOME/.cache/newt-bench-target/release/newt}

mkdir -p "$SANDBOX/.newt"
cat > "$SANDBOX/.newt/config.toml" <<EOF
# bench sandbox config (issue #245 baseline runs)
[tui]
debug = true
no_splash = true
inference_timeout_secs = 300

[tui.permissions]
preset = "workspace_dev"
EOF

env -i \
  HOME="$SANDBOX" \
  PATH=/usr/bin:/bin \
  TERM=dumb \
  NEWT_DGX_OLLAMA_URL="$URL" \
  NEWT_DGX_MODEL="$MODEL" \
  NEWT_DEBUG=1 \
  ${NUM_CTX:+NEWT_NUM_CTX="$NUM_CTX"} \
  "$NEWT_BIN" --no-splash code "$WS" < "$PROMPTS" > "$OUT" 2>&1 || {
    echo "[run_newt_session] newt exited non-zero: $?" >> "$OUT"
  }

echo "$SANDBOX"
