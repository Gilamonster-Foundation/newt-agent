#!/usr/bin/env bash
# Grade a completed autonomous eval RUN end-to-end (not just the live binary).
#
# Given the throwaway checkout an eval ran in + the base SHA it forked from, this:
#   1. finds the final consolidated crew branch (leaf chaining lands them in order),
#   2. builds `newt` from that branch (warm CARGO_TARGET_DIR),
#   3. runs the behavioral grader (grade-548.sh) on that binary  ← the real grade,
#   4. summarizes what the run actually changed (diff stat + did it touch the real
#      help_lines seam in newt-tui/src/help.rs?).
#
# Output: a JSON line on stdout (the run's behavioral grade, augmented with
# run-shape fields) + a human report on stderr.
#
# Usage:  grade-run.sh <throwaway-dir> <base-sha> <warm-cargo-target-dir>
set -uo pipefail

THROW="${1:?usage: grade-run.sh <throwaway-dir> <base-sha> <target-dir>}"
BASE="${2:?need base sha}"
TARGET="${3:?need CARGO_TARGET_DIR}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

git -C "$THROW" rev-parse --verify "$BASE" >/dev/null 2>&1 || {
  echo "grade-run: base sha '$BASE' not found in $THROW" >&2
  exit 2
}

FINAL="$(git -C "$THROW" branch --format='%(refname:short)' | grep '^crew/' | tail -1)"
if [ -z "$FINAL" ]; then
  echo "grade-run: no crew/* branch in $THROW (run produced no leaves)" >&2
  echo '{"graded":false,"reason":"no crew branch"}'
  exit 0
fi

# Run-shape facts (independent of building).
n_leaves="$(git -C "$THROW" branch --list 'crew/*' | wc -l | tr -d ' ')"
files_changed="$(git -C "$THROW" diff --name-only "$BASE".."$FINAL" | tr '\n' ',' | sed 's/,$//')"
help_lines_delta="$(git -C "$THROW" diff "$BASE".."$FINAL" -- newt-tui/src/help.rs | grep -cE '^[-+]' || true)"

# Build newt from the consolidated result, then behaviorally grade it.
git -C "$THROW" checkout -q "$FINAL" 2>/dev/null
if CARGO_TARGET_DIR="$TARGET" cargo build -q --manifest-path "$THROW/Cargo.toml" -p newt-agent >/dev/null 2>&1; then
  BIN="$TARGET/debug/newt"
  grade_json="$("$HERE/grade-548.sh" "$BIN" 2>/dev/null)"
  built=true
else
  grade_json='{"issue":548,"pass":false,"note":"final branch failed to build"}'
  built=false
fi

# Merge run-shape into the grade JSON (no jq dependency — string splice).
core="${grade_json%\}}"
printf '%s,"built":%s,"leaves":%s,"help_lines_delta":%s,"files":"%s"}\n' \
  "$core" "$built" "${n_leaves:-0}" "${help_lines_delta:-0}" "${files_changed:-}"

{
  echo "  final branch : $FINAL"
  echo "  leaves       : ${n_leaves}"
  echo "  files changed: ${files_changed:-<none>}"
  echo "  help_lines Δ : ${help_lines_delta} line(s) in newt-tui/src/help.rs"
  echo "  built ok     : ${built}"
} >&2
