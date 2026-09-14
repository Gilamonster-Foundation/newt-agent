#!/usr/bin/env bash
# Terminal-Bench campaign (#2318): every model in a roster x newt, pi, codex on
# ONE frozen task set, N trials per task, graded only by Harbor's verifier.
# Grouped by model: the router swaps models expensively, so each model is
# loaded once and all harnesses run against it before the next.
#
# Usage: tb-campaign.sh <roster.txt> <task-set.json> <campaign-name>
#   roster.txt     one model id as served per line (# comments allowed)
#   task-set.json  a Harbor job file; only its "datasets" are used (smart-ab-8.json)
#
# Required env (host-secret values stay in local files, never here):
#   NEWT_BENCH_BIN               bookworm-built newt (glibc floor enforced)
#   NEWT_BENCH_PROFILE_TEMPLATE  local newt profile; its `endpoint` serves all
#                                three harnesses, its `model` line is rewritten
#   TB_CTX_SIZE                  the ctx-size the server must be serving; every
#                                harness is pinned to it and a cell is skipped if
#                                the served value differs
# Optional: TB_TRIALS (3), TB_HARNESSES ("newt pi codex"), TB_TIMEOUT_MULT (3),
#   TB_JOBS_ROOT (/var/tmp/tbench-harbor), TB_MIN_FREE_GB (30),
#   NEWT_BENCH_MAX_ROUNDS (the adapter's 40).
#
# Output: $TB_JOBS_ROOT/<campaign>/ holds one Harbor job per cell
# (<model>__<harness>), trials.jsonl (every trial dir Harbor created),
# cells.jsonl (bindings + coverage), campaign.log. Re-running the same command
# skips cells already recorded (a skipped cell is retried). A partial job dir is moved aside, not
# deleted. Report: python3 tb_campaign.py table <out>.
set -euo pipefail
ROSTER="${1:?roster file}"; TASKS="${2:?task-set json}"; CAMPAIGN="${3:?campaign name}"
: "${NEWT_BENCH_BIN:?bookworm-built newt binary}"
: "${NEWT_BENCH_PROFILE_TEMPLATE:?local newt profile toml}"
: "${TB_CTX_SIZE:?ctx-size the server serves, e.g. 131072}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export PYTHONPATH="$HERE${PYTHONPATH:+:$PYTHONPATH}"
TRIALS="${TB_TRIALS:-3}"; MULT="${TB_TIMEOUT_MULT:-3}"
OUT="${TB_JOBS_ROOT:-/var/tmp/tbench-harbor}/$CAMPAIGN"; mkdir -p "$OUT"
LOG="$OUT/campaign.log"
log() { echo "$(date -Is) $*" | tee -a "$LOG"; }

"$HERE/glibc-floor.sh" "$NEWT_BENCH_BIN"
EP="$(sed -n 's/^endpoint *= *"\(.*\)"/\1/p' "$NEWT_BENCH_PROFILE_TEMPLATE" | head -1)"
[ -n "$EP" ] || { echo "no endpoint in $NEWT_BENCH_PROFILE_TEMPLATE" >&2; exit 2; }
export TB_LOCAL_BASE_URL="$EP/v1" TB_LOCAL_CONTEXT_WINDOW="$TB_CTX_SIZE"
unset OPENAI_BASE_URL NEWT_BENCH_SMART   # codex would append the first; the second changes newt's arm
export NEWT_BENCH_BIN NEWT_BENCH_CONTEXT_WINDOW="$TB_CTX_SIZE" NEWT_BENCH_OCAP=off

py() { python3 -c "$@"; }
N_TASKS="$(py 'import json,sys; print(sum(len(d["task_names"]) for d in json.load(open(sys.argv[1]))["datasets"]))' "$TASKS")"
ENGINE="llama.cpp router $(curl -s -m 20 "$EP/props" | py 'import json,sys; print(json.load(sys.stdin).get("build_info"))')"

# served <model> -> "<status> <ctx-size>", or "absent -"
served() {
  curl -s -m 20 "$EP/v1/models" | py '
import json,sys
for m in json.load(sys.stdin)["data"]:
    if m["id"]==sys.argv[1]:
        st=m.get("status") or {}; a=st.get("args") or []
        print(st.get("value"), a[a.index("--ctx-size")+1] if "--ctx-size" in a else "-"); break
else: print("absent -")' "$1"
}
loaded() { curl -s -m 20 "$EP/v1/models" | py 'import json,sys; print(",".join(m["id"] for m in json.load(sys.stdin)["data"] if (m.get("status") or {}).get("value")=="loaded"))'; }
# A one-token request loads the model; the router evicts by its own policy.
prewarm() {
  for _ in $(seq 1 60); do
    code=$(curl -s -m 600 -o /dev/null -w '%{http_code}' "$EP/v1/chat/completions" -H 'content-type: application/json' \
      -d "{\"model\":\"$1\",\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}],\"max_tokens\":1}") || code=000
    [ "$code" = 200 ] && return 0; sleep 10
  done; return 1
}
# A trial Harbor killed leaves its generation running server-side; let it drain
# so it neither steals the next cell's slot nor its timing.
wait_idle() {
  for _ in $(seq 1 180); do
    [ "$(curl -s -m 20 "$EP/slots?model=$1" | py 'import json,sys; print(any(s.get("is_processing") for s in json.load(sys.stdin)))')" = False ] && return 0
    sleep 10
  done; return 1
}
record() { # record <cell.json> [job dir]: bind the cell and ingest its trials
  if [ -n "${2:-}" ]; then python3 "$HERE/tb_campaign.py" ingest "$2" "$1" "$OUT"
  else py 'import json,sys; c=json.load(open(sys.argv[1])); open(sys.argv[2],"a").write(json.dumps({**c,"observed":0})+"\n")' "$1" "$OUT/cells.jsonl"; fi
}

