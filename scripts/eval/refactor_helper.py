"""Distinguish parser evidence from failures to execute the trusted parser."""

from pathlib import Path
import re
import subprocess


class HelperUnavailable(RuntimeError):
    """The grader cannot establish a verdict without its trusted parser."""


class EvidenceError(RuntimeError):
    """Unavailable evidence is a failing grade, not proof of fabrication."""


def invoke_helper(binary: Path, paths: list[Path], name: str, cwd: Path) -> str:
    """Accept only the parser's success protocol or explicit syntax rejection.

    The Rust helper emits MATCH/NONE on stdout at exit 0. Its supported syntax
    rejections use exit 2, empty stdout, and one known diagnostic on stderr.
    Other exit-2 errors (such as unreadable source files) are infrastructure.
    """
    argv = [str(binary), *map(str, paths), name]
    try:
        result = subprocess.run(
            argv,
            cwd=cwd,
            capture_output=True,
            text=True,
            encoding="utf-8",
            timeout=120,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired, UnicodeError) as exc:
        raise HelperUnavailable(f"syntax helper execution unavailable: {exc}") from exc
    if result.returncode == 0 and not result.stderr:
        if result.stdout in ("MATCH", "MATCH\n", "NONE", "NONE\n"):
            return result.stdout.rstrip("\n")
    diagnostic = result.stderr.removesuffix("\n")
    unsupported = diagnostic in {
        "unsupported attributes affecting candidate module",
        "unsupported attributes affecting candidate declarations",
    } or re.fullmatch(r"unsupported Rust syntax: [^\r\n]+", diagnostic)
    if result.returncode == 2 and not result.stdout and unsupported:
        raise EvidenceError(diagnostic)
    raise HelperUnavailable(
        f"syntax helper execution/protocol failure (exit {result.returncode}); "
        f"stdout={result.stdout[-2000:]!r}; stderr={result.stderr[-2000:]!r}"
    )
