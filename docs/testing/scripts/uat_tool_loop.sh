#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# Practical live regression suite (hard-failing UAT).
# See docs/testing/uat-tool-loop.md and docs/testing/bat-uat.md.
#
# Drives the REAL newt harness with user-phrased coding prompts on disposable
# workspaces against a real LLM, and asserts BOTH:
#   (a) OUTCOME  — workspace ended in the right state
#   (b) BEHAVIOR — tool-loop / evidence-path signals from the session log
#
# Status per scenario: PASS | FAIL | INFRA
# Aggregate exit:
#   0 — every selected scenario PASS
#   1 — at least one FAIL (behavioral / harness regression)
#   2 — INFRA or hard error (binary missing, endpoint down, bad args)
#
# Usage:
#   uat_tool_loop.sh [--suite smoke|full] [--case NAME] [--provider NAME]
#                    [--model NAME] [--config PATH] [--out DIR] [--self-test]
#   uat_tool_loop.sh [HOST] [MODEL]   # legacy: sets NEWT_DGX_OLLAMA_URL/MODEL
#
# Env:
#   NEWT_BIN      release newt binary (default: $CARGO_TARGET_DIR/release/newt)
#   UAT_WORKDIR   disposable root (default: /tmp/newt-uat-tool-loop)
#   NEWT_PROVIDER named [[backends]] entry from --config / ~/.newt
#   NEWT_DGX_MODEL / NEWT_DEFAULT_MODEL  model override
# ─────────────────────────────────────────────────────────────────────────────
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../../.." && pwd)"

SUITE="full"
CASE=""
PROVIDER="${NEWT_PROVIDER:-}"
MODEL="${NEWT_DGX_MODEL:-${NEWT_DEFAULT_MODEL:-}}"
CONFIG=""
OUT=""
SELF_TEST=false
LEGACY_HOST=""

usage() {
  sed -n '2,28p' "${BASH_SOURCE[0]}"
  exit 0
}

while [ $# -gt 0 ]; do
  case "$1" in
    --suite) SUITE="$2"; shift 2;;
    --case) CASE="$2"; shift 2;;
    --provider) PROVIDER="$2"; shift 2;;
    --model) MODEL="$2"; shift 2;;
    --config) CONFIG="$2"; shift 2;;
    --out) OUT="$2"; shift 2;;
    --self-test) SELF_TEST=true; shift;;
    -h|--help) usage;;
    --*) echo "uat_tool_loop: unknown flag '$1'" >&2; exit 2;;
    *)
      # Legacy positional: HOST [MODEL]
      if [ -z "$LEGACY_HOST" ]; then
        LEGACY_HOST="$1"
      elif [ -z "$MODEL" ] || [ "$MODEL" = "${NEWT_DGX_MODEL:-${NEWT_DEFAULT_MODEL:-}}" ]; then
        MODEL="$1"
      else
        echo "uat_tool_loop: unexpected positional '$1'" >&2
        exit 2
      fi
      shift
      ;;
  esac
done

case "$SUITE" in
  smoke|full) ;;
  *) echo "uat_tool_loop: --suite must be smoke|full (got '$SUITE')" >&2; exit 2;;
esac

NEWT_BIN="${NEWT_BIN:-${CARGO_TARGET_DIR:-$REPO_ROOT/target}/release/newt}"
R="${UAT_WORKDIR:-/tmp/newt-uat-tool-loop}"
mkdir -p "$R"
if [ -z "$OUT" ]; then
  OUT="$R/out-$(date -u +%Y%m%dT%H%M%SZ)"
fi
mkdir -p "$OUT"

TSV="$OUT/results.tsv"
SUMMARY="$OUT/summary.md"
: > "$TSV"

# Scenario lists. smoke ⊂ full.
SMOKE_CASES=(line-count rename bugfix)
FULL_CASES=(line-count rename bugfix branch refactor deadend duploop capexit)

