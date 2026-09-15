#!/usr/bin/env bash
# Terminal-Bench campaign (#2318): every model in a roster x newt, pi, codex on
# ONE frozen task set, N trials per task, graded only by Harbor's verifier.
# Grouped by model: the router swaps models expensively, so each model is
# loaded once and all harnesses run against it before the next.
#
# Usage: tb-campaign.sh <roster.txt> <task-set.json> <campaign-name>
#   roster.txt     one model id as served per line (# comments allowed), optionally
#                  followed by an operator-declared digest (recorded as declared,
#                  unverified, and passed to newt as its model digest)
#   task-set.json  a Harbor job file; only its "datasets" are used (smart-ab-8.json)
#
# Arms: TB_MATRIX names a file of "<harness> <treatment>" lines; without it every
# harness in TB_HARNESSES runs treatment "none". A treatment is
# treatments/<name>.toml (newt only; see tb_campaign.py). Cells are
# <model>__<harness> for "none" and <model>__<harness>__<treatment> otherwise.
# Every cell is checked against campaign.pin.json (task set, engine, served ctx,
# newt binary, instrument commit, model fingerprint, pi/codex versions) and a
# mismatch is refused, so cells that pool or pair share one pin.
#
# Required env (host-secret values stay in local files, never here):
#   NEWT_BENCH_BIN               bookworm-built newt (glibc floor enforced)
#   NEWT_BENCH_PROFILE_TEMPLATE  local newt profile; its `endpoint` serves all
#                                three harnesses, its `model` line is rewritten
#   TB_CTX_SIZE                  the ctx-size the server must be serving; every
#                                harness is pinned to it and a cell is skipped if
#                                the served value differs
# Windows (#2318): with TB_SCHEDULE set (see systemd/tb-windows.example) the
# runner starts a trial only inside a declared window, only while no model
# outside the roster is loaded (it waits TB_ROUTER_WAIT_MIN, default 30, then
# skips the window; it never unloads anything), and cancels a trial still
# running at window end + TB_DEADLINE_GRACE_MIN (a maintainer setting, default
# 120). A cancelled attempt is archived as interrupted and re-run next window;
# a second cancel of the same attempt is final (DeadlineExhausted, cause agent).
# $OUT/campaign.stop (any content) stops the runner at the next trial boundary.
# GPU-hours: every trial run appends one content-addressed line to
# $OUT/ledger.jsonl (runner wall clock around Harbor); with TB_GPU_HOURS_CEILING
# set, no trial starts once the total reaches it, and a tampered line refuses.
# Optional: TB_TRIALS (3), TB_HARNESSES ("newt pi codex"), TB_MATRIX, TB_TIMEOUT_MULT (3),
#   TB_JOBS_ROOT (/var/tmp/tbench-harbor), TB_MIN_FREE_GB (30), TB_INSTRUMENT_COMMIT
#   (default: this checkout's HEAD; set it when running a frozen copy),
#   NEWT_BENCH_MAX_ROUNDS (the adapter's 40).
#
# Output: $TB_JOBS_ROOT/<campaign>/<cell>/ holds ONE Harbor job per trial,
# <task>__a<attempt> (Harbor has no stop-after-this-trial, and resuming a job
# deletes result-less trial dirs, so the process boundary is the trial
# boundary); trials.jsonl, cells.jsonl and campaign.log sit beside the cells.
# Re-running the same command skips recorded cells and, inside an unrecorded
# cell, every trial job that already has a non-cancelled result.json: a graded
# trial is never re-run. A partial or cancelled trial job is moved to
# <cell>/interrupted/ (never deleted) before its attempt runs again.
# Report: python3 tb_campaign.py table <out>.
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
# Stock Codex copies OPENAI_API_KEY into the container and appends OPENAI_BASE_URL
# to its config; NEWT_BENCH_SMART and NEWT_BENCH_VERIFY_OUTCOMES would change
# newt's arm (a treatment sets them per cell).
unset OPENAI_API_KEY OPENAI_BASE_URL NEWT_BENCH_SMART NEWT_BENCH_VERIFY_OUTCOMES
export NEWT_BENCH_BIN NEWT_BENCH_CONTEXT_WINDOW="$TB_CTX_SIZE" NEWT_BENCH_OCAP=off

