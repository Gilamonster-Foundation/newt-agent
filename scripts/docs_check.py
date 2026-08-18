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

Why code spans are stripped: a document that *demonstrates* link syntax is not
making a reference. `docs/plan-readme-restructure.md` carries a fenced template
of the README's scoreboard block, placeholder `(…)` link and all; treating that
as a live link makes the checker demand edits to illustrative text, and the
"fix" for a root-relative path inside a template is wrong at the root anyway.
A checker that bends documents to satisfy it is worse than no checker.

Scope: markdown is scanned wherever it lives (repository root and `docs/**`),
because the root documents are precisely the ones that point into `docs/` and
break when it is reorganized. Rust is scanned across every crate rather than an
allowlist, so a new crate is covered the day it is added.

Usage:
    scripts/docs_check.py            # report and exit non-zero on any failure
    scripts/docs_check.py --quiet    # only print failures
    scripts/docs_check.py --self-test  # verify the scanner itself
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent

# `[text](target)` where target is relative. Skips URLs, anchors, and mailto.
MD_LINK = re.compile(r"\[[^\]]*\]\(([^)\s]+)\)")
# `[label]: target` — the reference-style definition. Its target is a real
# reference and must resolve just like an inline one. A label beginning with
# `^` is a FOOTNOTE definition (`[^venues]: **Candidate venues.** ...`), whose
# body is prose, not a target — matching it reports the first prose word as a
# dead link.
MD_REF = re.compile(r"^\s{0,3}\[(?!\^)[^\]]+\]:\s*(\S+)")
SKIP_LINK = re.compile(r"^(https?:|mailto:|#|<)")

# A fence opens/closes on ``` or ~~~ (any longer run, any info string).
FENCE = re.compile(r"^\s*(`{3,}|~{3,})")
# An inline code span. Non-greedy so adjacent spans do not merge.
CODE_SPAN = re.compile(r"`[^`]*`")

# A comment line, then any docs/... path ending in .md inside it. A path broken
# across a line wrap does not end in `.md` and is skipped rather than guessed.
RUST_COMMENT = re.compile(r"^\s*//")
DOCS_PATH = re.compile(r"docs/[A-Za-z0-9._/-]+\.md")

# Directories that never hold first-party references.
EXCLUDED_DIRS = {".git", "target", "node_modules", ".venv", "venv", "__pycache__"}


def _is_excluded(path: Path) -> bool:
    return any(part in EXCLUDED_DIRS for part in path.parts)


def prose_lines(text: str) -> list[str]:
    """The document's prose, with fenced blocks and inline code spans blanked.

    Blanking (rather than dropping) keeps line numbers aligned with the file so
    a reported failure points at the right line.
    """
    out: list[str] = []
    fence: str | None = None
    for line in text.splitlines():
        marker = FENCE.match(line)
        if fence is not None:
            # Inside a fence: only a matching (or longer) marker closes it.
            if marker and marker.group(1)[0] == fence[0] and len(marker.group(1)) >= len(fence):
                fence = None
            out.append("")
            continue
        if marker:
            fence = marker.group(1)
            out.append("")
            continue
        out.append(CODE_SPAN.sub("", line))
    return out


def markdown_files() -> list[Path]:
    """Repository-root markdown plus everything under `docs/`."""
    roots = sorted(p for p in REPO.glob("*.md") if p.is_file())
    docs = sorted(p for p in (REPO / "docs").rglob("*.md") if not _is_excluded(p))
    return roots + docs


def rust_files() -> list[Path]:
    """Every first-party Rust source file in the workspace."""
    return sorted(p for p in REPO.rglob("*.rs") if not _is_excluded(p))


def check_links() -> list[str]:
    """Every relative markdown link resolves, relative to the linking file."""
    failures = []
    for md in markdown_files():
        text = md.read_text(encoding="utf-8", errors="replace")
        for lineno, line in enumerate(prose_lines(text), 1):
            targets = MD_LINK.findall(line)
            ref = MD_REF.match(line)
            if ref:
                targets = targets + [ref.group(1)]
            for target in targets:
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
    for rs in rust_files():
        for lineno, line in enumerate(rs.read_text(encoding="utf-8", errors="replace").splitlines(), 1):
            if not RUST_COMMENT.match(line):
                continue
            for cited in DOCS_PATH.findall(line):
                if not (REPO / cited).exists():
                    rel = rs.relative_to(REPO)
                    failures.append(f"{rel}:{lineno}: cites missing doc -> {cited}")
    return failures


def self_test() -> int:
    """Verify the scanner on fixtures, not on the corpus.

    Each case is a real failure mode this checker had: an illustrative link
    inside a fence reported as dead (which caused a template in
    `docs/plan-readme-restructure.md` to be edited to satisfy the checker), and
    a reference-style definition that was never checked at all.
    """
    cases: list[tuple[str, str, list[str]]] = [
        (
            "fenced blocks are not references",
            "```markdown\n[x](nope.md)\n```\n",
            [],
        ),
        (
            "tilde fences too",
            "~~~\n[x](nope.md)\n~~~\n",
            [],
        ),
        (
            "a longer fence is not closed by a shorter run",
            "````\n```\n[x](nope.md)\n````\n",
            [],
        ),
        (
            "inline code spans are not references",
            "Write it as `[x](nope.md)` in the file.\n",
            [],
        ),
        (
            "a real inline link is still checked",
            "See [x](nope.md).\n",
            ["nope.md"],
        ),
        (
            "reference-style definitions are checked",
            "See [x][r].\n\n[r]: nope.md\n",
            ["nope.md"],
        ),
        (
            "footnote definitions are prose, not link targets",
            "[^venues]: **Candidate venues.** A workshop, not a link.\n",
            [],
        ),
        (
            "a footnote reference in prose is not a target",
            "As argued elsewhere[^venues], the framing differs.\n",
            [],
        ),
        (
            "line numbers survive fence blanking",
            "```\nfenced\n```\n[x](nope.md)\n",
            ["nope.md"],
        ),
    ]
    failed = 0
    for name, text, expected in cases:
        found = []
        for line in prose_lines(text):
            targets = MD_LINK.findall(line)
            ref = MD_REF.match(line)
            if ref:
                targets.append(ref.group(1))
            found.extend(t for t in targets if not SKIP_LINK.match(t))
        if found != expected:
            print(f"docs-check self-test FAIL: {name}: {found!r} != {expected!r}", file=sys.stderr)
            failed += 1

    # The line-number case must also report the correct line.
    lines = prose_lines("```\nfenced\n```\n[x](nope.md)\n")
    if len(lines) != 4 or "nope.md" not in lines[3]:
        print("docs-check self-test FAIL: line alignment", file=sys.stderr)
        failed += 1

    if failed:
        print(f"docs-check: {failed} self-test failure(s)", file=sys.stderr)
        return 1
    print(f"docs-check: {len(cases) + 1} self-tests passed")
    return 0


def main() -> int:
    if "--self-test" in sys.argv:
        return self_test()
    quiet = "--quiet" in sys.argv
    link_failures = check_links()
    citation_failures = check_citations()

    if not quiet:
        print(
            f"docs-check: {len(markdown_files())} documents, "
            f"{len(rust_files())} sources scanned"
        )

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
