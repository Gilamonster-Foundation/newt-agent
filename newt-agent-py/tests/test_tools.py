"""Smoke tests for newt_agent.tools."""

from __future__ import annotations

import tempfile
from pathlib import Path

import pytest

import newt_agent.tools as tools


def test_read_happy_path() -> None:
    with tempfile.NamedTemporaryFile("w", suffix=".txt", delete=False) as f:
        f.write("hello\nworld\n")
        path = f.name
    try:
        content = tools.read(path)
        assert "hello" in content and "world" in content
    finally:
        Path(path).unlink()


def test_read_missing_file_raises() -> None:
    with pytest.raises(Exception):
        tools.read("/tmp/newt-agent-py-no-such-file-xyz")


def test_edit_applies_patch() -> None:
    with tempfile.TemporaryDirectory() as d:
        path = Path(d) / "hello.txt"
        path.write_text("line1\nline2\n")
        diff = (
            "--- a/hello.txt\n"
            "+++ b/hello.txt\n"
            "@@ -1,2 +1,2 @@\n"
            " line1\n"
            "-line2\n"
            "+edited\n"
        )
        tools.edit(str(path), diff)
        assert path.read_text() == "line1\nedited\n"


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


def test_search_finds_matches() -> None:
    with tempfile.TemporaryDirectory() as d:
        (Path(d) / "a.txt").write_text("needle in haystack\nnothing here\n")
        (Path(d) / "b.txt").write_text("more needle stuff\n")
        hits = tools.search("needle", d)
        assert len(hits) == 2
        for h in hits:
            assert "needle" in h.line
            assert h.line_number >= 1
            assert h.path.endswith(".txt")


def test_search_no_hits_returns_empty_list() -> None:
    with tempfile.TemporaryDirectory() as d:
        (Path(d) / "a.txt").write_text("hello\n")
        assert tools.search("zzz_absent", d) == []


def test_search_invalid_regex_raises() -> None:
    with tempfile.TemporaryDirectory() as d:
        with pytest.raises(Exception):
            tools.search("[unclosed", d)
