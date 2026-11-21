#!/usr/bin/env bash
# Stage the carried command pack beside the newt executable (#3151): the Unix
# counterpart of scripts/windows/Install-CarriedTools.ps1. Build with
# `cargo build -p newt-carried-tools --release`, then
#
#   scripts/unix/install-carried-tools.sh --binary target/release/newt-carried-tools \
#     --newt-directory <release-dir> [--ripgrep-archive <the pinned archive>]
#
# Nothing is staged unless the pinned upstream ripgrep archive verifies, and it
# never edits PATH or overwrites a live installation. The host's own commands
# stay where they are: newt puts `tools/` ahead of them on its shells' PATH.
# Only macOS is packaged: other Unix hosts usually ship GNU tools already.
set -euo pipefail

version=14.1.1
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd "$here/../.." && pwd)

die() { echo "install-carried-tools: $*" >&2; exit 2; }

binary="" destination="" archive=""
while [ $# -gt 0 ]; do
  case $1 in
    --binary) [ $# -ge 2 ] || die "--binary needs a value"; binary=$2; shift 2;;
    --newt-directory) [ $# -ge 2 ] || die "--newt-directory needs a value"; destination=$2; shift 2;;
    --ripgrep-archive) [ $# -ge 2 ] || die "--ripgrep-archive needs a value"; archive=$2; shift 2;;
    *) die "unknown option $1";;
  esac
done
[ -n "$binary" ] && [ -n "$destination" ] ||
  die "usage: --binary <newt-carried-tools> --newt-directory <dir> [--ripgrep-archive <file>]"
[ -f "$binary" ] || die "no such binary: $binary"
[ -d "$destination" ] || die "no such directory: $destination"
destination=$(cd "$destination" && pwd)
tools="$destination/tools"
[ ! -e "$tools" ] || die "tools already exists; stage into a fresh release directory."

# The pinned upstream ripgrep for this host. Foreign checksums, as the Windows
# installer pins its own, published beside each release asset.
case "$(uname -s)-$(uname -m)" in
  Darwin-arm64)
    triple=aarch64-apple-darwin
    sha256=24ad76777745fbff131c8fbc466742b011f925bfa4fffa2ded6def23b5b937be;;
  Darwin-x86_64)
    triple=x86_64-apple-darwin
    sha256=fc87e78f7cb3fea12d69072e7ef3b21509754717b746368fd40d88963630e2b3;;
  *) die "no pinned ripgrep for $(uname -s) $(uname -m); only macOS is packaged here";;
esac
name=ripgrep-$version-$triple

# A temporary locator, not an artifact identity. Only verified bytes reach tools/.
stage=$(mktemp -d "$destination/.carried-XXXXXX")
trap 'rm -rf "$stage"' EXIT

file="$stage/$name.tar.gz"
if [ -n "$archive" ]; then
  [ -f "$archive" ] || die "no such archive: $archive"
  cp "$archive" "$file"
else
  curl --fail --location --silent --show-error --retry 4 --retry-all-errors \
    --retry-max-time 180 --connect-timeout 20 --max-time 120 --output "$file" \
    "https://github.com/BurntSushi/ripgrep/releases/download/$version/$name.tar.gz" ||
    die "pinned ripgrep download failed after bounded retries"
fi
actual=$(shasum -a 256 "$file" | cut -d' ' -f1)
[ "$actual" = "$sha256" ] || die "ripgrep archive SHA256 is $actual, expected $sha256; nothing was staged."
tar -xzf "$file" -C "$stage"

pack="$stage/pack"
mkdir "$pack"
multicall="$pack/newt-carried-tools"
cp "$binary" "$multicall"
chmod 755 "$multicall"
for tool in cat head tail wc ls mkdir rm cp mv sort uniq tr cut echo date grep which; do
  # Invocation dispatches on the executable's name, so each is its own name:
  # a hardlink where the filesystem allows, else a copy.
  ln "$multicall" "$pack/$tool" 2>/dev/null || cp "$multicall" "$pack/$tool"
done
upstream="$stage/$name"
cp "$upstream/rg" "$pack/rg"
chmod 755 "$pack/rg"
for license in COPYING LICENSE-MIT UNLICENSE; do
  cp "$upstream/$license" "$pack/ripgrep-$license"
done
cp "$repo/crates/newt-carried-tools/LICENSE-uutils" "$pack/"

mv "$pack" "$tools"
echo "Installed carried tools at $tools"
