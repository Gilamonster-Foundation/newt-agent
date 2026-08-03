#!/usr/bin/env python3
"""Registry-reference linter for spec/behavior-map.toml (epic #1529).

A behavioral change must not silently orphan a constitution entry. This checks
that every reference in the registry still resolves:

  * `lean`       theorems appear in formal/NewtPolicy/**.lean
  * `production` paths exist
  * `rust_tests` `fn <name>` appear somewhere in the workspace

Cross-branch reality: the registry lives on its own branch; some referenced Rust
tests / production files land only when a feature branch (e.g. the #1526 psyche
PR) merges. So by default this is STRICT on `lean` (fully present here) and
ADVISORY on `production` / `rust_tests` (warn, don't fail). Run with `--strict`
on `main` (post-merge) to fail on any unresolved reference.

Usage:  python3 spec/lint-behavior-map.py [--strict]
Exit 0 = clean (per the mode); 1 = an unresolved reference in a strict layer.
"""
from __future__ import annotations

import re
import subprocess
import sys
import tomllib
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
MAP = REPO / "spec" / "behavior-map.toml"
LEAN_DIR = REPO / "formal" / "NewtPolicy"


def grep(pattern: str, *globs: str) -> bool:
    """True if `pattern` (fixed string) appears in any file matching the globs."""
    cmd = ["grep", "-rslF", "--", pattern]
    cmd += [str(REPO / g) for g in globs] if globs else [str(REPO)]
    return subprocess.run(cmd, capture_output=True).returncode == 0


def lean_theorem_defined(fqn: str) -> bool:
    # `NewtPolicy.Backend.adding_a_backend_preserves_selection` -> theorem/def name.
    name = fqn.rsplit(".", 1)[-1]
    if not LEAN_DIR.exists():
        return False
    for f in LEAN_DIR.rglob("*.lean"):
        text = f.read_text(encoding="utf-8", errors="replace")
        if re.search(rf"^\s*(theorem|def|lemma)\s+{re.escape(name)}\b", text, re.M):
            return True
    return False


def rust_test_defined(ref: str) -> bool:
    # `module::path::test_name` -> `fn test_name`.
    name = ref.split("::")[-1]
    return grep(f"fn {name}(", "newt-core", "newt-inference", "newt-acp-worker", "newt-cli") or grep(
        f"fn {name}("
    )


def main() -> int:
    strict = "--strict" in sys.argv[1:]
    data = tomllib.loads(MAP.read_text())

    lean_missing: list[str] = []
    prod_missing: list[str] = []
    test_missing: list[str] = []

    for bhv, body in data.items():
        if not bhv.startswith("BHV-") or not isinstance(body, dict):
            continue
        refs = body.get("refs", {})
        for thm in refs.get("lean", []):
            if not lean_theorem_defined(thm):
                lean_missing.append(f"{bhv}: lean {thm}")
        for path in refs.get("production", []):
            if not (REPO / path).exists():
                prod_missing.append(f"{bhv}: production {path}")
        for test in refs.get("rust_tests", []):
            if not rust_test_defined(test):
                test_missing.append(f"{bhv}: rust_test {test}")

    def report(label: str, items: list[str], fatal: bool) -> None:
        if not items:
            return
        tag = "ERROR" if fatal else "warn "
        print(f"[{tag}] {label}: {len(items)} unresolved", file=sys.stderr)
        for it in items:
            print(f"        {it}", file=sys.stderr)

    # `lean` is always strict (fully present on this branch). `production` /
    # `rust_tests` are strict only under --strict (post-merge on main).
    report("lean references", lean_missing, fatal=True)
    report("production references", prod_missing, fatal=strict)
    report("rust_test references", test_missing, fatal=strict)

    failed = bool(lean_missing) or (strict and (prod_missing or test_missing))
    if not (lean_missing or prod_missing or test_missing):
        print("behavior-map.toml: all references resolve.")
    elif not failed:
        print(
            "behavior-map.toml: lean references resolve; "
            "production/rust advisory (unmerged branches resolve later — use --strict on main)."
        )
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
