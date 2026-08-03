#!/usr/bin/env bash
# Reproducible TLC runner for the behavioral constitution (epic #1529).
#
# Resolves a PINNED, checksum-verified tla2tools.jar, then model-checks each
# `<Name>.tla` that has a matching `<Name>.cfg` in this directory. Portable: uses
# a local jar if present, else downloads the pinned release into a cache and
# verifies its sha256 before running (so CI is reproducible and tamper-evident).
#
# Usage:  spec/tla/check.sh [Spec ...]     # default: every *.tla with a *.cfg
set -euo pipefail

# ── Pin (bump deliberately; update the checksum in lock-step) ────────────────
TLA2TOOLS_VERSION="1.7.4"
TLA2TOOLS_SHA256="936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88"
TLA2TOOLS_URL="https://github.com/tlaplus/tlaplus/releases/download/v${TLA2TOOLS_VERSION}/tla2tools.jar"

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cache="${XDG_CACHE_HOME:-$HOME/.cache}/newt-tla"

verify() { echo "${TLA2TOOLS_SHA256}  $1" | sha256sum -c --status; }

resolve_jar() {
  # 1) explicit override, 2) the conventional ~/opt install, 3) the cache,
  #    4) download the pinned release into the cache.
  for cand in "${TLA2TOOLS_JAR:-}" "$HOME/opt/tla2tools/tla2tools.jar" "$cache/tla2tools-${TLA2TOOLS_VERSION}.jar"; do
    [ -n "$cand" ] && [ -f "$cand" ] && verify "$cand" && { echo "$cand"; return; }
  done
  mkdir -p "$cache"
  local out="$cache/tla2tools-${TLA2TOOLS_VERSION}.jar"
  echo "[tla] fetching pinned tla2tools ${TLA2TOOLS_VERSION}…" >&2
  curl -fsSL "$TLA2TOOLS_URL" -o "$out"
  verify "$out" || { echo "[tla] CHECKSUM MISMATCH for $out — refusing to run." >&2; exit 2; }
  echo "$out"
}

jar="$(resolve_jar)"

specs=("$@")
if [ "${#specs[@]}" -eq 0 ]; then
  for cfg in "$here"/*.cfg; do
    [ -e "$cfg" ] || continue
    specs+=("$(basename "${cfg%.cfg}")")
  done
fi

rc=0
for spec in "${specs[@]}"; do
  cfg="$here/${spec}.cfg"
  [ -f "$cfg" ] || { echo "[tla] no ${spec}.cfg — skipping"; continue; }
  echo "[tla] TLC checking ${spec}…"
  # -XX:+UseParallelGC is the GC TLC recommends; run inside the spec dir.
  ( cd "$here" && java -XX:+UseParallelGC -cp "$jar" tlc2.TLC -config "${spec}.cfg" "${spec}.tla" ) || rc=1
done
exit "$rc"
