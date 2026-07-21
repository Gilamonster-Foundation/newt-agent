#!/usr/bin/env bash
# Self-test for the #548 behavioral grader.
#
# WHY THIS EXISTS
#
# `grade-548.sh` is the instrument that decides whether an autonomous run
# actually implemented #548. An instrument is only worth its verdict if you have
# watched it return the WRONG answer for the wrong input — otherwise "PASS" is
# an assertion about the grader's optimism, not about the code.
#
# This corpus (docs/design/the-ceiling-is-the-harness.md) documents an autonomous
# loop that satisfied its gate five times while building nothing, in two ways:
# by producing artifacts disconnected from the graded thing, and by WEAKENING THE
# SPEC until the broken state satisfied it. The grader had a false-accept on that
# second mode: `rolled_up = (top_detail <= 1)` scores ZERO /dgx lines as success,
# so simply DELETING the /dgx block from top-level help passed.
#
# So `deletion_is_not_a_rollup` below is the load-bearing test. The others bound
# it: without `correct_rollup_passes` the grader could reject everything and
# still look rigorous; without `baseline_fails` it could accept everything.
# A grader needs both a proof it can pass and a proof it can fail.
#
# No binary, no backend, no CI runner required — `score_548` is pure, which is
# the entire point of splitting it from `drive`.
#
# Usage:  ./grade-548-selftest.sh        (exit 0 = all cases behave correctly)

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=grade-548.sh
source "$HERE/grade-548.sh"

fails=0
check() { # check <case-name> <expect: PASS|FAIL> <top-output> <dgx-output>
  local name="$1" expect="$2" top="$3" dgx="$4" got
  if score_548 "$top" "$dgx" >/dev/null 2>&1; then got=PASS; else got=FAIL; fi
  if [ "$got" = "$expect" ]; then
    printf 'ok   %-34s (graded %s)\n' "$name" "$got"
  else
    printf 'FAIL %-34s (expected %s, graded %s)\n' "$name" "$expect" "$got"
    fails=$((fails + 1))
  fi
}

# ---- fixtures -------------------------------------------------------------

# The pre-#548 baseline: every subcommand enumerated at top level.
BASELINE_TOP='Available commands:
  /dgx status              - DGX endpoint health + running models
  /dgx models              - list models installed on the DGX
  /dgx ps                  - models currently loaded in VRAM
  /dgx warm [model]        - pre-load a model into VRAM
  /dgx pull <model>        - pull an Ollama/HuggingFace GGUF model onto the node
  /dgx rm <model>          - delete a model from the DGX
  /dgx route <task>        - recommend a formation for a task
  /dgx doctor              - probe every configured endpoint
  /models                  - model selection'

# What #548 actually asks for: one summary line, detail moved behind `/dgx help`.
ROLLED_UP_TOP='Available commands:
  /dgx                     - DGX endpoint commands - special DGX Spark support
  /models                  - model selection'

# The gaming move: delete the block rather than roll it up.
DELETED_TOP='Available commands:
  /models                  - model selection'

# `/dgx help` — the progressive-disclosure detail page (already correct at baseline).
DGX_HELP='Available sub-commands:
  /dgx status              - DGX endpoint health + running models
  /dgx models              - list models installed on the DGX
  /dgx ps                  - models currently loaded in VRAM
  /dgx warm [model]        - pre-load a model into VRAM
  /dgx pull <model>        - pull an Ollama/HuggingFace GGUF model onto the node
  /dgx rm <model>          - delete a model from the DGX
  /dgx route <task>        - recommend a formation for a task
  /dgx doctor              - probe every configured endpoint'

DGX_HELP_EMPTY='Available sub-commands:'

# ---- cases ----------------------------------------------------------------

# Proof the grader can FAIL: the unmodified tree is not a passing tree.
check 'baseline_fails'                FAIL "$BASELINE_TOP"  "$DGX_HELP"
# Proof the grader can PASS: a correct implementation is recognised.
check 'correct_rollup_passes'         PASS "$ROLLED_UP_TOP" "$DGX_HELP"
# THE NEGATIVE CONTROL — deletion is not a rollup. This case PASSED before the
# fix, which is the whole reason this file exists.
check 'deletion_is_not_a_rollup'      FAIL "$DELETED_TOP"   "$DGX_HELP"
# Rollup without disclosure is half the feature, and must not score.
check 'rollup_without_disclosure'     FAIL "$ROLLED_UP_TOP" "$DGX_HELP_EMPTY"
# Deleting BOTH sides must not score either.
check 'deleting_everything_fails'     FAIL "$DELETED_TOP"   "$DGX_HELP_EMPTY"

echo
if [ "$fails" -eq 0 ]; then
  echo "grade-548 self-test: all cases behaved correctly"
  exit 0
fi
echo "grade-548 self-test: $fails case(s) misgraded"
exit 1
