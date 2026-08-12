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
SESS="drive-$$"      # peer 1 TUI session
SESS2="drive2-$$"    # peer 2 TUI session (multi-dock)
CSESS="drivec-$$"    # the `newt dock approve` ceremony pane (SAS confirm)
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
  tmux kill-session -t "$SESS2" 2>/dev/null || true
  tmux kill-session -t "$CSESS" 2>/dev/null || true
  [ -n "$GHUB_PID" ] && kill "$GHUB_PID" 2>/dev/null || true
  [ -n "$HUB_PID" ] && kill "$HUB_PID" 2>/dev/null || true
  [ -n "$PEER2_PID" ] && kill "$PEER2_PID" 2>/dev/null || true
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

# --- TUI (newt in tmux), session-parameterized so we can run >1 peer --------
tui_capture() { tmux capture-pane -t "$1" -p 2>/dev/null; }
tui_wait()    { local sess="$1" pat="$2" n="${3:-30}"; for _ in $(seq 1 "$n"); do wait_ms 0.5; tui_capture "$sess" | grep -q "$pat" && return 0; done; return 1; }
tui_send()    { tmux send-keys -t "$1" "$2" C-m; }

start_tui() { # start_tui <tmux-session> <cfg-dir> <ws-dir>
  local sess="$1" cfg="$2" ws="$3"
  mkdir -p "$cfg" "$ws"
  tmux new-session -d -s "$sess" -x 120 -y 40
  tmux send-keys -t "$sess" \
    "cd '$ws' && NEWT_CONFIG_DIR='$cfg' NEWT_DGX_OLLAMA_URL='$STUB_URL' '$NEWT_BIN' 2>&1 | tee '$cfg/tui.log'" C-m
  tui_wait "$sess" 'start coder' 20 || { echo "newt ($sess) never showed the splash"; tui_capture "$sess"; exit 2; }
  tmux send-keys -t "$sess" Enter          # dismiss the splash → chat
  tui_wait "$sess" '❯' 30 || { echo "newt ($sess) never reached the chat prompt"; tui_capture "$sess"; exit 2; }
}

# --- web (newt-web cockpit) ------------------------------------------------
# Two PEER cockpits (each fronting a store with a session) + one HUB cockpit
# (own empty store) that DOCKS both peers — the multi-dock overview.
WEB_PORT=""; WEB_PID=""; PEER2_PORT=""; PEER2_PID=""; HUB_PORT=""; HUB_PID=""; DOCK_PEERS=""
MESH_PUBKEY=""; MESH_PORT=""; GHUB_PORT=""; GHUB_PID=""
_wait_web() { local port="$1" what="$2" log="$3"; for _ in $(seq 1 40); do wait_ms 0.3; curl -fsS "http://127.0.0.1:$port/healthz" >/dev/null 2>&1 && return 0; done; echo "newt-web ($what) never became ready"; cat "$log"; exit 2; }
start_web() { # peer 1, shares the peer-1 store; ALSO exposes over the mesh
  WEB_PORT="$(free_port)"
  NEWT_WEB_BIND="127.0.0.1:$WEB_PORT" NEWT_WEB_AUTH_HEADER="" \
    NEWT_WEB_STATE_DIR="$WORK/cfg" NEWT_WEB_WORKSPACE="$WORK/ws" \
    NEWT_WEB_MESH_BIND="0" \
    "$WEB_BIN" > "$WORK/web.log" 2>&1 &
  WEB_PID=$!; _wait_web "$WEB_PORT" peer1 "$WORK/web.log"
  # The mesh responder binds before the HTTP server accepts, so its line is in
  # the log by now: "mesh dock service on udp/<port> (agent …, pubkey <hex>)".
  MESH_PORT="$(grep -oE 'udp/[0-9]+' "$WORK/web.log" | head -1 | cut -d/ -f2)"
  MESH_PUBKEY="$(grep -oE 'pubkey [0-9a-f]{64}' "$WORK/web.log" | head -1 | awk '{print $2}')"
}
start_peer2() { # peer 2, its own store
  PEER2_PORT="$(free_port)"
  NEWT_WEB_BIND="127.0.0.1:$PEER2_PORT" NEWT_WEB_AUTH_HEADER="" \
    NEWT_WEB_STATE_DIR="$WORK/cfg2" NEWT_WEB_WORKSPACE="$WORK/ws2" \
    "$WEB_BIN" > "$WORK/peer2.log" 2>&1 &
  PEER2_PID=$!; _wait_web "$PEER2_PORT" peer2 "$WORK/peer2.log"
}
start_hub() { # docks whatever is in DOCK_PEERS
  HUB_PORT="$(free_port)"; mkdir -p "$WORK/hub"
  NEWT_WEB_BIND="127.0.0.1:$HUB_PORT" NEWT_WEB_AUTH_HEADER="" \
    NEWT_WEB_STATE_DIR="$WORK/hub" NEWT_WEB_WORKSPACE="$WORK/hub" \
    NEWT_WEB_DOCK_PEERS="$DOCK_PEERS" \
    "$WEB_BIN" > "$WORK/hub.log" 2>&1 &
  HUB_PID=$!; _wait_web "$HUB_PORT" hub "$WORK/hub.log"
}
# A hub that ENFORCES the approved-dock registry (requirement 5): it will only
# dial a mesh peer the operator has approved via `newt dock approve`.
start_gated_hub() {
  GHUB_PORT="$(free_port)"; mkdir -p "$WORK/ghub"
  cp "$WORK/cfg/identity.pem" "$WORK/ghub/identity.pem"   # same operator → mesh auto-teams
  NEWT_WEB_BIND="127.0.0.1:$GHUB_PORT" NEWT_WEB_AUTH_HEADER="" \
    NEWT_WEB_STATE_DIR="$WORK/ghub" NEWT_WEB_WORKSPACE="$WORK/ghub" \
    NEWT_WEB_REQUIRE_DOCK_APPROVAL=1 \
    NEWT_WEB_DOCK_PEERS="meshpeer=mesh:$MESH_PUBKEY@127.0.0.1:$MESH_PORT" \
    "$WEB_BIN" > "$WORK/ghub.log" 2>&1 &
  GHUB_PID=$!; _wait_web "$GHUB_PORT" ghub "$WORK/ghub.log"
}
ghub_get() { curl -fsS "http://127.0.0.1:$GHUB_PORT$1"; }
hub_get() { curl -fsS "http://127.0.0.1:$HUB_PORT$1"; }
web_get()  { curl -fsS "http://127.0.0.1:$WEB_PORT$1"; }
web_post() { local path="$1"; shift; curl -fsS -X POST "http://127.0.0.1:$WEB_PORT$path" "$@"; }

