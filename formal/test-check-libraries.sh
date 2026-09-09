#!/usr/bin/env bash
# Regression tests for the anti-vacuous guard (formal/check-libraries.sh).
#
# The guard exists to catch a manifest that silently stops building libraries
# whose proofs still exist. Case 2 below IS that regression, reduced to a
# fixture: eleven proof sources on disk, only two declared. The guard this
# replaces PASSED that fixture — it read its expectations out of the same
# manifest it was checking, so deleting nine entries deleted nine checks.
#
# Run: formal/test-check-libraries.sh
set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
check="$here/check-libraries.sh"
pass=0 fail=0

ok()  { printf 'ok   - %s\n' "$1"; pass=$((pass+1)); }
bad() { printf 'FAIL - %s\n' "$1"; fail=$((fail+1)); }

# The nine libraries on main plus the two this branch adds.
ALL_LIBS="CaveatLattice ProjectModel NewtPolicy CompactionProvenance
          CompactionLifecycle CompactionSpill ResponsesUsage ResponsesWire
          NewtInteraction ContextOps ContentAddressed"

# fixture <dir> <declared...> -- a git repo with ALL_LIBS as tracked sources,
# only the named libraries declared, and an olean for each declared library.
fixture() {
  local dir="$1"; shift
  mkdir -p "$dir/.lake/build/lib"
  ( cd "$dir"
    git init -q .
    git config user.email t@e.invalid; git config user.name t
    for lib in $ALL_LIBS; do
      printf 'import %s.Basic\n' "$lib" > "$lib.lean"
      mkdir -p "$lib"; printf 'namespace %s\nend %s\n' "$lib" "$lib" > "$lib/Basic.lean"
    done
    git add -A >/dev/null; git commit -qm fixture >/dev/null
    printf 'name = "fixture"\n\n' > lakefile.toml
    for lib in "$@"; do
      printf '[[lean_lib]]\nname = "%s"\n\n' "$lib" >> lakefile.toml
      touch ".lake/build/lib/$lib.olean"
    done )
}

# expect <0|nonzero> <desc> <dir>
expect() {
  local want="$1" desc="$2" dir="$3"
  FORMAL_DIR="$dir" bash "$check" >/dev/null 2>&1; local got=$?
  if [ "$want" = nonzero ]; then
    [ "$got" -ne 0 ] && ok "$desc (exit $got)" || bad "$desc — expected nonzero, got 0"
  else
    [ "$got" -eq 0 ] && ok "$desc" || bad "$desc — expected 0, got $got"
  fi
}

tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT

# 1. Happy path: every source declared and built.
fixture "$tmp/all" $ALL_LIBS
expect 0 "11 sources / 11 declared / 11 oleans passes" "$tmp/all"

# 2. THE REGRESSION. Nine libraries dropped from the manifest; their proof
#    sources are untouched on disk. The old guard passed this.
fixture "$tmp/dropped" ContextOps ContentAddressed
expect nonzero "9 libraries dropped from the manifest is CAUGHT" "$tmp/dropped"

# 3. A declared library that produced no olean fails (the one check the old
#    guard did get right — kept, so the fix does not regress it).
fixture "$tmp/noolean" $ALL_LIBS
rm -f "$tmp/noolean/.lake/build/lib/ResponsesWire.olean"
expect nonzero "declared library with no olean fails" "$tmp/noolean"

# 4. A manifest naming a library with no proof source fails.
fixture "$tmp/ghost" $ALL_LIBS
printf '[[lean_lib]]\nname = "GhostLib"\n\n' >> "$tmp/ghost/lakefile.toml"
touch "$tmp/ghost/.lake/build/lib/GhostLib.olean"
expect nonzero "declared library with no source fails" "$tmp/ghost"

# 5. A DELIBERATE removal, recorded in unbuilt-libraries.txt, is allowed.
fixture "$tmp/exempt" CaveatLattice ProjectModel NewtPolicy CompactionProvenance \
        CompactionLifecycle CompactionSpill ResponsesUsage ResponsesWire NewtInteraction ContextOps
printf '# why: fixture\nContentAddressed\n' > "$tmp/exempt/unbuilt-libraries.txt"
expect 0 "documented exemption permits a deliberate removal" "$tmp/exempt"

# 6. ...but it must not silence the bulk drop: exempting one of nine still fails.
fixture "$tmp/exempt-partial" ContextOps ContentAddressed
printf '# why: fixture\nResponsesWire\n' > "$tmp/exempt-partial/unbuilt-libraries.txt"
expect nonzero "one exemption does not excuse the other eight" "$tmp/exempt-partial"

# 7. A stale exemption (naming a library the manifest DOES declare) fails, so
#    the escape hatch cannot quietly rot into a permanent hole.
fixture "$tmp/stale" $ALL_LIBS
printf '# why: fixture\nCaveatLattice\n' > "$tmp/stale/unbuilt-libraries.txt"
expect nonzero "stale exemption of a declared library fails" "$tmp/stale"

# 8. An exemption for a library with no proof source at all fails.
fixture "$tmp/phantom" $ALL_LIBS
printf '# why: fixture\nNeverExisted\n' > "$tmp/phantom/unbuilt-libraries.txt"
expect nonzero "exemption naming a nonexistent source fails" "$tmp/phantom"

# 9. The positive read assertion: a tree with no tracked library roots must
#    FAIL, not pass vacuously. This is the failure mode that killed the guard
#    being replaced — an absence-check that read nothing reports no absences.
empty="$tmp/empty"; mkdir -p "$empty/.lake/build/lib"
( cd "$empty"; git init -q .; git config user.email t@e.invalid; git config user.name t
  printf 'name = "fixture"\n\n[[lean_lib]]\nname = "X"\n' > lakefile.toml
  git add -A >/dev/null; git commit -qm empty >/dev/null )
expect nonzero "no tracked library roots fails (does not pass vacuously)" "$empty"

# 10. An empty manifest fails rather than checking nothing.
fixture "$tmp/nolibs"
expect nonzero "lakefile declaring no lean_lib fails" "$tmp/nolibs"

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
