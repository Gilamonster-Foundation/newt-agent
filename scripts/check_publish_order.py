"""Validate release coverage and dependency order from Cargo metadata."""

import json
from pathlib import Path
import sys

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
