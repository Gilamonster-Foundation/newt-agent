#!/usr/bin/env python3
"""Resolve every documentation reference in the repository.

Two checks, both fail-closed:

1. **Links.** Every relative markdown link inside `docs/**/*.md` must resolve,
   relative to the *linking file's* directory. A decision record that points at
   a sibling it outlived is a dead end for whoever follows it.

2. **Citations.** Every `docs/....md` path named in a Rust **comment** must
   exist. Source cites documentation heavily (96 doc-comment references and 22
   plain-comment ones at the time of writing), and nothing checked them.

Why comments only: paths inside string literals are test fixtures, not
references. `touch(ws.path(), "docs/pyo3_module.md"); // decoy` is a deliberate
non-existent path, and five such fixtures would be false positives under a
naive grep. Scanning comment lines separates a citation from a test string
without needing to parse Rust.

Usage:
    scripts/docs_check.py            # report and exit non-zero on any failure
    scripts/docs_check.py --quiet    # only print failures
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent

# `[text](target)` where target is relative. Skips URLs, anchors, and mailto.
MD_LINK = re.compile(r"\[[^\]]*\]\(([^)\s]+)\)")
SKIP_LINK = re.compile(r"^(https?:|mailto:|#|<)")

# A comment line, then any docs/... path ending in .md inside it. A path broken
# across a line wrap does not end in `.md` and is skipped rather than guessed.
RUST_COMMENT = re.compile(r"^\s*//")
DOCS_PATH = re.compile(r"docs/[A-Za-z0-9._/-]+\.md")

RUST_ROOTS = ("newt-core", "newt-cli", "newt-tui", "newt-coder", "newt-data",
              "newt-eval", "newt-git", "newt-identity", "newt-inference",
              "newt-mcp-client", "newt-mcp-server", "newt-skills", "newt-tools",
              "newt-acp-worker", "crates")


def check_links() -> list[str]:
    """Every relative markdown link under docs/ resolves."""
    failures = []
    for md in sorted((REPO / "docs").rglob("*.md")):
        for lineno, line in enumerate(md.read_text(encoding="utf-8", errors="replace").splitlines(), 1):
            for target in MD_LINK.findall(line):
                if SKIP_LINK.match(target):
                    continue
                # Strip an anchor; a link to a heading still names a file.
                path_part = target.split("#", 1)[0]
                if not path_part:
                    continue
                resolved = (md.parent / path_part).resolve()
                if not resolved.exists():
                    rel = md.relative_to(REPO)
                    failures.append(f"{rel}:{lineno}: dead link -> {target}")
    return failures


def check_citations() -> list[str]:
    """Every docs/... path named in a Rust comment exists."""
    failures = []
    for root in RUST_ROOTS:
        base = REPO / root
        if not base.is_dir():
            continue
        for rs in sorted(base.rglob("*.rs")):
            if "target" in rs.parts:
                continue
            for lineno, line in enumerate(rs.read_text(encoding="utf-8", errors="replace").splitlines(), 1):
                if not RUST_COMMENT.match(line):
                    continue
                for cited in DOCS_PATH.findall(line):
                    if not (REPO / cited).exists():
                        rel = rs.relative_to(REPO)
                        failures.append(f"{rel}:{lineno}: cites missing doc -> {cited}")
    return failures


def main() -> int:
    quiet = "--quiet" in sys.argv
    link_failures = check_links()
    citation_failures = check_citations()

    if not quiet:
        docs = len(list((REPO / "docs").rglob("*.md")))
        print(f"docs-check: {docs} documents scanned")

    for failure in link_failures + citation_failures:
        print(f"docs-check: {failure}", file=sys.stderr)

    total = len(link_failures) + len(citation_failures)
    if total:
        print(
            f"docs-check: {len(link_failures)} dead link(s), "
            f"{len(citation_failures)} missing cited doc(s)",
            file=sys.stderr,
        )
        return 1

    if not quiet:
        print("docs-check: every documentation reference resolves")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
