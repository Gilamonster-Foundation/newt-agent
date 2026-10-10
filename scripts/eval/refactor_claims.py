"""Extract final refactor claims, keeping contradictions separate from missing proof."""

from __future__ import annotations

import re

from refactor_test_log import test_totals

ANSI = re.compile(r"\x1b\][^\x07]*(?:\x07|\x1b\\)|\x1b\[[0-?]*[ -/]*[@-~]")
NUMBER = r"[\d,]+"
FILE = r"[\w./-]+\.rs"


def clean_terminal(raw: str) -> str:
    """Remove terminal controls; don't treat prompt redraws as operator actions."""
    # Absolute cursor positioning can begin a new renderer row without a LF.
    # Preserve that boundary before removing controls, or spinner + reply merge.
    raw = re.sub(r"\x1b\[[0-9;]*[Hf]", "\n", raw)
    return ANSI.sub("", raw).replace("\r", "\n")


def operator_inputs(raw: str, submitted: str | None = None) -> dict:
    """Count submitted input echoes only; permission-menu redraws aren't grants."""
    text = clean_terminal(raw)
    inputs = (
        submitted.splitlines()
        if submitted is not None
        else re.findall(r"(?m)^\s*(?:❯|>)\s*(.*?)\s*$", text)
    )
    return {
        "continues": sum(bool(re.fullmatch(r"continue[.!]?", x, re.I)) for x in inputs),
        "allow_once": sum(
            bool(re.fullmatch(r"(?:allow[- _]once|a)", x)) for x in inputs
        ),
        "observed_inputs": len(inputs),
        "complete": submitted is not None,
        "scope": (
            "operator-supplied input log"
            if submitted is not None
            else "submitted prompt echoes only; raw menu redraws cannot prove a grant"
        ),
    }


