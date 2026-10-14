"""Validate release coverage and dependency order from Cargo metadata."""

import json
import os
from pathlib import Path
import re
import sys
import tomllib
import urllib.error
import urllib.request

import yaml

# Release jobs that upload to a registry, and the jobs that can fail without
# uploading. An upload cannot be taken back, so every publish job must reach
# every gate through `needs:` (#2873: v0.8.0 half-published twice).
PUBLISH_JOBS = ("publish-crates", "build-npm", "publish-npm-meta", "publish-pypi")
RELEASE_GATES = (
    "setup-acceptance",
    "mcp-import-acceptance",
    "build-binaries",
    "build-wheels",
    "build-sdist",
    "check-publish-order",
    "npm-preflight",
)


def gate_errors(jobs):
    """Errors for publish jobs that can start before a release gate has passed."""
    errors = []
    for job in PUBLISH_JOBS:
        if job not in jobs:
            errors.append(f"publish job '{job}' is missing from release.yml")
            continue
        seen, todo = set(), [job]
        while todo:
            needs = jobs.get(todo.pop(), {}).get("needs", [])
            for need in [needs] if isinstance(needs, str) else needs:
                if need not in seen:
                    seen.add(need)
                    todo.append(need)
        for gate in RELEASE_GATES:
            if gate not in seen:
                errors.append(f"'{job}' can publish before gate '{gate}' passes")
    return errors


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


def _load_toml(path):
    with open(path, "rb") as f:
        return tomllib.load(f)


def _declares_version(package, dep, load, workspace_manifest):
    """Whether the dev-dependency's declaration carries a `version` key.

    Metadata reports `req: "*"` both for an omitted version (Cargo strips that
    dev helper) and for an explicit `version = "*"` (Cargo ships it), so only
    the declaration can tell them apart. Anything unreadable counts as
    versioned, which keeps the dependency checked (fail closed).
    """
    key = dep.get("rename") or dep["name"]
    try:
        manifest = load(package["manifest_path"])
        if dep.get("target"):
            manifest = manifest["target"][dep["target"]]
        table = manifest.get("dev-dependencies", manifest.get("dev_dependencies", {}))
        entry = table[key]
        if isinstance(entry, dict) and entry.get("workspace") is True:
            entry = load(workspace_manifest)["workspace"]["dependencies"][key]
    except (OSError, KeyError, TypeError, AttributeError, tomllib.TOMLDecodeError):
        return True
    return not isinstance(entry, dict) or "version" in entry


def git_dependencies(packages, matrix, load=_load_toml, workspace_manifest="Cargo.toml"):
    """(crate, dependency, req) for every shipped git dependency of a published crate.

    `cargo publish` drops the git source and resolves `req` on crates.io, so a
    git-only dependency blocks the upload (v0.8.0: newtui was never on crates.io).
    Cargo strips only a dev-dependency declared without a `version` key.
    """
    published = set(matrix)
    return [
        (p["name"], d["name"], d["req"])
        for p in packages
        if p["name"] in published
        for d in p["dependencies"]
        if (d.get("source") or "").startswith("git+")
        and not (
            d.get("kind") == "dev"
            and d["req"] == "*"
            and not _declares_version(p, d, load, workspace_manifest)
        )
    ]


_VERSION = re.compile(
    r"(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)"
    r"(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?(?:\+[0-9A-Za-z.-]+)?"
)
_COMPARATOR = re.compile(
    r"(=|>=|>|<=|<|~|\^)?\s*(\d+|[*xX])(?:\.(\d+|[*xX]))?(?:\.(\d+|[*xX]))?"
    r"(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?(?:\+[0-9A-Za-z.-]+)?"
)


def _pre_key(pre):
    """SemVer precedence: a release outranks any prerelease of the same triple."""
    if not pre:
        return ((2,),)
    return tuple((0, int(i), "") if i.isdigit() else (1, 0, i) for i in pre.split("."))


def parse_version(text):
    """(major, minor, patch, pre); build metadata is dropped, as Cargo ignores it."""
    m = _VERSION.fullmatch(text.strip())
    if not m:
        raise ValueError(f"invalid version {text!r}")
    return int(m[1]), int(m[2]), int(m[3]), m[4] or ""


def parse_req(text):
    """Cargo requirement -> comparators (op, major, minor|None, patch|None, pre).

    `*` yields no comparators; a bare requirement is caret.
    """
    comparators = []
    for part in text.split(","):
        part = part.strip()
        m = _COMPARATOR.fullmatch(part)
        if not m:
            raise ValueError(f"cannot parse requirement {text!r}")
        op, *nums, pre = m.groups()
        fields = [None if n is None or n in "*xX" else int(n) for n in nums]
        wild = any(n is not None and n in "*xX" for n in nums)
        if wild:
            # A wildcard ends the version; nothing concrete may follow it.
            first = next(i for i, n in enumerate(nums) if n is not None and n in "*xX")
            if any(f is not None for f in fields[first:]) or pre or op not in (None, "="):
                raise ValueError(f"cannot parse requirement {text!r}")
            if fields[0] is None:
                if len(text.split(",")) > 1:
                    raise ValueError(f"cannot parse requirement {text!r}")
                continue  # `*`: matches every release
            op = "*"
        elif pre and fields[2] is None:
            raise ValueError(f"cannot parse requirement {text!r}")
        comparators.append((op or "^", *fields, pre or ""))
    return comparators


