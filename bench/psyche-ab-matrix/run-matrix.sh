#!/usr/bin/env bash
# Psyche × OCAP matrix runner. Qualification mode is deliberately fail-closed:
# it collects every requested cell, renders the report, then validates the raw
# contract/provenance artifacts and exits nonzero if any promise was violated.
set -u
set -o pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"

fatal() {
  echo "FATAL: $*" >&2
  exit 2
}

# Portable Python: reject Windows Store shims that resolve but cannot run.
if [ -z "${PY:-}" ]; then
  for candidate in python3 python; do
    if command -v "$candidate" >/dev/null 2>&1 && "$candidate" -c 'import sys' >/dev/null 2>&1; then
      PY="$candidate"
      break
    fi
  done
fi
[ -n "${PY:-}" ] || fatal "need a working python3/python on PATH"
"$PY" -c 'import sys; raise SystemExit(sys.version_info < (3, 9))' || fatal "need Python 3.9 or newer"
command -v curl >/dev/null 2>&1 || fatal "need curl on PATH"
command -v timeout >/dev/null 2>&1 || fatal "need GNU timeout on PATH"
command -v git >/dev/null 2>&1 || fatal "need git on PATH"
command -v just >/dev/null 2>&1 || fatal "need just on PATH"

NEWT="${NEWT:-}"
if [ -z "$NEWT" ]; then
  for candidate in "$REPO/target/release/newt" "$REPO/target/release/newt.exe" \
                   "$REPO/target/debug/newt" "$REPO/target/debug/newt.exe" \
                   "$(command -v newt 2>/dev/null)"; do
    [ -n "$candidate" ] && [ -x "$candidate" ] && { NEWT="$candidate"; break; }
  done
fi
[ -x "$NEWT" ] || fatal "no executable newt binary (set NEWT=...)"
NEWT="$("$PY" -c 'import os,sys; print(os.path.realpath(sys.argv[1]))' "$NEWT")"

sha256_file() {
  "$PY" -c 'import hashlib,sys; h=hashlib.sha256(); f=open(sys.argv[1],"rb"); [h.update(b) for b in iter(lambda:f.read(1048576),b"")]; print(h.hexdigest())' "$1"
}
NEWT_SHA_BEFORE="$(sha256_file "$NEWT")" || fatal "could not hash $NEWT"
NEWT_VERSION="$($NEWT --version 2>&1)" || fatal "newt --version failed"
SOURCE_COMMIT="$(git -C "$REPO" rev-parse HEAD 2>/dev/null || echo unavailable)"
if [ -n "$(git -C "$REPO" status --porcelain --untracked-files=normal 2>/dev/null)" ]; then
  SOURCE_DIRTY=true
else
  SOURCE_DIRTY=false
fi
# Test hook (consistent with FAKE_EFFECTIVE_MODEL / FAKE_TIMEOUT_LOG): let tests
# drive the dirty-tree gate deterministically without mutating the real repo.
SOURCE_DIRTY="${FAKE_SOURCE_DIRTY:-$SOURCE_DIRTY}"

# Run identity and backend contract. Qualification intentionally has no fake
# defaults for facts that must come from the inference-server launch.
MODE="${MODE:-qualification}"                         # qualification | exploratory
MODEL_ENDPOINT="${MODEL_ENDPOINT:-${ORNITH_ENDPOINT:-}}"
MODEL_ENDPOINT="${MODEL_ENDPOINT%/}"
MODEL_ID="${MODEL_ID:-${ORNITH_MODEL:-nvidia/NVIDIA-Nemotron-3-Nano-30B-A3B-BF16}}"
MODEL_API="${MODEL_API:-chat_completions}"
MODEL_KEY_FILE="${MODEL_KEY_FILE:-}"
MODEL_DIGEST="${MODEL_DIGEST:-}"
CONTEXT_WINDOW="${CONTEXT_WINDOW:-}"
BACKEND_NAME="${BACKEND_NAME:-nemotron}"
CAPABILITY_PROFILE="${CAPABILITY_PROFILE:-nemotron}" # nemotron | none
SERVER_KIND="${SERVER_KIND:-}"                       # vllm | llama_cpp
SERVER_VERSION="${SERVER_VERSION:-}"
CHAT_TEMPLATE_ID="${CHAT_TEMPLATE_ID:-}"
TOOL_PARSER_ID="${TOOL_PARSER_ID:-}"
REASONING_PARSER_ID="${REASONING_PARSER_ID:-}"
SERVER_LAUNCH_MANIFEST="${SERVER_LAUNCH_MANIFEST:-}"

TASKS_DIR="${TASKS_DIR:-$HERE/tasks}"
TASKS_DIR="$("$PY" -c 'import os,sys; print(os.path.realpath(sys.argv[1]))' "$TASKS_DIR")"
[ -d "$TASKS_DIR" ] || fatal "TASKS_DIR is not a directory: $TASKS_DIR"
MAX_ROUNDS="${MAX_ROUNDS:-15}"
TASK_TIMEOUT="${TASK_TIMEOUT:-600}"
POSTURES="${POSTURES:-baseline tenacity crew obsessive}"
OCAP_MODES="${OCAP_MODES:-off on}"
STAMP="$(date -u +%Y%m%d-%H%M%S)"
if [ "$MODE" = "qualification" ]; then
  OUT="${OUT:-$HERE/qualification-runs/$STAMP}"
else
  OUT="${OUT:-$HERE/runs/$STAMP}"
fi
OUT="$("$PY" -c 'import os,sys; print(os.path.realpath(sys.argv[1]))' "$OUT")"

