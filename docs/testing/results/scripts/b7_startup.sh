#!/usr/bin/env bash
# BASELINE SCRATCH HARNESS — B7 cold-start timing (issue #245).
# NOT wired into CI or the workspace build.
#
# Times `newt code` exec -> exit with a piped `exit` (no inference turn) in a
# sandbox HOME seeded with 0 / 100 / 1000 synthetic stored conversations.
# There is no resume feature on this baseline, so nothing reads the store at
# startup — this is the cold-start floor the future resume feature (17.7)
# must not regress.
#
# Usage: b7_startup.sh <ollama_url> [hyperfine_runs]
set -euo pipefail

URL=${1:-https://REDACTED-HOST}
RUNS=${2:-10}
SCRIPTS_DIR=$(cd "$(dirname "$0")" && pwd)
NEWT_BIN=${NEWT_BIN:-$HOME/.cache/newt-bench-target/release/newt}
BASE=/tmp/newt-bench/b7
rm -rf "$BASE"
mkdir -p "$BASE/ws"
printf 'exit\n' > "$BASE/exit.txt"

for n in 0 100 1000; do
  SANDBOX="$BASE/home-$n"
  mkdir -p "$SANDBOX/.newt"
  cat > "$SANDBOX/.newt/config.toml" <<EOF
[tui]
no_splash = true
[tui.permissions]
preset = "workspace_dev"
EOF
  if [ "$n" -gt 0 ]; then
    python3 "$SCRIPTS_DIR/b7_seed_store.py" --home "$SANDBOX" --workspace "$BASE/ws" --count "$n"
  fi
  # Warmup runs generate ~/.newt/identity.pem etc. so the measured runs are
  # steady-state cold starts (process start, not first-ever-run setup).
  CMD="env -i HOME=$SANDBOX PATH=/usr/bin:/bin TERM=dumb \
       NEWT_DGX_OLLAMA_URL=$URL NEWT_DGX_MODEL=llama3.1:8b \
       $NEWT_BIN --no-splash code $BASE/ws < $BASE/exit.txt > /dev/null 2>&1"
  echo "== B7: $n stored conversations =="
  hyperfine --shell bash --warmup 2 --runs "$RUNS" "$CMD"
done
