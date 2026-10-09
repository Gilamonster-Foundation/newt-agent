"""#2804: transcript size must not determine grader memory use."""

import contextlib
import io
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import test_refactor as fixtures

grader = fixtures.grader


class Transcript(unittest.TestCase):
    def fixture(self):
        case = fixtures.Grade()
        case.setUp()
        self.addCleanup(case.doCleanups)
        return case

    def test_absent_transcript_is_ungraded(self):
        case = self.fixture()
        case.args.transcript = None
        with patch.object(grader, "extraction", return_value=(True, "extracted")):
            report = case.grade()
        self.assertEqual(report["criteria"]["claims"]["status"], "UNGRADED")
        self.assertFalse(report["pass"])
        self.assertFalse(report["operator_inputs"]["complete"])
        with contextlib.redirect_stderr(io.StringIO()) as stderr:
            grader.render(report)
        self.assertEqual(stderr.getvalue().splitlines()[-1], "UNGRADED-claims")

    @unittest.skipUnless(sys.platform == "linux", "RLIMIT_AS proof requires Linux")
    def test_large_transcript_under_fixed_address_space_limit(self):
        # A sparse 256 MiB redraw log exceeds the child's entire 128 MiB limit.
        # No timing assertion: the old whole-file read deterministically fails.
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "large.raw"
            with path.open("wb") as stream:
                stream.seek(256 * 1024 * 1024)
                stream.write(
                    b"\r\x1b[2Kspinner\rSummary\nCommitted and pushed. Opened PR #20.\n"
                )
            code = """
import resource, sys
resource.setrlimit(resource.RLIMIT_AS, (128 * 1024**2, 128 * 1024**2))
from pathlib import Path
from unittest.mock import patch
from test_refactor import Grade, grader
case = Grade()
case.setUp()
try:
    case.args.transcript = Path(sys.argv[1])
    with patch.object(grader, "extraction", return_value=(True, "extracted")):
        result = case.grade()
    assert result["pass"], result
    assert result["summary"] == "Summary\\nCommitted and pushed. Opened PR #20."
finally:
    case.doCleanups()
"""
            result = subprocess.run(
                [sys.executable, "-c", code, str(path)],
                cwd=Path(__file__).parent,
                capture_output=True,
                text=True,
            )
            self.assertEqual(result.returncode, 0, result.stderr)

    def test_window_size_and_cropped_heading(self):
        from refactor_transcript import read_tail

        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "window.raw"
            report = b"Summary\nCommitted and pushed. Opened PR #20.\n"
            path.write_bytes(report + b"x" * (1024 * 1024))
            self.assertEqual(read_tail(path, 1), "")
            self.assertTrue(read_tail(path, 2).startswith("Summary\n"))
            # Cropping the word prefix must not turn NotSummary into Summary.
            path.write_bytes(b"NotSummary\n" + b"x" * (1024 * 1024 - 8))
            self.assertNotIn("Summary", read_tail(path, 1))
            path.write_bytes(b"Summary\ninvalid utf8: \xff\n")
            self.assertIn("\ufffd", read_tail(path, 1))

    def test_provided_transcript_without_report_is_ungraded(self):
        case = self.fixture()
        case.args.transcript.write_text("spinner\rstill working\r")
        with patch.object(grader, "extraction", return_value=(True, "extracted")):
            report = case.grade()
        self.assertEqual(report["criteria"]["claims"]["status"], "UNGRADED")

    def test_cli_accepts_absent_transcript_and_rejects_bad_window(self):
        argv = [
            "grade_refactor.py",
            "--repo",
            ".",
            "--worktree",
            ".",
            "--seed",
            "abc",
            "--branch",
            "refactor",
            "--github",
            "example/lab",
            "--crate",
            "crate",
            "--started-at",
            "100",
        ]
        report = {"pass": False, "criteria": {"claims": {"status": "UNGRADED"}}}
        with (
            patch.object(sys, "argv", argv),
            patch.object(grader, "grade", return_value=report) as grade,
            contextlib.redirect_stdout(io.StringIO()),
            contextlib.redirect_stderr(io.StringIO()),
        ):
            self.assertEqual(grader.main(), 1)
            self.assertIsNone(grade.call_args.args[0].transcript)
            self.assertEqual(grade.call_args.args[0].transcript_tail_mib, 4)
        for size in ("0", "-1", "bad"):
            with (
                self.subTest(size=size),
                patch.object(sys, "argv", argv + ["--transcript-tail-mib", size]),
                contextlib.redirect_stderr(io.StringIO()),
                self.assertRaises(SystemExit) as exit,
            ):
                grader.main()
            self.assertEqual(exit.exception.code, 2)