selected_cases() {
  if [ -n "$CASE" ]; then
    printf '%s\n' "$CASE"
    return
  fi
  if [ "$SUITE" = "smoke" ]; then
    printf '%s\n' "${SMOKE_CASES[@]}"
  else
    printf '%s\n' "${FULL_CASES[@]}"
  fi
}

sig() { # $1=ws $2=regex → count
  grep -ciE "$2" "$1/session.log" 2>/dev/null || echo 0
}

log_has() { # $1=ws $2=regex → 0 if match
  grep -qiE "$2" "$1/session.log" 2>/dev/null
}

# Classify a session as INFRA when the harness never reached a usable model.
is_infra_session() {
  local ws=$1
  [ -f "$ws/session.log" ] || return 0
  if log_has "$ws" 'empty response|model returned an empty response'; then
    # empty response after tool work is FAIL (harness/model regression), not INFRA
    if log_has "$ws" '⚙|tool_calls=|dispatching tool'; then
      return 1
    fi
  fi
  if log_has "$ws" \
    'connection refused|timed out|timeout|unreachable|HTTP 4[0-9]{2}|HTTP 5[0-9]{2}|error sending request|no such host|Could not connect|failed to connect|backend.*(failed|unavailable)|newt doctor'
  then
    return 0
  fi
  if ! log_has "$ws" 'ready —|tool_calls=|⚙|round [0-9]'; then
    # No evidence the model was reached at all.
    return 0
  fi
  return 1
}

record() { # $1=case $2=PASS|FAIL|INFRA $3=details
  local c=$1 st=$2 det=$3
  # TSV: case<TAB>status<TAB>details (details may contain spaces; no tabs)
  printf '%s\t%s\t%s\n' "$c" "$st" "$(printf '%s' "$det" | tr '\t\n' '  ')" >> "$TSV"
  printf '[%s] %s — %s\n' "$st" "$c" "$det"
}

seed_case() {
  local c=$1
  local ws="$R/$c"
  rm -rf "$ws"
  mkdir -p "$ws"
  case "$c" in
    line-count)
      # Byte order ≠ line order: fat.rs is largest by bytes, tall.rs by lines.
      # Avoid $(…) here — command substitution strips a trailing newline and
      # would under-count `wc -l` by one.
      : > "$ws/tall.rs"
      for _ in $(seq 120); do printf 'x\n'; done >> "$ws/tall.rs"
      : > "$ws/mid.rs"
      for _ in $(seq 40); do printf 'x\n'; done >> "$ws/mid.rs"
      # 2 newlines → 2 lines; ~5002 bytes.
      { for _ in $(seq 5000); do printf 'Y'; done; printf '\n\n'; } > "$ws/fat.rs"
      ;;
    rename)
      printf 'import math\n\ndef area_of_circle(r):\n    return math.pi * r * r\n\ndef describe(r):\n    return f"area {area_of_circle(r):.2f}"\n\nprint(describe(2)); print(area_of_circle(1))\n' > "$ws/geometry.py"
      ;;
    bugfix)
      printf 'def apply_discount(price, percent):\n    # apply_discount(100, 10) should return 90.0\n    return price + (price * percent / 100)\n' > "$ws/discount.py"
      ;;
    branch)
      printf '# config\nDEBUG = False\nMAX_RETRIES = 3\n' > "$ws/config.py"
      ( cd "$ws" && git init -q && git add -A && git -c user.email=t@t -c user.name=t commit -qm init )
      ;;
    refactor)
      printf 'def generate_report(rows):\n    clean=[r for r in rows if isinstance(r,dict) and "amount" in r]\n    total=sum(r["amount"] for r in clean); count=len(clean)\n    avg=total/count if count else 0\n    return "\\n".join(["REPORT",f"records: {count}",f"total: {total}",f"average: {avg:.2f}"])\nif __name__=="__main__":\n    print(generate_report([{"amount":10},None,{"amount":20},"bad"]))\n' > "$ws/report.py"
      ;;
    deadend)
      printf 'from payments_gateway import charge   # not in the repo\ncharge(100)\n' > "$ws/app.py"
      ;;
    duploop)
      echo "print('hi')" > "$ws/main.py"
      ;;
    capexit)
      echo "def add(a,b): return a+b" > "$ws/m.py"
      ;;
    *)
      echo "uat_tool_loop: unknown case '$c'" >&2
      return 2
      ;;
  esac
}

