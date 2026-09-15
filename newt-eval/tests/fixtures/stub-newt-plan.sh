#!/usr/bin/env bash
# Offline stand-in for `newt plan --one-shot` in the ratchet.sh crew-mode tests
# (newt-eval/tests/mock_e2e.rs, #2317). No inference: it lands the diff in
# $STUB_CREW_DIFF on a `crew/leaf-1` branch in --dir, where a crew leaf would.
# An empty diff file lands an empty leaf. With STUB_CREW_DIFF unset it lands
# nothing and prints nothing, which ratchet.sh reads as ERROR(infra).
set -euo pipefail
dir=""
while [ $# -gt 0 ]; do
  case "$1" in
    --dir) dir="$2"; shift 2;;
    *) shift;;
  esac
done
[ -n "${STUB_CREW_DIFF:-}" ] || exit 1
cd "$dir"
git checkout -q -b crew/leaf-1
[ -s "$STUB_CREW_DIFF" ] && git apply "$STUB_CREW_DIFF"
git add -A && git reset -q -- .plan.log
git -c user.email=stub@stub -c user.name=stub commit -q --allow-empty -m "stub crew leaf"
echo "stub crew: landed crew/leaf-1"
