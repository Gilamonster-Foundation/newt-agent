#!/usr/bin/env bash
# llm-mode.sh — switch dgx1 (DGX Spark GB10, 128 GB UNIFIED) between inference
# modes. Unified memory means only ONE heavy config fits at a time.
#
#   ornith            vLLM serves Ornith-1.0-35B (:8000, util 0.65) + llama-router
#                     (:8080) co-hosts small/medium GGUFs. DEFAULT operating mode.
#   <big-model mode>  vLLM DOWN; the whole box goes to llama-router so it can hold
#                     one large GGUF resident on :8080. Registered modes below.
#
# Big-model modes (vLLM-off; each maps to a GGUF stem in ~/models/gguf/):
#   super       -> nemotron-3-super_120b   (~86 GB)
#   kimi-dev    -> kimi-dev_72b            (~54 GB, coding)
#   kimi-linear -> kimi-linear_48b         (~30 GB; above the co-host budget)
#   big <stem>  -> any stem you name
#
# Usage: llm-mode.sh {status|list|ornith|super|kimi-dev|big <stem>}
#
# Safe hand-off rules baked in:
#   -> big  : stop vLLM FIRST (frees ~79 GB), then load the large GGUF.
#   -> ornith: restart llama-router to EVICT the big model BEFORE starting vLLM,
#              else vLLM OOMs grabbing its 79 GB reservation.
set -uo pipefail

VLLM_PORT=8000; ROUTER_PORT=8080
GGUF_DIR="/home/hartsock/models/gguf"
ORNITH_SH="/home/hartsock/ornith.sh"
VLLM_PAT="vllm.entrypoints.openai.api_server"
VLLM_OUT="/home/hartsock/ornith.out"

# --- big-model registry: mode name -> gguf stem (file <stem>.gguf) ---
declare -A BIG=(
  [super]="nemotron-3-super_120b"
  [kimi-dev]="kimi-dev_72b"
  [kimi-linear]="kimi-linear_48b"
)

say(){ echo "$(date +%H:%M:%S) [llm-mode] $*"; }
vllm_up(){ curl -sf -m5 "localhost:$VLLM_PORT/v1/models" >/dev/null 2>&1; }
router_up(){ curl -sf -m5 "localhost:$ROUTER_PORT/v1/models" >/dev/null 2>&1; }
stem_file(){ echo "$GGUF_DIR/$1.gguf"; }

router_resident(){
  curl -sf -m5 "localhost:$ROUTER_PORT/v1/models" 2>/dev/null | python3 -c 'import json,sys
try: d=json.load(sys.stdin)
except Exception: sys.exit()
for m in d.get("data",[]):
    st=str(m.get("state") or m.get("status") or "").lower()
    if st in ("loaded","ready","resident") or m.get("loaded"): print(m["id"])' 2>/dev/null
}

list_modes(){
  echo "=== big-model modes (vLLM-off) ==="
  for k in "${!BIG[@]}"; do
    f="$(stem_file "${BIG[$k]}")"
    if [[ -f "$f" ]]; then sz="$(du -h "$f" | cut -f1)"; echo "  $k -> ${BIG[$k]}  (${sz}, present)"
    else echo "  $k -> ${BIG[$k]}  (MISSING $f -- download first)"; fi
  done | sort
}

status(){
  echo "=== dgx1 inference status ==="
  if vllm_up; then echo "  vLLM (:$VLLM_PORT)   : UP  -> $(curl -sf -m5 localhost:$VLLM_PORT/v1/models | python3 -c 'import json,sys;print(json.load(sys.stdin)["data"][0]["id"])' 2>/dev/null)"
  else echo "  vLLM (:$VLLM_PORT)   : down"; fi
  if router_up; then res="$(router_resident | paste -sd, -)"; echo "  router (:$ROUTER_PORT) : UP  (resident: ${res:-none})"
  else echo "  router (:$ROUTER_PORT) : down"; fi
  vllm_up && echo "  mode            : ornith" || echo "  mode            : big/idle (vLLM off)"
  free -h | awk 'NR==1||/Mem:/{print "  "$0}'
}

stop_vllm(){
  if ! vllm_up && ! pgrep -f "$VLLM_PAT" >/dev/null; then say "vLLM already down"; return; fi
  say "stopping vLLM ..."; pkill -f "$VLLM_PAT" || true
  for i in $(seq 1 60); do pgrep -f "$VLLM_PAT" >/dev/null || break; sleep 2; done
  pgrep -f "$VLLM_PAT" >/dev/null && { say "force-killing vLLM"; pkill -9 -f "$VLLM_PAT" || true; sleep 3; }
  say "vLLM stopped"
}
start_vllm(){
  if vllm_up; then say "vLLM already up"; return; fi
  say "starting vLLM ($ORNITH_SH) ..."
  setsid nohup bash "$ORNITH_SH" >"$VLLM_OUT" 2>&1 < /dev/null &
  for i in $(seq 1 180); do vllm_up && { say "vLLM healthy on :$VLLM_PORT"; return 0; }; sleep 5; done
  say "ERROR: vLLM not healthy in ~15m -- check $VLLM_OUT"; return 1
}
router_reset(){ say "restarting llama-router (evict resident) ..."; sudo systemctl restart llama-router; for i in $(seq 1 30); do router_up && break; sleep 2; done; }
warm(){
  local model="$1" tmo="${2:-1200}"
  say "loading '$model' via router (timeout ${tmo}s; first cold load of a big GGUF is slow) ..."
  local r; r="$(curl -s -m "$tmo" "localhost:$ROUTER_PORT/v1/chat/completions" -H 'Content-Type: application/json' \
        -d "{\"model\":\"$model\",\"messages\":[{\"role\":\"user\",\"content\":\"say ok\"}],\"max_tokens\":10}")"
  if echo "$r" | grep -q '"content"'; then say "OK: '$model' loaded and replied."; else
    say "FAIL: '$model' :: $(echo "$r" | tr -d '\n' | head -c 240)"; return 1; fi
}
go_big(){
  local stem="$1" f; f="$(stem_file "$stem")"
  [[ -f "$f" ]] || { say "ERROR: $f not present. Download it into $GGUF_DIR first."; exit 1; }
  say "==> switching to BIG model '$stem' (vLLM off)"
  stop_vllm; router_reset; warm "$stem" 1200 || { say "big-model load failed (see above)"; exit 1; }
  echo; status
}

case "${1:-status}" in
  status) status ;;
  list)   list_modes ;;
  ornith) say "==> switching to ORNITH (vLLM up, router co-hosts)"; router_reset; start_vllm || exit 1; echo; status ;;
  big)    [[ -n "${2:-}" ]] || { echo "usage: $0 big <stem>"; exit 64; }; go_big "$2" ;;
  *)      if [[ -n "${BIG[${1}]:-}" ]]; then go_big "${BIG[$1]}"; else echo "usage: $0 {status|list|ornith|$(IFS='|'; echo "${!BIG[*]}")|big <stem>}"; exit 64; fi ;;
esac
