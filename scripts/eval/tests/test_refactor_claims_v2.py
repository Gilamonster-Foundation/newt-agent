"""Counted E1 regression: absolute sizes and a named filtered test invocation."""

import subprocess
import sys
import unittest
from pathlib import Path
from unittest.mock import patch

import test_refactor as fixtures
from session_fixture import session_db

claims, grader = fixtures.claims, fixtures.grader
# Exact relevant lines from the stored assistant reply, without publication data.
SIZE = "**Refactor complete:** Extracted the tool-call classification cluster (11,726-line `newt-core/src/agentic/mod.rs`) into a new cohesive `agentic/tool_classification.rs` submodule — 6 pure predicates + 1 helper, re-exported `pub(crate)`. Pure code motion, no behavior change."
TEST = "- `cargo test -p newt-core cap_exit_unit_tests` — 15 passed"


def log(passed=15, failed=0):
    return (
        "    Finished `test` profile [unoptimized] target(s) in 0.1s\n"
        "     Running unittests src/lib.rs (target/debug/deps/example)\n"
        f"running {passed + failed} tests\n"
        f'test result: {"FAILED" if failed else "ok"}. {passed} passed; {failed} failed; '
        "0 ignored; 0 measured; 100 filtered out; finished in 0.1s\n"
        f"cargo-test-exit: {101 if failed else 0}\n"
    )


class Sizes(unittest.TestCase):
    facts = {
        "before": {"newt-core/src/agentic/mod.rs": 11726},
        "after": {"newt-core/src/agentic/mod.rs": 11000},
    }

    def test_actual_e1_size_is_verified(self):
        rows = claims.check_claims(SIZE, self.facts)
        self.assertTrue(rows)
        self.assertTrue(all(r["status"] == "verified" for r in rows), rows)

    def test_unqualified_sizes_match_either_revision_or_contradict(self):
        for n, status in (
            (11726, "verified"),
            (11000, "verified"),
            (15, "contradicted"),
        ):
            for text in (
                f"{n}-line mod.rs",
                f"mod.rs ({n} lines)",
                f"mod.rs is {n} lines",
            ):
                with self.subTest(text=text):
                    self.assertEqual(
                        claims.check_claims(text, self.facts)[0]["status"], status
                    )

    def test_explicit_after_size_cannot_match_only_before(self):
        for text in ("mod.rs now 11726 lines", "mod.rs after: 11726"):
            self.assertEqual(
                claims.check_claims(text, self.facts)[0]["status"], "contradicted"
            )

    def test_missing_or_ambiguous_measurements_stay_unknown(self):
        for facts in (
            {},
            {"before": {"a/mod.rs": 11726}, "after": {"b/mod.rs": 11726}},
            {"before": {"a/mod.rs": 11726, "b/mod.rs": 11726}},
        ):
            self.assertEqual(
                claims.check_claims("11726-line mod.rs", facts)[0]["status"],
                "unverifiable",
            )

    def test_embedded_delta_is_never_an_absolute(self):
        for text in (
            "The reduction (11726-line mod.rs)",
            "mod.rs (11726 lines) smaller",
            "The difference between old and new (11726-line mod.rs)",
        ):
            self.assertTrue(
                all(
                    r["status"] == "unverifiable"
                    for r in claims.check_claims(text, self.facts)
                )
            )


