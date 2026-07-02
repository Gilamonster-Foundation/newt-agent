#!/usr/bin/env bash
# install.sh — reconstitute the runnable Claude Code workflows from the
# canonical in-repo templates (issue #805).
#
# The *.workflow.js files beside this script are the SOURCE OF TRUTH for the
# eval analysis workflows. `.claude/workflows/` is a GENERATED artifact
# (gitignored): on any fresh machine, clone the repo and run this script to
# reconstitute the /commands. Claude Code is one renderer of the methodology,
# not a dependency — the method itself (taxonomy, constraints, statistics) is
# documented tool-neutrally in README.md and executable by hand or by a
# future newt-native orchestrator.
#
# Usage:
#   scripts/eval/workflows/install.sh            # -> <repo>/.claude/workflows/
#   scripts/eval/workflows/install.sh --user     # -> ~/.claude/workflows/
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
DEST="$REPO/.claude/workflows"
[ "${1:-}" = "--user" ] && DEST="$HOME/.claude/workflows"

mkdir -p "$DEST"
n=0
for tpl in "$HERE"/*.workflow.js; do
  [ -e "$tpl" ] || { echo "install: no *.workflow.js templates in $HERE" >&2; exit 1; }
  name="$(basename "$tpl" .workflow.js)"
  cp "$tpl" "$DEST/$name.js"
  n=$((n + 1))
  echo "installed /$name -> $DEST/$name.js"
done
echo "install: $n workflow(s) reconstituted into $DEST"
