#!/usr/bin/env python3
"""Automated process-spawn inventory gate (P4 `p4-constrained-executor`).

Counts raw process-spawn primitives (`Command::new` / `process::Command`) per
source file across the security-relevant crates and compares against the
allowlist in `docs/security/spawn-inventory.toml`. FAILS (exit 1) when:

  * a file has MORE spawn sites than its allowlisted `count` (a NEW, unreviewed
    subprocess was added); or
  * a file that is NOT in the allowlist introduces any spawn site.

So a future subprocess cannot land without a reviewer updating the inventory and
justifying its trust class. Removing a spawn (count drops) is always fine.

Run: `python3 scripts/spawn_inventory.py` (also self-tests with `--self-test`).
"""
from __future__ import annotations

import re
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
ALLOWLIST = REPO / "docs" / "security" / "spawn-inventory.toml"

# The crates a hostile repo / model turn can influence, plus the operator infra
# crates whose spawns we still want inventoried. (Pure library crates with no
# subprocess use are simply absent from the allowlist and expected to stay so.)
SCAN_ROOTS = [
    "newt-core/src",
    "newt-cli/src",
    "newt-tui/src",
    "newt-tools/src",
    "newt-mcp-client/src",
    "newt-acp-worker/src",
    "newt-coder/src",
    # step-7.2 (convergence audit): widened so the no-bypass gate covers EVERY
    # crate that spawns, not just the agent-loop crates — a future attacker-
    # influenced spawn added to any of these is now caught, not silently missed.
    "newt-git/src",
    "newt-inference/src",
    "newt-mesh/src",
    "plugins-protocol/src",
    "newt-eval/src",
    "newt-mcp-server/src",
]

SPAWN_RE = re.compile(r"Command::new|process::Command")
# `"path" = { count = N, ... }`
ALLOW_RE = re.compile(r'"([^"]+)"\s*=\s*\{\s*count\s*=\s*(\d+)')


def parse_allowlist(text: str) -> dict[str, int]:
    return {m.group(1): int(m.group(2)) for m in ALLOW_RE.finditer(text)}


def count_spawns(text: str) -> int:
    return len(SPAWN_RE.findall(text))


def scan(repo: Path) -> dict[str, int]:
    """Actual spawn count per repo-relative .rs file that has ≥1 spawn."""
    found: dict[str, int] = {}
    for root in SCAN_ROOTS:
        base = repo / root
        if not base.exists():
            continue
        for path in base.rglob("*.rs"):
            n = count_spawns(path.read_text(encoding="utf-8", errors="ignore"))
            if n:
                found[str(path.relative_to(repo))] = n
    return found


def check(allow: dict[str, int], actual: dict[str, int]) -> list[str]:
    violations: list[str] = []
    for path, n in sorted(actual.items()):
        allowed = allow.get(path)
        if allowed is None:
            violations.append(
                f"NEW spawn site: {path} has {n} `Command`/`process::Command` "
                f"occurrence(s) but is not in the inventory. Add it to "
                f"docs/security/spawn-inventory.toml with a trust class."
            )
        elif n > allowed:
            violations.append(
                f"UNREVIEWED spawn added: {path} now has {n} spawn site(s), "
                f"allowlisted for {allowed}. Route it through ConstrainedExecutor "
                f"(or justify + bump the inventory count)."
            )
    return violations


def self_test() -> None:
    allow = parse_allowlist(
        '[files]\n"a/b.rs" = { count = 2, class = "x", note = "y" }\n'
        '"c/d.rs" = { count = 0, class = "z" }\n'
    )
    assert allow == {"a/b.rs": 2, "c/d.rs": 0}, allow
    assert count_spawns("Command::new('x'); std::process::Command::new('y')") == 2
    # Excess + new-file both flagged; equal/fewer are clean.
    v = check({"a/b.rs": 2}, {"a/b.rs": 3, "e/f.rs": 1})
    assert any("UNREVIEWED" in x for x in v) and any("NEW spawn" in x for x in v), v
    assert check({"a/b.rs": 2}, {"a/b.rs": 2}) == []
    assert check({"a/b.rs": 2}, {"a/b.rs": 1}) == []  # removing a spawn is fine
    print("spawn_inventory self-test OK")


def main() -> int:
    if "--self-test" in sys.argv:
        self_test()
        return 0
    if not ALLOWLIST.exists():
        print(f"error: missing {ALLOWLIST}", file=sys.stderr)
        return 1
    allow = parse_allowlist(ALLOWLIST.read_text(encoding="utf-8"))
    actual = scan(REPO)
    violations = check(allow, actual)
    total = sum(actual.values())
    todo = sum(
        n
        for p, n in actual.items()
        if 'class = "agent-exec-todo-p4"'
        in "".join(
            line
            for line in ALLOWLIST.read_text(encoding="utf-8").splitlines()
            if f'"{p}"' in line
        )
    )
    print(f"spawn-inventory: {total} spawn site(s) across {len(actual)} file(s); "
          f"{todo} still classed agent-exec-todo-p4 (pending ConstrainedExecutor migration).")
    if violations:
        print("\n".join(f"  ✗ {v}" for v in violations), file=sys.stderr)
        print(
            "\nspawn-inventory FAILED — a subprocess site changed without review.",
            file=sys.stderr,
        )
        return 1
    print("spawn-inventory OK — no unreviewed spawn site; every spawn is inventoried.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
