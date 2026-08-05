#!/usr/bin/env bash
# Regression tests for the fail-closed TLC runner (spec/tla/check.sh).
# Proves a green exit means specs were actually checked, and every degenerate
# input fails closed. Run: spec/tla/test-check.sh
set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
check="$here/check.sh"
pass=0 fail=0

ok()   { printf 'ok   - %s\n' "$1"; pass=$((pass+1)); }
bad()  { printf 'FAIL - %s\n' "$1"; fail=$((fail+1)); }

# expect_exit <expected> <desc> -- <cmd...>
expect_exit() {
  local want="$1" desc="$2"; shift 3   # drop the "--"
  "$@" >/dev/null 2>&1; local got=$?
  if [ "$want" = "nonzero" ]; then
    [ "$got" -ne 0 ] && ok "$desc (exit $got)" || bad "$desc (expected nonzero, got 0)"
  else
    [ "$got" -eq "$want" ] && ok "$desc" || bad "$desc (expected $want, got $got)"
  fi
}

# 1. Happy path: the real Smoke spec checks clean (invokes TLC).
expect_exit 0 "Smoke checks clean" -- bash "$check" Smoke

# 2. A requested spec that does not exist fails.
expect_exit nonzero "requested nonexistent spec fails" -- bash "$check" DoesNotExist

# 3. Unsafe / path-traversal spec names are rejected.
expect_exit nonzero "path-traversal spec name rejected" -- bash "$check" ../Smoke
expect_exit nonzero "slash in spec name rejected" -- bash "$check" a/b

# 4. Default discovery in an EMPTY dir fails (no false success).
empty="$(mktemp -d)"; trap 'rm -rf "$empty"' EXIT
expect_exit nonzero "empty dir default-discovery fails" -- \
  env TLA_SPEC_DIR="$empty" bash "$check"

# 5. A .cfg with no matching .tla is NOT a checkable pair → default discovery fails.
cfgonly="$(mktemp -d)"; touch "$cfgonly/Foo.cfg"
expect_exit nonzero "Foo.cfg without Foo.tla → no pair, fails" -- \
  env TLA_SPEC_DIR="$cfgonly" bash "$check"
rm -rf "$cfgonly"

# 6. A .tla with no .cfg, requested explicitly → fails.
tlaonly="$(mktemp -d)"; printf '%s\n' '---- MODULE Foo ----' '====' > "$tlaonly/Foo.tla"
expect_exit nonzero "requested Foo without Foo.cfg fails" -- \
  env TLA_SPEC_DIR="$tlaonly" bash "$check" Foo
rm -rf "$tlaonly"

# 7. A wrong-version $TLA2TOOLS_JAR is refused (not silently run).
badjar="$(mktemp)"; printf 'not a jar' > "$badjar"
expect_exit nonzero "wrong-version \$TLA2TOOLS_JAR refused" -- \
  env TLA2TOOLS_JAR="$badjar" bash "$check" Smoke
rm -f "$badjar"

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