log "campaign=$CAMPAIGN tasks=$N_TASKS x trials=$TRIALS engine=$ENGINE newt=$("$NEWT_BENCH_BIN" --version)"
for MODEL in $(grep -vE '^\s*(#|$)' "$ROSTER"); do
  read -r status ctx < <(served "$MODEL") || true
  log "model=$MODEL status=$status ctx=$ctx loaded=$(loaded)"
  [ "$status" = loaded ] || prewarm "$MODEL" || log "model=$MODEL did not load"
  PROFILE="$OUT/newt-$MODEL.toml"
  sed "s/^model *=.*/model = \"$MODEL\"/" "$NEWT_BENCH_PROFILE_TEMPLATE" > "$PROFILE"
  for H in ${TB_HARNESSES:-newt pi codex}; do
    NAME="${MODEL}__${H}"; JOB="$OUT/$NAME"; CELL="$OUT/$NAME.cell.json"
    if [ -f "$OUT/cells.jsonl" ] && py 'import json,sys; sys.exit(not any(c["job"]==sys.argv[2] and not c.get("skipped") for c in map(json.loads,open(sys.argv[1]))))' "$OUT/cells.jsonl" "$NAME"; then
      log "$NAME recorded; skipping"; continue
    fi
    [ -d "$JOB" ] && { mv "$JOB" "$JOB.partial.$(date +%s)"; log "$NAME: moved an unrecorded partial job aside"; }
    case "$H" in
      newt) AGENT=newt_agent:NewtAgent; LABEL="newt/$MODEL"; HV="$("$NEWT_BENCH_BIN" --version | sed "s/^newt //")" ;;
      pi) AGENT=pi_local:PiLocal; LABEL="local/$MODEL"; HV="" ;;
      codex) AGENT=codex_local:CodexLocal; LABEL="local/$MODEL"; HV="" ;;
      *) echo "unknown harness $H" >&2; exit 2 ;;
    esac
    wait_idle "$MODEL" || log "$NAME: slot still busy after 30 min"
    read -r status ctx < <(served "$MODEL") || true
    free_gb=$(df -BG --output=avail "$OUT" | tail -1 | tr -dc 0-9)
    skip=""
    [ "$status" = loaded ] || skip="model not loaded (status=$status)"
    [ "$ctx" = "$TB_CTX_SIZE" ] || skip="served ctx-size $ctx != pinned $TB_CTX_SIZE"
    [ "$free_gb" -ge "${TB_MIN_FREE_GB:-30}" ] || skip="only ${free_gb}G free"
    py 'import json,sys; k=sys.argv[1::2]; v=sys.argv[2::2]; c=dict(zip(k,v))
for n in ("expected","trials","n_tasks"): c[n]=int(c[n])
c["skipped"]=c["skipped"] or None; c["harness_version"]=c["harness_version"] or None
json.dump(c,open(c.pop("_path"),"w"))' \
      _path "$CELL" campaign "$CAMPAIGN" job "$NAME" model "$MODEL" harness "$H" harness_version "$HV" \
      engine "$ENGINE" ctx_served "$ctx" ctx_pinned "$TB_CTX_SIZE" loaded_at_start "$(loaded)" \
      task_set_sha256 "$(sha256sum "$TASKS" | cut -d' ' -f1)" n_tasks "$N_TASKS" trials "$TRIALS" \
      expected "$((N_TASKS * TRIALS))" agent_timeout_multiplier "$MULT" sandbox "none (newt --unsafe-host-exec)" \
      newt_max_rounds "${NEWT_BENCH_MAX_ROUNDS:-40}" pi_codex_round_cap "none" \
      newt_binary_sha256 "$(sha256sum "$NEWT_BENCH_BIN" | cut -d' ' -f1)" skipped "$skip" started_at "$(date -Is)"
    if [ -n "$skip" ]; then log "$NAME skipped: $skip"; record "$CELL"; continue; fi
    py 'import json,sys; j=json.load(open(sys.argv[1]))
json.dump({"jobs_dir":sys.argv[3],"datasets":j["datasets"],"agents":[{"import_path":sys.argv[4],"model_name":sys.argv[5]}]},open(sys.argv[2],"w"),indent=1)' \
      "$TASKS" "$OUT/$NAME.job.json" "$OUT" "$AGENT" "$LABEL"
    log "$NAME start"
    rc=0
    NEWT_BENCH_PROFILE="$PROFILE" harbor run --config "$OUT/$NAME.job.json" --job-name "$NAME" \
      --n-attempts "$TRIALS" --n-concurrent 1 --max-retries 0 --agent-timeout-multiplier "$MULT" \
      --no-delete -y > "$OUT/$NAME.harbor.log" 2>&1 || rc=$?
    py 'import json,sys; c=json.load(open(sys.argv[1])); c.update(harbor_exit=int(sys.argv[2]), finished_at=sys.argv[3], loaded_at_end=sys.argv[4]); json.dump(c,open(sys.argv[1],"w"))' \
      "$CELL" "$rc" "$(date -Is)" "$(loaded)"
    record "$CELL" "$JOB"
    log "$NAME done harbor_exit=$rc"
  done
done
python3 "$HERE/tb_campaign.py" table "$OUT" | tee -a "$LOG"
