#!/usr/bin/env bash
# Refuse a newt binary that cannot exec inside the oldest task image.
# A binary that fails at exec turns every trial into a silent reward-0 with no
# events -- a run that "completes" having measured nothing. Observed
# 2026-09-11: a host-built newt needed GLIBC_2.39; three of eight task images
# are Debian bookworm at 2.36 and refused it at exec. Override the floor only if
# the task set's oldest base image really is newer (NEWT_BENCH_GLIBC_FLOOR).
# Usage: glibc-floor.sh <newt binary>
set -euo pipefail
BIN="${1:?newt binary}"
FLOOR="${NEWT_BENCH_GLIBC_FLOOR:-2.36}"
NEED="$(objdump -T "$BIN" 2>/dev/null | grep -oE 'GLIBC_[0-9.]+' | sed 's/GLIBC_//' | sort -V | tail -1)"
if [ -n "$NEED" ] && [ "$(printf '%s\n%s\n' "$FLOOR" "$NEED" | sort -V | tail -1)" != "$FLOOR" ]; then
  echo "refusing: $BIN requires GLIBC_$NEED but the task-image floor is $FLOOR;" >&2
  echo "          build it against the oldest base image (e.g. rust:<msrv>-bookworm) or raise NEWT_BENCH_GLIBC_FLOOR." >&2
  exit 2
fi
