"""Validate release coverage and dependency order from Cargo metadata."""

import json
from pathlib import Path
import sys
import urllib.error
import urllib.request

import yaml


def check(packages, matrix):
    """Return actionable errors without dropping renamed or optional dependencies."""
    by_name = {p["name"]: p for p in packages}
    positions = {name: i for i, name in enumerate(matrix)}
    errors = []
    if len(positions) != len(matrix):
        errors.append("duplicate crate in publish matrix")
    for name, package in by_name.items():
        if package.get("publish") != [] and name not in positions:
            errors.append(
                f"'{name}' is publishable but missing from the publish matrix"
            )
    for name in matrix:
        if name not in by_name:
            errors.append(f"'{name}' is not a workspace package")
            continue
        package = by_name[name]
        if package.get("publish") == []:
            errors.append(f"'{name}' is marked publish = false")
        for dep in package["dependencies"]:
            if dep.get("kind") == "dev" and dep["req"] == "*":
                continue  # Cargo strips path-only test helpers; pinning can create cycles.
            target = dep["name"]  # Actual package name, not a local dependency alias.
            if dep["req"] == "*":
                errors.append(f"'{name}' has an unversioned dependency on '{target}'")
            if target not in by_name:
                continue
            expected = "=" + by_name[target]["version"]
            if dep["req"] != expected:
                errors.append(f"'{name}' must pin '{target}' to {expected}")
            if target not in positions:
                errors.append(f"'{name}' depends on missing '{target}'")
            elif positions[target] >= positions[name]:
                errors.append(f"'{name}' is published before dependency '{target}'")
    return errors


def git_dependencies(packages, matrix):
    """(crate, dependency, req) for every shipped git dependency of a published crate.

    `cargo publish` drops the git source and resolves `req` on crates.io, so a
    git-only dependency blocks the upload (v0.8.0: newtui was never on crates.io).
    """
    published = set(matrix)
    return [
        (p["name"], d["name"], d["req"])
        for p in packages
        if p["name"] in published
        for d in p["dependencies"]
        if d.get("kind") != "dev" and (d.get("source") or "").startswith("git+")
    ]


def _version(text):
    return tuple(int(part) for part in text.split("."))


def caret_matches(req, version):
    """Cargo caret semantics for a plain release version; prereleases never match."""
    if "-" in version or not req.startswith("^"):
        return False
    base = req[1:].split(".")
    have = _version(version)
    if have < _version(".".join(base + ["0"] * (3 - len(base)))):
        return False
    # Same components up to and including the leftmost non-zero one.
    for i, part in enumerate(base):
        if have[i] != int(part):
            return False
        if part != "0" or i == len(base) - 1:
            return True
    return True


def registry_gaps(git_deps, versions_of):
    """Errors for git dependencies whose `req` no crates.io release satisfies."""
    errors = []
    for crate, dep, req in git_deps:
        if not any(caret_matches(req, v) for v in versions_of(dep)):
            errors.append(
                f"'{crate}' depends on '{dep}' {req} from git, but crates.io has no "
                f"release matching it; publish '{dep}' to crates.io first"
            )
    return errors


def crates_io_versions(name):
    """Non-yanked versions from the crates.io sparse index."""
    lower = name.lower()
    if len(lower) <= 2:
        path = f"{len(lower)}/{lower}"
    elif len(lower) == 3:
        path = f"3/{lower[0]}/{lower}"
    else:
        path = f"{lower[:2]}/{lower[2:4]}/{lower}"
    try:
        with urllib.request.urlopen(f"https://index.crates.io/{path}") as resp:
            lines = resp.read().decode().splitlines()
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return []
        raise
    entries = (json.loads(line) for line in lines if line)
    return [e["vers"] for e in entries if not e.get("yanked")]


def topological_order(packages):
    """Return every publishable package in dependency order, or identify a cycle."""
    names = {p["name"] for p in packages if p.get("publish") != []}
    remaining = {
        p["name"]: {
            d["name"]
            for d in p["dependencies"]
            if d["name"] in names and (d.get("kind") != "dev" or d["req"] != "*")
        }
        for p in packages
        if p["name"] in names
    }
    order = []
    while remaining:
        ready = sorted(n for n, deps in remaining.items() if deps <= set(order))
        if not ready:
            raise ValueError("dependency cycle among: " + ", ".join(sorted(remaining)))
        order.extend(ready)
        for name in ready:
            del remaining[name]
    return order


def main():
    metadata = (
        json.load(sys.stdin)
        if sys.argv[1] == "-"
        else json.loads(Path(sys.argv[1]).read_text())
    )
    workflow = yaml.safe_load(Path(".github/workflows/release.yml").read_text())
    matrix = workflow["jobs"]["publish-crates"]["strategy"]["matrix"]["crate"]
    errors = check(metadata["packages"], matrix)
    errors += registry_gaps(
        git_dependencies(metadata["packages"], matrix), crates_io_versions
    )
    for error in errors:
        print(f"::error::publish matrix: {error}")
    if errors:
        try:
            order = topological_order(metadata["packages"])
            print("A valid topological order would be:")
            for name in order:
                print(f"          - {name}")
        except ValueError as error:
            print(f"::error::{error}")
        return 1
    print(f"publish matrix OK: {len(matrix)} crates in topological order")
    return 0


if __name__ == "__main__":
    sys.exit(main())
