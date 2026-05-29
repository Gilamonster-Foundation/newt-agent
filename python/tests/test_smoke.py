"""Smoke tests for the newt_agent Python wrapper.

These run after the wheel is installed; they don't compile Rust.
Skip if the binary isn't on PATH (so they don't fail in pure-cargo CI).
"""

from __future__ import annotations

import shutil
import subprocess

import pytest


@pytest.mark.skipif(
    shutil.which("newt") is None,
    reason="newt binary not on PATH; run after `pip install`",
)
def test_newt_help_runs():
    result = subprocess.run(
        ["newt", "--help"], capture_output=True, text=True, check=False
    )
    assert result.returncode == 0
    assert "newt" in result.stdout.lower()


@pytest.mark.skipif(
    shutil.which("newt") is None,
    reason="newt binary not on PATH; run after `pip install`",
)
def test_python_m_invocation():
    result = subprocess.run(
        ["python", "-m", "newt_agent", "--version"],
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0
