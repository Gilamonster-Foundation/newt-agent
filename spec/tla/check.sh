#!/usr/bin/env bash
# Reproducible, FAIL-CLOSED TLC runner for the behavioral constitution (#1529).
#
# Resolves a PINNED, checksum-verified tla2tools.jar, then model-checks each
# requested `<Name>.tla` (which must have a matching `<Name>.cfg`). Invariants
# (deliberately fail-closed — a green exit must mean specs were actually checked):
#   * an explicitly-requested spec with no .tla or no .cfg is an ERROR;
#   * default discovery finding zero complete <Name>.{tla,cfg} pairs is an ERROR;
#   * a TLC failure is an ERROR; a skipped spec never yields success;
#   * at least one spec must be checked before exit 0; the count is printed;
#   * unsafe spec names (path separators / traversal) are rejected;
#   * the jar checksum is enforced for EVERY source (env / local / cache /
#     download); a locally-supplied jar of another version is refused, not run.
#
# Usage:  spec/tla/check.sh [Spec ...]     # default: every complete pair in dir
# Env:    TLA2TOOLS_JAR  — explicit jar (still checksum-enforced)
#         TLA_SPEC_DIR   — spec directory (default: this script's dir; for tests)
set -euo pipefail

# ── Pin (bump deliberately; update the checksum in lock-step) ────────────────
readonly TLA2TOOLS_VERSION="1.7.4"
readonly TLA2TOOLS_SHA256="936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88"
readonly TLA2TOOLS_URL="https://github.com/tlaplus/tlaplus/releases/download/v${TLA2TOOLS_VERSION}/tla2tools.jar"

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
spec_dir="${TLA_SPEC_DIR:-$here}"
cache="${XDG_CACHE_HOME:-$HOME/.cache}/newt-tla"
readonly cached_jar="$cache/tla2tools-${TLA2TOOLS_VERSION}.jar"

log() { printf '[tla] %s\n' "$*" >&2; }
die() { log "ERROR: $*"; exit "${2:-2}"; }

# ── Portable sha256 (sha256sum OR shasum -a 256) ────────────────────────────
sha256_hex() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | awk '{print $1}'
  else die "no sha256 tool found (need 'sha256sum' or 'shasum -a 256')" 3; fi
}
matches_pin() { [ "$(sha256_hex "$1")" = "$TLA2TOOLS_SHA256" ]; }

# ── Atomic, checksum-verified download into the cache ───────────────────────
download_jar() {
  mkdir -p "$cache"
  local tmp
  tmp="$(mktemp "$cache/.tla2tools.XXXXXX")"
  # Clean the partial on ANY exit from this function (SIGINT/failure included).
  trap 'rm -f "$tmp"' RETURN
  log "fetching pinned tla2tools ${TLA2TOOLS_VERSION} …"
  command -v curl >/dev/null 2>&1 || die "curl not found; cannot fetch tla2tools" 2
  curl -fsSL "$TLA2TOOLS_URL" -o "$tmp" || die "download failed from $TLA2TOOLS_URL"
  matches_pin "$tmp" || die "downloaded jar checksum MISMATCH — refusing"
  # Atomic publish: a concurrent reader sees either the old jar or the fully
  # verified new one, never a partial.
  mv -f "$tmp" "$cached_jar"
  trap - RETURN
  printf '%s\n' "$cached_jar"
}

resolve_jar() {
  # Sweep any orphaned partials from an interrupted prior run.
  rm -f "$cache"/.tla2tools.* 2>/dev/null || true

  # 1) $TLA2TOOLS_JAR — explicit, so a wrong-version jar is a HARD failure
  #    (never silently accept another version).
  if [ -n "${TLA2TOOLS_JAR:-}" ]; then
    [ -f "$TLA2TOOLS_JAR" ] || die "\$TLA2TOOLS_JAR=$TLA2TOOLS_JAR does not exist"
    matches_pin "$TLA2TOOLS_JAR" \
      || die "\$TLA2TOOLS_JAR is not the pinned tla2tools ${TLA2TOOLS_VERSION} (checksum mismatch) — refusing"
    printf '%s\n' "$TLA2TOOLS_JAR"; return 0
  fi
  # 2) conventional local install (soft: ignore if mismatched)
  local local_install="$HOME/opt/tla2tools/tla2tools.jar"
  if [ -f "$local_install" ] && matches_pin "$local_install"; then
    printf '%s\n' "$local_install"; return 0
  fi
  # 3) cache (reject + remove a corrupt cached jar, then re-download)
  if [ -f "$cached_jar" ]; then
    if matches_pin "$cached_jar"; then printf '%s\n' "$cached_jar"; return 0; fi
    log "cached jar failed checksum — discarding and re-fetching"
    rm -f "$cached_jar"
  fi
  # 4) atomic, verified download
  download_jar
}

# ── Collect specs (fail-closed) ─────────────────────────────────────────────
specs=()
if [ "$#" -gt 0 ]; then
  for s in "$@"; do
    case "$s" in
      "" | */* | *\\* | .. | *..*) die "invalid spec name: '$s' (no path separators or traversal)" ;;
    esac
    [ -f "$spec_dir/$s.tla" ] || die "requested spec '$s' has no $spec_dir/$s.tla"
    [ -f "$spec_dir/$s.cfg" ] || die "requested spec '$s' has no $spec_dir/$s.cfg"
    specs+=("$s")
  done
else
  shopt -s nullglob
  for cfg in "$spec_dir"/*.cfg; do
    name="$(basename "${cfg%.cfg}")"
    [ -f "$spec_dir/$name.tla" ] && specs+=("$name")
  done
  [ "${#specs[@]}" -gt 0 ] \
    || die "no complete <Name>.tla + <Name>.cfg pair found in $spec_dir"
fi

jar="$(resolve_jar)"
[ -n "$jar" ] && [ -f "$jar" ] || die "could not resolve a verified tla2tools.jar"
log "using tla2tools ${TLA2TOOLS_VERSION}: $jar"

# ── Check each spec; a failure or a skip must NOT produce overall success ───
checked=0
for spec in "${specs[@]}"; do
  log "TLC checking ${spec}.tla …"
  # -XX:+UseParallelGC is TLC's recommended GC; run inside the spec dir.
  ( cd "$spec_dir" && java -XX:+UseParallelGC -cp "$jar" tlc2.TLC \
      -config "${spec}.cfg" "${spec}.tla" ) || die "TLC FAILED on ${spec}" 1
  checked=$((checked + 1))
done

[ "$checked" -gt 0 ] || die "no specifications were checked"
log "OK — ${checked} specification(s) checked with tla2tools ${TLA2TOOLS_VERSION} (TLC)."
printf 'tla-checked-count=%d\n' "$checked"