py() { python3 -c "$@"; }
tbc() { python3 "$HERE/tb_campaign.py" "$@"; }
INSTRUMENT="${TB_INSTRUMENT_COMMIT:-$(git -C "$HERE" rev-parse --short=12 HEAD 2>/dev/null || echo unknown)}"
N_TASKS="$(py 'import json,sys; print(sum(len(d["task_names"]) for d in json.load(open(sys.argv[1]))["datasets"]))' "$TASKS")"
ENGINE="llama.cpp router $(curl -s -m 20 "$EP/props" | py 'import json,sys; print(json.load(sys.stdin).get("build_info"))')"

# served <model> -> "<status> <ctx-size> <fingerprint json>", or "absent - null".
# The fingerprint (tb_campaign.fingerprint) is the server's model metadata, the
# GGUF basename and the preset's chat-template kwargs; the router exposes no
# weights digest.
served() {
  curl -s -m 20 "$EP/v1/models" | py '
import json,sys
from tb_campaign import fingerprint
for m in json.load(sys.stdin)["data"]:
    if m["id"]==sys.argv[1]:
        st=m.get("status") or {}; a=st.get("args") or []
        print(st.get("value"), a[a.index("--ctx-size")+1] if "--ctx-size" in a else "-", json.dumps(fingerprint(m), sort_keys=True)); break
else: print("absent - null")' "$1"
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
STOP="$OUT/campaign.stop"; DEADLINE=""
# Exit cleanly at a trial boundary when asked to, or when the window has closed.
boundary() {
  if [ -f "$STOP" ]; then log "stop requested ($(head -c 200 "$STOP")); exiting at a trial boundary"; exit 0; fi
  [ -n "${TB_SCHEDULE:-}" ] || return 0
  read -r WINDOW WEND < <(tbc window "$TB_SCHEDULE") || { log "outside the declared windows; exiting"; exit 0; }
  [ ! -f "$OUT/skipped-window.$WINDOW" ] || exit 0
  DEADLINE=$(( WEND + ${TB_DEADLINE_GRACE_MIN:-120} * 60 ))
}
# No slot sharing with interactive use: wait while a model outside the roster is
# loaded, then skip this window. The runner never unloads the maintainer's models.
router_clear() {
  [ -n "${TB_SCHEDULE:-}" ] || return 0
  local others waited=0
  while :; do
    others=$(loaded | tr ',' '\n' | { grep -vxF -f <(printf '%s\n' "${MODELS[@]%% *}") || true; } | paste -sd, -)
    [ -z "$others" ] && return 0
    [ "$waited" -lt $(( ${TB_ROUTER_WAIT_MIN:-30} * 60 )) ] || break
    sleep 30; waited=$((waited + 30))
  done
  log "window $WINDOW skipped: $others loaded"; : > "$OUT/skipped-window.$WINDOW"; exit 0
}
record() { # record <cell.json> [job dir]: bind the cell and ingest its trials
  if [ -n "${2:-}" ]; then python3 "$HERE/tb_campaign.py" ingest "$2" "$1" "$OUT"
  else py 'import json,sys; c=json.load(open(sys.argv[1])); open(sys.argv[2],"a").write(json.dumps({**c,"observed":0})+"\n")' "$1" "$OUT/cells.jsonl"; fi
}

if [ -n "${TB_MATRIX:-}" ]; then mapfile -t ARMS < <(grep -vE '^\s*(#|$)' "$TB_MATRIX")
else ARMS=(); for H in ${TB_HARNESSES:-newt pi codex}; do ARMS+=("$H none"); done; fi
mapfile -t MODELS < <(grep -vE '^\s*(#|$)' "$ROSTER")

log "campaign=$CAMPAIGN tasks=$N_TASKS x trials=$TRIALS engine=$ENGINE instrument=$INSTRUMENT newt=$("$NEWT_BENCH_BIN" --version) arms=${#ARMS[@]}"
boundary; router_clear
for MLINE in "${MODELS[@]}"; do
  read -r MODEL DIGEST <<< "$MLINE"
  read -r status ctx _ < <(served "$MODEL") || true
  log "model=$MODEL status=$status ctx=$ctx loaded=$(loaded)"
  [ "$status" = loaded ] || prewarm "$MODEL" || log "model=$MODEL did not load"
  for ARM in "${ARMS[@]}"; do
    read -r H T <<< "$ARM"
    TFILE=none; [ "${T:-none}" = none ] || TFILE="$HERE/treatments/$T.toml"
    [ "$TFILE" = none ] || [ "$H" = newt ] || { echo "treatment $T is newt-only (arm: $ARM)" >&2; exit 2; }
    NAME="${MODEL}__${H}"; [ "$TFILE" = none ] || NAME+="__$T"
    JOB="$OUT/$NAME"; CELL="$OUT/$NAME.cell.json"; PROFILE="$OUT/$NAME.profile.toml"
    envs=$(tbc profile "$TFILE" "$MODEL" "$NEWT_BENCH_PROFILE_TEMPLATE" "$PROFILE") || { log "$NAME: treatment refused"; exit 2; }
    TENV=(); [ -z "$envs" ] || mapfile -t TENV <<< "$envs"
    if [ -f "$OUT/cells.jsonl" ] && py 'import json,sys; sys.exit(not any(c["job"]==sys.argv[2] and not c.get("skipped") for c in map(json.loads,open(sys.argv[1]))))' "$OUT/cells.jsonl" "$NAME"; then
      log "$NAME recorded; skipping"; continue
    fi
    case "$H" in
      newt) AGENT=newt_agent:NewtAgent; LABEL="newt/$MODEL"; HV="$("$NEWT_BENCH_BIN" --version | sed "s/^newt //")" ;;
      pi) AGENT=pi_local:PiLocal; LABEL="local/$MODEL"; HV="" ;;
      codex) AGENT=codex_local:CodexLocal; LABEL="local/$MODEL"; HV="" ;;
      *) echo "unknown harness $H" >&2; exit 2 ;;
    esac
    wait_idle "$MODEL" || log "$NAME: slot still busy after 30 min"
    read -r status ctx fp < <(served "$MODEL") || true
    free_gb=$(df -BG --output=avail "$OUT" | tail -1 | tr -dc 0-9)
    skip=""
    [ "$status" = loaded ] || skip="model not loaded (status=$status)"
    [ "$ctx" = "$TB_CTX_SIZE" ] || skip="served ctx-size $ctx != pinned $TB_CTX_SIZE"
    [ "$free_gb" -ge "${TB_MIN_FREE_GB:-30}" ] || skip="only ${free_gb}G free"
    tbc cell "$CELL" "$TFILE" campaign "$CAMPAIGN" job "$NAME" model "$MODEL" harness "$H" harness_version "$HV" \
      model_fingerprint_json "${fp:-null}" model_digest_declared "${DIGEST:-}" instrument_commit "$INSTRUMENT" \
      engine "$ENGINE" ctx_served "$ctx" ctx_pinned "$TB_CTX_SIZE" loaded_at_start "$(loaded)" \
      task_set_sha256 "$(sha256sum "$TASKS" | cut -d' ' -f1)" n_tasks "$N_TASKS" trials "$TRIALS" \
      expected "$((N_TASKS * TRIALS))" agent_timeout_multiplier "$MULT" sandbox "none (newt --unsafe-host-exec)" \
      newt_max_rounds "${NEWT_BENCH_MAX_ROUNDS:-40}" pi_codex_round_cap "none" \
      newt_binary_sha256 "$(sha256sum "$NEWT_BENCH_BIN" | cut -d' ' -f1)" skipped "$skip" started_at "$(date -Is)"
    [ -n "$skip" ] || { bad=$(tbc pin-check "$OUT" "$CELL") || skip="pin mismatch: $bad"; }
    if [ -n "$skip" ]; then log "$NAME skipped: $skip"; record "$CELL"; continue; fi
    # pi and codex install exactly the version the pin recorded (their first run records it).
    # pi and codex install exactly the version the pin recorded (their first run records it).
    PV="$([ "$H" = newt ] || tbc pinned-version "$OUT" "$H")"
    log "$NAME start treatment=${T:-none} env=${TENV[*]:-}"
    mkdir -p "$JOB"; failed=0
    mapfile -t PLAN < <(tbc plan "$TASKS" "$TRIALS")
    for STEP in "${PLAN[@]}"; do
      read -r TASK K DPATH <<< "$STEP"
      TJ="$JOB/${TASK}__a$K"
      case "$(tbc job-state "$TJ")" in
        done) continue ;;
        partial) mkdir -p "$JOB/interrupted"; mv "$TJ" "$JOB/interrupted/$(basename "$TJ").$(date +%s)"
                 log "$NAME: archived an interrupted trial job ${TASK}__a$K" ;;
      esac
      boundary; router_clear
      if [ -n "${TB_GPU_HOURS_CEILING:-}" ]; then
        crc=0; used=$(tbc ledger-check "$OUT" "$TB_GPU_HOURS_CEILING") || crc=$?
        [ "$crc" = 3 ] && { log "GPU-hour ceiling reached: ${used} h of ${TB_GPU_HOURS_CEILING} h; no new trial"; exit 0; }
        [ "$crc" = 0 ] || { log "ledger refused: $used"; exit 2; }
      fi
      wait_idle "$MODEL" || log "$NAME: slot still busy after 30 min"
      py 'import json,sys; a={"import_path":sys.argv[3],"model_name":sys.argv[4]}
