#!/usr/bin/env bash
# tui-uat.sh — User Acceptance Test run for the newt TUI, driven over tmux.
#
# Drives the REAL binary through operator scenarios, the way a person types
# them, and asserts on the rendered pane (capture-pane), not on emitted bytes.
# This is the expensive tier (real binary, real pty, real fs in an isolated
# NEWT_CONFIG_DIR): run it on demand, on the weekly schedule, and on release
# gates — NOT per-PR (per docs: testing strategy tiers). The per-PR TUI
# acceptance lives in the cargo test PTY tier (settings_form_pty_test etc.);
# this run grounds those against the actual shipped binary and full dispatch.
#
#   scripts/uat/tui-uat.sh [path-to-newt-binary]     (default target/debug/newt)
#
# Driving notes, learned the hard way (2026-08-31):
#   - The slash palette completes on Enter, never submits (#1674). A scripted
#     drive therefore types the command, sends Escape (closes the palette
#     without touching the buffer), THEN Enter. Deterministic whether or not
#     the palette knows the command.
#   - tmux send-keys sometimes needs Enter as a separate send.
set -u

BIN="${1:-target/debug/newt}"
SOCK="newt-uat-$$"
SES="uat"
HOME_DIR="$(mktemp -d "${TMPDIR:-/tmp}/newt-uat-XXXXXX")"
PASS=0; FAIL=0; FAILED_NAMES=()

tmx() { tmux -L "$SOCK" "$@"; }
pane() { tmx capture-pane -t "$SES" -p; }
type_cmd() { # type a slash command past the palette: text, Esc, Enter
  tmx send-keys -t "$SES" "$1"; sleep 1
  tmx send-keys -t "$SES" Escape; sleep 1
  tmx send-keys -t "$SES" Enter
}

# expect <scenario> <deadline-s> <grep-ERE>... — every pattern must appear on
# the rendered pane before the deadline. On failure, dump the pane.
expect() {
  local name="$1" deadline="$2"; shift 2
  local t=0 ok
  while :; do
    ok=1
    for pat in "$@"; do pane | grep -qE "$pat" || { ok=0; break; }; done
    if [ "$ok" = 1 ]; then echo "  ok: $name"; PASS=$((PASS+1)); return 0; fi
    t=$((t+1))
    if [ "$t" -ge "$deadline" ]; then
      echo "  FAIL: $name — missing \`$pat\`; pane:"
      pane | sed 's/^/    | /' | tail -25
      FAIL=$((FAIL+1)); FAILED_NAMES+=("$name"); return 1
    fi
    sleep 1
  done
}

cleanup() { tmx kill-server 2>/dev/null; rm -rf "$HOME_DIR"; }
trap cleanup EXIT

[ -x "$BIN" ] || { echo "no binary at $BIN — build first (cargo build --bin newt)"; exit 2; }

# Seed an operator-shaped config (one localhost Ollama drop-in), so the run
# exercises the configured-box path rather than the first-run wizard — and so
# nothing depends on the network beyond the local Ollama the box already runs.
mkdir -p "$HOME_DIR/backends"
cat > "$HOME_DIR/backends/default.toml" <<'TOML'
name = "default"
endpoint = "http://127.0.0.1:11434"
model = "llama3.1:8b"
tiers = ["FAST", "STANDARD", "COMPLEX", "REVIEW"]
kind = "ollama"
TOML
echo "UAT against: $($BIN --version 2>/dev/null | head -1)  (config: $HOME_DIR)"

tmx kill-server 2>/dev/null || true
tmx new-session -d -s "$SES" -x 120 -y 32 \
  "NEWT_CONFIG_DIR='$HOME_DIR' '$BIN'"

# ── S1: the TUI comes up on the seeded config and offers a prompt ──────────
expect "S1 ready line + prompt" 45 "ready — " "INSERT"

# ── S2: bare /settings renders the six-field form ──────────────────────────
type_cmd "/settings"
expect "S2 /settings form: all six fields render" 15 \
  "line-editor key bindings" "tenacity" "cognition" \
  "thinking spinner" "action-pressure nudges" "tool-call round limit"

# ── S3: apply a value through the form; the receipt lands on disk ──────────
tmx send-keys -t "$SES" "1" Enter; sleep 2
expect "S3a edit-mode value menu renders" 15 "nano"
NANO_N="$(pane | grep nano | grep -oE '[0-9]+' | head -1)"
if [ -n "$NANO_N" ]; then
  tmx send-keys -t "$SES" "$NANO_N" Enter; sleep 2
  if grep -q "nano" "$HOME_DIR/receipts.jsonl" 2>/dev/null \
     && grep -q "/settings" "$HOME_DIR/receipts.jsonl"; then
    echo "  ok: S3b receipt on disk (value + verb bound)"; PASS=$((PASS+1))
  else
    echo "  FAIL: S3b no receipt with value+verb in $HOME_DIR/receipts.jsonl"
    FAIL=$((FAIL+1)); FAILED_NAMES+=("S3b receipt")
  fi
else
  echo "  FAIL: S3b could not read nano's option number off the pane"
  FAIL=$((FAIL+1)); FAILED_NAMES+=("S3b receipt")
fi

# ── S4: the deep link applies without the form, and still receipts ─────────
type_cmd "/settings edit-mode vi"
sleep 2
if grep -q '"vi"' "$HOME_DIR/receipts.jsonl" 2>/dev/null \
   || grep -qE 'vi' "$HOME_DIR/receipts.jsonl" 2>/dev/null; then
  echo "  ok: S4 deep link receipts"; PASS=$((PASS+1))
else
  echo "  FAIL: S4 deep-link receipt missing"; FAIL=$((FAIL+1)); FAILED_NAMES+=("S4")
fi

# ── S5: the palette itself offers /settings (needs #2003's help row) ───────
tmx send-keys -t "$SES" "/settings"; sleep 2
if pane | grep -qE "/settings \[field" ; then
  echo "  ok: S5 palette advertises /settings"; PASS=$((PASS+1))
else
  echo "  FAIL: S5 palette does not list /settings (pre-#2003 build?)"
  FAIL=$((FAIL+1)); FAILED_NAMES+=("S5 palette")
fi
tmx send-keys -t "$SES" Escape; sleep 1; tmx send-keys -t "$SES" C-u

# ── S6: /backends opens its panel (the #1977/#1952 regression surface) ─────
type_cmd "/backends"
expect "S6 /backends renders" 15 "backend|Backends|configured"
tmx send-keys -t "$SES" Escape; sleep 1

# ── S7: clean exit ─────────────────────────────────────────────────────────
type_cmd "/exit"
sleep 3
if pane | grep -qE "\\$|exited|bye" || ! tmx list-sessions 2>/dev/null | grep -q "$SES"; then
  echo "  ok: S7 clean exit"; PASS=$((PASS+1))
else
  echo "  FAIL: S7 still running after /exit"; FAIL=$((FAIL+1)); FAILED_NAMES+=("S7")
fi

echo
echo "UAT: $PASS passed, $FAIL failed${FAILED_NAMES:+ — ${FAILED_NAMES[*]}}"
[ "$FAIL" = 0 ]