class NamedTests(unittest.TestCase):
    def setUp(self):
        self.case = fixtures.Grade()
        self.case.setUp()
        self.addCleanup(self.case.doCleanups)
        self.output = log()
        self.seq = 1

    def grade(self, text):
        self.seq += 1
        session_db(self.case.args.session_db, text, seq=self.seq)

        def command(argv, cwd):
            result = self.case.fake(argv, cwd)
            return self.output if "cargo" in argv and "test" in argv else result

        with patch.object(grader, "extraction", return_value=["extracted.rs"]):
            return grader.grade(self.case.args, command)

    def test_e1_runs_exact_filter_in_existing_clone(self):
        report = self.grade(TEST)
        self.assertEqual(report["criteria"]["claims"]["status"], "PASS", report)
        calls = [(a, c) for a, c in self.case.fake.calls if "cargo" in a]
        self.assertEqual(len(calls), 2)
        check, test = calls
        self.assertEqual(check[1], test[1])
        self.assertEqual(
            test[0],
            [
                *grader.cargo_prefix(),
                "cargo",
                "test",
                "-p",
                "newt-core",
                "cap_exit_unit_tests",
            ],
        )

    def test_lib_and_one_filter_are_allowed_in_either_order(self):
        for tail in (
            "--lib",
            "--lib cap_exit_unit_tests",
            "cap_exit_unit_tests --lib",
            "",
        ):
            with self.subTest(tail=tail):
                report = self.grade(f"cargo test -p newt-core {tail} — 15 passed")
                self.assertEqual(report["criteria"]["claims"]["status"], "PASS")

    def test_repeated_command_runs_once_and_other_counts_do_not_borrow(self):
        report = self.grade(TEST + "\n" + TEST + "\n16 tests passed")
        self.assertEqual(report["criteria"]["claims"]["status"], "UNGRADED")
        self.assertEqual(sum("test" in a for a, _ in self.case.fake.calls), 1)

    def test_different_commands_keep_their_own_counts(self):
        session_db(
            self.case.args.session_db,
            TEST + "\n`cargo test -p newt-core other` — 14 passed",
            seq=2,
        )

        def command(argv, cwd):
            output = self.case.fake(argv, cwd)
            if "test" in argv:
                return log(14 if argv[-1] == "other" else 15)
            return output

        with patch.object(grader, "extraction", return_value=["extracted.rs"]):
            report = grader.grade(self.case.args, command)
        self.assertEqual(report["criteria"]["claims"]["status"], "PASS")
        rows = report["criteria"]["claims"]["evidence"]
        self.assertEqual([r["actual"] for r in rows], [15, 14])

    def test_clone_failure_does_not_run_tests(self):
        self.case.fake.check_fails = True
        report = self.grade(TEST)
        self.assertEqual(report["criteria"]["claims"]["status"], "UNGRADED")
        self.assertFalse(any("test" in a for a, _ in self.case.fake.calls))

    def test_filtered_mismatch_is_contradicted(self):
        self.output = log(14)
        self.assertEqual(self.grade(TEST)["criteria"]["claims"]["status"], "FAIL")

    def test_restricted_commands_never_run_or_borrow_supplied_log(self):
        for suffix in (
            "--workspace",
            "-- --ignored",
            "; touch bad",
            "one two",
            "--manifest-path Cargo.toml",
            "--lib --lib",
            "--release",
            "$(touch bad)",
            "filter && echo yes",
        ):
            with self.subTest(suffix=suffix):
                self.case.fake.calls.clear()
                self.case.args.test_log = self.case.args.transcript.parent / "tests.log"
                self.case.args.test_log.write_text(log())
                report = self.grade(f"`cargo test -p newt-core {suffix}` — 15 passed")
                self.assertEqual(report["criteria"]["claims"]["status"], "UNGRADED")
                self.assertFalse(any("test" in a for a, _ in self.case.fake.calls))

    def test_failed_and_partial_invocations_do_not_pass(self):
        for output, status in (
            (log(15, 1), "FAIL"),
            (log().split("cargo-test-exit:")[0], "UNGRADED"),
        ):
            self.output = output
            self.assertEqual(self.grade(TEST)["criteria"]["claims"]["status"], status)

    def test_capture_preserves_order_and_nonzero_completion(self):
        """The real runner must combine Cargo stderr and libtest stdout before parsing."""
        result = subprocess.CompletedProcess(
            [], 101, log(15, 1).split("cargo-test-exit:")[0], ""
        )
        with patch.object(grader.subprocess, "run", return_value=result) as run:
            output = grader.run(
                [*grader.cargo_prefix(), "cargo", "test", "-p", "example"], Path("/tmp")
            )
        self.assertEqual(output, log(15, 1))
        self.assertEqual(run.call_args.kwargs["stderr"], subprocess.STDOUT)
        self.assertEqual(run.call_args.kwargs["env"]["CARGO_BUILD_JOBS"], "4")


class CaptureGroundTruth(unittest.TestCase):
    def test_real_child_combines_stderr_stdout_and_exit_status(self):
        """Ground the mocked capture: one child alternates channels and exits 101."""
        source = (
            "import os; "
            'os.write(2, b"    Finished `test` profile target(s) in 0.1s\\n"); '
            'os.write(2, b"     Running unittests src/lib.rs (target/debug/example)\\n"); '
            'os.write(1, b"running 1 test\\n"); '
            'os.write(1, b"test result: FAILED. 0 passed; 1 failed; 0 ignored; '
            '0 measured; 0 filtered out; finished in 0.1s\\n"); '
            "raise SystemExit(101)"
        )
        # The trailing argv selects the production Cargo-test capture path;
        # this hermetic child stands in for Cargo without building a crate.
        output = grader.run([sys.executable, "-c", source, "cargo", "test"], Path.cwd())
        totals = claims.test_totals(output)
        self.assertIsNotNone(totals, output)
        self.assertEqual(totals["failed"], 1)
        self.assertFalse(totals["success"])
