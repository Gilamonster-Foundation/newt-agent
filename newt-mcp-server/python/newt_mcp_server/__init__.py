"""Newt-MCP-Server — stdio MCP server for newt-agent.

This package is a thin Python wrapper around the ``newt-mcp-server``
binary that maturin ships in the wheel. The binary lives next to this
module after installation (maturin places ``bin/newt-mcp-server`` in the
wheel's data scripts dir).

Usage:
    >>> import newt_mcp_server
    >>> newt_mcp_server.main()      # runs the MCP server (stdio JSON-RPC)

Or from the shell:
    $ newt-mcp-server
    $ python -m newt_mcp_server     # equivalent

The MCP server speaks JSON-RPC over stdio. It is intended to be spawned
by an MCP host (e.g. Claude Code, claude.ai) rather than run interactively.
"""

from __future__ import annotations

import os
import sys
from typing import NoReturn

__all__ = ["main", "binary_path"]


def binary_path() -> str:
    """Return the absolute path to the bundled ``newt-mcp-server`` binary."""
    import shutil

    found = shutil.which("newt-mcp-server")
    if found is None:
        raise RuntimeError(
            "newt-mcp-server binary not found on PATH. "
            "Did `pip install newt-mcp-server` complete successfully?"
        )
    return found


def main(argv: list[str] | None = None) -> NoReturn:
    """Exec the ``newt-mcp-server`` binary, replacing the current process."""
    if argv is None:
        argv = sys.argv[1:]
    os.execvp("newt-mcp-server", ["newt-mcp-server", *argv])


if __name__ == "__main__":
    main()
