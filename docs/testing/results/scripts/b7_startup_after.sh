#!/usr/bin/env bash
# AFTER SCRATCH HARNESS — B7 cold-start timing on the SQLite store + 17.7
# auto-resume (issues #245/#246). NOT wired into CI or the workspace build.
#
# Same methodology as the baseline `b7_startup.sh` (hyperfine, 2 warmups,
# 10 runs, `exit` piped, sandbox HOME, env -i): times `newt --no-splash code`
# from exec to exit at 0 / 100 / 1000 stored conversations. Differences from
# the baseline script, both forced by what landed since:
#   - the store is SQLite now, so seeding goes through the REAL
#     `ConversationStore` API via the `b7_seed` bench bin (the baseline's
#     Python seeder wrote the retired JSON schema, which would now measure
#     the one-time legacy import instead of steady-state startup);
#   - auto-resume (17.7) is ON by default — startup now actually reads the
#     store, which is exactly what B7 exists to measure.
#
# Usage: b7_startup_after.sh <ollama_url> [hyperfine_runs]
#   b7_startup_after.sh https://REDACTED-HOST 10
#   b7_startup_after.sh http://127.0.0.1:9 10   # refused-port control (no probe tax)
set -euo pipefail

URL=${1:-https://REDACTED-HOST}
RUNS=${2:-10}
SCRIPTS_DIR=$(cd "$(dirname "$0")" && pwd)
NEWT_BIN=${NEWT_BIN:-$HOME/.cache/newt-bench-target/release/newt}
SEED_BIN=${SEED_BIN:-$HOME/.cache/newt-bench-target/release/b7_seed}
BASE=/tmp/newt-bench-after/b7
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
    HOME="$SANDBOX" "$SEED_BIN" --home "$SANDBOX" --workspace "$BASE/ws" --count "$n"
  fi
  # Warmup runs generate ~/.newt/identity.pem etc. so the measured runs are
  # steady-state cold starts (process start, not first-ever-run setup).
  CMD="env -i HOME=$SANDBOX PATH=/usr/bin:/bin TERM=dumb \
       NEWT_DGX_OLLAMA_URL=$URL NEWT_DGX_MODEL=llama3.1:8b \
       $NEWT_BIN --no-splash code $BASE/ws < $BASE/exit.txt > /dev/null 2>&1"
  echo "== B7: $n stored conversations =="
  hyperfine --shell bash --warmup 2 --runs "$RUNS" "$CMD"
done
