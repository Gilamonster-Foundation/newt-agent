"""Bounded transcript ingestion, independent of the size of a tmux capture."""

from __future__ import annotations

import argparse
from pathlib import Path


def positive_mib(value: str) -> int:
    """Reject zero/negative windows before gathering grading evidence."""
    try:
        size = int(value)
    except ValueError as exc:
        raise argparse.ArgumentTypeError(
            "expected a positive integer MiB size"
        ) from exc
    if size <= 0:
        raise argparse.ArgumentTypeError("expected a positive integer MiB size")
    return size


def read_tail(path: Path, mib: int = 4) -> str:
    """Read only the bounded final window of a seekable transcript snapshot.

    Seek past old redraws instead of scanning gigabytes. Bound every read even
    if the live log keeps growing. Discard a cropped first terminal line so a
    fragment cannot manufacture a report heading or an operator prompt.
    """
    if mib <= 0:
        raise ValueError("transcript tail size must be positive")
    limit = mib * 1024 * 1024
    with path.open("rb") as stream:
        end = stream.seek(0, 2)
        start = max(0, end - limit)
        stream.seek(start)
        data = stream.read(end - start)
    if start:
        boundaries = [at for mark in (b"\r", b"\n") if (at := data.find(mark)) >= 0]
        data = data[min(boundaries) + 1 :] if boundaries else b""
    return data.decode("utf-8", errors="replace")
