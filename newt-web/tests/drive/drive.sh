#!/usr/bin/env bash
# drive.sh — headless local end-to-end driver for the newt-web docking work.
#
# Drives the REAL newt TUI (in a detached tmux session) and the REAL newt-web
# cockpit against ONE shared ConversationStore, backed by a stub Ollama — no
# human, no real model, no network. It proves the coequal mirror+inject loop and
# the multi-session overview headlessly ("drive dock/undock/multi-dock/select
# without me"). The dock/undock/multi-dock legs are wired as explicit PENDING
# slots that light up as Phases 2–5 land.
#
# Usage:
#   newt-web/tests/drive/drive.sh                 # run the whole scenario
#   NEWT_BIN=/path/to/newt WEB_BIN=/path/to/newt-web  drive.sh   # pin binaries
#
# Requires: tmux, node, and a built `newt` + `newt-web`. Exits non-zero on any
# failed assertion so it can gate CI or a pre-merge check.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NEWT_BIN="${NEWT_BIN:-$HOME/bin/newt}"
WEB_BIN="${WEB_BIN:-$HOME/.cargo-target/newtweb/debug/newt-web}"
SESS="drive-$$"
PASS=0
FAIL=0

say()  { printf '\033[1m== %s\033[0m\n' "$*"; }
ok()   { printf '  \033[32mPASS\033[0m %s\n' "$*"; PASS=$((PASS+1)); }
bad()  { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAIL=$((FAIL+1)); }
skip() { printf '  \033[33mPENDING\033[0m %s\n' "$*"; }
wait_ms() { perl -e "select(undef,undef,undef,$1)"; }

# --- lifecycle -------------------------------------------------------------
STUB_PID=""; WEB_PID=""
teardown() {
  tmux kill-session -t "$SESS" 2>/dev/null || true
  [ -n "$HUB_PID" ] && kill "$HUB_PID" 2>/dev/null || true
  [ -n "$WEB_PID" ] && kill "$WEB_PID" 2>/dev/null || true
  [ -n "$STUB_PID" ] && kill "$STUB_PID" 2>/dev/null || true
  [ -n "${WORK:-}" ] && rm -rf "$WORK" 2>/dev/null || true
}
trap teardown EXIT

free_port() { node -e 'const s=require("net").createServer();s.listen(0,"127.0.0.1",()=>{console.log(s.address().port);s.close()})'; }

# --- stub Ollama -----------------------------------------------------------
start_stub() {
  node "$HERE/stub-ollama.mjs" > "$WORK/stub.out" 2>"$WORK/stub.err" &
  STUB_PID=$!
  for _ in $(seq 1 25); do wait_ms 0.2; grep -q STUB_OLLAMA_READY "$WORK/stub.out" && break; done
  STUB_URL="$(awk '/STUB_OLLAMA_READY/{print $2}' "$WORK/stub.out")"
  [ -n "$STUB_URL" ] || { echo "stub failed to start"; cat "$WORK/stub.err"; exit 2; }
}

# --- TUI (newt in tmux) ----------------------------------------------------
tui_capture() { tmux capture-pane -t "$SESS" -p 2>/dev/null; }
tui_wait()    { local pat="$1" n="${2:-30}"; for _ in $(seq 1 "$n"); do wait_ms 0.5; tui_capture | grep -q "$pat" && return 0; done; return 1; }
tui_send()    { tmux send-keys -t "$SESS" "$1" C-m; }

start_tui() {
  tmux new-session -d -s "$SESS" -x 120 -y 40
  tmux send-keys -t "$SESS" \
    "cd '$WORK/ws' && NEWT_CONFIG_DIR='$WORK/cfg' NEWT_DGX_OLLAMA_URL='$STUB_URL' '$NEWT_BIN' 2>&1 | tee '$WORK/tui.log'" C-m
  tui_wait 'start coder' 20 || { echo "newt never showed the splash"; tui_capture; exit 2; }
  tmux send-keys -t "$SESS" Enter          # dismiss the splash → chat
  tui_wait '❯' 30 || { echo "newt never reached the chat prompt"; tui_capture; exit 2; }
}

# --- web (newt-web cockpit) ------------------------------------------------
# The PEER cockpit shares the TUI's store (so it exposes the TUI's session at
# /api/sessions). The HUB cockpit has its own empty store and DOCKS the peer.
WEB_PORT=""; HUB_PORT=""; HUB_PID=""
start_web() {
  WEB_PORT="$(free_port)"
  NEWT_WEB_BIND="127.0.0.1:$WEB_PORT" NEWT_WEB_AUTH_HEADER="" \
    NEWT_WEB_STATE_DIR="$WORK/cfg" NEWT_WEB_WORKSPACE="$WORK/ws" \
    "$WEB_BIN" > "$WORK/web.log" 2>&1 &
  WEB_PID=$!
  for _ in $(seq 1 40); do wait_ms 0.3; curl -fsS "http://127.0.0.1:$WEB_PORT/healthz" >/dev/null 2>&1 && return 0; done
  echo "newt-web (peer) never became ready"; cat "$WORK/web.log"; exit 2
}
start_hub() {
  HUB_PORT="$(free_port)"
  mkdir -p "$WORK/hub"
  NEWT_WEB_BIND="127.0.0.1:$HUB_PORT" NEWT_WEB_AUTH_HEADER="" \
    NEWT_WEB_STATE_DIR="$WORK/hub" NEWT_WEB_WORKSPACE="$WORK/hub" \
    NEWT_WEB_DOCK_PEERS="laptop-b=http://127.0.0.1:$WEB_PORT" \
    "$WEB_BIN" > "$WORK/hub.log" 2>&1 &
  HUB_PID=$!
  for _ in $(seq 1 40); do wait_ms 0.3; curl -fsS "http://127.0.0.1:$HUB_PORT/healthz" >/dev/null 2>&1 && return 0; done
  echo "newt-web (hub) never became ready"; cat "$WORK/hub.log"; exit 2
}
hub_get() { curl -fsS "http://127.0.0.1:$HUB_PORT$1"; }
web_get()  { curl -fsS "http://127.0.0.1:$WEB_PORT$1"; }
web_post() { local path="$1"; shift; curl -fsS -X POST "http://127.0.0.1:$WEB_PORT$path" "$@"; }