prompt_for() {
  local c=$1
  case "$c" in
    line-count)
      printf '%s\n' 'show me the 10 code files with the highest line counts in this repository?'
      ;;
    rename)
      printf '%s\n' 'Rename the function area_of_circle to circle_area everywhere in geometry.py.'
      ;;
    bugfix)
      printf '%s\n' 'There is a bug in apply_discount in discount.py: apply_discount(100, 10) should return 90.0 but returns 110.0. Fix it, then create test_discount.py with a test asserting apply_discount(100, 10) == 90.0.'
      ;;
    branch)
      printf '%s\n' 'Create a git branch feature/timeout-config, switch to it, add a line DEFAULT_TIMEOUT = 30 to config.py, and commit that change ON the branch.'
      ;;
    refactor)
      printf '%s\n' 'Refactor generate_report() in report.py into three smaller helper functions (validate rows, compute totals, format output), keeping the printed output identical.'
      ;;
    deadend)
      printf '%s\n' 'app.py fails to run. Diagnose WHY in words and explain how to fix it. Do NOT create or modify any files.'
      ;;
    duploop)
      printf '%s\n' 'Use ONLY the run_command shell tool. Run the shell command: make build. If it fails, run the exact same command "make build" again, and keep retrying that same command.'
      ;;
    capexit)
      printf '%s\n' 'Use the shell to run the test suite (pytest) and fix every failing test until they all pass. Run the tests with the shell each round.'
      ;;
  esac
}

write_prompt() {
  local c=$1 pf="$R/$c/prompt.txt"
  { prompt_for "$c"; printf 'exit\n'; } > "$pf"
}

# Build a minimal sandbox HOME config, optionally overlaying --config.
sandbox_home() {
  local cap=${1:-} SB
  SB=$(mktemp -d "$R/home.XXXXXX")
  mkdir -p "$SB/.newt"
  if [ -n "$CONFIG" ] && [ -f "$CONFIG" ]; then
    cp "$CONFIG" "$SB/.newt/config.toml"
  elif [ -f "$HOME/.newt/config.toml" ] && [ -z "${UAT_ISOLATE_CONFIG:-}" ]; then
    # Prefer the operator's real backends (DGX/Nemotron named provider) when
    # present — CI runners carry this locally and never commit it.
    cp "$HOME/.newt/config.toml" "$SB/.newt/config.toml"
  else
    {
      echo "[tui]"
      echo "debug = true"
      echo "no_splash = true"
      echo "inference_timeout_secs = 180"
      echo "[tui.permissions]"
      echo 'preset = "workspace_dev"'
    } > "$SB/.newt/config.toml"
  fi
  # Ensure debug/no_splash/timeout even when overlaying an operator config.
  if ! grep -q '^\[tui\]' "$SB/.newt/config.toml" 2>/dev/null; then
    {
      echo ""
      echo "[tui]"
      echo "debug = true"
      echo "no_splash = true"
      echo "inference_timeout_secs = 180"
    } >> "$SB/.newt/config.toml"
  fi
  if [ -n "$cap" ]; then
    if grep -q '^max_tool_rounds' "$SB/.newt/config.toml" 2>/dev/null; then
      sed -i.bak "s/^max_tool_rounds.*/max_tool_rounds = $cap/" "$SB/.newt/config.toml"
      rm -f "$SB/.newt/config.toml.bak"
    else
      printf '\nmax_tool_rounds = %s\n' "$cap" >> "$SB/.newt/config.toml"
    fi
  fi
  printf '%s' "$SB"
}

