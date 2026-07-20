#!/usr/bin/env bash
# Behavioral grader for issue #548 — /help rollups for the /dgx command group.
#
# #548 asks to ROLL UP the verbose per-subcommand /dgx lines at the TOP-LEVEL
# /help into a single summary line, while keeping `/dgx help` as the
# progressive-disclosure detail page. In the baseline, `/dgx help` ALREADY
# expands the subcommands — the missing piece is the top-level rollup.
#
# This is a BEHAVIORAL check (not `just check`): it inspects the ACTUAL help
# output of a built `newt`. A plausible-but-unwired implementation (e.g. an
# orphan module never hooked into help_lines) FAILS here even though it compiles
# + passes the unit tests.
#
#   PASS  ⇔  top-level /help shows a /dgx SUMMARY line   (rolled up)
#            AND lists no /dgx subcommand detail lines   (rolled up)
#            AND `/dgx help` still lists them            (disclosure kept)
#
# Usage:  grade-548.sh <path-to-newt-binary>
#
# Note: the help text is backend-independent, and as of the help-render-decouple
# change it is rendered by the STARTUP-FREE `newt help [command]` subcommand —
# no session, no backend connect, no splash/wizard. That is what lets this
# grader run on a hosted CI runner rather than a self-hosted box with a live
# Ollama. `newt help` is the exact byte-source of the interactive `/help`, and
# `newt help dgx` of `/dgx help` (both route through `newt_tui::render_help`).
# Output: one JSON result line on stdout; human-readable on stderr.
#
# ---------------------------------------------------------------------------
# MECHANISM vs POLICY
#
# `drive` is the impure mechanism (spawn a binary, capture bytes). `score_548`
# is the pure policy (given two captured outputs, did #548 land?). They are split
# so the POLICY is testable without a binary, a backend, or a CI runner — see
# `grade-548-selftest.sh`, which drives `score_548` over fixtures including the
# negative controls named below.
#
# A grader with no negative control is the same trap it exists to catch: an
# instrument that reports success is worthless until you have watched it report
# failure. The self-test is therefore part of the grader, not an optional extra.
# ---------------------------------------------------------------------------
set -uo pipefail

SUBS='status|models|ps|warm|pull|rm|route|doctor'

# Count /dgx <sub> DETAIL lines (a subcommand name followed by a space).
count_subs() { grep -cE "/dgx (${SUBS}) " <<<"${1:-}" || true; }
# Count ALL lines mentioning /dgx — detail or summary.
count_any_dgx() { grep -cE '/dgx' <<<"${1:-}" || true; }

# score_548 <top-level-help-output> <dgx-help-output>
#
# Emits the machine-readable JSON result on stdout, human-readable on stderr,
# and returns 0 on PASS.
#
# The `rolled_up` predicate deliberately requires a summary line to be PRESENT,
# not merely that detail lines are ABSENT. The earlier form was
#
#     rolled_up = (top_detail <= 1)
#
# which scores 0 detail lines as success — so an implementation that DELETES the
# /dgx block from top-level help entirely PASSED, despite being strictly worse
# than the baseline and not what #548 asks for. That is a false-accept on
# precisely the "weaken the spec until the gate goes green" failure mode this
# corpus documents (docs/design/the-ceiling-is-the-harness.md §5) — which makes
# it the one defect this grader must not have.
score_548() {
  local top="${1:-}" dgx="${2:-}"
  local top_detail top_any top_summary dgx_detail rolled_up disclosure pass

  top_detail="$(count_subs "$top")"
  top_any="$(count_any_dgx "$top")"
  top_summary=$(( top_any - top_detail ))
  dgx_detail="$(count_subs "$dgx")"

  # Rolled up <=> a summary line exists AND no detail lines remain at top level.
  if [ "$top_summary" -ge 1 ] && [ "$top_detail" -eq 0 ]; then
    rolled_up=true
  else
    rolled_up=false
  fi

  # Disclosure kept <=> `/dgx help` still expands the subcommands.
  if [ "$dgx_detail" -ge 5 ]; then disclosure=true; else disclosure=false; fi

  if [ "$rolled_up" = true ] && [ "$disclosure" = true ]; then pass=true; else pass=false; fi

  printf '{"issue":548,"top_dgx_subs":%s,"top_dgx_summary":%s,"dgx_help_subs":%s,"rolled_up":%s,"disclosure":%s,"pass":%s}\n' \
    "$top_detail" "$top_summary" "$dgx_detail" "$rolled_up" "$disclosure" "$pass"

  {
    echo "  top-level /help : ${top_detail} detail line(s), ${top_summary} summary line(s)  (rolled up => 0 detail AND >= 1 summary)"
    echo "  /dgx help       : ${dgx_detail} /dgx subcommand line(s)  (disclosure => >= 5)"
    if [ "$pass" = true ]; then
      echo "RESULT: PASS — /dgx is rolled up at the top level and /dgx help still expands it."
    elif [ "$top_summary" -eq 0 ] && [ "$top_detail" -eq 0 ]; then
      echo "RESULT: FAIL — /dgx is ABSENT from top-level help entirely. #548 asks for a rollup"
      echo "               (one summary line), not deletion."
    else
      echo "RESULT: FAIL — #548 not implemented (top-level still lists ${top_detail} /dgx subcommands)."
    fi
  } >&2

  [ "$pass" = true ]
}

# Sourced by the self-test? Stop here — everything below is the mechanism.
if [ "${BASH_SOURCE[0]}" != "${0}" ]; then
  return 0
fi

NEWT="${1:?usage: grade-548.sh <newt-binary>}"

# Render help startup-free: `newt help` == interactive `/help`,
# `newt help <cmd>` == interactive `/<cmd> --help`. No backend required.
drive() { "$NEWT" help ${1:+"$1"} 2>/dev/null; }

score_548 "$(drive)" "$(drive dgx)"
