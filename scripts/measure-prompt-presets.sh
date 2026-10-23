#!/usr/bin/env bash
# Measure what newt sends, per prompt preset (#2933 / #2983).
#
# Usage: scripts/measure-prompt-presets.sh [path/to/newt]
#
# Each case starts the real TUI in a private tmux session (throwaway HOME, config and
# fixture workspace) against a RECORDING loopback provider, types one ordinary task,
# and reads the first Chat Completions request that carried it. The sizes are those of
# the request body newt actually sent:
#
#   system chars    every system message, summed (the frozen prompt, the per-turn
#                   system message, the active-prompt card, and the driver's own text
#                   such as observed facts and output planning)
#   tools           the number of entries in the `tools` array
#   tools chars     the `tools` array as compact JSON
#   body chars      the whole request body
#
# The last column is the `/context request` PREVIEW total, kept for comparison only:
# it is partial (it leaves out the driver's own text and per-turn head blocks) and is
# not the request. Needs tmux and python3; no real model is contacted.
set -uo pipefail

BIN=${1:-target/debug/newt}
BIN=$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(mktemp -d "${TMPDIR:-/tmp}/measure-presets.XXXXXX")
PROVIDER_PID=""
cleanup() {
  tmux kill-session -t measure-presets 2>/dev/null || true
  [ -n "$PROVIDER_PID" ] && kill "$PROVIDER_PID" 2>/dev/null || true
}
trap cleanup EXIT

# One fixture workspace for every case.
WS=$ROOT/ws
mkdir -p "$WS/src"
git -C "$WS" init -q .
printf '[package]\nname = "fixture"\n' >"$WS/Cargo.toml"
printf '# Fixture\n\nA tiny project used to measure prompt sizes.\n' >"$WS/README.md"
printf 'fn main() {}\n' >"$WS/src/main.rs"
printf '# Project instructions\n\nUse four spaces. Run `cargo test` before committing.\n' >"$WS/AGENTS.md"

PI_TOOLS='[tool_exposure]
allow_list = ["read_file", "write_file", "edit_file", "run_command"]
'
MARKER=WIRE_MEASURE_TURN

measure() { # name, extra-config
  local name=$1 extra=$2 slug
  slug=$(printf '%s' "$1" | tr -c 'A-Za-z0-9\n' '-')
  local home=$ROOT/home-$slug wire=$ROOT/wire-$slug
  mkdir -p "$home/.newt" "$wire"

  python3 -I "$HERE/measure_prompt_wire.py" serve "$wire" &
  PROVIDER_PID=$!
  local i
  for i in $(seq 1 50); do [ -s "$wire/port" ] && break; sleep 0.1; done
  local port
  port=$(cat "$wire/port")

  printf 'default_backend = "fixture"\n\n[[backends]]\nname = "fixture"\nendpoint = "http://127.0.0.1:%s"\nmodel = "measure-fixture"\nkind = "openai"\n\n%s\n' \
    "$port" "$extra" >"$home/.newt/config.toml"
  tmux kill-session -t measure-presets 2>/dev/null || true
  tmux new-session -d -s measure-presets -x 200 -y 50 \
    "cd $WS && HOME=$home NEWT_CONFIG_DIR=$home/.newt NEWT_CONFIG=$home/.newt/config.toml NEWT_NO_MODEL_PULL=1 $BIN; sleep 600"
  for i in $(seq 1 60); do
    sleep 1
    tmux capture-pane -t measure-presets -p 2>/dev/null | grep -q "ready — type a task" && break
  done

  # The wire: one ordinary task, then the first request that carried it.
  tmux send-keys -t measure-presets "$MARKER"
  sleep 1
  tmux send-keys -t measure-presets Enter
  local wired=""
  for i in $(seq 1 60); do
    sleep 1
    wired=$(python3 -I "$HERE/measure_prompt_wire.py" report "$wire" "$MARKER" 2>/dev/null) && break
    wired=""
  done
  sleep 3

  # The preview, for comparison only.
  tmux clear-history -t measure-presets
  tmux send-keys -t measure-presets "/context request"
  sleep 1
  tmux send-keys -t measure-presets Enter
  sleep 4
  tmux send-keys -t measure-presets Enter
  sleep 3
  local dump=$ROOT/$slug.txt preview
  tmux capture-pane -t measure-presets -p -S -20000 >"$dump" 2>/dev/null || true
  preview=$(grep -oE 'system [0-9]+ \+ tools [0-9]+ = [0-9]+' "$dump" | head -1 | grep -oE '[0-9]+$' || true)

  kill "$PROVIDER_PID" 2>/dev/null || true
  PROVIDER_PID=""
  tmux kill-session -t measure-presets 2>/dev/null || true

  local sys tools tchars body
  read -r sys tools tchars body <<<"${wired:-? ? ? ?}"
  printf '| %-26s | %12s | %5s | %11s | %10s | %22s |\n' \
    "$name" "$sys" "$tools" "$tchars" "$body" "${preview:-?}"
}

measure_headless() { # name, extra-config, extra headless args
  local name=$1 extra=$2 slug
  shift 2
  slug=$(printf 'headless %s' "$name" | tr -c 'A-Za-z0-9\n' '-')
  local home=$ROOT/home-$slug wire=$ROOT/wire-$slug
  mkdir -p "$home/.newt" "$wire"
  python3 -I "$HERE/measure_prompt_wire.py" serve "$wire" &
  PROVIDER_PID=$!
  local i
  for i in $(seq 1 50); do [ -s "$wire/port" ] && break; sleep 0.1; done
  local port
  port=$(cat "$wire/port")
  printf 'default_backend = "fixture"\n\n[[backends]]\nname = "fixture"\nendpoint = "http://127.0.0.1:%s"\nmodel = "measure-fixture"\nkind = "openai"\n\n%s\n' \
    "$port" "$extra" >"$home/.newt/config.toml"
  printf '%s\n' "$MARKER" >"$ROOT/instruction-$slug.md"
  (cd "$WS" && HOME=$home NEWT_CONFIG_DIR=$home/.newt NEWT_CONFIG=$home/.newt/config.toml NEWT_NO_MODEL_PULL=1 \
    "$BIN" headless --cwd "$WS" --instruction-file "$ROOT/instruction-$slug.md" --max-rounds 1 "$@" >/dev/null 2>&1)
  local wired sys tools tchars body
  wired=$(python3 -I "$HERE/measure_prompt_wire.py" report "$wire" "$MARKER" 2>/dev/null) || wired=""
  kill "$PROVIDER_PID" 2>/dev/null || true
  PROVIDER_PID=""
  read -r sys tools tchars body <<<"${wired:-? ? ? ?}"
  printf '| %-26s | %12s | %5s | %11s | %10s | %22s |\n' \
    "headless $name" "$sys" "$tools" "$tchars" "$body" "-"
}

echo "| case                       | system chars | tools | tools chars | body chars | preview total (partial) |"
echo "|----------------------------|-------------:|------:|------------:|-----------:|------------------------:|"
measure "default" ""
measure "pi (full tools, card on)" '[prompt]
preset = "pi"
components = { on = ["card"] }
'
measure "pi + /settings tools pi" "$PI_TOOLS
[prompt]
preset = \"pi\"
"
measure_headless "bare (--prompt-preset headless)" "" --prompt-preset headless
measure_headless "default (= the TUI's)" ""
measure_headless "pi + tools pi" "$PI_TOOLS
[prompt]
preset = \"pi\"
"
echo
echo "wire columns: the first Chat Completions request carrying the task, as recorded by a"
echo "loopback provider. fixture: $WS; request logs: $ROOT/wire-*/requests.jsonl"
