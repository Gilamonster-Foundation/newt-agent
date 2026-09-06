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
making a reference. The case that established this was a now-retired planning
document that carried a fenced template of the README's scoreboard block,
placeholder `(…)` link and all; treating that as a live link made the checker
demand edits to illustrative text, and the "fix" for a root-relative path
inside a template is wrong at the root anyway. A checker that bends documents
to satisfy it is worse than no checker.

Scope: markdown is scanned wherever it lives (repository root and `docs/**`),
because the root documents are precisely the ones that point into `docs/` and
break when it is reorganized. Rust is scanned across every crate rather than an
allowlist, so a new crate is covered the day it is added.

3. **Retirement.** When a change DELETES a markdown document, nothing left in
   the tree may still point at it — by link or by name.

Why the third check reads raw text: checks 1 and 2 answer "does this reference
resolve", and for that, blanking code spans is right (a document demonstrating
link syntax is not making a reference). Retirement asks the opposite question —
"does anything still NAME this file" — and the references that survive a
deletion are overwhelmingly *prose* mentions and backtick citations, not links.
A `docs/foo.md` written in backticks is invisible to check 1 by design. So the
retirement sweep matches raw lines, code spans included, and is deliberately
NOT built on `prose_lines`.

Tombstones are the intended exception. A README that records "Retired
2026-08-18: `foo.md` was removed" SHOULD keep naming the file — that is a
manifest, not a dangling pointer. A mention is treated as acknowledged when a
retirement marker appears in the same paragraph, which is the idiom the corpus
already uses.

Usage:
    scripts/docs_check.py            # report and exit non-zero on any failure
    scripts/docs_check.py --quiet    # only print failures
    scripts/docs_check.py --self-test  # verify the scanner itself
    scripts/docs_check.py --deleted-refs <base>
                                     # nothing still names a doc deleted since <base>
