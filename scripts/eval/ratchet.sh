#!/usr/bin/env bash
# ratchet.sh — run one newt-eval task through an execution MODE and behaviorally
# grade the result. The matrix driver for the capability ratchet (see RATCHET.md).
#
#   single   — one agent, one turn         → `newt-eval run` (it grades structurally)
#   crew     — autonomous plan + crew       → `newt plan --one-shot` → cargo test (behavioral)
#
# Behavioral grade = does the feature WORK when you run it (#672's North-Star),
# i.e. `cargo test` in the produced tree. Tasks whose seed test FAILS until the
# feature is implemented (e.g. T0) make this an honest pass/fail.
#
# SECURITY: no home-network specifics live here. Models are NAMES (--model);
# crew rosters/endpoints come from the operator's local ~/.newt config. Nothing
# committed names a host.
#
# Usage:
#   ratchet.sh --task T0-fix-add --mode single --model qwen2.5-coder:7b --coder
#   ratchet.sh --task T0-fix-add --mode crew   --max-leaves 6 [--timeout 1200]
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
CASES_DIR="$REPO/newt-eval/cases"
TARGET="${CARGO_TARGET_DIR:-$REPO/target}"
NEWT="${NEWT_BIN:-$TARGET/debug/newt}"
NEWT_EVAL="${NEWT_EVAL_BIN:-$TARGET/debug/newt-eval}"

TASK="" MODE="" MODEL="" CODER="" MAX_LEAVES=6 TIMEOUT=1200 WORKER_TIMEOUT_MS=120000
while [ $# -gt 0 ]; do
  case "$1" in
    --task) TASK="$2"; shift 2;;
    --mode) MODE="$2"; shift 2;;
    --model) MODEL="$2"; shift 2;;
    --coder) CODER="--coder"; shift;;
    --max-leaves) MAX_LEAVES="$2"; shift 2;;
    --timeout) TIMEOUT="$2"; shift 2;;
    --worker-timeout-ms) WORKER_TIMEOUT_MS="$2"; shift 2;;
    *) echo "ratchet: unknown arg '$1'" >&2; exit 2;;
  esac
done
if [ -z "$TASK" ] || [ -z "$MODE" ]; then
  echo "usage: ratchet.sh --task <T> --mode single|crew [...]" >&2; exit 2
fi
CASE_DIR="$CASES_DIR/$TASK"
[ -f "$CASE_DIR/case.toml" ] || { echo "ratchet: no case at $CASE_DIR" >&2; exit 2; }

