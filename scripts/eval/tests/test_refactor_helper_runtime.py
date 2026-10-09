"""#2804: helper execution/protocol failures invalidate grading and replay."""

import contextlib
import io
import json
import os
from pathlib import Path
import unittest
from unittest.mock import patch

import test_refactor as fixtures

grader = fixtures.grader


@unittest.skipUnless(os.name == "posix", "executable fixture scripts require POSIX")
class HelperRuntime(unittest.TestCase):
    def setUp(self):
        self.case = fixtures.Grade()
        self.case.setUp()
        self.addCleanup(self.case.doCleanups)
        self.addCleanup(grader.syntax_helper.cache_clear)
        self.binary = Path(self.case.temp.name) / "helper"
        self.case.args.syntax_helper = self.binary

    def executable(self, contents):
        self.binary.write_text(contents)
        self.binary.chmod(0o755)
        grader.syntax_helper.cache_clear()

    def assert_invalid_and_replay(self, report):
        self.assertEqual(report.get("status"), "INVALID", report)
        self.assertFalse(report["pass"])
        self.assertEqual(report["criteria"]["extraction"]["status"], "UNGRADED")
        self.assertIn("helper", report["error"])
        path = Path(self.case.temp.name) / "invalid.json"
        path.write_text(json.dumps(grader.seal(report)))
        with (
            patch.object(grader.sys, "argv", ["grader", "--verify-report", str(path)]),
            contextlib.redirect_stdout(io.StringIO()) as stdout,
            contextlib.redirect_stderr(io.StringIO()) as stderr,
        ):
            self.assertEqual(grader.main(), 2)
        self.assertEqual(grader.verify(json.loads(stdout.getvalue())), report)
        self.assertIn("INVALID", stderr.getvalue())

    def test_unlaunchable_executable_is_invalid(self):
        self.executable("not an executable format\n")
        self.assert_invalid_and_replay(self.case.grade())

    def test_missing_loader_is_invalid(self):
        self.executable("#!/nonexistent-helper-loader\n")
        self.assert_invalid_and_replay(self.case.grade())

    def test_crash_and_bad_responses_are_invalid(self):
        for body in (
            "kill -TERM $$",
            "exit 1",
            "exit 2",
            "exit 0",
            "printf 'garbage\\n'",
            "printf 'MATCH\\nNONE\\n'",
            "printf 'MATCH\\n'; exit 2",
            "printf 'MATCH\\n'; printf 'unexpected warning\\n' >&2",
            "printf 'unsupported Rust syntax: bad\\n' >&2; exit 1",
            "printf 'unsupported Rust syntax: bad\\nextra noise\\n' >&2; exit 2",
        ):
            with self.subTest(body=body):
                self.executable("#!/bin/sh\n" + body + "\n")
                self.assert_invalid_and_replay(self.case.grade())

    def test_explicit_unsupported_syntax_still_fails_extraction(self):
        for diagnostic in (
            "unsupported Rust syntax: expected an item",
            "unsupported attributes affecting candidate module",
            "unsupported attributes affecting candidate declarations",
        ):
            with self.subTest(diagnostic=diagnostic):
                self.executable(
                    f"#!/bin/sh\nprintf '%s\\n' '{diagnostic}' >&2\nexit 2\n"
                )
                report = self.case.grade()
                self.assertNotEqual(report.get("status"), "INVALID")
                self.assertEqual(report["criteria"]["extraction"]["status"], "FAIL")
                self.assertIn(diagnostic, report["criteria"]["extraction"]["evidence"])

    def test_timeout_and_decode_error_are_invalid(self):
        import subprocess

        self.executable("#!/bin/sh\nexit 0\n")
        for error in (
            subprocess.TimeoutExpired([str(self.binary)], 120),
            UnicodeDecodeError("utf-8", b"\xff", 0, 1, "invalid start byte"),
        ):
            with (
                self.subTest(error=type(error).__name__),
                patch.object(grader.subprocess, "run", side_effect=error),
            ):
                self.assert_invalid_and_replay(self.case.grade())
