"""Smoke tests for newt_agent.coder."""

from __future__ import annotations

import tempfile
from pathlib import Path

import newt_agent.coder as coder


def test_normalize_emission_whole_files() -> None:
    raw = "FILE: src/lib.rs\npub fn hello() {}\nEND-FILE\n"
    em = coder.normalize_emission(raw)
    assert em.kind == "whole_files"
    assert em.is_whole_files()
    files = em.as_whole_files()
    assert files == {"src/lib.rs": "pub fn hello() {}"}


def test_normalize_emission_unified_diff() -> None:
    raw = "--- a/foo.rs\n+++ b/foo.rs\n@@ -1 +1 @@\n-old\n+new\n"
    em = coder.normalize_emission(raw)
    assert em.kind == "unified_diff"
    assert em.is_unified_diff()
    # `normalize_emission` peels a single enclosing fence and trims; for
    # bare diff input it returns the same payload minus the trailing
    # newline that `strip_outer_fences` strips.
    diff = em.as_unified_diff()
    assert "@@ -1 +1 @@" in diff
    assert "-old" in diff
    assert "+new" in diff


def test_normalize_emission_prose() -> None:
    em = coder.normalize_emission("I've updated the file.")
    assert em.kind == "prose"
    assert em.is_prose()
    assert em.as_prose() == "I've updated the file."


def test_build_prompt_injects_mentioned_files() -> None:
    with tempfile.TemporaryDirectory() as d:
        (Path(d) / "src").mkdir()
        (Path(d) / "src" / "lib.rs").write_text("pub fn greet() {}\n")
        (Path(d) / "src" / "other.rs").write_text("pub fn other() {}\n")
        p = coder.build_prompt(d, "Rename greet to hello in src/lib.rs")
        assert "FILE: src/lib.rs" in p.user
        assert "pub fn greet" in p.user
        assert "src/other.rs" not in p.user
        assert len(p.included_files) == 1


def test_scan_workspace_for_files_prefers_mentioned() -> None:
    with tempfile.TemporaryDirectory() as d:
        (Path(d) / "src").mkdir()
        (Path(d) / "src" / "lib.rs").write_text("pub fn x() {}\n")
        (Path(d) / "src" / "other.rs").write_text("pub fn y() {}\n")
        files = coder.scan_workspace_for_files(d, "Update src/lib.rs please")
        assert "src/lib.rs" in files
        assert "src/other.rs" not in files


def test_whole_files_emission_helper() -> None:
    em = coder.whole_files_emission({"a.rs": "fn a() {}"})
    assert em.is_whole_files()
    assert em.as_whole_files() == {"a.rs": "fn a() {}"}


def test_system_prompt_pinned() -> None:
    # Load-bearing per the bake-off; regression-pinned in Rust tests too.
    assert "FILE: <relative path>" in coder.WHOLE_FILE_SYSTEM_PROMPT
    assert "END-FILE" in coder.WHOLE_FILE_SYSTEM_PROMPT