case "$MODE" in qualification|exploratory) ;; *) fatal "MODE must be qualification or exploratory" ;; esac
[ -n "$MODEL_ENDPOINT" ] || fatal "MODEL_ENDPOINT is required"
case "$CAPABILITY_PROFILE" in nemotron|none) ;; *) fatal "CAPABILITY_PROFILE must be nemotron or none" ;; esac
case "$MAX_ROUNDS" in ''|*[!0-9]*) fatal "MAX_ROUNDS must be a positive integer" ;; esac
case "$TASK_TIMEOUT" in ''|*[!0-9]*) fatal "TASK_TIMEOUT must be a positive integer" ;; esac
[ "$MAX_ROUNDS" -gt 0 ] || fatal "MAX_ROUNDS must be greater than zero"
[ "$TASK_TIMEOUT" -gt 0 ] || fatal "TASK_TIMEOUT must be greater than zero"
if [ -n "$CONTEXT_WINDOW" ]; then
  case "$CONTEXT_WINDOW" in *[!0-9]*|'') fatal "CONTEXT_WINDOW must be a positive integer" ;; esac
  [ "$CONTEXT_WINDOW" -gt 0 ] || fatal "CONTEXT_WINDOW must be greater than zero"
fi
if [ -n "$MODEL_DIGEST" ] \
  && [[ ! "$MODEL_DIGEST" =~ ^[0-9a-fA-F]{40}$ ]] \
  && [[ ! "$MODEL_DIGEST" =~ ^[0-9a-fA-F]{64}$ ]] \
  && [[ ! "$MODEL_DIGEST" =~ ^sha256:[0-9a-fA-F]{64}$ ]]; then
  fatal "MODEL_DIGEST must be 40 hex (immutable revision), 64 hex, or sha256:<64 hex>"
fi

if [ "$MODE" = "qualification" ]; then
  # A dirty source tree makes the bundle irreproducible: `commit` + the candidate
  # binary hash then identify something that cannot be reconstructed from git —
  # the hash is a museum label on an empty pedestal. Reject by default. An
  # explicit override still runs, but the diff + untracked sources are retained
  # in the bundle (below) so the run remains reproducible/inspectable.
  if [ "$SOURCE_DIRTY" = "true" ] && [ "${ALLOW_DIRTY_QUALIFICATION:-}" != "1" ]; then
    fatal "qualification requires a clean source tree (git status is dirty). Commit or stash changes, or set ALLOW_DIRTY_QUALIFICATION=1 to override (the diff + untracked listing are then retained in the bundle)."
  fi
  [ "$MODEL_API" = "chat_completions" ] || fatal "qualification requires MODEL_API=chat_completions"
  [ "$CAPABILITY_PROFILE" = "nemotron" ] || fatal "qualification requires CAPABILITY_PROFILE=nemotron"
  [ -n "$MODEL_DIGEST" ] || fatal "qualification requires MODEL_DIGEST"
  [ -n "$CONTEXT_WINDOW" ] || fatal "qualification requires CONTEXT_WINDOW"
  case "$SERVER_KIND" in vllm|llama_cpp) ;; *) fatal "qualification requires SERVER_KIND=vllm or llama_cpp" ;; esac
  [ -n "$SERVER_VERSION" ] || fatal "qualification requires SERVER_VERSION"
  [ -n "$CHAT_TEMPLATE_ID" ] || fatal "qualification requires CHAT_TEMPLATE_ID"
  [ -n "$TOOL_PARSER_ID" ] || fatal "qualification requires TOOL_PARSER_ID"
  [ -n "$REASONING_PARSER_ID" ] || fatal "qualification requires REASONING_PARSER_ID"
  [ -n "$SERVER_LAUNCH_MANIFEST" ] || fatal "qualification requires SERVER_LAUNCH_MANIFEST"
  [ -r "$SERVER_LAUNCH_MANIFEST" ] || fatal "SERVER_LAUNCH_MANIFEST is not readable"
  SERVER_LAUNCH_MANIFEST="$("$PY" -c 'import os,sys; print(os.path.realpath(sys.argv[1]))' "$SERVER_LAUNCH_MANIFEST")"
  if [ "$SERVER_KIND" = "vllm" ]; then
    [ "$TOOL_PARSER_ID" = "qwen3_coder" ] || fatal "vLLM reference lane requires TOOL_PARSER_ID=qwen3_coder"
    [ "$REASONING_PARSER_ID" = "nano_v3" ] || fatal "vLLM reference lane requires REASONING_PARSER_ID=nano_v3"
  fi
fi

read -r -a POSTURE_LIST <<< "$POSTURES"
read -r -a OCAP_LIST <<< "$OCAP_MODES"
[ "${#POSTURE_LIST[@]}" -gt 0 ] || fatal "POSTURES is empty"
[ "${#OCAP_LIST[@]}" -gt 0 ] || fatal "OCAP_MODES is empty"
declare -A SEEN_POSTURES=()
for posture in "${POSTURE_LIST[@]}"; do
  case "$posture" in baseline|tenacity|crew|obsessive) ;; *) fatal "unknown posture: $posture" ;; esac
  [ -z "${SEEN_POSTURES[$posture]+present}" ] || fatal "duplicate posture: $posture"
  SEEN_POSTURES[$posture]=1
done
declare -A SEEN_OCAP=()
for ocap in "${OCAP_LIST[@]}"; do
  case "$ocap" in off|on) ;; *) fatal "unknown OCAP mode: $ocap" ;; esac
  [ -z "${SEEN_OCAP[$ocap]+present}" ] || fatal "duplicate OCAP mode: $ocap"
  SEEN_OCAP[$ocap]=1
