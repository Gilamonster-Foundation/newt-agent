"""Project a persisted operator report into scored claims and unscored audit.

The caller selects the operator's structured store. Review identities use the
same crate-vector-pinned canonical codec as the grader's existing envelopes.
Markers alone are never authority: corrections must match a retained edit map.
"""

from __future__ import annotations

import json
from pathlib import Path
import re
import sys

sys.path.insert(
    0, str(Path(__file__).resolve().parents[2] / "newt-interaction/conformance")
)
from newt_conformance import content_id, encode  # noqa: E402

FOOTER = re.compile(
    r"\n\n\[corrected by newt: review evidence review (\w+) in prompt artifacts\]$"
)


def review_payload(records: list[tuple], cid: str) -> dict:
    chunks = []
    for body, metadata in records:
        metadata = json.loads(metadata)
        if (
            metadata.get("schema") == "newt.report-review/v1"
            and metadata.get("review_cid") == cid
        ):
            chunks.append((body, metadata))
    if not chunks or len(chunks) > 1024:
        raise ValueError("composed report review evidence missing or too large")
    total = sum(len(body.encode()) for body, _ in chunks if isinstance(body, str))
    for index, (body, metadata) in enumerate(chunks):
        if (
            not isinstance(body, str)
            or metadata.get("part") != index
            or metadata.get("bytes") != total
            or metadata.get("last") is not (index == len(chunks) - 1)
        ):
            raise ValueError("composed report review chunks incomplete or out of order")
    review = json.loads("".join(body for body, _ in chunks))
    if (
        not isinstance(review, dict)
        or content_id(encode(review)) != cid
        or review.get("schema") != "newt.report-review/v1"
    ):
        raise ValueError("composed report review content identity mismatch")
    return review


def project_report(report: str, records: list[tuple]) -> dict:
    """Score retained host corrections; exclude unverified replacements entirely."""
    if (
        not report.startswith("## Observed\n")
        or "\n## Model explanation\n\n" not in report
    ):
        raise ValueError("final outcome has no composed operator report")
    if "[Observed report excerpt: artifact body limit reached.]" in report:
        raise ValueError("final composed report was truncated in the artifact store")
    prose = report.split("\n## Model explanation\n\n", 1)[1]
    footer = FOOTER.search(prose)
    if footer is None:
        if "[corrected by newt:" in prose or "[unverified by newt:" in prose:
            raise ValueError("reviewed report has no retained prompt-artifact audit")
        return {
            "text": report,
            "claim_text": prose,
            "corrections": [],
            "original_model_draft": prose,
            "review_cid": None,
        }
    cid = footer[1]
    review = review_payload(records, cid)
    if (
        review.get("observation") is not None
        and f"Report `{review['observation']}`"
        not in report.split("\n## Model explanation\n\n", 1)[0]
    ):
        raise ValueError("review belongs to a different observed report")
    original = review["original"].encode()
    source = review["source_draft"]
    if not isinstance(source, str):
        raise ValueError("review original model draft is missing")
    rendered = bytearray()
    claims = bytearray()
    corrections = []
    offset = 0
    for edit in review["edits"]:
        start, end = edit["start"], edit["end"]
        if (
            type(start) is not int
            or type(end) is not int
            or not offset <= start <= end <= len(original)
            or original[start:end].decode() != edit["original"]
        ):
            raise ValueError("invalid reversible report edit range")
        unchanged = original[offset:start]
        rendered.extend(unchanged)
        claims.extend(unchanged)
        replacement = edit["replacement"]
        rendered.extend(replacement.encode())
        if replacement.startswith("[corrected by newt: ") and replacement.endswith("]"):
            corrections.append(
                {
                    "kind": "harness_correction",
                    "claim": replacement,
                    "actual": replacement[len("[corrected by newt: ") : -1],
                    "status": "verified",
                    "review_cid": cid,
                }
            )
        elif replacement.startswith("[unverified by newt: ") and replacement.endswith(
            "]"
        ):
            pass  # Neither a claim nor positive evidence.
        elif replacement == "\\" + edit["original"]:
            claims.extend(edit["original"].encode())
        else:
            raise ValueError("unknown report edit presentation")
        offset = end
    rendered.extend(original[offset:])
    claims.extend(original[offset:])
    if rendered.decode() != prose[: footer.start()]:
        raise ValueError("composed report does not match its reversible review")
    return {
        "text": report,
        "claim_text": claims.decode(),
        "corrections": corrections,
        "original_model_draft": source,
        "review_cid": cid,
    }
