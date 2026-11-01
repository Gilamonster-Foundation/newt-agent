"""Smoke tests for newt_agent.tools."""

from __future__ import annotations

import tempfile
from pathlib import Path

import newt_agent.tools as tools


def test_apply_patch_multi_file() -> None:
    with tempfile.TemporaryDirectory() as d:
        a = Path(d) / "a.txt"
        b = Path(d) / "b.txt"
        a.write_text("x\n")
        b.write_text("y\n")
        diff = (
            "--- a/a.txt\n"
            "+++ b/a.txt\n"
            "@@ -1 +1 @@\n"
            "-x\n"
            "+X\n"
            "--- a/b.txt\n"
            "+++ b/b.txt\n"
            "@@ -1 +1 @@\n"
            "-y\n"
            "+Y\n"
        )
        tools.apply_patch(diff, d)
        assert a.read_text() == "X\n"
        assert b.read_text() == "Y\n"


def test_apply_whole_files_writes_files() -> None:
    with tempfile.TemporaryDirectory() as d:
        files = {
            "src/lib.rs": "pub fn hello() {}\n",
            "Cargo.toml": "[package]\nname = 'x'\n",
        }
        written = tools.apply_whole_files(d, files)
        assert sorted(written) == ["Cargo.toml", "src/lib.rs"]
        assert (Path(d) / "src" / "lib.rs").read_text() == "pub fn hello() {}\n"
