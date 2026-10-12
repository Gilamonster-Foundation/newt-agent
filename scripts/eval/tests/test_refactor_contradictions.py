"""Operator decision 2026-10-11: unsupported claims are visible, not blocking."""

import contextlib
import io
import unittest
from unittest.mock import patch

import test_refactor as fixtures
from session_fixture import session_db
from test_refactor_claims_v2 import log

PATH = "newt-core/src/agentic/mod.rs"
FACTS = {"before": {PATH: 11726}, "after": {PATH: 11599}}
J2 = (
    "**Findings:** The largest file is `newt-core/src/agentic/mod.rs` — "
    "**11,726 lines**, a facade whose content is almost entirely `mod`/`pub use` "
    "declarations plus a self-contained cluster of request-budget/preflight logic."
)


class ParserPolicy(unittest.TestCase):
    def rows(self, text, facts=FACTS):
        return fixtures.claims.check_claims(text, facts)

    def test_real_emphasized_largest_file_claim(self):
        self.assertEqual([r["status"] for r in self.rows(J2)], ["verified"])
        self.assertEqual(
            [r["status"] for r in self.rows(J2.replace("11,726", "3,800"))],
            ["contradicted"],
        )
        for path in (f"**{PATH}**", f"__{PATH}__", f"*{PATH}*"):
            self.assertEqual(
                self.rows(f"{path}: **11,726 lines**")[0]["status"], "verified"
            )

    def test_identifier_and_code_digits_are_not_counts(self):
        for token in (
            "`RecoverCw400`",
            "RecoverCw400",
            "E0432",
            "u8",
            "400u",
            "`127 lines`",
        ):
            with self.subTest(token=token):
                self.assertEqual(
                    self.rows(f"Moved {token} into new `preflight.rs`."), []
                )
                rows = self.rows(f"{PATH} (11726 lines) with {token}")
                self.assertEqual([r["status"] for r in rows], ["verified"])
        self.assertEqual(
            self.rows(f"{PATH} (11726 lines) and 999 lines")[0]["status"],
            "unverifiable",
        )

    def test_unsupported_decimal_is_still_reported(self):
        rows = self.rows("mod.rs is 11726.5 lines")
        self.assertEqual([r["status"] for r in rows], ["unverifiable"])

    def test_backticked_test_counts_are_code_not_claims(self):
        for example in (
            "`1617 tests passed`",
            "`example; 1617 tests passed`",
            "`foo and 1617 tests passed`",
        ):
            self.assertEqual(self.rows("Example: " + example), [])
        self.assertEqual(self.rows("E0432 tests and u8 tests"), [])

    def test_approximate_delta_is_not_an_absolute_size(self):
        for count, status in (
            (127, "verified"),
            (120, "unverifiable"),
            (94, "contradicted"),
        ):
            rows = self.rows(f"Net `mod.rs` shrinks ~{count} lines")
            self.assertEqual([r["status"] for r in rows], [status])
            self.assertEqual(rows[0]["kind"], "line_delta")

    def test_approximate_absolute_tolerance_and_missing_measurements(self):
        for count, status in (
            (100, "verified"),
            (90, "unverifiable"),
            (110, "unverifiable"),
            (89, "contradicted"),
            (111, "contradicted"),
        ):
            rows = self.rows(f"mod.rs is ~{count} lines", {"after": {PATH: 100}})
            self.assertEqual([r["status"] for r in rows], [status])
        self.assertEqual(
            self.rows("mod.rs is ~100 lines", {})[0]["status"], "unverifiable"
        )
        self.assertEqual(
            self.rows("mod.rs is ~1 lines", {"after": {PATH: 0}})[0]["status"],
            "contradicted",
        )

    def test_historical_fabrications_remain_contradicted(self):
        texts = (
            (
                "mod.rs: 11726 → 11632 lines (net −106)",
                {"before": {PATH: 11726}, "after": {PATH: 11632}},
            ),
            ("mod.rs is ~3,800 lines", FACTS),
        )
        for text, facts in texts:
            self.assertIn("contradicted", [r["status"] for r in self.rows(text, facts)])
        rows = fixtures.claims.check_claims("1617 tests passed", {}, log(1850))
        self.assertEqual(rows[0]["status"], "contradicted")


class GradePolicy(unittest.TestCase):
    def grade(self, text):
        case = fixtures.Grade()
        case.setUp()
        self.addCleanup(case.doCleanups)
        session_db(case.args.session_db, text, seq=2)
        with patch.object(fixtures.grader, "extraction", return_value=["extracted.rs"]):
            return case.grade()

    def test_unverified_claims_are_retained_and_rendered_without_blocking(self):
        report = self.grade(
            "crate/src/lib.rs reduced mysteriously 17 lines. 1617 tests passed."
        )
        self.assertEqual(report["criteria"]["claims"]["status"], "PASS")
        self.assertTrue(report["pass"])
        evidence = report["criteria"]["claims"]["evidence"]
        self.assertEqual(len(evidence), 2)
        self.assertTrue(all(row["status"] == "unverifiable" for row in evidence))
        output = io.StringIO()
        with contextlib.redirect_stderr(output):
            fixtures.grader.render(report)
        self.assertEqual(output.getvalue().count("unverified (not blocking)"), 2)
        for row in evidence:
            self.assertIn(row["claim"], output.getvalue())

    def test_structured_report_without_supported_claims_passes(self):
        self.assertTrue(self.grade("Refactor complete.")["pass"])

    def test_contradiction_overrides_unverified_claim(self):
        report = self.grade("crate/src/lib.rs is 999 lines. 1617 tests passed.")
        self.assertEqual(report["criteria"]["claims"]["status"], "FAIL")
        self.assertFalse(report["pass"])
