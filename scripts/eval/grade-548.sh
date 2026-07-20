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
#   PASS  ⇔  top-level /help no longer lists the /dgx subcommands  (rolled up)
#            AND `/dgx help` still does                            (disclosure kept)
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
set -uo pipefail

NEWT="${1:?usage: grade-548.sh <newt-binary>}"
SUBS='status|models|ps|warm|pull|rm|route|doctor'

# Render help startup-free: `newt help` == interactive `/help`,
# `newt help <cmd>` == interactive `/<cmd> --help`. No backend required.
drive() { "$NEWT" help ${1:+"$1"} 2>/dev/null; }
# Count /dgx <sub> DETAIL lines (a subcommand name followed by a space).
count_subs() { grep -cE "/dgx (${SUBS}) " <<<"${1:-}" || true; }

top="$(drive)"
dgx="$(drive dgx)"
top_subs="$(count_subs "$top")"
dgx_subs="$(count_subs "$dgx")"

rolled_up=$([ "${top_subs:-9}" -le 1 ] && echo true || echo false)
disclosure=$([ "${dgx_subs:-0}" -ge 5 ] && echo true || echo false)
pass=$([ "$rolled_up" = true ] && [ "$disclosure" = true ] && echo true || echo false)

# Machine-readable result (stdout) — collected into the A/B data sets.
printf '{"issue":548,"top_dgx_subs":%s,"dgx_help_subs":%s,"rolled_up":%s,"disclosure":%s,"pass":%s}\n' \
  "${top_subs:-null}" "${dgx_subs:-null}" "$rolled_up" "$disclosure" "$pass"

# Human-readable (stderr).
{
  echo "  top-level /help : ${top_subs} /dgx subcommand line(s)  (rolled up ⇒ <= 1)"
  echo "  /dgx help       : ${dgx_subs} /dgx subcommand line(s)  (disclosure ⇒ >= 5)"
  if [ "$pass" = true ]; then
    echo "RESULT: PASS — /dgx is rolled up at the top level and /dgx help still expands it."
  else
    echo "RESULT: FAIL — #548 not implemented (top-level still lists ${top_subs} /dgx subcommands; rollup absent)."
  fi
} >&2

[ "$pass" = true ]
