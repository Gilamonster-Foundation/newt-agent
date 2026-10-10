"""Conservative totals from one complete, operator-supplied Cargo test log."""

from __future__ import annotations

import re

RESULT = re.compile(
    r"test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; "
    r"(\d+) measured; (\d+) filtered out; finished in [\d.]+s"
)


def test_totals(log: str) -> dict | None:
    """Reject partial/combined runs; per-binary success is not completion proof.

    Cargo's final failed-target list closes a --no-fail-fast invocation. For
    other logs the capture wrapper must append `cargo-test-exit: <status>`.
    Only ordinary libtest output is counted; unsupported harnesses fail closed.
    """
    lines = [line.strip() for line in log.splitlines() if line.strip()]
    totals = dict(passed=0, failed=0, ignored=0, total=0)
    started = False
    pending = False
    running = None
    binaries = failed_binaries = 0
    failed_targets = None
    targets = 0
    exit_status = None
    for index, line in enumerate(lines):
        if re.fullmatch(r"cargo-test-exit: \d+", line):
            if index != len(lines) - 1 or pending or not binaries:
                return None
            exit_status = int(line.split(": ")[1])
            continue
        if failed_targets is not None:
            if not re.fullmatch(r"`[^`]+`", line):
                return None
            targets += 1
            continue
        if re.match(r"Finished `(?:test|release)` profile ", line):
            if started:
                return None
            started = True
        elif re.match(r"(?:Running |Doc-tests )", line):
            if not started or pending:
                return None
            pending = True
        elif match := re.fullmatch(r"running (\d+) tests?", line):
            if not pending or running is not None:
                return None
            running = int(match[1])
        elif line.startswith("test result:"):
            match = RESULT.fullmatch(line)
            if not pending or running is None or match is None:
                return None
            passed, failed, ignored, measured, _ = map(int, match.groups()[1:])
            if passed + failed + ignored + measured != running:
                return None
            if (match[1] == "FAILED") != (failed > 0):
                return None
            totals["passed"] += passed
            totals["failed"] += failed
            totals["ignored"] += ignored
            totals["total"] += passed + failed
            binaries += 1
            failed_binaries += bool(failed)
            pending = False
            running = None
        elif match := re.fullmatch(r"error: (\d+) targets? failed:", line):
            if pending or not binaries:
                return None
            failed_targets = int(match[1])
        elif line.startswith("error:"):
            if not (
                line.startswith("error: test failed, to rerun pass ")
                and failed_binaries
                and not pending
            ):
                return None
    if not started or pending or not binaries:
        return None
    if failed_targets is not None:
        if failed_targets != targets or failed_targets != failed_binaries:
            return None
        if exit_status == 0:
            return None
    elif exit_status is None:
        return None
    if exit_status == 0 and failed_binaries:
        return None
    totals["success"] = not failed_binaries and exit_status == 0
    return totals