def check_claims(summary: str, facts: dict, test_log: str = "") -> list[dict]:
    """Check explicit assistant text; caller must establish its structured role."""
    rows: list[dict] = []
    test_evidence = test_totals(clean_terminal(test_log))

    def add(kind: str, claim: str, actual: object, matches: bool | None) -> None:
        row = {
            "kind": kind,
            "claim": claim,
            "actual": actual,
            "status": (
                "unverifiable"
                if matches is None
                else "verified" if matches else "contradicted"
            ),
        }
        if row not in rows:
            rows.append(row)

    def lines(path: str, count: str, stage: str) -> None:
        candidates = facts.get(stage, {})
        matches = [p for p in candidates if p == path or p.endswith("/" + path)]
        actual = candidates[matches[0]] if len(matches) == 1 else None
        expected = int(count.replace(",", ""))
        add(
            "lines",
            f"{path} {stage}: {expected}",
            actual,
            None if actual is None else actual == expected,
        )

    def delta(path: str, count: str) -> None:
        before, after = facts.get("before", {}), facts.get("after", {})
        candidates = {
            p
            for p in before.keys() | after.keys()
            if p == path or p.endswith("/" + path)
        }
        actual = None
        if len(candidates) == 1:
            candidate = candidates.pop()
            if candidate in before and candidate in after:
                actual = before[candidate] - after[candidate]
        expected = int(count.replace(",", ""))
        add(
            "line_delta",
            f"{path} reduction: {expected}",
            actual,
            None if actual is None else actual == expected,
        )

    # Sentence/contrast boundaries reset context. Within coordination, an
    # explicit future/negative assertion must not hide a completed assertion.
    clauses = re.split(r"\n|;|\bbut\b|,(?!\d)|(?<=[.!?])\s+", summary, flags=re.I)
    assertions = []
    for clause in clauses:
        negative = False
        future_auxiliary = False
        for assertion in re.split(r"\band\b", clause, flags=re.I):
            explicit = re.match(r"\s*(?:I|we|have|has|already)\b", assertion, re.I)
            if explicit:
                negative = False
                future_auxiliary = False
            negative = negative or bool(
                re.search(r"\b(?:not|never|cannot|can't)\b", assertion, re.I)
            )
            future_auxiliary = future_auxiliary or bool(
                re.search(r"\b(?:will|would)\s+(?:be|have)\b", assertion, re.I)
            )
            if not negative and not future_auxiliary:
                assertions.append(assertion)
    for line in assertions:
        # A future plan or quoted earlier claim isn't a completed success claim.
        if re.search(
            r"\b(?:unverified|will |would |earlier turns|before acting|baseline )",
            line,
            re.I,
        ):
            continue
        if re.search(r"\bcommitted\b|\bcommit\s+pushed\b|│\s*Commit\s*│", line, re.I):
            shas = re.findall(r"\b[0-9a-f]{7,40}\b", line)
            commits = facts.get("commits")
            ok = (
                None
                if commits is None
                else bool(commits)
                and all(any(c.startswith(s) for c in commits) for s in shas)
            )
            add(
                "commit",
                "committed" + (" " + ", ".join(shas) if shas else ""),
                commits,
                ok,
            )
        if re.search(r"\bpushed\b", line, re.I):
            add("push", "branch pushed", facts.get("pushed"), facts.get("pushed"))
        numbers = re.findall(r"(?:\bPR\s*[:│(]?\s*#?|/pull/)(\d+)", line, re.I)
        for number in dict.fromkeys(numbers):
            prs = facts.get("prs")
            pr = next((p for p in prs or [] if p["number"] == int(number)), None)
            ok = None if prs is None else pr is not None
            if pr is not None and re.search(
                r"\bPR\s*#?" + number + r"\s+(?:is\s+)?open\b", line, re.I
            ):
                ok = pr.get("state") == "OPEN"
            urls = re.findall(r"github\.com/([^/]+/[^/]+)/pull/" + number + r"\b", line)
            if facts.get("github") and any(repo != facts["github"] for repo in urls):
                ok = False
            add("pr", f"PR #{number} belongs to this branch/head", pr, ok)
        if re.search(r"\bmerged\b", line, re.I):
            prs = facts.get("prs")
            add(
                "merged",
                "PR merged",
                prs,
                None if prs is None else any(p.get("state") == "MERGED" for p in prs),
            )
        # Scope every numeric statement to its own filename, never across the
        # next file mentioned in the same sentence.
        for mention in re.finditer(rf"({FILE})(.*?)(?={FILE}|$)", line):
            path, detail = mention.groups()
            pair = re.search(
                rf"(~?{NUMBER})\s*(?:lines)?\s*(?:→|->|to)\s*~?({NUMBER})", detail
            )
            reduction = re.search(
                rf"(?:\b(?:drops?|reduced|reducing|removed?|removing)(?:\s+by)?\s*|(?<![\w>])-\s*)~?({NUMBER})\b(?:\s+lines)?",
                detail,
                re.I,
            )
            if reduction is None and re.search(
                r"\b(?:reduced|reducing)\s+`?$", line[: mention.start()], re.I
            ):
                reduction = re.match(rf"`?\s+by\s+~?({NUMBER})\b(?:\s+lines)?", detail)
            if pair:
                lines(path, pair[1].lstrip("~"), "before")
                lines(path, pair[2], "after")
            if reduction:
                delta(path, reduction[1])
            if not pair and not reduction:
                absolute = re.search(rf"~?({NUMBER})\s+lines", detail)
                if absolute:
                    stage = (
                        "before"
                        if re.search(
                            r"longest|Target file|largest", line[: mention.end()], re.I
                        )
                        and "is now" not in detail
                        else "after"
                    )
                    lines(path, absolute[1], stage)
            table = re.search(rf"\s+after\s*│\s*({NUMBER})", detail)
            if table:
                lines(path, table[1], "after")
        for reduction in re.finditer(
            rf"\bremoved\s+~?({NUMBER})\s+lines\s+from\s+`?({FILE})", line, re.I
        ):
            delta(reduction[2], reduction[1])
        # The supplied log must represent this claimed invocation. Never use a
        # final doctest result as a whole-run count or turn partial logs green.
        counts = []
        for count in re.finditer(
            rf"({NUMBER})\s+tests?(?:\s+(passed|pass|failed|fail|ignored|total)\b)?|({NUMBER})\s+(passed|failed|ignored|failures)\b",
            line,
            re.I,
        ):
            number = count[1] or count[3]
            kind = (count[2] or count[4] or "total").lower()
            kind = {"pass": "passed", "fail": "failed", "failures": "failed"}.get(
                kind, kind
            )
            counts.append((number, kind))
        for count, kind in counts:
            actual = test_evidence[kind] if test_evidence is not None else None
            expected = int(count.replace(",", ""))
            add(
                "tests",
                f"{expected} tests {kind}",
                actual,
                None if actual is None else actual == expected,
            )
        if counts and test_evidence is not None and not test_evidence["success"]:
            add("tests", "supplied test invocation succeeded", False, False)
    return rows