# --- store probe (arg 1 = config dir holding conversations.db) -------------
store_conv()  { python3 -c "import sqlite3;r=sqlite3.connect('$1/conversations.db').execute('select id from conversations').fetchone();print(r[0] if r else '')"; }
store_wspath(){ python3 -c "import sqlite3;r=sqlite3.connect('$1/conversations.db').execute('select workspace_path from conversations').fetchone();print(r[0] if r else '')"; }
store_inbox() { python3 -c "import sqlite3;print(len(sqlite3.connect('$1/conversations.db').execute('select 1 from conversation_inbox').fetchall()))"; }

# ===========================================================================
main() {
  [ -x "$NEWT_BIN" ] || { echo "no newt binary at $NEWT_BIN (set NEWT_BIN=)"; exit 2; }
  [ -x "$WEB_BIN" ]  || { echo "no newt-web binary at $WEB_BIN (build it or set WEB_BIN=)"; exit 2; }
  WORK="$(mktemp -d)/drive"; mkdir -p "$WORK"/{cfg,ws}

  say "boot: stub Ollama + two peer newt TUIs (tmux), each a claimed turn"
  start_stub
  start_tui "$SESS" "$WORK/cfg" "$WORK/ws"
  tui_send "$SESS" "hello from the driver"
  tui_wait "$SESS" 'STUB_REPLY' 30 && ok "peer-1 TUI completes a turn (claims a conversation)" \
    || bad "peer-1 TUI turn did not complete"
  CONV="$(store_conv "$WORK/cfg")"; WSP="$(store_wspath "$WORK/cfg")"
  [ -n "$CONV" ] && ok "peer-1 store has the claimed conversation ($CONV)" || bad "no conversation in peer-1 store"
  start_tui "$SESS2" "$WORK/cfg2" "$WORK/ws2"
  tui_send "$SESS2" "second peer working on kyln"
  tui_wait "$SESS2" 'STUB_REPLY' 30 && ok "peer-2 TUI completes a turn (a DISTINCT session)" \
    || bad "peer-2 TUI turn did not complete"

  say "peer-1 web against its store: multi-session SELECT + coequal INJECT (D2)"
  start_web
  web_get "/" | grep -q 'hello from the driver' \
    && ok "peer-1 cockpit lists the session (select/overview sees it)" \
    || bad "peer-1 cockpit did not list the session"
  FOLLOW="$(web_post /follow --data-urlencode "conv_id=$CONV" --data-urlencode "title=hello from the driver" --data-urlencode "workspace=$WSP")"
  AID="$(printf '%s' "$FOLLOW" | grep -oE '/agents/[0-9]+/' | head -1 | grep -oE '[0-9]+')"
  [ -n "$AID" ] && ok "web follow/attach created a tab (agent $AID)" || bad "follow did not create a tab"
  code="$(curl -s -o /dev/null -w '%{http_code}' -X POST "http://127.0.0.1:$WEB_PORT/agents/$AID/prompt" --data-urlencode "text=INJECTED_VIA_WEB run the tests")"
  [ "$code" = "204" ] && ok "inject accepted (204)" || bad "inject returned $code"
  [ "$(store_inbox "$WORK/cfg")" -ge 1 ] && ok "inbox row landed in the shared store" || bad "no inbox row"

  say "the Phase-1b idle-wake gap (headless, reproducible)"
  wait_ms 1.5
  if tui_capture "$SESS" | grep -q 'echo: INJECTED_VIA_WEB'; then
    ok "TUI consumed the inject WHILE IDLE — Phase 1b is live"
  else
    skip "TUI did NOT consume the inject while idle (pre-1b: only drains at a turn boundary)"
    tui_send "$SESS" ""   # a keypress triggers the turn-boundary drain
    tui_wait "$SESS" 'echo: INJECTED_VIA_WEB' 30 \
      && ok "…and the inject is consumed after a keypress (baseline mirror+inject works)" \
      || bad "inject never consumed even after a keypress"
  fi

  say "DOCK (MVP, HTTP transport): the peer exposes /api/sessions"
  SESSJSON="$(web_get /api/sessions)"
  printf '%s' "$SESSJSON" | grep -q '"title":"hello from the driver"' \
    && ok "peer exposes GET /api/sessions (JSON) with its session" \
    || bad "peer /api/sessions did not list the session"
  printf '%s' "$SESSJSON" | grep -q '"live":true' \
    && ok "peer reports the session LIVE (the 'Connected' signal)" \
    || bad "peer did not report the live-owner status"

  say "MULTI-DOCK: a hub cockpit docks BOTH peers into one overview"
  start_peer2
  # The hub shares the operator identity so a same-user mesh dock auto-teams.
  mkdir -p "$WORK/hub"; cp "$WORK/cfg/identity.pem" "$WORK/hub/identity.pem"
  DOCK_PEERS="laptop-b=http://127.0.0.1:$WEB_PORT,nuc=http://127.0.0.1:$PEER2_PORT,meshpeer=mesh:$MESH_PUBKEY@127.0.0.1:$MESH_PORT"
  start_hub
  HUBHOME="$(hub_get /)"
  printf '%s' "$HUBHOME" | grep -q 'docked peers' \
    && ok "hub renders a 'docked peers' overview" || bad "hub has no docked section"
  printf '%s' "$HUBHOME" | grep -q 'laptop-b' && printf '%s' "$HUBHOME" | grep -q 'nuc' \
    && ok "overview groups BOTH peers (laptop-b + nuc)" || bad "hub did not show both peers"
  printf '%s' "$HUBHOME" | grep -q 'hello from the driver' && printf '%s' "$HUBHOME" | grep -q 'second peer working on kyln' \
    && ok "hub mirrors each peer's distinct session (multi-dock works)" \
    || bad "hub did not surface both peers' sessions"
  printf '%s' "$HUBHOME" | grep -q '▶ hello from the driver' \
    && ok "docked sessions carry the ▶ Connected marker" || bad "no live marker"

  say "SELECT: click a docked session → its transcript mirrors into the hub panel"
  PANEL="$(hub_get "/dock/panel?peer=laptop-b&conv=$CONV")"
  printf '%s' "$PANEL" | grep -q 'dock-remote' \
    && ok "hub renders a docked panel (mirror + D2 inject)" || bad "docked panel not rendered"
  printf '%s' "$PANEL" | grep -q 'STUB_REPLY ok — echo: hello from the driver' \
    && ok "the docked panel MIRRORS the remote transcript (select works)" \
    || bad "docked panel did not carry the remote transcript"

  say "MESH TRANSPORT: the hub also docks a peer over agent-mesh (not HTTP)"
  [ -n "$MESH_PUBKEY" ] && [ -n "$MESH_PORT" ] \
    && ok "the peer bound a mesh dock service (pubkey ${MESH_PUBKEY:0:12}…, udp/$MESH_PORT)" \
    || bad "the peer did not bind a mesh dock service (no identity?)"
  # A peer h3 with sessions renders '· mesh · remote'; a failed mesh fetch would
  # render '· mesh · <error>' instead — so this proves a real mesh round-trip.
  printf '%s' "$HUBHOME" | grep -q '· mesh · remote' \
    && ok "hub lists the mesh peer's session OVER THE MESH (agent-mesh transport works in newt-web)" \
    || bad "hub did not surface the mesh peer's session over the mesh"
  hub_get "/dock/panel?peer=meshpeer&conv=$CONV" | grep -q 'echo: hello from the driver' \
    && ok "SELECT over the MESH mirrors the remote transcript (full dock over agent-mesh)" \
    || bad "mesh select did not mirror the transcript"

  say "INJECT OVER A DOCK (D2 across the dock: the remote host stays sole writer)"
  code="$(curl -s -o /dev/null -w '%{http_code}' -X POST "http://127.0.0.1:$HUB_PORT/dock/inject?peer=laptop-b&conv=$CONV" --data-urlencode "text=DOCK_INJECT check the lints")"
  [ "$code" = "200" ] && ok "hub /dock/inject accepted (re-mirrors the panel)" || bad "dock inject returned $code"
  [ "$(store_inbox "$WORK/cfg")" -ge 2 ] \
    && ok "the inject landed in the REMOTE peer's store inbox (the hub only enqueued)" \
    || bad "remote inbox did not receive the dock inject"
  tui_send "$SESS" ""   # nudge the remote TUI to drain its inbox
  tui_wait "$SESS" 'echo: DOCK_INJECT' 30 \
    && ok "the REMOTE host ran the injected prompt (it — not the hub — wrote the turn)" \
    || bad "remote did not run the dock inject"
  hub_get "/dock/panel?peer=laptop-b&conv=$CONV" | grep -q 'echo: DOCK_INJECT' \
    && ok "hub re-mirrors the remote turn the dock inject produced (full D2 loop over a dock)" \
    || bad "hub did not mirror the remote turn"

  say "UNDOCK / kill-switch (req 7: the TUI forcibly stops exposing to any hub)"
  # Driven through the REAL TUI slash command — `/dock disable` writes the marker
  # the co-located newt-web + its mesh responder both honor (shared config dir).
  if tui_send "$SESS" "/dock disable" && tui_wait "$SESS" 'DISABLED' 15; then
    ok "the TUI '/dock disable' command runs (req-7 kill-switch surface)"
  else
    bad "the TUI '/dock disable' command did not confirm"
  fi
  [ -f "$WORK/cfg/dock-exposure-disabled" ] \
    && ok "the TUI wrote the dock-exposure kill-switch marker" \
    || bad "the marker was not written by the TUI command"
  c="$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$WEB_PORT/api/sessions")"
  [ "$c" = "403" ] && ok "the co-located newt-web honors the TUI kill-switch (/api/sessions → 403)" \
    || bad "peer did not refuse while disabled ($c)"
  HUB2="$(hub_get /)"
  printf '%s' "$HUB2" | grep -q 'hello from the driver' \
    && bad "hub still surfaces the undocked peer's session" \
    || ok "hub no longer surfaces peer-1's sessions (forcibly undocked from every hub)"
  printf '%s' "$HUB2" | grep -q 'second peer working on kyln' \
    && ok "the OTHER peer (nuc) is unaffected — undock is per-box" \
    || bad "undocking peer-1 wrongly dropped peer-2"
  tui_send "$SESS" "/dock enable"; tui_wait "$SESS" 're-enabled' 15
  [ -f "$WORK/cfg/dock-exposure-disabled" ] \
    && bad "the TUI '/dock enable' did not clear the marker" \
    || ok "the TUI '/dock enable' clears the marker"
  hub_get / | grep -q 'hello from the driver' \
    && ok "re-enabling exposure re-docks the peer" || bad "re-enable did not restore the dock"

  say "COEQUAL REFRESH (req 3: the web self-refreshes so a TUI change needs no F5)"
  web_get / | grep -q 'hx-get="/overview"' && web_get / | grep -q 'every 3s' \
    && ok "the cockpit page polls /overview (docked + sessions self-refresh)" \
    || bad "the page does not wire the self-refresh"
  web_get /overview | grep -q 'hello from the driver' \
    && ok "GET /overview reflects the shared store (the TUI session appears without a reload)" \
    || bad "/overview did not reflect the store"

  say "DOCKING CEREMONY (req 5: a gated hub dials a mesh peer ONLY after approval)"
  if "$NEWT_BIN" dock --help >/dev/null 2>&1; then
    start_gated_hub
    # 1. Before approval, the gated hub REFUSES the mesh peer (fail-closed).
    GBEFORE="$(ghub_get /)"
    printf '%s' "$GBEFORE" | grep -q 'not approved' \
      && ok "gated hub REFUSES the unapproved mesh peer (registry gate is load-bearing)" \
      || bad "gated hub did not refuse the unapproved peer"
    printf '%s' "$GBEFORE" | grep -q '· mesh · remote' \
      && bad "gated hub mirrored an UNAPPROVED peer (gate bypassed!)" \
      || ok "gated hub does NOT mirror the unapproved peer's session"

    # 2. The operator runs the SAS ceremony: `newt dock approve` in a real TTY
    #    (tmux), compares the 6-word SAS, and confirms with `y`.
    tmux new-session -d -s "$CSESS" -x 200 -y 50 \
      "'$NEWT_BIN' --config '$WORK/ghub/config.toml' dock approve --operator-key-path '$WORK/ghub/identity.pem' --pubkey '$MESH_PUBKEY' --label meshpeer; echo CEREMONY_EXIT=\$? >> '$WORK/ghub/approve.out'"
    if tui_wait "$CSESS" 'y/N' 20; then
      tui_capture "$CSESS" | grep -q 'SAS words' \
        && ok "approve shows the 6-word SAS bound to the peer pubkey (the ceremony 'secret')" \
        || bad "approve did not display the SAS words"
      tmux send-keys -t "$CSESS" 'y' C-m
      tui_wait "$CSESS" 'CEREMONY_EXIT=0' 15 || wait_ms 1.0
    else
      bad "approve did not reach the SAS confirm prompt"
    fi
    "$NEWT_BIN" --config "$WORK/ghub/config.toml" dock list --operator-key-path "$WORK/ghub/identity.pem" 2>/dev/null | grep -q 'meshpeer' \
      && ok "the signed approval is recorded (newt dock list shows meshpeer)" \
      || bad "the approval was not recorded in the registry"

    # 3. After approval, the SAME gated hub now ADMITS the mesh peer.
    GAFTER="$(ghub_get /)"
    printf '%s' "$GAFTER" | grep -q '· mesh · remote' \
      && ok "gated hub now ADMITS the approved peer and mirrors it over the mesh (ceremony end-to-end)" \
      || bad "gated hub still refuses the peer after approval"

    # 4. `/undock all` re-closes the gate (req 7). Terminal-gated, so drive it
    #    through a real TTY too.
    tmux kill-session -t "$CSESS" 2>/dev/null || true
    tmux new-session -d -s "$CSESS" -x 200 -y 50 \
      "'$NEWT_BIN' --config '$WORK/ghub/config.toml' dock revoke-all --operator-key-path '$WORK/ghub/identity.pem'; echo REVOKE_EXIT=\$? >> '$WORK/ghub/approve.out'"
    tui_wait "$CSESS" 'y/N' 15 && tmux send-keys -t "$CSESS" 'y' C-m
    tui_wait "$CSESS" 'REVOKE_EXIT=' 15 || wait_ms 1.0
    "$NEWT_BIN" --config "$WORK/ghub/config.toml" dock list --operator-key-path "$WORK/ghub/identity.pem" 2>/dev/null | grep -q 'meshpeer' \
      && bad "revoke-all did not remove the approval" \
      || ok "newt dock revoke-all removes the approval (the /undock all write path)"
  else
    skip "docking ceremony E2E (needs a branch newt with 'dock'; set NEWT_BIN= to the built binary)"
  fi

  say "still pending (later phases)"
  skip "terminate an already-live dock the instant a revoke lands (verify_at(gen) push, not next re-check)"
  skip "TUI notification lines on a web-injected prompt while idle (Phase 1b; drain-at-idle)"
  skip "swap HTTP transport → agent-mesh session_streams      (Phase 2, behind dock::DockSource)"

  echo
  say "result: $PASS passed, $FAIL failed"
  [ "$FAIL" -eq 0 ]
}
main "$@"
