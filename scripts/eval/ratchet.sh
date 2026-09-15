#!/usr/bin/env bash
# ratchet.sh — run one newt-eval task through an execution MODE and behaviorally
# grade the result. The matrix driver for the capability ratchet (see RATCHET.md).
#
#   single   — one agent, one turn         → `newt-eval run --json`
#   crew     — autonomous plan + crew       → `newt plan --one-shot` → `newt-eval grade --json`
#
# Behavioral grade = does the feature WORK (#672's North-Star), judged by ONE
# canonical grader for both modes (#2317): the case's withheld grade_spec.rs,
# run by newt-eval against a copy of the produced tree (newt-eval/src/grade.rs).
# The row's behavioral column is PASS | FAIL | UNGRADABLE(reason) | ERROR(reason);
# the candidate's own tests and the structural evaluators are diagnostics only.
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

# newt-eval's scorecard JSON (stdin) → "<verdict>\t<details>", read by KEY for
# both arms. Never parse the human table: its reshaping (#1883) silently
# emptied the single arm. No behavioral grade at all means no tree was graded.
row_from_scorecard() { # task
  python3 -c '
import json, sys
try:
    cases = [c for c in json.load(sys.stdin)["cases"] if c["case_name"] == sys.argv[1]]
except (ValueError, KeyError, TypeError):
    cases = []
rows = [r for c in cases for r in c["results"]]
grade = next((c["behavioral"] for c in cases if c.get("behavioral")), None)
if grade is None:
    runner = any(r["evaluator"] == "runner" and not r["passed"] for r in rows)
    grade = {"verdict": "ERROR(runner)" if runner else "ERROR(no_scorecard)",
             "grader": "none", "spec_cid": None, "tests_run": 0, "timeout": "none"}
tp = next((r for r in rows if r["evaluator"] == "tests_pass"), None)
tp = "" if tp is None else "skipped" if tp.get("skipped") else "ok" if tp["passed"] else "fail"
allok = "yes" if rows and all(r["passed"] for r in rows) else "no"
details = "grader=%s spec_cid=%s tests_run=%s timeout=%s tests_pass=%s all_evaluators_ok=%s" % (
    grade["grader"], grade["spec_cid"] or "none", grade["tests_run"], grade["timeout"], tp, allok)
if grade.get("harness_subversion"):
    details += " harness_subversion=" + grade["harness_subversion"]
print(grade["verdict"] + "\t" + details)
' "$1"
}

case "$MODE" in
  single)
    [ -n "$MODEL" ] || { echo "ratchet: --mode single needs --model" >&2; exit 2; }
    out="$("$NEWT_EVAL" run --json --case "$TASK" --model "$MODEL" $CODER \
            --worker-timeout-ms "$WORKER_TIMEOUT_MS" 2>/dev/null)"
    echo "$out" >&2
    IFS=$'\t' read -r behavioral details < <(row_from_scorecard "$TASK" <<<"$out")
    emit "$behavioral" "$details"
    ;;
  crew)
    [ -x "$NEWT" ] || { echo "ratchet: newt binary not at $NEWT (build it / set NEWT_BIN)" >&2; exit 2; }
    throw="$(mktemp -d)"
    cp -r "$CASE_DIR/workspace/." "$throw/"
    # #1320 bridge: `newt plan`'s backend selection honors ONLY the
    # project-local `.newt/config.toml` walk-up — the base-config chain
    # ($NEWT_CONFIG env / --config / --config-dir) never reaches it. sweep.sh
    # exports $NEWT_CONFIG ("newt plan resolves this"), which is silently
    # inert → crew cells would run the box's default backend under the swept
    # model's label. Install the sweep config INTO the throwaway workspace as
    # its project config so the pin actually lands. Remove when #1320 fixes
    # the base chain.
    if [ -n "${NEWT_CONFIG:-}" ] && [ -f "$NEWT_CONFIG" ]; then
      mkdir -p "$throw/.newt"
      cp "$NEWT_CONFIG" "$throw/.newt/config.toml"
    fi
    ( cd "$throw" && git init -q -b main && git add -A \
        && git -c user.email=r@r -c user.name=r commit -qm baseline )
    base="$(cd "$throw" && git rev-parse HEAD)"
    # What the grader must see again after the run: the spec's content id, and
    # that the spec is not already in the tree the crew is about to work in.
    pre="$("$NEWT_EVAL" grade --pre-run --case "$TASK" --workspace "$throw" 2>/dev/null)"
    read -r cid_before visible_before < <(python3 -c '
import json, sys
try:
    p = json.load(sys.stdin)
    print(p["spec_cid"] or "none", "yes" if p["spec_visible"] else "no")
except (ValueError, KeyError, TypeError):
    pass
' <<<"$pre")
    if [ -z "${cid_before:-}" ]; then
      emit "ERROR(pre_run)" "grader=none spec_cid=none tests_run=0 timeout=none dir=$throw"
      exit 0
    fi
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
      #   ERROR(infra)              — the model was never exercised
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
        emit "ERROR(infra)" "grader=none spec_cid=$cid_before tests_run=0 timeout=none plan_rc=$plan_rc no_crew_branch_infra (see $throw/.plan.log)"
      else
        emit FAIL "grader=none spec_cid=$cid_before tests_run=0 timeout=none plan_rc=$plan_rc no_crew_branch_exercised dir=$throw"
      fi
      exit 0
    fi
    ( cd "$throw" && git checkout -q "$final" )
    leaves="$(cd "$throw" && git branch --list 'crew/*' | wc -l | tr -d ' ')"
    files="$(cd "$throw" && git diff --name-only "$base..$final" | tr '\n' ',' | sed 's/,$//')"
    touched_seam=$(cd "$throw" && git diff "$base..$final" -- src/lib.rs | grep -qE '^[-+]' && echo yes || echo no)
    # Diagnostic: did the crew edit its OWN test assertion (the #672 gaming move)?
    edited_test=$(cd "$throw" && git diff "$base..$final" -- src/lib.rs | grep -qE '^[-+].*assert' && echo yes || echo no)
    # The canonical grade, the same call single mode makes: the #887 guard,
    # then the hidden spec in a copy of the tree (newt-eval/src/grade.rs).
    visible_flag=()
    [ "$visible_before" = yes ] && visible_flag=(--spec-visible-before)
    out="$("$NEWT_EVAL" grade --json --case "$TASK" --workspace "$throw" \
            --spec-cid-before "$cid_before" "${visible_flag[@]}" 2>/dev/null)"
    IFS=$'\t' read -r behavioral details < <(row_from_scorecard "$TASK" <<<"$out")
    emit "$behavioral" "$details leaves=$leaves touched_src_lib=$touched_seam edited_own_test=$edited_test files=[$files] plan_rc=$plan_rc dir=$throw"
    ;;
  *) echo "ratchet: --mode must be single|crew" >&2; exit 2;;
esac