done
if [ "$MODE" = "qualification" ]; then
  if [ "${#POSTURE_LIST[@]}" -ne 4 ] \
    || [ -z "${SEEN_POSTURES[baseline]+present}" ] \
    || [ -z "${SEEN_POSTURES[tenacity]+present}" ] \
    || [ -z "${SEEN_POSTURES[crew]+present}" ] \
    || [ -z "${SEEN_POSTURES[obsessive]+present}" ]; then
    fatal "qualification requires the complete posture set: baseline tenacity crew obsessive"
  fi
  if [ "${#OCAP_LIST[@]}" -ne 2 ] \
    || [ -z "${SEEN_OCAP[off]+present}" ] \
    || [ -z "${SEEN_OCAP[on]+present}" ]; then
    fatal "qualification requires the complete OCAP set: off on"
  fi
fi

TASK_NAMES=()
TASK_PATHS=()
for task_path in "$TASKS_DIR"/*; do
  [ -e "$task_path" ] || [ -L "$task_path" ] || continue
  [ ! -L "$task_path" ] || fatal "symlinked task entry is not allowed: $task_path"
  [ -d "$task_path" ] || continue
  task_name="$(basename "$task_path")"
  case "$task_name" in *[!A-Za-z0-9_.-]*) fatal "task name is not CSV-safe: $task_name" ;; esac
  [ ! -L "$task_path/instruction.txt" ] || fatal "$task_name has a symlinked instruction.txt"
  [ ! -L "$task_path/verify.sh" ] || fatal "$task_name has a symlinked verify.sh"
  [ ! -L "$task_path/setup.sh" ] || fatal "$task_name has a symlinked setup.sh"
  [ -f "$task_path/instruction.txt" ] || fatal "$task_name lacks instruction.txt"
  [ -x "$task_path/verify.sh" ] || fatal "$task_name lacks an executable verify.sh"
  [ ! -e "$task_path/setup.sh" ] || [ -x "$task_path/setup.sh" ] \
    || fatal "$task_name has a non-executable setup.sh"
  TASK_NAMES+=("$task_name")
  TASK_PATHS+=("$task_path")
done
[ "${#TASK_NAMES[@]}" -gt 0 ] || fatal "no task directories found under $TASKS_DIR"

[ ! -e "$OUT" ] || [ -d "$OUT" ] || fatal "OUT exists and is not a directory: $OUT"
if [ -d "$OUT" ] && [ -n "$(ls -A "$OUT")" ]; then
  fatal "OUT must be empty so JSONL contract records cannot be inherited: $OUT"
fi
mkdir -p "$OUT"
mkdir -p "$OUT/events" "$OUT/traces" "$OUT/ws" "$OUT/preflight" \
  "$OUT/workspace-baselines"
# Reproducibility for a dirty override (only reachable when
# ALLOW_DIRTY_QUALIFICATION=1, or in exploratory mode): retain the exact uncommitted
# state so `commit` + binary hash are not the only record of what ran.
if [ "$SOURCE_DIRTY" = "true" ]; then
  git -C "$REPO" diff --binary HEAD > "$OUT/source-dirty.patch" 2>/dev/null || true
  git -C "$REPO" status --porcelain --untracked-files=all > "$OUT/source-dirty-status.txt" 2>/dev/null || true
fi
CSV="$OUT/results.csv"
MD="$OUT/matrix.md"
CFG="$OUT/config.toml"
SOURCE_SNAPSHOT="$OUT/harness-sources.json"
HARNESS_SHA_BEFORE="$("$PY" "$HERE/qualification_harness.py" snapshot-sources \
  --phase before --output "$SOURCE_SNAPSHOT" --harness-root "$HERE" --tasks-root "$TASKS_DIR")" \
  || fatal "could not hash harness sources before the run"

LAUNCH_MANIFEST_SHA=""
if [ "$MODE" = "qualification" ]; then
  LAUNCH_MANIFEST_SHA="$("$PY" "$HERE/qualification_harness.py" copy-launch-manifest \
    --source "$SERVER_LAUNCH_MANIFEST" --destination "$OUT/server-launch-manifest.json" \
    --model-id "$MODEL_ID" --model-digest "$MODEL_DIGEST" \
    --context-window "$CONTEXT_WINDOW" --server-kind "$SERVER_KIND" \
    --server-version "$SERVER_VERSION" --chat-template-id "$CHAT_TEMPLATE_ID" \
    --tool-parser-id "$TOOL_PARSER_ID" --reasoning-parser-id "$REASONING_PARSER_ID")" \
    || fatal "SERVER_LAUNCH_MANIFEST does not match the declared qualification lane"
fi

# One rendering operation is used for the live backend and loopback preflight,
# preventing the capability declaration under test from drifting between them.
render_config() {
  local destination="$1"
  local endpoint="$2"
  local key_file="$MODEL_KEY_FILE"
  [ "$#" -lt 3 ] || key_file="$3"
  "$PY" - "$destination" "$BACKEND_NAME" "$endpoint" "$MODEL_ID" "$MODEL_API" \
    "$key_file" "$CAPABILITY_PROFILE" <<'PY'
import json, sys
dest, name, endpoint, model, api, key_file, profile = sys.argv[1:]
lines = [
    f"default_backend = {json.dumps(name)}",
    "[[backends]]",
    f"name = {json.dumps(name)}",
    f"endpoint = {json.dumps(endpoint)}",
    f"model = {json.dumps(model)}",
    'kind = "openai"',
    f"api = {json.dumps(api)}",
]
if key_file:
    lines.append(f"api_key_file = {json.dumps(key_file)}")
lines.append('tiers = ["FAST", "STANDARD", "COMPLEX", "REVIEW"]')
if profile == "nemotron":
    lines.extend([
        "[backends.capability]",
        'reasoning_replay_scope = "current_user_turn"',
        "[backends.capability.chat_completions]",
        "cognition = true",
        "chat_template_kwargs = true",
        "parallel_tool_calls = false",
        "bounded_reasoning_continuation = true",
    ])
open(dest, "w", encoding="utf-8").write("\n".join(lines) + "\n")
PY
}
render_config "$CFG" "$MODEL_ENDPOINT" || fatal "could not render backend config"

# Manifest declares the exact cells before inference starts. The validator uses
# this immutable expectation rather than trusting whatever rows the loop wrote.
POSTURES_JSON="$("$PY" -c 'import json,sys; print(json.dumps(sys.argv[1:]))' "${POSTURE_LIST[@]}")"
OCAPS_JSON="$("$PY" -c 'import json,sys; print(json.dumps(sys.argv[1:]))' "${OCAP_LIST[@]}")"
TASKS_JSON="$("$PY" -c 'import json,sys; print(json.dumps(sys.argv[1:]))' "${TASK_NAMES[@]}")"
if ! "$PY" - "$OUT/manifest.json" "$MODE" "$MODEL_ID" "$MODEL_DIGEST" "$CONTEXT_WINDOW" \
  "$BACKEND_NAME" "$MODEL_API" "$CAPABILITY_PROFILE" "$MODEL_ENDPOINT" "$MAX_ROUNDS" \
  "$POSTURES_JSON" "$OCAPS_JSON" "$TASKS_JSON" "$TASKS_DIR" "$SERVER_KIND" \
  "$SERVER_VERSION" "$CHAT_TEMPLATE_ID" "$TOOL_PARSER_ID" "$REASONING_PARSER_ID" \
  "$LAUNCH_MANIFEST_SHA" <<'PY'
import json, os, sys
(path, mode, model, digest, context, backend, api, profile, endpoint, rounds,
 postures, ocaps, tasks, tasks_dir, server_kind, server_version, chat_template,
 tool_parser, reasoning_parser, launch_sha) = sys.argv[1:]
task_list = json.loads(tasks)
# Pin, at RUN time, which tasks carry a setup.sh. The validator reads this fact
# instead of stat()-ing the live task tree — so a bundle validates identically
# after the checkout moves or a task gains/loses a setup.sh later (hermeticity).
tasks_with_setup = [
    t for t in task_list if os.path.isfile(os.path.join(tasks_dir, t, "setup.sh"))
]
value = {
    "schema_version": 1,
    "mode": mode,
    "model": {
        "requested_id": model,
        "digest": digest or None,
        "context_window": int(context) if context else None,
    },
    "backend": {"name": backend, "api": api, "capability_profile": profile},
    "server": {
        "endpoint": endpoint,
        "kind": server_kind or None,
        "version": server_version or None,
        "chat_template_id": chat_template or None,
        "tool_parser_id": tool_parser or None,
        "reasoning_parser_id": reasoning_parser or None,
        "launch_manifest_sha256": launch_sha or None,
    },
    "matrix": {
        "postures": json.loads(postures),
        "ocap_modes": json.loads(ocaps),
        "tasks": task_list,
        "tasks_dir": tasks_dir,
        "tasks_with_setup": tasks_with_setup,
        "max_rounds": int(rounds),
    },
}
open(path, "w", encoding="utf-8").write(json.dumps(value, indent=2) + "\n")
PY
then
  fatal "could not write manifest.json"
fi

MODEL_TOKEN=""
if [ -n "$MODEL_KEY_FILE" ]; then
  [ -r "$MODEL_KEY_FILE" ] || fatal "MODEL_KEY_FILE is not readable: $MODEL_KEY_FILE"
  IFS= read -r MODEL_TOKEN < "$MODEL_KEY_FILE"
  [ -n "$MODEL_TOKEN" ] || fatal "MODEL_KEY_FILE is empty"
fi

# Feed the authorization header over stdin. The secret never enters curl's
# argv and is unset immediately after the identity probes complete.
curl_probe() {
  if [ -n "$MODEL_TOKEN" ]; then
    printf 'Authorization: Bearer %s\n' "$MODEL_TOKEN" | curl -H @- "$@"
  else
    curl "$@"
  fi
}

# Retain raw inference-server identity responses, including /v1/models. A
# qualification run cannot proceed when its kind-specific identity probe fails.
curl_probe -fsS --max-time 20 "$MODEL_ENDPOINT/v1/models" \
  -o "$OUT/server-models.json" || fatal "$MODEL_ENDPOINT/v1/models is unreachable"
PROBE_OK=false
case "$SERVER_KIND" in
  vllm)
    if curl_probe -fsS --max-time 20 "$MODEL_ENDPOINT/version" -o "$OUT/server-version.json"; then
      PROBE_OK=true
    fi
    ;;
  llama_cpp)
    # llama.cpp router mode needs the requested model selector; retaining an
    # unqualified /props response could otherwise prove a different loaded
    # model's context/template identity.
    if curl_probe -fsS --max-time 20 --get "$MODEL_ENDPOINT/props" \
      --data-urlencode "model=$MODEL_ID" -o "$OUT/server-props.json"; then
      PROBE_OK=true
    fi
    ;;
  *) PROBE_OK=true ;; # exploratory unknown server: /v1/models above is retained
esac
unset MODEL_TOKEN
[ "$MODE" != "qualification" ] || [ "$PROBE_OK" = true ] || fatal "$SERVER_KIND identity probe failed"
if [ "$MODE" = "qualification" ]; then
  probe_args=("$PY" "$HERE/qualification_harness.py" validate-probes
    --models "$OUT/server-models.json" --model-id "$MODEL_ID"
    --server-kind "$SERVER_KIND" --server-version "$SERVER_VERSION"
    --context-window "$CONTEXT_WINDOW")
  if [ "$SERVER_KIND" = "vllm" ]; then
    probe_args+=(--version-file "$OUT/server-version.json")
  else
    probe_args+=(--props-file "$OUT/server-props.json" --chat-template-id "$CHAT_TEMPLATE_ID")
  fi
  "${probe_args[@]}" || fatal "inference-server identity evidence did not validate"
fi

if ! "$PY" - "$OUT/provenance.json" "$MODEL_ID" "$MODEL_DIGEST" "$CONTEXT_WINDOW" \
  "$MODEL_ENDPOINT" "$SERVER_KIND" "$SERVER_VERSION" "$CHAT_TEMPLATE_ID" \
  "$TOOL_PARSER_ID" "$REASONING_PARSER_ID" "$PROBE_OK" "$NEWT" "$NEWT_VERSION" \
  "$NEWT_SHA_BEFORE" "$SOURCE_COMMIT" "$SOURCE_DIRTY" "$LAUNCH_MANIFEST_SHA" \
  "$HARNESS_SHA_BEFORE" <<'PY'
import datetime, json, sys
(path, model, digest, context, endpoint, kind, version, template, tool_parser,
 reasoning_parser, probe_ok, newt_path, newt_version, newt_sha, source_commit,
 source_dirty, launch_sha, harness_sha) = sys.argv[1:]
value = {
    "schema_version": 1,
    "captured_at_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
    "model": {"id": model, "digest": digest or None, "context_window": int(context) if context else None},
    "server": {
        "endpoint": endpoint,
        "kind": kind or "unknown",
        "version": version or "unknown",
        "chat_template_id": template or "unknown",
        "tool_parser_id": tool_parser or "unknown",
        "reasoning_parser_id": reasoning_parser or "unknown",
        "probe_ok": probe_ok == "true",
        "identity_source": "operator declaration plus retained endpoint probes",
        "launch_manifest": {
            "artifact": "server-launch-manifest.json",
            "sha256": launch_sha,
        } if launch_sha else None,
    },
    "newt": {
        "path": newt_path,
        "version": newt_version,
        "sha256_before": newt_sha,
        "sha256_after": None,
    },
    "harness": {
        "commit": source_commit,
        "dirty": source_dirty == "true",
        "source_manifest": "harness-sources.json",
        "source_sha256_before": harness_sha,
        "source_sha256_after": None,
    },
}
open(path, "w", encoding="utf-8").write(json.dumps(value, indent=2) + "\n")
PY
then
  fatal "could not write provenance.json"
fi

# Real loopback preflight: drive this exact Newt binary through solve with the
# same rendered capability profile, capture the initial and bounded-continuation
# requests, and assert every wire field before spending server time on the matrix.
if [ "$MODE" = "qualification" ]; then
  PREFLIGHT_CFG="$OUT/preflight/config.toml"
  PREFLIGHT_PORT_FILE="$OUT/preflight/port"
  PREFLIGHT_REQUEST="$OUT/preflight/first-request.json"
  PREFLIGHT_SECOND_REQUEST="$OUT/preflight/second-request.json"
  "$PY" "$HERE/loopback-preflight.py" serve --port-file "$PREFLIGHT_PORT_FILE" \
    --request-file "$PREFLIGHT_REQUEST" --second-request-file "$PREFLIGHT_SECOND_REQUEST" \
    --model "$MODEL_ID" --timeout-seconds 65 \
    >"$OUT/preflight/server.trace" 2>&1 &
  PREFLIGHT_SERVER_PID=$!
  cleanup_preflight() {
    if [ -n "${PREFLIGHT_SERVER_PID:-}" ] && kill -0 "$PREFLIGHT_SERVER_PID" 2>/dev/null; then
      kill "$PREFLIGHT_SERVER_PID" 2>/dev/null || true
      wait "$PREFLIGHT_SERVER_PID" 2>/dev/null || true
    fi
  }
  trap cleanup_preflight EXIT
  trap 'cleanup_preflight; exit 130' INT TERM
  for _ in $(seq 1 100); do
    [ -s "$PREFLIGHT_PORT_FILE" ] && break
    kill -0 "$PREFLIGHT_SERVER_PID" 2>/dev/null || fatal "loopback preflight server exited early"
    sleep 0.02
  done
  [ -s "$PREFLIGHT_PORT_FILE" ] || fatal "loopback preflight server did not become ready"
  PREFLIGHT_PORT="$(tr -d '[:space:]' < "$PREFLIGHT_PORT_FILE")"
  # The capability profile is identical; the live credential path is omitted
  # because the loopback endpoint neither needs nor receives operator secrets.
  render_config "$PREFLIGHT_CFG" "http://127.0.0.1:$PREFLIGHT_PORT" "" \
    || fatal "could not render preflight config"
  printf '%s\n' 'Answer briefly that the qualification preflight is complete. Do not call a tool.' \
    > "$OUT/preflight/instruction.txt"
  mkdir -p "$OUT/preflight/ws"
  preflight_cmd=("$NEWT" --cognition contemplating --config "$PREFLIGHT_CFG" solve
    --instruction-file "$OUT/preflight/instruction.txt" --cwd "$OUT/preflight/ws"
    --events "$OUT/preflight/events.jsonl" --non-interactive true --max-rounds 2 --plain)
  preflight_cmd+=(--model-digest "$MODEL_DIGEST" --context-window "$CONTEXT_WINDOW")
  if ! env -u NEWT_TEAM NEWT_NO_MODEL_PULL=1 \
      timeout --kill-after=5s 60 "${preflight_cmd[@]}" \
      > "$OUT/preflight/newt.trace" 2>&1; then
    cleanup_preflight
    fatal "Newt loopback preflight solve failed (see $OUT/preflight/newt.trace)"
  fi
  if [ ! -s "$PREFLIGHT_REQUEST" ] || [ ! -s "$PREFLIGHT_SECOND_REQUEST" ] \
    || [ -L "$PREFLIGHT_REQUEST" ] || [ -L "$PREFLIGHT_SECOND_REQUEST" ]; then
    cleanup_preflight
    PREFLIGHT_SERVER_PID=""
    fatal "loopback preflight did not capture exactly two requests"
  fi
  wait "$PREFLIGHT_SERVER_PID" || fatal "loopback preflight server failed"
  PREFLIGHT_SERVER_PID=""
  "$PY" "$HERE/loopback-preflight.py" assert-request --request-file "$PREFLIGHT_REQUEST" \
    --second-request-file "$PREFLIGHT_SECOND_REQUEST" --model "$MODEL_ID" \
    || fatal "Nemotron two-request preflight assertion failed"
fi

prepare_workspace() {
  local workspace="$1"
  local verifier="$2"
  local proof_file="$3"
  local cell="$4"
  local posture="$5"
  local ocap="$6"
  local task="$7"
  local justfile justfile_name summary git_root dry_run generated
  local baseline_commit baseline_tree git_blob justfile_sha verifier_sha dry_run_sha

  generated=false
  if [ -L "$workspace/justfile" ] || [ -L "$workspace/Justfile" ]; then
    echo "workspace contains a symlinked justfile" >&2
    return 1
  elif [ -e "$workspace/justfile" ]; then
    justfile="$workspace/justfile"
  elif [ -e "$workspace/Justfile" ]; then
    justfile="$workspace/Justfile"
  else
    justfile="$workspace/Justfile"
    generated=true
    "$PY" - "$justfile" "$verifier" <<'PY'
import shlex
import sys
from pathlib import Path

Path(sys.argv[1]).write_text(
    "check:\n\t@" + shlex.quote(sys.argv[2]) + "\n", encoding="utf-8"
)
PY
  fi
  justfile_name="$(basename "$justfile")"
  summary="$(cd "$workspace" && just --summary)" || return 1
  case " $summary " in
    *" check "*) ;;
    *) echo "$justfile_name does not define a check recipe" >&2; return 1 ;;
  esac
  dry_run="$(cd "$workspace" && just --dry-run check)" || return 1

  git -C "$workspace" init -q || return 1
  git -C "$workspace" config user.name "Newt Qualification Harness" || return 1
  git -C "$workspace" config user.email "qualification@newt.invalid" || return 1
  git -C "$workspace" add -A || return 1
  git -C "$workspace" add -f -- "$justfile_name" || return 1
  git -C "$workspace" -c core.hooksPath=/dev/null -c commit.gpgsign=false \
    commit -q --allow-empty -m "qualification task baseline" || return 1
  git -C "$workspace" cat-file -e "HEAD:$justfile_name" || return 1
  git_root="$(git -C "$workspace" rev-parse --show-toplevel)" || return 1
  [ "$git_root" = "$workspace" ] || {
    echo "workspace Git root is $git_root, expected $workspace" >&2
    return 1
  }
  [ -z "$(git -C "$workspace" status --porcelain --untracked-files=all)" ] || {
    echo "workspace baseline is not clean after its commit" >&2
    return 1
  }
  baseline_commit="$(git -C "$workspace" rev-parse HEAD)" || return 1
  baseline_tree="$(git -C "$workspace" rev-parse 'HEAD^{tree}')" || return 1
  git_blob="$(git -C "$workspace" rev-parse "HEAD:$justfile_name")" || return 1
  justfile_sha="$(sha256_file "$justfile")" || return 1
  verifier_sha="$(sha256_file "$verifier")" || return 1
  dry_run_sha="$(printf '%s' "$dry_run" | "$PY" -c \
    'import hashlib,sys; print(hashlib.sha256(sys.stdin.buffer.read()).hexdigest())')" \
    || return 1
  "$PY" - "$proof_file" "$cell" "$posture" "$ocap" "$task" "$workspace" \
    "$git_root" "$baseline_commit" "$baseline_tree" "$justfile_name" \
    "$justfile_sha" "$git_blob" "$summary" "$dry_run_sha" "$generated" \
    "$verifier" "$verifier_sha" <<'PY'
import json
import sys
from pathlib import Path

(path, cell, posture, ocap, task, workspace, git_root, commit, tree,
 justfile, justfile_sha, git_blob, summary, dry_run_sha, generated,
 verifier, verifier_sha) = sys.argv[1:]
value = {
    "schema_version": 1,
    "cell": cell,
    "posture": posture,
    "ocap": ocap,
    "task": task,
    "workspace": workspace,
    "git": {
        "root": git_root,
        "baseline_commit": commit,
        "baseline_tree": tree,
        "clean": True,
    },
    "just": {
        "file": justfile,
        "sha256": justfile_sha,
        "git_blob": git_blob,
        "recipes": summary.split(),
        "dry_run_sha256": dry_run_sha,
        "generated": generated == "true",
    },
    "verifier": {"path": verifier, "sha256": verifier_sha},
}
destination = Path(path)
temporary = destination.with_suffix(destination.suffix + ".tmp")
temporary.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")
temporary.replace(destination)
PY
}

echo "posture,ocap,task,verify,status,tool_calls,write_calls,total_tokens,wall_secs,solve_rc,events_file" > "$CSV" \
  || fatal "could not create results.csv"
echo "== Psyche × OCAP matrix =="
echo "mode:       $MODE"
echo "newt:      $NEWT ($NEWT_SHA_BEFORE)"
echo "endpoint:  $MODEL_ENDPOINT"
echo "model:     $MODEL_ID"
echo "server:    ${SERVER_KIND:-unknown} ${SERVER_VERSION:-unknown}"
echo "postures:  ${POSTURE_LIST[*]}"
echo "ocap:      ${OCAP_LIST[*]}"
echo "tasks:     ${TASK_NAMES[*]}"
echo "out:       $OUT"
echo

for posture in "${POSTURE_LIST[@]}"; do
  for ocap in "${OCAP_LIST[@]}"; do
    for task_index in "${!TASK_NAMES[@]}"; do
      task="${TASK_NAMES[$task_index]}"
      task_path="${TASK_PATHS[$task_index]}"
      cell="$posture-$ocap-$task"
      ws="$OUT/ws/$cell"
      events="$OUT/events/$cell.jsonl"
      trace="$OUT/traces/$cell.trace"
      mkdir -p "$ws"

      setup_rc=0
      setup_trace="$OUT/traces/$cell.setup.trace"
      : > "$setup_trace"
      if [ -x "$task_path/setup.sh" ]; then
        (cd "$ws" && timeout --kill-after=5s "$TASK_TIMEOUT" "$task_path/setup.sh") \
          >> "$setup_trace" 2>&1 || setup_rc=$?
      fi
      if [ "$setup_rc" -eq 0 ]; then
        prepare_workspace "$ws" "$task_path/verify.sh" \
          "$OUT/workspace-baselines/$cell.json" "$cell" "$posture" "$ocap" "$task" \
          >> "$setup_trace" 2>&1 || setup_rc=$?
      fi
      rc=$setup_rc
      if [ "$setup_rc" -eq 0 ]; then
        cmd=("$NEWT")
        # NEWT_TEAM is a presence gate (`0` still means ON). Start every cell
        # from an explicit absence, then enable only the two crew postures.
        env_args=(-u NEWT_TEAM -u NEWT_BENCH_OCAP NEWT_NO_MODEL_PULL=1)
        case "$posture" in
          tenacity) cmd+=(--tenacity relentless) ;;
          crew) env_args+=(NEWT_TEAM=1) ;;
          obsessive) cmd+=(--obsessive) ;;
        esac
        cmd+=(--config "$CFG" solve --instruction-file "$task_path/instruction.txt" --cwd "$ws"
          --events "$events" --max-rounds "$MAX_ROUNDS" --plain)
        if [ "$ocap" = "on" ]; then
          cmd+=(--confined)
          env_args+=(NEWT_BENCH_OCAP=on)
        else
          cmd+=(--non-interactive true)
        fi
        [ -z "$MODEL_DIGEST" ] || cmd+=(--model-digest "$MODEL_DIGEST")
        [ -z "$CONTEXT_WINDOW" ] || cmd+=(--context-window "$CONTEXT_WINDOW")
        env "${env_args[@]}" timeout --kill-after=5s "$TASK_TIMEOUT" \
          "${cmd[@]}" > "$trace" 2>&1
        rc=$?
      else
        printf 'task setup failed with rc=%s\n' "$setup_rc" > "$trace"
        : > "$events"
      fi

      read -r status tool_calls write_calls total_tokens wall_secs < <(
        "$PY" - "$events" <<'PY'
import json, sys
record = {}
try:
    lines = open(sys.argv[1], encoding="utf-8", errors="replace")
except OSError:
    lines = []
for line in lines:
    try:
        candidate = json.loads(line)
    except Exception:
        continue
    if candidate.get("kind") == "solve_result":
        record = candidate
print(record.get("status", "?"), record.get("tool_calls", 0),
      record.get("write_calls", 0), record.get("usage_total_tokens") or 0,
      round(float(record.get("wall_secs") or 0), 1))
PY
      )
      if (cd "$ws" \
          && timeout --kill-after=5s "$TASK_TIMEOUT" just check \
          && timeout --kill-after=5s "$TASK_TIMEOUT" "$task_path/verify.sh") \
          >/dev/null 2>&1; then
        verify=pass
      else
        verify=fail
      fi
      echo "$posture,$ocap,$task,$verify,${status:-?},${tool_calls:-0},${write_calls:-0},${total_tokens:-0},${wall_secs:-0},$rc,events/$cell.jsonl" >> "$CSV"
      printf "  %-9s ocap=%-3s %-14s -> %-4s (status=%s rc=%s tools=%s writes=%s tok=%s %ss)\n" \
        "$posture" "$ocap" "$task" "$verify" "${status:-?}" "$rc" "${tool_calls:-0}" \
        "${write_calls:-0}" "${total_tokens:-0}" "${wall_secs:-0}"
    done
  done
done

# Pin the candidate at both ends of the run. The final gate rejects a changed
# binary or harness source even if all cells happened to pass.
NEWT_SHA_AFTER="$(sha256_file "$NEWT")" || NEWT_SHA_AFTER=unreadable
HARNESS_SHA_AFTER="$("$PY" "$HERE/qualification_harness.py" snapshot-sources \
  --phase after --output "$SOURCE_SNAPSHOT" --harness-root "$HERE" --tasks-root "$TASKS_DIR")"
HARNESS_HASH_RC=$?
[ -n "$HARNESS_SHA_AFTER" ] || HARNESS_SHA_AFTER=unreadable
"$PY" - "$OUT/provenance.json" "$NEWT_SHA_AFTER" "$HARNESS_SHA_AFTER" <<'PY'
import json, sys
path, digest, harness_digest = sys.argv[1:]
value = json.load(open(path, encoding="utf-8"))
value["newt"]["sha256_after"] = digest
value["harness"]["source_sha256_after"] = harness_digest
open(path, "w", encoding="utf-8").write(json.dumps(value, indent=2) + "\n")
PY
PROVENANCE_UPDATE_RC=$?

# Render the complete report before evaluating the gate. Failed cells remain
# visible and diagnosable instead of aborting the matrix at the first error.
"$PY" - "$CSV" "$MD" "$OUT/manifest.json" "$OUT/provenance.json" <<'PY'
import collections, csv, json, sys
csv_path, md_path, manifest_path, provenance_path = sys.argv[1:]
rows = list(csv.DictReader(open(csv_path, encoding="utf-8")))
manifest = json.load(open(manifest_path, encoding="utf-8"))
provenance = json.load(open(provenance_path, encoding="utf-8"))
postures = manifest["matrix"]["postures"]
ocaps = manifest["matrix"]["ocap_modes"]
cell = collections.defaultdict(list)
for row in rows:
    cell[(row["posture"], row["ocap"])].append(row)
def summary(group):
    if not group:
        return "MISSING"
    passed = sum(row["verify"] == "pass" and row["solve_rc"] == "0" for row in group)
    tokens = sum(int(row["total_tokens"] or 0) for row in group) // len(group)
    wall = sum(float(row["wall_secs"] or 0) for row in group) / len(group)
    return f"{passed}/{len(group)} valid · {tokens} tok · {wall:.0f}s"
model = manifest["model"]
server = provenance["server"]
newt = provenance["newt"]
with open(md_path, "w", encoding="utf-8") as out:
    out.write(f"# Psyche × OCAP {manifest['mode']} matrix — {model['requested_id']}\n\n")
    out.write("## Immutable provenance\n\n")
    out.write(f"- Model digest: `{model.get('digest')}`\n")
    out.write(f"- Context window: `{model.get('context_window')}`\n")
    out.write(f"- Server: `{server['kind']} {server['version']}`\n")
    out.write(f"- Chat template: `{server['chat_template_id']}`\n")
    out.write(f"- Tool / reasoning parser: `{server['tool_parser_id']}` / `{server['reasoning_parser_id']}`\n")
    if server.get("launch_manifest"):
        out.write(f"- Server launch manifest sha256: `{server['launch_manifest']['sha256']}`\n")
    out.write(f"- Newt: `{newt['version']}` · sha256 `{newt['sha256_before']}`\n")
    out.write(f"- Harness source: `{provenance['harness']['commit']}` · dirty `{provenance['harness']['dirty']}`\n")
    out.write(f"- Harness files sha256: `{provenance['harness']['source_sha256_before']}`\n")
    identity = ["[`provenance.json`](provenance.json)", "[`manifest.json`](manifest.json)"]
    if manifest["mode"] == "qualification":
        identity.append("[`server launch`](server-launch-manifest.json)")
    identity.extend(["[`harness sources`](harness-sources.json)", "[`config.toml`](config.toml)"])
    out.write("- Raw identity: " + ", ".join(identity) + "\n\n")
    out.write("## Matrix\n\n")
    out.write("| posture \\ OCAP | " + " | ".join(ocaps) + " |\n")
    out.write("|---|" + "|".join(["---"] * len(ocaps)) + "|\n")
    for posture in postures:
        out.write(f"| **{posture}** | " + " | ".join(summary(cell[(posture, ocap)]) for ocap in ocaps) + " |\n")
    out.write("\n## Per-cell task detail\n\n")
    out.write("| posture | ocap | task | verify | status | solve rc | tools | writes | tokens | wall | events |\n")
    out.write("|---|---|---|---|---|---:|---:|---:|---:|---:|---|\n")
    for row in rows:
        out.write("| " + " | ".join([
            row["posture"], row["ocap"], row["task"], row["verify"], row["status"],
            row["solve_rc"], row["tool_calls"], row["write_calls"], row["total_tokens"],
            row["wall_secs"], f"[`jsonl`]({row['events_file']})",
        ]) + " |\n")
PY
REPORT_RC=$?

VALIDATION_LOG="$OUT/validation.txt"
"$PY" "$HERE/validate-run.py" --expected-mode "$MODE" "$OUT" > "$VALIDATION_LOG" 2>&1
VALIDATION_RC=$?
if [ "$PROVENANCE_UPDATE_RC" -ne 0 ]; then
  echo "harness error: could not record the final Newt binary digest" >> "$VALIDATION_LOG"
  VALIDATION_RC=1
fi
if [ "$HARNESS_HASH_RC" -ne 0 ]; then
  echo "harness error: harness source files changed while the matrix was running" >> "$VALIDATION_LOG"
  VALIDATION_RC=1
fi
if [ "$REPORT_RC" -ne 0 ]; then
  echo "harness error: could not render matrix.md" >> "$VALIDATION_LOG"
  VALIDATION_RC=1
fi
cat "$VALIDATION_LOG"
{
  echo
  echo "## Validation gate"
  echo
  if [ "$VALIDATION_RC" -eq 0 ]; then
    echo "**PASS** — every declared cell, contract, model identity, and required provenance artifact validated."
  else
    echo "**FAIL** — see [\`validation.txt\`](validation.txt). This run is not qualification evidence."
  fi
} >> "$MD" || VALIDATION_RC=1

echo
echo "matrix:      $MD"
echo "csv:         $CSV"
echo "provenance:  $OUT/provenance.json"
echo "raw events:  $OUT/events/"
exit "$VALIDATION_RC"
