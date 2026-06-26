#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# Live behavioral tool-loop UAT (L2/L3). See docs/testing/uat-tool-loop.md.
#
# Drives the REAL newt harness with user-phrased coding prompts on disposable
# workspaces, against a real LLM, and asserts BOTH:
#   (a) OUTCOME  — did the workspace end in the right state? (file edited, branch
#                  created, bug fixed, test passes, nothing fabricated)
#   (b) BEHAVIOR — tool-loop signals from the session log: rounds, tool calls,
#                  hallucination-corrected (27.1), dup-guard steer (27.3),
#                  plan ledger use (27.4), honest cap-exit (27.5).
#
# This is the LIVE counterpart to the mocked `tool_round_cap_tests`
# (newt-core/src/agentic/mod.rs) and complements the golden-diff `newt-eval`
# cases — those assert diff correctness; this asserts loop BEHAVIOR end-to-end.
#
# GOTCHA (do not "fix"): NEVER `pkill -f <proxy-or-newt-name>` here — the pattern
# matches THIS script's own command line and self-kills the shell (exit 144).
# Kill background helpers by tracked PID, clear ports with `fuser -k <port>/tcp`.
#
# Usage: uat_tool_loop.sh [HOST] [MODEL]
#   HOST  default http://REDACTED-HOST:11434  (dgx1 is reliable; gnuc-ollama
#         flakes under multi-turn/large-ctx load — connection drops → timeouts)
#   MODEL default qwen3-coder:30b
# Env:  NEWT_BIN (default: $CARGO_TARGET_DIR/release/newt or target/release/newt)
# ─────────────────────────────────────────────────────────────────────────────
set -uo pipefail
HOST="${1:-http://REDACTED-HOST:11434}"
MODEL="${2:-qwen3-coder:30b}"
NEWT_BIN="${NEWT_BIN:-${CARGO_TARGET_DIR:-target}/release/newt}"
R="${UAT_WORKDIR:-/tmp/newt-uat-tool-loop}"
mkdir -p "$R"
[ -x "$NEWT_BIN" ] || { echo "newt binary not found at $NEWT_BIN (build: cargo build --release --bin newt)"; exit 2; }
# Shell-dependent scenarios (S6/S7, run-shaped prompts) need the REAL brush shell:
# run `just install-real` first (and point NEWT_BIN at $HOME/bin/newt) until
# reubeno/brush#1184 lands, else run_command is the dead stub. S1-S5 are fine on
# the default build. (See docs/testing/uat-tool-loop.md → Prerequisite.)
echo "[uat] note: run_command needs 'just install-real' (real brush shell) until brush#1184; S1-S5 are fine on the stub build." >&2

run(){ # $1=ws  $2=promptfile  $3=max_tool_rounds(optional)
  local ws=$1 pf=$2 cap=${3:-} SB; SB=$(mktemp -d "$R/home.XXXXXX")
  mkdir -p "$SB/.newt"
  { echo "[tui]"; echo "debug = true"; echo "no_splash = true"; echo "inference_timeout_secs = 120"
    [ -n "$cap" ] && echo "max_tool_rounds = $cap"
    echo "[tui.permissions]"; echo 'preset = "workspace_dev"'; } > "$SB/.newt/config.toml"
  timeout 360 env -i HOME="$SB" PATH=/usr/bin:/bin TERM=dumb \
    NEWT_DGX_OLLAMA_URL="$HOST" NEWT_DGX_MODEL="$MODEL" NEWT_DEBUG=1 \
    "$NEWT_BIN" --no-splash code "$ws" < "$pf" > "$ws/session.log" 2>&1 \
    || echo "[run exit $?]" >> "$ws/session.log"
}
sig(){ grep -ciE "$2" "$1/session.log" 2>/dev/null || echo 0; }   # count a signal in a session log

# ── Scenario fixtures (generated; disposable) ───────────────────────────────
seed(){
  mkdir -p "$R"/{s1-rename,s2-branch,s3-bugfix,s4-refactor,s5-deadend,s6-duploop,s7-capexit}
  printf 'import math\n\ndef area_of_circle(r):\n    return math.pi * r * r\n\ndef describe(r):\n    return f"area {area_of_circle(r):.2f}"\n\nprint(describe(2)); print(area_of_circle(1))\n' > "$R/s1-rename/geometry.py"
  printf '# config\nDEBUG = False\nMAX_RETRIES = 3\n' > "$R/s2-branch/config.py"
  ( cd "$R/s2-branch" && git init -q && git add -A && git -c user.email=t@t -c user.name=t commit -qm init )
  printf 'def apply_discount(price, percent):\n    # apply_discount(100, 10) should return 90.0\n    return price + (price * percent / 100)\n' > "$R/s3-bugfix/discount.py"
  printf 'def generate_report(rows):\n    clean=[r for r in rows if isinstance(r,dict) and "amount" in r]\n    total=sum(r["amount"] for r in clean); count=len(clean)\n    avg=total/count if count else 0\n    return "\\n".join(["REPORT",f"records: {count}",f"total: {total}",f"average: {avg:.2f}"])\nif __name__=="__main__":\n    print(generate_report([{"amount":10},None,{"amount":20},"bad"]))\n' > "$R/s4-refactor/report.py"
  printf 'from payments_gateway import charge   # not in the repo\ncharge(100)\n' > "$R/s5-deadend/app.py"
  echo "print('hi')" > "$R/s6-duploop/main.py"
  echo "def add(a,b): return a+b" > "$R/s7-capexit/m.py"
}