run_case() { # $1=case  [$2=max_tool_rounds]
  local c=$1 cap=${2:-} ws="$R/$c" SB
  seed_case "$c" || return 2
  write_prompt "$c"
  SB=$(sandbox_home "$cap")
  local env_args=(
    env -i
    "HOME=$SB"
    "PATH=/usr/bin:/bin:/usr/local/bin:${HOME:-/usr}/bin"
    "TERM=dumb"
    "NEWT_DEBUG=1"
    "NEWT_EPHEMERAL=1"
  )
  if [ -n "$PROVIDER" ]; then
    env_args+=("NEWT_PROVIDER=$PROVIDER")
  fi
  if [ -n "$MODEL" ]; then
    env_args+=("NEWT_DGX_MODEL=$MODEL" "NEWT_DEFAULT_MODEL=$MODEL")
  fi
  if [ -n "$LEGACY_HOST" ]; then
    env_args+=("NEWT_DGX_OLLAMA_URL=$LEGACY_HOST")
    [ -n "$MODEL" ] && env_args+=("NEWT_DGX_MODEL=$MODEL")
  fi
  # Bound each scenario; empty/hung inference must not wed the suite.
  timeout 420 "${env_args[@]}" \
    "$NEWT_BIN" --no-splash --ephemeral --debug --mono --no-prompt-for-permissions \
    --no-agents-file code "$ws" < "$ws/prompt.txt" > "$ws/session.log" 2>&1 \
    || echo "[run exit $?]" >> "$ws/session.log"
  # Keep a copy under --out for CI artifacts.
  mkdir -p "$OUT/sessions"
  cp "$ws/session.log" "$OUT/sessions/${c}.log" 2>/dev/null || true
}

assert_line_count() {
  local ws=$1
  if is_infra_session "$ws"; then
    record line-count INFRA "model/endpoint unreachable or session never started"
    return 2
  fi
  if log_has "$ws" 'empty response|model returned an empty response'; then
    record line-count FAIL "empty model response after find"
    return 1
  fi
  if ! log_has "$ws" 'sort=lines|show_lines'; then
    record line-count FAIL "session did not use find sort=lines/show_lines"
    return 1
  fi
  if log_has "$ws" 'sort=size' && ! log_has "$ws" 'sort=lines'; then
    record line-count FAIL "bytesize fallback (sort=size without sort=lines)"
    return 1
  fi
  # Must report line counts in descending order: tall(120) before mid(40) before fat(2).
  local log="$ws/session.log"
  local tall mid fat
  tall=$(grep -nE '120[[:space:]]+tall\.rs' "$log" | head -1 | cut -d: -f1)
  mid=$(grep -nE '40[[:space:]]+mid\.rs' "$log" | head -1 | cut -d: -f1)
  fat=$(grep -nE '2[[:space:]]+fat\.rs' "$log" | head -1 | cut -d: -f1)
  if [ -z "$tall" ] || [ -z "$mid" ] || [ -z "$fat" ]; then
    record line-count FAIL "missing expected line-count rows (120 tall / 40 mid / 2 fat)"
    return 1
  fi
  if ! [ "$tall" -lt "$mid" ] || ! [ "$mid" -lt "$fat" ]; then
    record line-count FAIL "line counts not descending (tall@$tall mid@$mid fat@$fat)"
    return 1
  fi
  # Anti-bytesize: fat.rs's byte size must not appear as the metric.
  local fat_bytes
  fat_bytes=$(wc -c < "$ws/fat.rs" | tr -d ' ')
  if grep -qE "${fat_bytes}[[:space:]]+fat\.rs" "$log"; then
    record line-count FAIL "answered with fat.rs byte size ($fat_bytes) — bytesize fallback"
    return 1
  fi
  record line-count PASS "find sort=lines; 120 tall → 40 mid → 2 fat"
  return 0
}