# Extract the task prompt (between `prompt = """` and the closing `"""`).
prompt="$(awk '/^prompt = """/{f=1;next} f&&/^"""/{f=0} f{print}' "$CASE_DIR/case.toml")"

emit() { # task mode model behavioral details...
  printf 'RATCHET\t%s\t%s\t%s\t%s\t%s\n' "$TASK" "$MODE" "${MODEL:-config-crew}" "$1" "$2"
}

case "$MODE" in
  single)
    [ -n "$MODEL" ] || { echo "ratchet: --mode single needs --model" >&2; exit 2; }
    out="$("$NEWT_EVAL" run --case "$TASK" --model "$MODEL" $CODER \
            --worker-timeout-ms "$WORKER_TIMEOUT_MS" 2>/dev/null)"
    echo "$out" >&2
    # Behavioral truth = the tests_pass row.
    tp="$(awk '$2=="tests_pass"{print $3}' <<<"$out")"
    behavioral=$([ "$tp" = "ok" ] && echo PASS || echo FAIL)
    allok=$(awk '$1=="'"$TASK"'"&&$3!="ok"{n++} END{print (n?"no":"yes")}' <<<"$out")
    emit "$behavioral" "tests_pass=$tp all_evaluators_ok=$allok"
    ;;
  crew)
    [ -x "$NEWT" ] || { echo "ratchet: newt binary not at $NEWT (build it / set NEWT_BIN)" >&2; exit 2; }
    throw="$(mktemp -d)"
    cp -r "$CASE_DIR/workspace/." "$throw/"
    ( cd "$throw" && git init -q -b main && git add -A \
        && git -c user.email=r@r -c user.name=r commit -qm baseline )
    base="$(cd "$throw" && git rev-parse HEAD)"
    echo "ratchet: crew run on $TASK in $throw (max-leaves $MAX_LEAVES, timeout ${TIMEOUT}s)" >&2
    # The autonomous loop. --one-shot is the headless approval. Crew roster from ~/.newt.
    # -k 60: timeout setpgid()s newt into its own process group, which escapes an
    # outer group-kill — escalate to SIGKILL so a TERM-immune run can't orphan.
    CARGO_TARGET_DIR="$TARGET" timeout -k 60 "$TIMEOUT" \
      "$NEWT" plan --goal "$prompt" --one-shot --dir "$throw" --max-leaves "$MAX_LEAVES" \
      >"$throw/.plan.log" 2>&1
    plan_rc=$?
    final="$(cd "$throw" && git branch --list 'crew/*' --format='%(refname:short)' | tail -1)"
    if [ -z "$final" ]; then
      # No branch landed. Discriminate WHY (#820) — a dead endpoint and a
      # model that ran but landed nothing are different results:
      #   no_crew_branch_infra      — the model was never exercised
      #                               (connection/availability errors, or an
      #                               empty log). Drivers exclude from n.
      #   no_crew_branch_exercised  — real inference happened but the crew
      #                               landed no work: a LEGITIMATE behavioral
      #                               FAIL (root causes #1/#2/#4 of the
      #                               improving-crew-results doc). Counts as
      #                               a trial; dir= kept for autopsy.
      if [ ! -s "$throw/.plan.log" ] || grep -qiE \
          'connection refused|error sending request|tcp connect|connection reset|no such host|dns error|not found, try pulling|timed out waiting for' \
          "$throw/.plan.log"; then
        emit FAIL "plan_rc=$plan_rc no_crew_branch_infra (see $throw/.plan.log)"
      else
        emit FAIL "plan_rc=$plan_rc no_crew_branch_exercised dir=$throw"
      fi
      exit 0
    fi
    ( cd "$throw" && git checkout -q "$final" )
    leaves="$(cd "$throw" && git branch --list 'crew/*' | wc -l | tr -d ' ')"
    files="$(cd "$throw" && git diff --name-only "$base..$final" | tr '\n' ',' | sed 's/,$//')"
    touched_seam=$(cd "$throw" && git diff "$base..$final" -- src/lib.rs | grep -qE '^[-+]' && echo yes || echo no)
    # Diagnostic: did the crew edit its OWN test assertion (the #672 gaming move)?
    edited_test=$(cd "$throw" && git diff "$base..$final" -- src/lib.rs | grep -qE '^[-+].*assert' && echo yes || echo no)
    # UNGAMEABLE behavioral grade — structurally-enforced TDD, measurement side.
    # Drop the case's HIDDEN canonical spec (which the agent never saw, so it
    # could not edit it) into the produced tree and run ONLY that. A crew that
    # "passed" by rewriting its own assertion still FAILS here unless the code is
    # actually correct.
    spec="$CASE_DIR/grade_spec.rs"
    if [ -f "$spec" ]; then
      mkdir -p "$throw/tests"; cp "$spec" "$throw/tests/grade_spec.rs"
      if ( cd "$throw" && CARGO_TARGET_DIR="$TARGET" cargo test --test grade_spec -q >/dev/null 2>&1 ); then
        behavioral=PASS
      else
        behavioral=FAIL
      fi
    else
      # No hidden spec → fall back to the seed's own tests (only honest for
      # fail-until-fixed seeds; gameable by a test-editing agent — flagged).
      ( cd "$throw" && CARGO_TARGET_DIR="$TARGET" cargo test -q >/dev/null 2>&1 ) \
        && behavioral="PASS?gameable" || behavioral="FAIL"
    fi
    emit "$behavioral" "leaves=$leaves touched_src_lib=$touched_seam edited_own_test=$edited_test files=[$files] plan_rc=$plan_rc dir=$throw"
    ;;
  *) echo "ratchet: --mode must be single|crew" >&2; exit 2;;
esac
