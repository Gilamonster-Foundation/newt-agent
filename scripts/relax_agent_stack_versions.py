#!/usr/bin/env python3
"""Relax branch-stack dependency versions inside a trap-backed checkout.

Cargo patches select a source but still enforce the consumer's semver
requirement. The `build-agent-stack` recipe backs these manifests up byte for
byte, invokes this helper, and restores them on every exit so an unpublished
branch with an older or future development version can still be integration
tested without weakening the committed release floor.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path


def replace_once(path: Path, pattern: str, dependency: str) -> None:
    original = path.read_text(encoding="utf-8")
    updated, count = re.subn(pattern, r'\g<1>"*"', original, count=1, flags=re.MULTILINE)
    if count != 1:
        raise SystemExit(
            f"build-agent-stack: expected one {dependency} version in {path}, found {count}"
        )
    path.write_text(updated, encoding="utf-8")


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit("usage: relax_agent_stack_versions.py <newt-workspace>")
    root = Path(sys.argv[1]).resolve()
    bridle_pattern = r'^(\s*agent-bridle\s*=\s*\{\s*version\s*=\s*)"[^"]+"'
    for relative in (
        "newt-core/Cargo.toml",
        "newt-mcp-client/Cargo.toml",
        "newt-mcp-server/Cargo.toml",
    ):
        replace_once(root / relative, bridle_pattern, "agent-bridle")
    replace_once(
        root / "Cargo.toml",
        r'^(\s*agent-mesh-protocol\s*=\s*)"[^"]+"',
        "agent-mesh-protocol",
    )


if __name__ == "__main__":
    main()