assert_rename() {
  local ws=$1
  if is_infra_session "$ws"; then
    record rename INFRA "model/endpoint unreachable or session never started"
    return 2
  fi
  local old new out
  old=$(grep -c area_of_circle "$ws/geometry.py" 2>/dev/null || echo 0)
  new=$(grep -c circle_area "$ws/geometry.py" 2>/dev/null || echo 0)
  out=$(cd "$ws" && python3 geometry.py 2>&1 | head -1 || true)
  if [ "${old:-0}" != "0" ] || [ "${new:-0}" -lt 1 ]; then
    record rename FAIL "old-left=$old new=$new out=[$out]"
    return 1
  fi
  if ! printf '%s' "$out" | grep -q 'area'; then
    record rename FAIL "geometry.py no longer runs: [$out]"
    return 1
  fi
  record rename PASS "old-left=0 new=$new runs=[$out]"
  return 0
}

assert_bugfix() {
  local ws=$1
  if is_infra_session "$ws"; then
    record bugfix INFRA "model/endpoint unreachable or session never started"
    return 2
  fi
  local result
  result=$(cd "$ws" && python3 -c 'from discount import apply_discount;print(apply_discount(100,10))' 2>&1 || true)
  if [ ! -f "$ws/test_discount.py" ]; then
    record bugfix FAIL "test_discount.py missing; result=[$result]"
    return 1
  fi
  if [ "$result" != "90.0" ] && [ "$result" != "90" ]; then
    record bugfix FAIL "discount still wrong: [$result]"
    return 1
  fi
  if ! (cd "$ws" && python3 -m pytest -q test_discount.py >/dev/null 2>&1) \
     && ! (cd "$ws" && python3 test_discount.py >/dev/null 2>&1); then
    # Accept a test file that imports/asserts even if pytest isn't installed.
    if ! grep -q 'apply_discount(100' "$ws/test_discount.py"; then
      record bugfix FAIL "test_discount.py does not assert the fix"
      return 1
    fi
  fi
  record bugfix PASS "result=$result test=yes"
  return 0
}

assert_branch() {
  local ws=$1
  if is_infra_session "$ws"; then
    record branch INFRA "model/endpoint unreachable or session never started"
    return 2
  fi
  local branch const commits
  branch=$(git -C "$ws" branch --show-current 2>/dev/null || echo "")
  const=$(grep -c DEFAULT_TIMEOUT "$ws/config.py" 2>/dev/null || echo 0)
  commits=$(git -C "$ws" log feature/timeout-config --oneline 2>/dev/null | wc -l | tr -d ' ')
  if [ "$branch" != "feature/timeout-config" ]; then
    record branch FAIL "current branch=[$branch] (want feature/timeout-config)"
    return 1
  fi
  if [ "${const:-0}" -lt 1 ]; then
    record branch FAIL "DEFAULT_TIMEOUT missing"
    return 1
  fi
  if [ "${commits:-0}" -lt 1 ]; then
    record branch FAIL "no commits on feature/timeout-config"
    return 1
  fi
  record branch PASS "branch=$branch const=$const commits=$commits"
  return 0
}

assert_refactor() {
  local ws=$1
  if is_infra_session "$ws"; then
    record refactor INFRA "model/endpoint unreachable or session never started"
    return 2
  fi
  local defs out
  defs=$(grep -c '^def ' "$ws/report.py" 2>/dev/null || echo 0)
  out=$(cd "$ws" && python3 report.py 2>&1 | tr '\n' '/' || true)
  if [ "${defs:-0}" -lt 4 ]; then
    record refactor FAIL "defs=$defs (want ≥4) out=[$out]"
    return 1
  fi
  if ! printf '%s' "$out" | grep -q 'REPORT'; then
    record refactor FAIL "output broken: [$out]"
    return 1
  fi
  record refactor PASS "defs=$defs out=[$out]"
  return 0
}

assert_deadend() {
  local ws=$1
  if is_infra_session "$ws"; then
    record deadend INFRA "model/endpoint unreachable or session never started"
    return 2
  fi
  if [ -f "$ws/payments_gateway.py" ]; then
    record deadend FAIL "fabricated payments_gateway.py"
    return 1
  fi
  local named
  named=$(sig "$ws" 'payments_gateway|missing|no module|not found|ModuleNotFound')
  if [ "${named:-0}" -lt 1 ]; then
    record deadend FAIL "did not name the missing dependency"
    return 1
  fi
  record deadend PASS "fabricated=no named-missing=$named"
  return 0
}

