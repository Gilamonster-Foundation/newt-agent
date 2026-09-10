#!/usr/bin/env bash
# ANTI-VACUOUS guard for the machine-checked core (`formal/`).
#
# `lake build` on a mis-configured package exits 0 having checked nothing, so a
# green tick is not by itself evidence that any theorem was proved. This script
# is the evidence.
#
# THE MISTAKE THIS REPLACES. The first version of this guard derived its
# expected library list from the SAME `lakefile.toml` it then checked:
#
#     libs=$(grep -A1 '^\[\[lean_lib\]\]' lakefile.toml | sed -n 's/name = "\(.*\)"/\1/p')
#     for lib in $libs; do find .lake/build -name "$lib.olean" | grep -q . || exit 1; done
#
# Drop nine libraries from the manifest and that loop checks the remaining two,
# finds them, and exits 0 — passing the exact regression it was written to
# catch. An expectation read out of the artifact under test is not a check.
#
# THE FIX. Three INDEPENDENT sources, compared pairwise:
#
#   S — proof sources:  the git-TRACKED `<Name>.lean` library roots on disk.
#                       Ground truth; a manifest edit cannot move it.
#   D — declared:       `[[lean_lib]]` entries in lakefile.toml.
#   B — built:          `<Name>.olean` artifacts under .lake/build.
#
#   S \ D  ⇒ ERROR  a library's proofs exist but nothing builds them  ← the regression
#   D \ S  ⇒ ERROR  the manifest names a library with no source
#   D \ B  ⇒ ERROR  a declared library produced no olean
#   |S| = 0 ⇒ ERROR  the scan read nothing (an absence-check that never read
#                    anything fails OPEN, which is how this class of guard dies)
#
# DELIBERATE REMOVAL. To take a library out of the build on purpose, list it in
# `formal/unbuilt-libraries.txt` (see that file's header). Exemptions are echoed
# as CI warnings on every run, and a stale exemption — one naming a library that
# IS declared — is an error, so the file cannot quietly rot.
#
# Usage: formal/check-libraries.sh
# Env:   FORMAL_DIR — directory to check (default: this script's dir; for tests)
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
formal_dir="${FORMAL_DIR:-$here}"
readonly exempt_file="unbuilt-libraries.txt"

log()  { printf '[formal] %s\n' "$*" >&2; }
die()  { log "ERROR: $*"; printf '::error::%s\n' "$*"; exit 1; }
warn() { log "WARNING: $*"; printf '::warning::%s\n' "$*"; }

[ -d "$formal_dir" ] || die "no such directory: $formal_dir"
cd "$formal_dir"

# ── S: proof sources (git-tracked library roots) ────────────────────────────
# Tracked, so an untracked scratch file never gates CI — and a source file that
# was deleted along with its manifest entry correctly leaves the set.
sources="$(git ls-files -- '*.lean' \
           | grep -E '^[^/]+\.lean$' \
           | sed 's/\.lean$//' \
           | sort -u || true)"

# The positive read assertion. Without this the whole script degrades to "no
# discrepancies found" on an empty scan — passing loudest exactly when it has
# learned nothing.
[ -n "$sources" ] || die "found no tracked <Name>.lean library roots in $formal_dir — the scan read nothing, so it proves nothing"

# ── D: declared in the manifest ─────────────────────────────────────────────
[ -f lakefile.toml ] || die "no lakefile.toml in $formal_dir"
declared="$(grep -A1 '^\[\[lean_lib\]\]' lakefile.toml \
            | sed -n 's/^name *= *"\(.*\)"/\1/p' \
            | sort -u || true)"
[ -n "$declared" ] || die "lakefile.toml declares no [[lean_lib]] — nothing would be proved"

# ── E: deliberate, documented exemptions ────────────────────────────────────
exempt=""
if [ -f "$exempt_file" ]; then
  exempt="$(sed 's/#.*//' "$exempt_file" | tr -d '[:blank:]' | grep -v '^$' | sort -u || true)"
fi

has() { printf '%s\n' "$2" | grep -qx -- "$1"; }

# ── S \ D — the regression the old guard could not see ──────────────────────
missing_from_manifest=""
for lib in $sources; do
  has "$lib" "$declared" && continue
  if has "$lib" "$exempt"; then
    warn "$lib: proof source present but deliberately not built (listed in $exempt_file)"
    continue
  fi
  missing_from_manifest="$missing_from_manifest $lib"
done
[ -z "$missing_from_manifest" ] || die "proof sources exist but no [[lean_lib]] builds them:$missing_from_manifest — restore the manifest entries, or record the removal in $exempt_file"

# ── stale exemptions ────────────────────────────────────────────────────────
for lib in $exempt; do
  has "$lib" "$declared" \
    && die "$exempt_file exempts '$lib', but lakefile.toml declares it — delete the stale exemption"
  has "$lib" "$sources" \
    || die "$exempt_file exempts '$lib', which has no proof source — delete the stale exemption"
done

# ── D \ S ───────────────────────────────────────────────────────────────────
for lib in $declared; do
  has "$lib" "$sources" \
    || die "lakefile.toml declares '$lib' but there is no tracked $lib.lean library root"
done

# ── D \ B ───────────────────────────────────────────────────────────────────
for lib in $declared; do
  find .lake/build -name "$lib.olean" 2>/dev/null | grep -q . \
    || die "declared library '$lib' produced no olean — the build did not check it"
done

n_src=$(printf '%s\n' "$sources" | wc -l | tr -d '[:blank:]')
n_dec=$(printf '%s\n' "$declared" | wc -l | tr -d '[:blank:]')
log "OK — $n_src tracked library roots, all declared and all built ($n_dec oleans)."
printf 'formal-libraries-checked=%d\n' "$n_dec"
printf '::notice title=Lean::%d libraries proved: %s\n' "$n_dec" "$(printf '%s ' $declared)"
