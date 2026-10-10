"""Whole Cargo invocations and reductions must not become last-binary/file sizes."""

from pathlib import Path
import unittest

import test_refactor as fixtures

claims = fixtures.claims
FIXTURES = Path(__file__).parent / "fixtures" / "refactor"


class TestLogClaims(unittest.TestCase):
    def test_multiple_binaries_include_failures_before_successful_doctests(self):
        """The final doctest's three passes are not the invocation's total."""
        log = (FIXTURES / "cargo-multiple-binaries.txt").read_text()
        rows = claims.check_claims(
            "1846 tests pass; 1 failed; 7 ignored; 1847 tests total", {}, log
        )
        for label, number in (
            ("passed", 1846),
            ("failed", 1),
            ("ignored", 7),
            ("total", 1847),
        ):
            row = next(row for row in rows if row["claim"] == f"{number} tests {label}")
            self.assertEqual(row["actual"], number)
        self.assertTrue(any(row["status"] == "contradicted" for row in rows))

    def test_incomplete_or_concatenated_logs_cannot_verify_counts(self):
        """A final per-binary success does not prove invocation completion."""
        log = (FIXTURES / "cargo-multiple-binaries.txt").read_text()
        for incomplete in (
            log[: log.index("error: 1 target failed:")],
            log + log,
            "test result: ok. 3 passed; 0 failed;",
            log + "     Running tests/missing.rs (target/missing)\n",
        ):
            with self.subTest(log=incomplete[-80:]):
                rows = claims.check_claims("3 tests passed", {}, incomplete)
                self.assertTrue(rows)
                self.assertTrue(
                    all(row["status"] == "unverifiable" for row in rows), rows
                )


class LineDeltaClaims(unittest.TestCase):
    facts = {
        "before": {"crate/mod.rs": 11726},
        "after": {"crate/mod.rs": 10746, "crate/workflow.rs": 1064},
    }

    def test_real_summary_does_not_turn_reduction_into_absolute_size(self):
        """Round 3: compound/approximate reduction prose is not an absolute claim."""
        rows = claims.check_claims(
            (FIXTURES / "line-delta-summary.txt").read_text(), self.facts
        )
        self.assertTrue(rows)
        self.assertTrue(all(row["status"] == "unverifiable" for row in rows), rows)
        self.assertFalse(any(row["claim"] == "mod.rs after: 1041" for row in rows))

    def test_delta_spellings_are_outside_the_absolute_grammar(self):
        for text in (
            "mod.rs drops ~980 lines",
            "mod.rs reduced by 980 lines",
            "mod.rs -980 lines",
            "mod.rs -980",
            "mod.rs reduced by 980",
            "reduced mod.rs by 980 lines",
            "reducing `mod.rs` by 980 lines",
            "mod.rs removed 980 lines",
            "removed 980 lines from mod.rs",
        ):
            with self.subTest(text=text):
                rows = claims.check_claims(text, self.facts)
                self.assertEqual(len(rows), 1, rows)
                self.assertEqual(rows[0]["kind"], "lines")
                self.assertEqual(rows[0]["status"], "unverifiable")

    def test_reduction_before_filename_is_checked_beside_absolute_pair(self):
        """The first incident bullet reports a delta as well as two sizes."""
        rows = claims.check_claims(
            "reducing `mod.rs` by ~1,041 lines (11,726 → 10,746)", self.facts
        )
        self.assertTrue(rows)
        self.assertTrue(all(row["status"] == "unverifiable" for row in rows), rows)

    def test_delta_needs_unambiguous_matching_before_and_after(self):
        for facts in (
            {"after": self.facts["after"]},
            {"before": {"one/mod.rs": 11726}, "after": {"two/mod.rs": 10746}},
            {
                "before": {"one/mod.rs": 11726, "two/mod.rs": 11726},
                "after": {"one/mod.rs": 10746, "two/mod.rs": 10746},
            },
        ):
            rows = claims.check_claims("mod.rs drops 980 lines", facts)
            self.assertEqual(rows[0]["status"], "unverifiable")
            self.assertEqual(rows[0]["kind"], "lines")


class CompleteLogControls(unittest.TestCase):
    log = (
        "    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.1s\n"
        "     Running unittests src/lib.rs (target/debug/deps/example)\n"
        "running 4 tests\n"
        "test result: ok. 3 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.1s\n"
        "   Doc-tests example\n"
        "running 2 tests\n"
        "test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.1s\n"
        "cargo-test-exit: 0\n"
    )

    def test_complete_success_aggregates_binaries_without_counting_ignored_as_run(self):
        rows = claims.check_claims(
            "5 tests pass; 0 failed; 1 ignored; 5 tests total", {}, self.log
        )
        self.assertEqual(len(rows), 4)
        self.assertTrue(all(row["status"] == "verified" for row in rows), rows)

    def test_partial_conflicting_and_combined_captures_are_unverifiable(self):
        for log in (
            self.log.replace("cargo-test-exit: 0\n", ""),
            self.log + self.log,
            self.log.replace("running 4 tests", "running 5 tests"),
            self.log.replace("   Doc-tests example\n", ""),
            self.log.replace(
                "test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.1s\n",
                "",
            ),
            self.log + "unexpected trailing text\n",
        ):
            with self.subTest(log=log):
                rows = claims.check_claims("5 tests passed", {}, log)
                self.assertTrue(
                    all(row["status"] == "unverifiable" for row in rows), rows
                )

    def test_failed_exit_dominates_successful_binary_counts(self):
        rows = claims.check_claims(
            "5 tests passed", {}, self.log.replace("exit: 0", "exit: 101")
        )
        self.assertTrue(any(row["status"] == "contradicted" for row in rows))

    def test_failed_binary_cannot_be_laundered_by_zero_exit(self):
        log = (
            FIXTURES / "cargo-multiple-binaries.txt"
        ).read_text() + "cargo-test-exit: 0\n"
        rows = claims.check_claims("1846 tests passed", {}, log)
        self.assertTrue(all(row["status"] == "unverifiable" for row in rows))


class GradeEvidenceControls(unittest.TestCase):
    def test_incomplete_counts_remain_ungraded_and_never_pass(self):
        """Incomplete invocation evidence must preserve UNGRADED != PASS."""
        from unittest.mock import patch
        from session_fixture import session_db

        case = fixtures.Grade()
        case.setUp()
        self.addCleanup(case.doCleanups)
        session_db(case.args.session_db, "5 tests passed", seq=2)
        case.args.test_log = Path(case.temp.name) / "test.log"
        case.args.test_log.write_text(
            CompleteLogControls.log.replace("cargo-test-exit: 0\n", "")
        )
        with patch.object(fixtures.grader, "extraction", return_value=["extracted.rs"]):
            report = case.grade()
        self.assertEqual(report["criteria"]["claims"]["status"], "UNGRADED")
        self.assertFalse(report["pass"])
