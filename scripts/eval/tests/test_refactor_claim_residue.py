"""PR #2852 review: recognized sizes must not hide unproved adjacent claims."""

import unittest
from unittest.mock import patch

import test_refactor as fixtures
from session_fixture import session_db
from test_refactor_claims_v2 import log


class Residue(unittest.TestCase):
    facts = {
        "before": {"src/mod.rs": 11726},
        "after": {"src/mod.rs": 11632, "src/responses.rs": 254},
        "changed": ["src/mod.rs", "src/responses.rs"],
    }
    pair = "mod.rs: 11726 → 11632 lines (net −94)"

    def test_reviewers_mixed_clause_cannot_pass(self):
        """The false 999-line assertion disappeared after a verified pair/net."""
        for suffix in (
            "responses.rs is 999 lines",
            "responses.rs: 999",
            "999 lines",
            "1 line",
            "responses.rs has 999",
            "responses.rs (254 lines) and 999 lines",
        ):
            for text in (self.pair + " and " + suffix, suffix + " and " + self.pair):
                with self.subTest(text=text):
                    rows = fixtures.claims.check_claims(text, self.facts)
                    self.assertTrue(rows)
                    self.assertTrue(any(r["status"] != "verified" for r in rows), rows)

    def test_all_recognized_sizes_and_non_size_numbers_still_verify(self):
        for text in (
            self.pair + " and responses.rs (254 lines)",
            "responses.rs (254 lines) with 3 helpers and 2 callers",
            "src/mod.rs (11726 lines) and src/responses.rs (254 lines)",
            "src/mod.rs (11726 lines) into responses.rs with 6 pure predicates + 1 helper",
        ):
            rows = fixtures.claims.check_claims(text, self.facts)
            self.assertTrue(rows)
            self.assertTrue(all(r["status"] == "verified" for r in rows), rows)

    def test_inventories_ignore_adjectives_and_unrelated_logs(self):
        """A run with ten tests does not prove a module contains ten tests."""
        for description in (
            "10 tests",
            "10 pure fs-free tests",
            "10 unit tests",
            "10 new regression tests",
            "10 tests total",
        ):
            with self.subTest(description=description):
                rows = fixtures.claims.check_claims(
                    "New src/responses.rs (254 lines) with " + description,
                    self.facts,
                    log(10),
                )
                inventory = [r for r in rows if r["kind"] == "tests"]
                self.assertTrue(inventory, rows)
                self.assertTrue(
                    all(r["status"] == "unverifiable" for r in inventory), rows
                )

    def test_named_run_and_independent_run_log_still_verify(self):
        text = "`cargo test -p example --lib` — 10 passed"
        argv = ("cargo", "test", "-p", "example", "--lib")
        for rows in (
            fixtures.claims.check_claims(text, {}, "", {argv: log(10)}),
            fixtures.claims.check_claims("10 tests passed", {}, log(10)),
        ):
            self.assertTrue(rows)
            self.assertTrue(all(r["status"] == "verified" for r in rows), rows)

    def test_production_grade_cannot_hide_residue_or_plain_inventory(self):
        for summary in (
            "lib.rs: 10 → 1 lines (net −9) and extracted.rs is 999 lines",
            "New crate/src/extracted.rs (1 lines) with 10 tests",
        ):
            with self.subTest(summary=summary):
                case = fixtures.Grade()
                case.setUp()
                try:
                    session_db(case.args.session_db, summary, seq=2)
                    case.args.test_log = case.args.transcript.parent / "tests.log"
                    case.args.test_log.write_text(log(10))
                    with patch.object(
                        fixtures.grader, "extraction", return_value=["extracted.rs"]
                    ):
                        report = case.grade()
                    self.assertEqual(
                        report["criteria"]["claims"]["status"], "UNGRADED", report
                    )
                    self.assertFalse(report["pass"])
                finally:
                    case.doCleanups()