if sys.argv[5]: a["kwargs"]={"version":sys.argv[5]}
json.dump({"jobs_dir":sys.argv[2],"datasets":[{"path":sys.argv[6],"task_names":[sys.argv[7]]}],"agents":[a]},open(sys.argv[1],"w"),indent=1)' \
        "$TJ.job.json" "$JOB" "$AGENT" "$LABEL" "$PV" "$DPATH" "$TASK"
      rc=0; cancelled=""; t0=$(date +%s)
      env "${TENV[@]}" NEWT_BENCH_PROFILE="$PROFILE" NEWT_BENCH_MODEL_DIGEST="${DIGEST:-}" harbor run --config "$TJ.job.json" \
        --job-name "${TASK}__a$K" --n-attempts 1 --n-concurrent 1 --max-retries 0 --agent-timeout-multiplier "$MULT" \
        --no-delete -y > "$TJ.harbor.log" 2>&1 &
      hp=$!
      while kill -0 "$hp" 2>/dev/null; do
        if [ -n "$DEADLINE" ] && [ "$(date +%s)" -ge "$DEADLINE" ]; then kill -TERM "$hp"; cancelled=1; break; fi
        sleep 10
      done
      wait "$hp" || rc=$?
      if [ -n "$cancelled" ]; then run_state="cancelled-at-deadline"; else run_state=$(tbc job-state "$TJ"); fi
      hours=$(tbc ledger-add "$OUT" "$TJ" campaign "$CAMPAIGN" model "$MODEL" cell "$NAME" task "$TASK" attempt "$K" \
        window "${WINDOW:-}" state "$run_state" wall_s "$(( $(date +%s) - t0 ))")
      log "$NAME ${TASK}__a$K ledger total ${hours} h"
      [ "$rc" = 0 ] || { failed=$((failed + 1)); log "$NAME ${TASK}__a$K harbor_exit=$rc"; }
      if [ -n "$cancelled" ] && [ "$(tbc job-state "$TJ")" != "done" ]; then
        if [ "$(tbc deadline-cancels "$JOB" "${TASK}__a$K")" -ge 1 ]; then
          : > "$TJ.deadline-exhausted"; log "$NAME ${TASK}__a$K cancelled at the hard deadline again: deadline exhausted (final)"
        else
          mkdir -p "$JOB/interrupted"; mv "$TJ" "$JOB/interrupted/$(basename "$TJ").$(date +%s).deadline"
          log "$NAME ${TASK}__a$K cancelled at the hard deadline; archived, re-runs next window"
        fi
        exit 0
      fi
    done
    missing=0
    for STEP in "${PLAN[@]}"; do
      read -r TASK K _ <<< "$STEP"
      state="$(tbc job-state "$JOB/${TASK}__a$K")"
      [ "$state" = "done" ] || missing=$((missing + 1))
    done
    if [ "$missing" -gt 0 ]; then  # not recorded: the next run resumes at the missing attempts
      log "$NAME incomplete: $missing trial(s) without a result; re-run to resume"; continue
    fi
    py 'import json,sys; c=json.load(open(sys.argv[1])); c.update(harbor_nonzero_exits=int(sys.argv[2]), finished_at=sys.argv[3], loaded_at_end=sys.argv[4]); json.dump(c,open(sys.argv[1],"w"))' \
      "$CELL" "$failed" "$(date -Is)" "$(loaded)"
    record "$CELL" "$JOB"
    log "$NAME done harbor_nonzero_exits=$failed"
  done
done
python3 "$HERE/tb_campaign.py" table "$OUT" | tee -a "$LOG"