assert_duploop() {
  local ws=$1
  if is_infra_session "$ws"; then
    record duploop INFRA "model/endpoint unreachable or session never started"
    return 2
  fi
  local calls steer
  calls=$(sig "$ws" '⚙[[:space:]]*run_command|dispatching tool name="run_command"')
  steer=$(sig "$ws" 'already|repeat|same .*(call|command)|tried that|short-circuit|steer|unavailable')
  # Soft pass: either the dup-guard steers, or a capable model adapts after ≤2 calls.
  if [ "${calls:-0}" -gt 3 ] && [ "${steer:-0}" -lt 1 ]; then
    record duploop FAIL "run_command-calls=$calls without dup-steer"
    return 1
  fi
  record duploop PASS "run_command-calls=$calls dup-steer=$steer"
  return 0
}

assert_capexit() {
  local ws=$1
  if is_infra_session "$ws"; then
    record capexit INFRA "model/endpoint unreachable or session never started"
    return 2
  fi
  local raise
  raise=$(sig "$ws" 'raise.*max_tool_rounds|increase.*the limit')
  if [ "${raise:-0}" -gt 0 ]; then
    record capexit FAIL "advised raising max_tool_rounds"
    return 1
  fi
  # Honest exit: either reached cap with tooling language, or abandoned cleanly.
  local honest reached
  reached=$(sig "$ws" 'max_tool_rounds|reached.*(cap|limit)|tool round')
  honest=$(sig "$ws" 'failed tool|tooling|permission|unavailable|cannot|unable')
  if [ "${reached:-0}" -gt 0 ] && [ "${honest:-0}" -lt 1 ]; then
    record capexit FAIL "reached cap without honest tooling language"
    return 1
  fi
  record capexit PASS "reached-cap=$reached honest=$honest raise-advice=$raise"
  return 0
}

assert_case() {
  local c=$1
  case "$c" in
    line-count) assert_line_count "$R/$c";;
    rename)     assert_rename "$R/$c";;
    bugfix)     assert_bugfix "$R/$c";;
    branch)     assert_branch "$R/$c";;
    refactor)   assert_refactor "$R/$c";;
    deadend)    assert_deadend "$R/$c";;
    duploop)    assert_duploop "$R/$c";;
    capexit)    assert_capexit "$R/$c";;
    *)
      record "$c" FAIL "unknown case"
      return 1
      ;;
  esac
}

cap_for() {
  case "$1" in
    capexit) echo 6;;
    *) echo "";;
  esac
}

write_summary() {
  {
    echo "# Practical live regression — $SUITE"
    echo
    echo "- Provider: \`${PROVIDER:-"(default / config)"}\`"
    echo "- Model: \`${MODEL:-"(config default)"}\`"
    echo "- Out: \`$OUT\`"
    echo
    echo "| Case | Status | Details |"
    echo "|---|---|---|"
    while IFS=$'\t' read -r c st det; do
      [ -n "$c" ] || continue
      echo "| \`$c\` | **$st** | $det |"
    done < "$TSV"
    echo
    local pass fail infra
    pass=$(awk -F'\t' '$2=="PASS"{n++} END{print n+0}' "$TSV")
    fail=$(awk -F'\t' '$2=="FAIL"{n++} END{print n+0}' "$TSV")
    infra=$(awk -F'\t' '$2=="INFRA"{n++} END{print n+0}' "$TSV")
    echo "Totals: PASS=$pass FAIL=$fail INFRA=$infra"
  } > "$SUMMARY"
  if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    cat "$SUMMARY" >> "$GITHUB_STEP_SUMMARY"
  fi
}

aggregate_exit() {
  local fail infra
  fail=$(awk -F'\t' '$2=="FAIL"{n++} END{print n+0}' "$TSV")
  infra=$(awk -F'\t' '$2=="INFRA"{n++} END{print n+0}' "$TSV")
  if [ "$fail" -gt 0 ]; then
    return 1
  fi
  if [ "$infra" -gt 0 ]; then
    return 2
  fi
  return 0
}

