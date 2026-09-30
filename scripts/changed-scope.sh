#!/usr/bin/env bash
#
# changed-scope.sh — classify a git pre-push ref range for issue #1098.
#
# Reads git's pre-push stdin format on stdin: one
#   "<local-ref> <local-sha> <remote-ref> <remote-sha>"
# line per ref (see githooks(5)). Prints exactly one line to stdout:
#
#   (nothing)   — a deletion, a tag push, or a docs-only change: no cargo
#                 gate is needed for this ref range.
#   ALL         — a workspace-wide file changed (Cargo.toml, Cargo.lock,
#                 rust-toolchain*, .cargo/**, vendor/**, build.rs) or a
#                 changed file could not be attributed to any crate: test
#                 everything.
#   <crate> ... — the space-separated set of crates that OWN a changed
#                 file. The caller is expected to widen this with
#                 `cargo nextest run -E 'rdeps(<crate>) + rdeps(<crate>) + ...'`
#                 so reverse dependents are covered too — this script only
#                 does the file → owning-crate mapping.
#
# A new branch (remote sha all-zero) diffs against `git merge-base
# <local-sha> origin/main`, matching what actually reaches main.
#
# PIPELINE PARITY: consumed by .githooks/pre-push. When this script's
# classification rules change, update that hook's comment header to match.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

ZERO="0000000000000000000000000000000000000000"
changed=""
saw_ref=0

while read -r local_ref local_sha remote_ref remote_sha; do
    [ -z "${local_sha:-}" ] && continue
    [ "$local_sha" = "$ZERO" ] && continue              # branch/tag deletion
    case "$local_ref" in refs/tags/*) continue ;; esac  # tag push: no code gate

    if [ "$remote_sha" = "$ZERO" ]; then
        base="$(git merge-base "$local_sha" origin/main 2>/dev/null || true)"
        [ -z "$base" ] && base="origin/main"            # no common ancestor: fail safe to whole diff
        range="$base..$local_sha"
    else
        range="$remote_sha..$local_sha"
    fi
    saw_ref=1
    changed="$changed
$(git diff --name-only "$range" 2>/dev/null || true)"
done

[ "$saw_ref" -eq 0 ] && exit 0

changed="$(printf '%s\n' "$changed" | sed '/^$/d' | sort -u)"
[ -z "$changed" ] && exit 0

# Any workspace-wide file forces ALL — these can change what every crate builds against.
while IFS= read -r f; do
    case "$f" in
        Cargo.toml|Cargo.lock|rust-toolchain|rust-toolchain.toml|.cargo/*|vendor/*|build.rs)
            echo ALL
            exit 0
            ;;
    esac
done <<EOF
$changed
EOF

# Drop docs-only files. If nothing code-shaped remains, no gate is needed.
code_changed="$(printf '%s\n' "$changed" | grep -Ev '(^|/)[^/]*\.(md|txt)$|^docs/|^LICENSE' || true)"
[ -z "$code_changed" ] && exit 0

# Map each remaining file to its owning crate: the longest manifest-dir
# prefix match, per `cargo metadata`. No Cargo.toml at all (or any other
# metadata failure) fails closed to ALL rather than feeding empty/invalid
# JSON to the parser below.
metadata_json="$(cargo metadata --no-deps --format-version 1 2>/dev/null || true)"
if [ -z "$metadata_json" ]; then
    echo ALL
    exit 0
fi

manifest_dirs="$(printf '%s' "$metadata_json" | python3 -c '
import json, sys
data = json.load(sys.stdin)
for pkg in data["packages"]:
    manifest_dir = pkg["manifest_path"].rsplit("/", 1)[0]
    print(manifest_dir + "\t" + pkg["name"])
' 2>/dev/null || true)"

if [ -z "$manifest_dirs" ]; then
    echo ALL
    exit 0
fi

root="$PWD"
crates="$(printf '%s\n' "$code_changed" | while IFS= read -r f; do
    abs="$root/$f"
    best_len=-1
    best_name=""
    while IFS="$(printf '\t')" read -r dir name; do
        case "$abs" in
            "$dir"/*)
                if [ ${#dir} -gt "$best_len" ]; then
                    best_len=${#dir}
                    best_name="$name"
                fi
                ;;
        esac
    done <<EOF2
$manifest_dirs
EOF2
    echo "$best_name"
done)"

# Any file that didn't map to a crate (a root-level script, a justfile, a
# hook) is treated as build-affecting: fail closed to ALL.
if printf '%s\n' "$crates" | grep -qx ''; then
    echo ALL
    exit 0
fi

printf '%s\n' "$crates" | sort -u | tr '\n' ' ' | sed 's/ $/\n/'
