"""Newt-Agent — small, fast, local-first agentic coder.

This package is a thin Python wrapper around the ``newt`` binary that
maturin ships in the wheel. The binary lives next to this module after
installation (maturin places ``bin/newt`` in the wheel's data scripts dir).

Usage:
    >>> import newt_agent
    >>> newt_agent.main()           # runs the `newt` CLI with sys.argv[1:]

Or from the shell:
    $ newt --help
    $ python -m newt_agent --help    # equivalent
"""

from __future__ import annotations

import os
import sys
from typing import NoReturn

__all__ = ["main", "binary_path"]


def binary_path() -> str:
    """Return the absolute path to the bundled ``newt`` binary."""
    # maturin installs `bin/newt` next to the wheel's `scripts` dir, which
    # is what shutil.which("newt") will pick up if the venv's bin/ is on PATH.
    # We just defer to the executable resolution by name.
    import shutil

    found = shutil.which("newt")
    if found is None:
        raise RuntimeError(
            "newt binary not found on PATH. "
            "Did `pip install newt-agent` complete successfully?"
        )
    return found


def main(argv: list[str] | None = None) -> NoReturn:
    """Exec the ``newt`` binary, replacing the current process."""
    if argv is None:
        argv = sys.argv[1:]
    os.execvp("newt", ["newt", *argv])


if __name__ == "__main__":
    main()