# ── Self-test (no model, no newt binary) ────────────────────────────────────
self_test() {
  local tmp; tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' RETURN
  R="$tmp/ws"
  OUT="$tmp/out"
  mkdir -p "$R" "$OUT"
  TSV="$OUT/results.tsv"
  SUMMARY="$OUT/summary.md"
  : > "$TSV"

  # PASS line-count fixture + canned session
  seed_case line-count
  mkdir -p "$R/line-count"
  cat > "$R/line-count/session.log" <<'LOG'
> v0.7.4 ready — NVIDIA-Nemotron @ http://example (openai)
⚙  find: . (type=f, max=10, sort=lines, lines)
▒ 120	tall.rs
▒ 40	mid.rs
▒ 2	fat.rs
[debug] round 1: tool_calls=0
LOG
  assert_line_count "$R/line-count"
  grep -q $'line-count\tPASS\t' "$TSV" || { echo "self-test: expected PASS line-count"; return 1; }

  # FAIL: bytesize fallback
  : > "$TSV"
  seed_case line-count
  local fat_bytes
  fat_bytes=$(wc -c < "$R/line-count/fat.rs" | tr -d ' ')
  cat > "$R/line-count/session.log" <<LOG
> ready —
⚙  find: . (type=f, sort=size, size)
▒ ${fat_bytes}	fat.rs
▒ 240	tall.rs
[debug] round 1: tool_calls=0
LOG
  assert_line_count "$R/line-count" || true
  grep -q $'line-count\tFAIL\t' "$TSV" || { echo "self-test: expected FAIL bytesize"; return 1; }

  # INFRA: connection refused
  : > "$TSV"
  seed_case rename
  cat > "$R/rename/session.log" <<'LOG'
error sending request for url (http://example): connection refused
LOG
  assert_rename "$R/rename" || true
  grep -q $'rename\tINFRA\t' "$TSV" || { echo "self-test: expected INFRA rename"; return 1; }

  # Aggregate exit codes
  : > "$TSV"
  printf 'line-count\tPASS\toK\n' >> "$TSV"
  aggregate_exit
  [ $? -eq 0 ] || { echo "self-test: all-PASS should exit 0"; return 1; }

  : > "$TSV"
  printf 'line-count\tFAIL\tbad\n' >> "$TSV"
  aggregate_exit
  [ $? -eq 1 ] || { echo "self-test: FAIL should exit 1"; return 1; }

  : > "$TSV"
  printf 'line-count\tINFRA\tdown\n' >> "$TSV"
  aggregate_exit
  [ $? -eq 2 ] || { echo "self-test: INFRA should exit 2"; return 1; }

  echo "self-test: OK"
  return 0
}

if $SELF_TEST; then
  self_test
  exit $?
fi

if [ ! -x "$NEWT_BIN" ]; then
  echo "newt binary not found at $NEWT_BIN (build: cargo build --release --bin newt)" >&2
  exit 2
fi

echo "[uat] suite=$SUITE provider=${PROVIDER:--} model=${MODEL:--} out=$OUT" >&2
echo "[uat] note: shell-dependent cases (duploop/capexit) need 'just install-real' until brush#1184." >&2

FAILS=0
INFRAS=0
while IFS= read -r c; do
  [ -n "$c" ] || continue
  echo "## run $c"
  cap=$(cap_for "$c")
  run_case "$c" "$cap"
  rc=0
  assert_case "$c" || rc=$?
  if [ "$rc" -eq 1 ]; then FAILS=$((FAILS + 1)); fi
  if [ "$rc" -eq 2 ]; then INFRAS=$((INFRAS + 1)); fi
done < <(selected_cases)

write_summary
echo "================= ASSESSMENT ================="
column -t -s $'\t' "$TSV" 2>/dev/null || cat "$TSV"
echo "================= DONE (FAIL=$FAILS INFRA=$INFRAS) ================="
echo "results: $TSV"
echo "summary: $SUMMARY"

aggregate_exit
exit $?
