#!/usr/bin/env bash
# Measure what newt sends: the final system message(s) and the tools array, for the
# default prompt, preset `pi` over the full tool list, and preset `pi` over
# `/settings tools pi` (#2933 / #2983).
#
# Usage: scripts/measure-prompt-presets.sh [path/to/newt]
#
# Each case starts the real TUI in a private tmux session with a throwaway HOME,
# config and fixture workspace, runs `/context request` (which prepares the first
# request and sends nothing), and reads the sizes off its report. Needs tmux. No
# model is contacted. Sizes are characters.
set -uo pipefail

BIN=${1:-target/debug/newt}
BIN=$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")
ROOT=$(mktemp -d "${TMPDIR:-/tmp}/measure-presets.XXXXXX")
trap 'tmux kill-session -t measure-presets 2>/dev/null || true' EXIT

# One fixture workspace for every case.
WS=$ROOT/ws
mkdir -p "$WS/src"
git -C "$WS" init -q .
printf '[package]\nname = "fixture"\n' >"$WS/Cargo.toml"
printf '# Fixture\n\nA tiny project used to measure prompt sizes.\n' >"$WS/README.md"
printf 'fn main() {}\n' >"$WS/src/main.rs"
printf '# Project instructions\n\nUse four spaces. Run `cargo test` before committing.\n' >"$WS/AGENTS.md"

BACKEND='default_backend = "fixture"

[[backends]]
name = "fixture"
endpoint = "http://127.0.0.1:9"
model = "measure-fixture"
kind = "openai"
'
PI_TOOLS='[tool_exposure]
allow_list = ["read_file", "write_file", "edit_file", "run_command"]
'

measure() { # name, extra-config
  local name=$1 extra=$2 slug
  slug=$(printf '%s' "$1" | tr -c 'A-Za-z0-9\n' '-')
  local home=$ROOT/home-$slug
  mkdir -p "$home/.newt"
  printf '%s\n%s\n' "$BACKEND" "$extra" >"$home/.newt/config.toml"
  tmux kill-session -t measure-presets 2>/dev/null || true
  tmux new-session -d -s measure-presets -x 200 -y 50 \
    "cd $WS && HOME=$home NEWT_CONFIG_DIR=$home/.newt NEWT_CONFIG=$home/.newt/config.toml NEWT_NO_MODEL_PULL=1 $BIN; sleep 600"
  local i
  for i in $(seq 1 60); do
    sleep 1
    tmux capture-pane -t measure-presets -p 2>/dev/null | grep -q "ready — type a task" && break
  done
  tmux clear-history -t measure-presets
  tmux send-keys -t measure-presets "/context request"
  sleep 1
  tmux send-keys -t measure-presets Enter
  sleep 4
  tmux send-keys -t measure-presets Enter
  sleep 3
  local dump=$ROOT/$slug.txt
  tmux capture-pane -t measure-presets -p -S -20000 >"$dump" 2>/dev/null || true
  local sys1 sys2 tools ttl
  sys1=$(sed -nE 's/.*System message 1 \(([0-9]+) chars\).*/\1/p' "$dump" | head -1)
  sys2=$(sed -nE 's/.*System message 2 \(([0-9]+) chars\).*/\1/p' "$dump" | head -1)
  tools=$(grep -oE 'Tools \([0-9]+ tools, [0-9]+ chars' "$dump" | head -1 || true)
  local n c
  n=$(echo "$tools" | grep -oE '[0-9]+ tools' | grep -oE '[0-9]+' || true)
  c=$(echo "$tools" | grep -oE ', [0-9]+ chars' | grep -oE '[0-9]+' || true)
  ttl=$(grep -oE 'system [0-9]+ \+ tools [0-9]+ = [0-9]+' "$dump" | head -1 | grep -oE '[0-9]+$' || true)
  printf '| %-30s | %6s | %6s | %5s | %7s | %7s |\n' \
    "$name" "${sys1:-?}" "${sys2:--}" "${n:-?}" "${c:-?}" "${ttl:-?}"
}

echo "| case                           | system | card   | tools | tools chars | total   |"
echo "|--------------------------------|-------:|-------:|------:|------------:|--------:|"
measure "default" ""
measure "pi (full tools, card on)" '[prompt]
preset = "pi"
components = { on = ["card"] }
'
measure "pi + /settings tools pi" "$PI_TOOLS
[prompt]
preset = \"pi\"
"
echo
echo "fixture: $WS (README, Cargo.toml, src/, AGENTS.md); dumps: $ROOT/*.txt"
