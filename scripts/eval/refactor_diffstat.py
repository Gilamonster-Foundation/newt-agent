"""Independent compact diffstat evidence, bound to a uniquely named commit."""

from __future__ import annotations

from collections.abc import Callable, Iterator
import re
from refactor_helper import EvidenceError

PAIR = re.compile(r"\+([\d,]+)\s+[−-]([\d,]+)\b")
COMMIT = re.compile(r"\bcommitted\b\**\s+`?([0-9a-f]{7,40})\b", re.I)
CODE = re.compile(r"`+[^`\n]*`+")


def claims(text: str) -> Iterator[tuple[str, str | None, list[int]]]:
    """Keep the enclosing line so an isolated numeric span cannot borrow a SHA."""
    fence = None
    for line in text.splitlines():
        stripped = line.lstrip()
        if stripped.startswith(("```", "~~~")):
            marker = stripped[:3]
            fence = None if fence == marker else marker if fence is None else fence
            continue
        if fence or stripped.startswith(">"):
            continue
        code = list(CODE.finditer(line))
        for pair in PAIR.finditer(line):
            if any(m.start() <= pair.start() < m.end() for m in code):
                continue
            prefix = line[: pair.start()]
            visible = re.sub(r"https?://[^\s`]+", " ", CODE.sub(" ", prefix))
            if re.search(
                r"\b(?:will|would|could|should|plan|goal|example)\b", visible, re.I
            ):
                continue
            names = COMMIT.findall(prefix)
            yield pair[0], names[0] if len(names) == 1 else None, [
                int(v.replace(",", "")) for v in pair.groups()
            ]


def resolve(prefix: str | None, commits: list[str]) -> str | None:
    matches = [sha for sha in commits if prefix and sha.startswith(prefix)]
    return matches[0] if len(matches) == 1 else None


def observe(
    text: str, commits: list[str], git: Callable[..., str]
) -> dict[str, list[int] | None]:
    """Read single-parent Git diffs; binary or unavailable counts are not zero."""
    result: dict[str, list[int] | None] = {}
    for _, prefix, _ in claims(text):
        sha = resolve(prefix, commits)
        if sha is None or sha in result:
            continue
        result[sha] = None
        try:
            parents = git("rev-list", "--parents", "-n", "1", sha).split()
            if len(parents) != 2 or parents[0] != sha:
                continue
            output = git(
                "diff",
                "--numstat",
                "--no-ext-diff",
                "--no-textconv",
                "--no-renames",
                parents[1],
                sha,
                "--",
            )
            rows = [line.split("\t", 2) for line in output.splitlines()]
            if all(
                len(row) == 3 and all(v.isascii() and v.isdigit() for v in row[:2])
                for row in rows
            ):
                result[sha] = [sum(int(row[i]) for row in rows) for i in (0, 1)]
        except EvidenceError:
            pass
    return result