"""

from __future__ import annotations

import re
import subprocess
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


# INERT-CODE-RATCHET: S02 WIRE: documentation checker scans root and docs only, omitting tracked script reports.
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
    inside a fence reported as dead (which caused a template in a planning
    document, since retired, to be edited to satisfy the checker), and a
    reference-style definition that was never checked at all.
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

    # --- retirement rule (--deleted-refs) ---------------------------------
    retirement: list[tuple[str, list[str], str, list[int]]] = [
        (
            "a plain prose mention is dangling",
            ["gone.md"],
            "See gone.md for details.\n",
            [1],
        ),
        (
            "a BACKTICK citation is dangling too — the case links cannot see",
            ["gone.md"],
            "Cited by `gone.md` in passing.\n",
            [1],
        ),
        (
            "a tombstone paragraph may name what it retires",
            ["gone.md"],
            "**Retired 2026-08-18.** `gone.md` was removed.\n",
            [],
        ),
        (
            "the marker covers the whole paragraph, not just its line",
            ["a.md", "b.md"],
            "**Retired.** `a.md` went first.\nLater `b.md` went with it.\n",
            [],
        ),
        (
            "a marker in a DIFFERENT paragraph does not excuse a mention",
            ["gone.md"],
            "**Retired.** Something else was removed.\n\nSee gone.md.\n",
            [3],
        ),
        (
            "unrelated text is not a mention",
            ["gone.md"],
            "Nothing here names it.\n",
            [],
        ),
    ]
    for name, names, text, expected in retirement:
        got = [lineno for lineno, _ in unacknowledged_mentions(names, text)]
        if got != expected:
            print(
                f"docs-check self-test FAIL: {name}: {got!r} != {expected!r}",
                file=sys.stderr,
            )
            failed += 1

    # The line-number case must also report the correct line.
    lines = prose_lines("```\nfenced\n```\n[x](nope.md)\n")
    if len(lines) != 4 or "nope.md" not in lines[3]:
        print("docs-check self-test FAIL: line alignment", file=sys.stderr)
        failed += 1

    if failed:
        print(f"docs-check: {failed} self-test failure(s)", file=sys.stderr)
        return 1
    print(f"docs-check: {len(cases) + len(retirement) + 1} self-tests passed")
    return 0


# A paragraph naming a deleted document is a tombstone, not a dangling pointer,
# when it says so. These are the words the corpus already uses.
RETIREMENT_MARKER = re.compile(r"\b(retired|removed|deleted|superseded)\b", re.I)


def paragraphs(lines: list[str]) -> list[tuple[int, list[str]]]:
    """Contiguous non-blank runs, each with its 1-based starting line number.

    Paragraph granularity is deliberate. The corpus writes a tombstone as one
    block — "**Retired 2026-08-18.** `a.md` was removed. ... `b.md` went with
    it" — so the marker sits near, but not necessarily on, each mention's line.
    """
    out: list[tuple[int, list[str]]] = []
    start, buf = 0, []
    for i, line in enumerate(lines, 1):
        if line.strip():
            if not buf:
                start = i
            buf.append(line)
        elif buf:
            out.append((start, buf))
            buf = []
    if buf:
        out.append((start, buf))
    return out


def unacknowledged_mentions(names: list[str], text: str) -> list[tuple[int, str]]:
    """Which of `names` this document still mentions WITHOUT retiring them.

    Pure, so the rule is testable without a filesystem or a git history. Matches
    raw lines — code spans included — see the module docstring.
    """
    lines = text.splitlines()
    found: list[tuple[int, str]] = []
    for start, para in paragraphs(lines):
        block = "\n".join(para)
        if RETIREMENT_MARKER.search(block):
            continue  # a tombstone may name what it retires
        for offset, line in enumerate(para):
            for name in names:
                if name in line:
                    found.append((start + offset, name))
    return found


def git_deleted_markdown(base: str) -> list[str]:
    """Markdown paths deleted between `base` and the working tree.

    # Errors
    Raises CalledProcessError when `base` is not a valid revision — a wrong
    base must fail loudly rather than silently checking nothing.
    """
    proc = subprocess.run(
        ["git", "diff", "--diff-filter=D", "--name-only", f"{base}...HEAD"],
        cwd=REPO,
        capture_output=True,
        text=True,
        check=False,
    )
    if proc.returncode != 0:
        # The overwhelmingly common cause is a shallow clone with no reachable
        # merge base. Say so — a raw CalledProcessError traceback sends the
        # reader hunting through the checker instead of the checkout depth.
        raise SystemExit(
            f"docs-check: cannot diff against '{base}': "
            f"{proc.stderr.strip() or 'git failed'}\n"
            "docs-check: a merge base must be reachable — in CI, check out with "
            "fetch-depth: 0."
        )
    return [p for p in proc.stdout.splitlines() if p.endswith(".md")]


def check_retirement(base: str, quiet: bool = False) -> list[str]:
    """Nothing left in the tree still names a document deleted since `base`."""
    deleted = git_deleted_markdown(base)
    if not deleted:
        if not quiet:
            print(f"docs-check: no markdown deleted since {base}")
        return []

    tracked = subprocess.run(
        ["git", "ls-files"], cwd=REPO, capture_output=True, text=True, check=True
    ).stdout.splitlines()
    # Text the corpus actually cites documents from.
    suffixes = (".md", ".rs", ".py", ".sh", ".toml", ".yml", ".yaml")
    corpus = [
        f for f in tracked if (f.endswith(suffixes) or Path(f).name == "justfile")
    ]

    failures = []
    for path in deleted:
        # Both spellings: the full path and the bare filename, since a citation
        # is as often `foo.md` as `docs/dir/foo.md`.
        names = [path, Path(path).name]
        for rel in corpus:
            fp = REPO / rel
            if not fp.is_file():
                continue
            hits = unacknowledged_mentions(names, fp.read_text(encoding="utf-8", errors="replace"))
            # `names` holds both the full path and the bare filename, so one
            # citation matches twice. Report the line once, naming the most
            # specific spelling that matched.
            best: dict[int, str] = {}
            for lineno, name in hits:
                if len(name) > len(best.get(lineno, "")):
                    best[lineno] = name
            for lineno in sorted(best):
                failures.append(
                    f"{rel}:{lineno}: names deleted document -> {best[lineno]}"
                )
    if not quiet:
        print(f"docs-check: {len(deleted)} deleted document(s) swept since {base}")
    return failures


def main() -> int:
    if "--self-test" in sys.argv:
        return self_test()
    quiet = "--quiet" in sys.argv

    if "--deleted-refs" in sys.argv:
        i = sys.argv.index("--deleted-refs")
        if i + 1 >= len(sys.argv):
            print("docs-check: --deleted-refs needs a base revision", file=sys.stderr)
            return 2
        failures = check_retirement(sys.argv[i + 1], quiet)
        for failure in failures:
            print(f"docs-check: {failure}", file=sys.stderr)
        if failures:
            print(
                f"docs-check: {len(failures)} reference(s) to deleted document(s). "
                "Remove them, or record the retirement in the same paragraph.",
                file=sys.stderr,
            )
            return 1
        if not quiet:
            print("docs-check: no dangling reference to any deleted document")
        return 0

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
