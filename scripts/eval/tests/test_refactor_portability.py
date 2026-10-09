"""Grading must not require Linux-only scheduling tools."""

from pathlib import Path
import unittest
from unittest.mock import patch

import test_refactor as fixtures

grader = fixtures.grader


class Portability(unittest.TestCase):
    def test_helper_build_omits_unavailable_ionice(self):
        """macOS lacks ionice; helper preparation must still launch Cargo."""
        self.addCleanup(grader.syntax_helper.cache_clear)
        grader.syntax_helper.cache_clear()
        with (
            patch("shutil.which", return_value=None),
            patch.object(grader, "run") as run,
        ):
            grader.syntax_helper(prepare=True)
        argv = run.call_args.args[0]
        self.assertNotIn("ionice", argv)
        self.assertEqual(
            argv[:6],
            ["env", "RUSTC_WRAPPER=", "CARGO_BUILD_JOBS=4", "nice", "-n", "10"],
        )

    def test_clone_check_prefix_tracks_ionice_availability(self):
        """Both grading hosts keep nice; only hosts with ionice invoke it."""
        for available in (None, "/usr/bin/ionice"):
            with (
                self.subTest(available=available),
                patch("shutil.which", return_value=available),
            ):
                case = fixtures.Grade()
                case.setUp()
                try:
                    # Avoid building the Rust parser: this tests the clone command.
                    with (
                        patch.object(
                            grader, "syntax_helper", return_value=Path("helper")
                        ),
                        patch.object(grader, "invoke_helper", return_value="MATCH"),
                    ):
                        case.grade()
                    argv = next(a for a, _ in case.fake.calls if "cargo" in a)
                    self.assertEqual("ionice" in argv, available is not None)
                    self.assertIn("nice", argv)
                finally:
                    case.doCleanups()