# --- store probe -----------------------------------------------------------
db() { python3 - "$WORK/cfg/conversations.db" "$@"; }
store_conv()  { python3 -c "import sqlite3;r=sqlite3.connect('$WORK/cfg/conversations.db').execute('select id from conversations').fetchone();print(r[0] if r else '')"; }
store_wspath(){ python3 -c "import sqlite3;r=sqlite3.connect('$WORK/cfg/conversations.db').execute('select workspace_path from conversations').fetchone();print(r[0] if r else '')"; }
store_inbox() { python3 -c "import sqlite3;print(len(sqlite3.connect('$WORK/cfg/conversations.db').execute('select 1 from conversation_inbox').fetchall()))"; }

# ===========================================================================
main() {
  [ -x "$NEWT_BIN" ] || { echo "no newt binary at $NEWT_BIN (set NEWT_BIN=)"; exit 2; }
  [ -x "$WEB_BIN" ]  || { echo "no newt-web binary at $WEB_BIN (build it or set WEB_BIN=)"; exit 2; }
  WORK="$(mktemp -d)/drive"; mkdir -p "$WORK"/{cfg,ws}

  say "boot: stub Ollama + newt TUI (tmux) + a first claimed turn"
  start_stub
  start_tui
  tui_send "hello from the driver"
  tui_wait 'STUB_REPLY' 30 && ok "TUI completes a turn against the stub (claims a conversation)" \
    || bad "TUI turn did not complete"
  CONV="$(store_conv)"; WSP="$(store_wspath)"
  [ -n "$CONV" ] && ok "store has the claimed conversation ($CONV)" || bad "no conversation in the store"

  say "web cockpit against the SAME store: multi-session SELECT"
  start_web
  web_get "/" | grep -q 'hello from the driver' \
    && ok "cockpit lists the session (select/overview sees it)" \
    || bad "cockpit did not list the session"

  say "coequal mirror + INJECT through the web (D2: hub enqueues, TUI stays sole writer)"
  FOLLOW="$(web_post /follow --data-urlencode "conv_id=$CONV" --data-urlencode "title=hello from the driver" --data-urlencode "workspace=$WSP")"
  AID="$(printf '%s' "$FOLLOW" | grep -oE '/agents/[0-9]+/' | head -1 | grep -oE '[0-9]+')"
  [ -n "$AID" ] && ok "web follow/attach created a tab (agent $AID)" || bad "follow did not create a tab"
  code="$(curl -s -o /dev/null -w '%{http_code}' -X POST "http://127.0.0.1:$WEB_PORT/agents/$AID/prompt" --data-urlencode "text=INJECTED_VIA_WEB run the tests")"
  [ "$code" = "204" ] && ok "inject accepted (204)" || bad "inject returned $code"
  [ "$(store_inbox)" -ge 1 ] && ok "inbox row landed in the shared store" || bad "no inbox row"

  say "the Phase-1b idle-wake gap (headless, reproducible)"
  wait_ms 1.5
  if tui_capture | grep -q 'echo: INJECTED_VIA_WEB'; then
    ok "TUI consumed the inject WHILE IDLE — Phase 1b is live"
  else
    skip "TUI did NOT consume the inject while idle (pre-1b: only drains at a turn boundary)"
    tui_send ""   # a keypress triggers the turn-boundary drain
    tui_wait 'echo: INJECTED_VIA_WEB' 30 \
      && ok "…and the inject is consumed after a keypress (baseline mirror+inject works)" \
      || bad "inject never consumed even after a keypress"
  fi

  say "DOCK (MVP, HTTP transport): a hub cockpit surfaces a peer's sessions"
  # /api/sessions on the peer is the machine-readable surface a hub reads.
  web_get "/api/sessions" | grep -q '"title":"hello from the driver"' \
    && ok "peer exposes GET /api/sessions (JSON) with its session" \
    || bad "peer /api/sessions did not list the session"
  start_hub
  HUBHOME="$(hub_get /)"
  printf '%s' "$HUBHOME" | grep -q 'docked peers' \
    && ok "hub cockpit renders a 'docked peers' section" \
    || bad "hub has no docked section"
  printf '%s' "$HUBHOME" | grep -q 'laptop-b' \
    && ok "hub shows the configured peer (laptop-b)" \
    || bad "hub did not show the peer label"
  printf '%s' "$HUBHOME" | grep -q 'hello from the driver' \
    && ok "hub MIRRORS the peer's remote session into its overview (dock works)" \
    || bad "hub did not surface the peer's session"

  say "undock / multi-dock (Phases 4/5 refinements)"
  skip "multi-dock N peers into the overview          (repeat NEWT_WEB_DOCK_PEERS; overview groups by peer)"
  skip "remote transcript mirror + inject over a dock (refine: /api/sessions/:id/transcript + SessionInput)"
  skip "undock <peer> / undock all from the TUI       (Phase 5 kill-switch)"
  skip "swap HTTP transport → agent-mesh session_streams (Phase 2, behind dock::DockSource)"

  echo
  say "result: $PASS passed, $FAIL failed"
  [ "$FAIL" -eq 0 ]
}
main "$@"