prompt(){ printf '%s\nexit\n' "$2" > "$R/$1/prompt.txt"; }

seed
prompt s1-rename   'Rename the function area_of_circle to circle_area everywhere in geometry.py.'
prompt s2-branch   'Create a git branch feature/timeout-config, switch to it, add a line DEFAULT_TIMEOUT = 30 to config.py, and commit that change ON the branch.'
prompt s3-bugfix   'There is a bug in apply_discount in discount.py: apply_discount(100, 10) should return 90.0 but returns 110.0. Fix it, then create test_discount.py with a test asserting apply_discount(100, 10) == 90.0.'
prompt s4-refactor 'Refactor generate_report() in report.py into three smaller helper functions (validate rows, compute totals, format output), keeping the printed output identical.'
prompt s5-deadend  'app.py fails to run. Diagnose WHY in words and explain how to fix it. Do NOT create or modify any files.'
prompt s6-duploop  'Use ONLY the run_command shell tool. Run the shell command: make build. If it fails, run the exact same command "make build" again, and keep retrying that same command.'
prompt s7-capexit  'Use the shell to run the test suite (pytest) and fix every failing test until they all pass. Run the tests with the shell each round.'

for s in s1-rename s2-branch s3-bugfix s4-refactor s5-deadend s6-duploop; do echo "## run $s"; run "$R/$s" "$R/$s/prompt.txt"; done
echo "## run s7-capexit (cap=6)"; run "$R/s7-capexit" "$R/s7-capexit/prompt.txt" 6

echo "================= ASSESSMENT ================="
echo "S1 rename   | old-left=$(grep -c area_of_circle "$R/s1-rename/geometry.py") new=$(grep -c circle_area "$R/s1-rename/geometry.py") runs=[$(cd "$R/s1-rename" && python3 geometry.py 2>&1|head -1)]"
echo "S2 branch   | branch=$(git -C "$R/s2-branch" branch --show-current) const=$(grep -c DEFAULT_TIMEOUT "$R/s2-branch/config.py") feat-commits=[$(git -C "$R/s2-branch" log feature/timeout-config --oneline 2>/dev/null|tr '\n' '/')]"
echo "S3 bugfix   | result=$(cd "$R/s3-bugfix" && python3 -c 'from discount import apply_discount;print(apply_discount(100,10))' 2>&1) test=$([ -f "$R/s3-bugfix/test_discount.py" ]&&echo yes||echo no)"
echo "S4 refactor | defs=$(grep -c 'def ' "$R/s4-refactor/report.py") out=[$(cd "$R/s4-refactor" && python3 report.py 2>&1|tr '\n' '/')]"
echo "S5 deadend  | fabricated=$([ -f "$R/s5-deadend/payments_gateway.py" ]&&echo YES||echo no) named-missing=$(sig "$R/s5-deadend" 'payments_gateway|missing|no module|not found')"
echo "S6 duploop  | run_command-calls=$(sig "$R/s6-duploop" '⚙ *run_command') dup-steer=$(sig "$R/s6-duploop" 'already|repeat|same .*(call|command)|tried that|short-circuit|steer') rounds=$(sig "$R/s6-duploop" 'round [0-9].*probe')"
echo "S7 capexit  | reached-cap=$(sig "$R/s7-capexit" 'max_tool_rounds|reached.*(cap|limit)|tool round') honest=$(sig "$R/s7-capexit" 'failed tool|tooling|permission|unavailable') raise-cap-advice=$(sig "$R/s7-capexit" 'raise.*max_tool_rounds|increase.*the limit')"
echo "--- tool-loop signals (all sessions) ---"
echo "  hallucination-corrected (27.1): $(grep -riE 'hallucination\(s\) corrected' "$R"/s*/session.log 2>/dev/null|wc -l)"
echo "  plan-ledger used (27.4):        $(grep -riE 'plan_set|plan_advance|state_set' "$R"/s*/session.log 2>/dev/null|wc -l)"
echo "================= DONE ================="
