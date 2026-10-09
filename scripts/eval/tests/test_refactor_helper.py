"""#2804: grader infrastructure failure is not a failed refactor."""

import contextlib
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import test_refactor as fixtures

grader = fixtures.grader


class Helper(unittest.TestCase):
    def tearDown(self):
        grader.syntax_helper.cache_clear()

    def test_missing_offline_dependency_is_invalid_and_replays_exit_two(self):
        case = fixtures.Grade()
        case.setUp()
        self.addCleanup(case.doCleanups)
        grader.syntax_helper.cache_clear()
        message = (
            "env exited 101: failed to download proc-macro2; --offline was specified"
        )
        with patch.object(grader, "run", side_effect=grader.EvidenceError(message)):
            report = case.grade()
        self.assertEqual(report.get("status"), "INVALID", report)
        self.assertEqual(report["criteria"]["extraction"]["status"], "UNGRADED")
        self.assertIn("prepare-helper", report["error"])
        self.assertIn("proc-macro2", report["error"])
        self.assertFalse(report["pass"])
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "report.json"
            path.write_text(json.dumps(grader.seal(report)))
            with (
                patch.object(
                    grader.sys, "argv", ["grader", "--verify-report", str(path)]
                ),
                contextlib.redirect_stdout(io.StringIO()),
                contextlib.redirect_stderr(io.StringIO()) as stderr,
            ):
                self.assertEqual(grader.main(), 2)
            self.assertIn("INVALID", stderr.getvalue())

    def test_prebuilt_helper_needs_no_cargo_cache(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / "helper"
            binary.write_text("#!/bin/sh\nexit 0\n")
            binary.chmod(0o755)
            with patch.object(grader, "run") as run:
                self.assertEqual(grader.syntax_helper(binary), binary.resolve())
            run.assert_not_called()

    def test_prepare_builds_locked_with_downloads_allowed(self):
        grader.syntax_helper.cache_clear()
        with patch.object(grader, "run") as run:
            grader.syntax_helper(prepare=True)
        argv = run.call_args.args[0]
        self.assertIn("build", argv)
        self.assertIn("--locked", argv)
        self.assertNotIn("--offline", argv)

    def test_preparation_failure_is_invalid_exit_two(self):
        grader.syntax_helper.cache_clear()
        with (
            patch.object(grader.sys, "argv", ["grader", "--prepare-helper"]),
            patch.object(
                grader, "run", side_effect=grader.EvidenceError("registry unavailable")
            ),
            contextlib.redirect_stdout(io.StringIO()) as stdout,
            contextlib.redirect_stderr(io.StringIO()),
        ):
            self.assertEqual(grader.main(), 2)
        report = grader.verify(json.loads(stdout.getvalue()))
        self.assertEqual(report["status"], "INVALID")

    def test_missing_prebuilt_helper_is_invalid(self):
        case = fixtures.Grade()
        case.setUp()
        self.addCleanup(case.doCleanups)
        case.args.syntax_helper = Path(case.temp.name) / "missing-helper"
        with patch.object(grader, "run") as run:
            report = case.grade()
        self.assertEqual(report["status"], "INVALID")
        self.assertEqual(report["criteria"]["extraction"]["status"], "UNGRADED")
        run.assert_not_called()

    def test_prebuilt_parser_grades_without_cargo_even_with_empty_cache(self):
        # Real parser, fake Git/GitHub: the prepared binary is the only build input.
        import os

        binary = grader.syntax_helper()
        case = fixtures.Grade()
        case.setUp()
        self.addCleanup(case.doCleanups)
        case.args.syntax_helper = binary
        original_run = grader.run

        def no_cargo(argv, cwd):
            self.assertNotIn("cargo", argv)
            return original_run(argv, cwd)

        with (
            tempfile.TemporaryDirectory() as empty_cache,
            patch.dict(os.environ, {"CARGO_HOME": empty_cache}),
            patch.object(grader, "run", side_effect=no_cargo),
        ):
            report = case.grade()
        self.assertTrue(report["pass"], report)