def _compare(op, major, minor, patch, pre, ver):
    vmaj, vmin, vpat, vpre = ver
    if op in ("=", "*"):
        return (
            vmaj == major
            and minor in (None, vmin)
            and patch in (None, vpat)
            and vpre == pre  # a wildcard, too, excludes prereleases on its own
        )
    if op in (">", "<"):
        sign = 1 if op == ">" else -1
        for want, have in ((major, vmaj), (minor, vmin), (patch, vpat)):
            if want is None:
                return False
            if have != want:
                return (have - want) * sign > 0
        return (_pre_key(vpre) > _pre_key(pre)) == (sign > 0) and vpre != pre
    if op in (">=", "<="):
        return _compare("=", major, minor, patch, pre, ver) or _compare(
            op[0], major, minor, patch, pre, ver
        )
    if vmaj != major:
        return False
    if op == "~":
        if minor is not None and vmin != minor:
            return False
        if patch is not None and vpat != patch:
            return vpat > patch
    else:  # caret: compatible up to the leftmost non-zero component
        if minor is None:
            return True  # `^1` is the one partial form with no prerelease rule
        if patch is None:
            return vmin >= minor if major > 0 else vmin == minor
        if vmin != minor:
            return major > 0 and vmin > minor
        if vpat != patch:
            return (major > 0 or minor > 0) and vpat > patch
    return _pre_key(vpre) >= _pre_key(pre)


def req_matches(req, version):
    """Cargo's `VersionReq::matches`: every comparator holds, and a prerelease
    matches only a comparator that names a prerelease of the same triple."""
    comparators = parse_req(req) if isinstance(req, str) else req
    ver = parse_version(version)
    if not all(_compare(*c, ver) for c in comparators):
        return False
    return not ver[3] or any(
        c[4] and c[1:4] == ver[:3] for c in comparators
    )


def registry_gaps(git_deps, versions_of):
    """Errors for git dependencies whose `req` no crates.io release satisfies.

    Each crate is looked up once per run; a failed lookup is a gap, never a pass.
    """
    errors = []
    seen = {}
    for crate, dep, req in git_deps:
        try:
            comparators = parse_req(req)
        except ValueError:
            errors.append(f"'{crate}' depends on '{dep}' with {req!r}: cannot parse it")
            continue
        if dep not in seen:
            try:
                seen[dep] = versions_of(dep)
            except RegistryError as error:
                seen[dep] = error
        if isinstance(seen[dep], RegistryError):
            errors.append(
                f"'{crate}' depends on '{dep}' {req} from git, but could not query "
                f"crates.io for it ({seen[dep]}); failing closed"
            )
            continue
        if not any(_matches_quietly(comparators, v) for v in seen[dep]):
            errors.append(
                f"'{crate}' depends on '{dep}' {req} from git, but crates.io has no "
                f"release matching it; publish '{dep}' to crates.io first"
            )
    return errors


def _matches_quietly(comparators, version):
    """An index entry we cannot parse satisfies nothing (fail closed)."""
    try:
        return req_matches(comparators, version)
    except ValueError:
        return False


REGISTRY_TIMEOUT = 20  # seconds per request; the hook and CI must not hang.


class RegistryError(RuntimeError):
    """crates.io could not be queried; the preflight treats this as a gap."""


def crates_io_versions(name, urlopen=urllib.request.urlopen):
    """Non-yanked versions from the crates.io sparse index.

    A 404 means "never published" and returns no versions; every other
    failure raises RegistryError so the caller fails closed.
    """
    lower = name.lower()
    if len(lower) <= 2:
        path = f"{len(lower)}/{lower}"
    elif len(lower) == 3:
        path = f"3/{lower[0]}/{lower}"
    else:
        path = f"{lower[:2]}/{lower[2:4]}/{lower}"
    try:
        with urlopen(
            f"https://index.crates.io/{path}", timeout=REGISTRY_TIMEOUT
        ) as resp:
            lines = resp.read().decode().splitlines()
        entries = [json.loads(line) for line in lines if line]
        return [e["vers"] for e in entries if not e.get("yanked")]
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return []
        raise RegistryError(f"HTTP {error.code}") from error
    except (OSError, ValueError, KeyError, TypeError) as error:
        raise RegistryError(f"{type(error).__name__}: {error}") from error


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
    errors = check(metadata["packages"], matrix) + gate_errors(workflow["jobs"])
    errors += registry_gaps(
        git_dependencies(
            metadata["packages"],
            matrix,
            workspace_manifest=os.path.join(metadata.get("workspace_root", "."), "Cargo.toml"),
        ),
        crates_io_versions,
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
