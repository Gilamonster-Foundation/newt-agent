#!/usr/bin/env python3
"""reverse_deps.py — expand a crate set to its TRANSITIVE reverse dependents.

Issue #1098 fix-first review (finding P2 #1): `.githooks/pre-push` fed the
owning-crate set straight to `cargo clippy -p ...` and the no-nextest `cargo
test -p ...` fallback, so a change to a leaf crate never gated whatever
depends on it -- only nextest's own `rdeps()` filterset expanded the set.
This script computes the same closure from `cargo metadata`, restricted to
workspace members (with dependency edges, not `--no-deps`), so clippy and
the cargo-test fallback see the identical set nextest computes.

Usage: reverse_deps.py <crate> [<crate> ...]
Prints the space-separated closure (inputs included) to stdout, sorted.
An empty argv prints nothing and exits 0.
"""
import json
import subprocess
import sys


def transitive_reverse_dependents(crates, metadata):
    members = set(metadata["workspace_members"])
    id_to_name = {}
    name_to_id = {}
    for pkg in metadata["packages"]:
        if pkg["id"] in members:
            id_to_name[pkg["id"]] = pkg["name"]
            name_to_id[pkg["name"]] = pkg["id"]

    # reverse[X] = packages that depend ON X (workspace members only)
    reverse = {pid: [] for pid in members}
    for node in metadata["resolve"]["nodes"]:
        if node["id"] not in members:
            continue
        for dep_id in node["dependencies"]:
            if dep_id in members:
                reverse[dep_id].append(node["id"])

    seen = {name_to_id[name] for name in crates if name in name_to_id}
    queue = list(seen)
    while queue:
        current = queue.pop()
        for dependent in reverse.get(current, []):
            if dependent not in seen:
                seen.add(dependent)
                queue.append(dependent)

    return sorted(id_to_name[pid] for pid in seen)


def main(argv):
    crates = argv[1:]
    if not crates:
        return 0
    metadata = json.loads(
        subprocess.run(
            ["cargo", "metadata", "--format-version", "1"],
            check=True, capture_output=True, text=True,
        ).stdout
    )
    print(" ".join(transitive_reverse_dependents(crates, metadata)))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
