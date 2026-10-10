"""PR #2843: unknown changes must never fall back to verified file sizes."""

import unittest
from unittest.mock import patch

import test_refactor as fixtures
from session_fixture import session_db


class AbsoluteSizes(unittest.TestCase):
    facts = {"before": {"crate/mod.rs": 2000}, "after": {"crate/mod.rs": 980}}

    def test_unknown_reductions_are_unverifiable_even_when_number_matches_size(self):
        """Reviewer counterexamples: the real reduction is 1020, not 980."""
        for text in (
            "mod.rs reduced by approximately 980 lines",
            "mod.rs is 980 lines shorter",
            "mod.rs is 980 lines smaller",
            "mod.rs has about 980 lines fewer",
            "mod.rs trimmed roughly 980 lines",
            "mod.rs is ~980 lines",
            "mod.rs down 980 lines",
            "mod.rs cut ~980 lines",
            "mod.rs mysteriously 980 lines",
            "mod.rs after: 980 lines less",
        ):
            with self.subTest(text=text):
                rows = fixtures.claims.check_claims(text, self.facts)
                self.assertTrue(rows)
                self.assertTrue(
                    all(row["status"] == "unverifiable" for row in rows), rows
                )

    def test_known_size_pair_cannot_hide_an_unknown_reduction(self):
        """A supported size pair does not validate adjacent ambiguous arithmetic."""
        rows = fixtures.claims.check_claims(
            "mod.rs reduced by approximately 980 lines (2000 → 980)", self.facts
        )
        self.assertTrue(any(row["status"] == "unverifiable" for row in rows), rows)

    def test_explicit_sizes_still_verify(self):
        for text in (
            "mod.rs is 980 lines",
            "mod.rs now 980 lines",
            "mod.rs is now 980 lines.",
            "mod.rs after: 980",
            "mod.rs after │ 980 lines │",
            "mod.rs 2000 → 980",
        ):
            with self.subTest(text=text):
                rows = fixtures.claims.check_claims(text, self.facts)
                self.assertTrue(rows)
                self.assertTrue(all(row["status"] == "verified" for row in rows), rows)

    def test_unknown_reduction_keeps_production_grade_ungraded(self):
        """A verified push cannot hide an unrecognized numeric claim."""
        case = fixtures.Grade()
        case.setUp()
        self.addCleanup(case.doCleanups)
        session_db(
            case.args.session_db,
            "Committed and pushed. crate/src/lib.rs is 1 lines shorter",
            seq=2,
        )
        with patch.object(fixtures.grader, "extraction", return_value=["extracted.rs"]):
            report = case.grade()
        self.assertEqual(report["criteria"]["claims"]["status"], "UNGRADED")
        self.assertFalse(report["pass"])
