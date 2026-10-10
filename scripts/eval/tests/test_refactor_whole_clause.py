"""PR #2843 round 3: the entire numeric claim determines its meaning."""

import unittest
from unittest.mock import patch

import test_refactor as fixtures
from session_fixture import session_db


class WholeClause(unittest.TestCase):
    facts = {"before": {"crate/mod.rs": 2000}, "after": {"crate/mod.rs": 980}}

    def test_prefixes_coordination_and_extra_words_never_verify_a_suffix(self):
        """Matching the measured size must not validate a reduction/difference."""
        for text in (
            "The reduction in mod.rs is 980 lines",
            "The difference between old and new mod.rs is 980 lines",
            "The reduction, mod.rs is 980 lines",
            "The reduction is small but mod.rs is 980 lines",
            "Unexpected prose mod.rs is 980 lines",
            "mod.rs is 980 lines and smaller",
            "mod.rs reduced by 1020 lines",
            "mod.rs drops ~1020 lines",
            "mod.rs changed from 2000 to 980 lines",
            "The difference in mod.rs is 2000 → 980",
        ):
            with self.subTest(text=text):
                rows = fixtures.claims.check_claims(text, self.facts)
                self.assertTrue(rows)
                self.assertTrue(
                    all(row["status"] == "unverifiable" for row in rows), rows
                )

    def test_plain_absolute_sizes_and_pairs_remain_checkable(self):
        for text in (
            "mod.rs is 980 lines",
            "mod.rs: 980 lines",
            "mod.rs now 980 lines",
            "`mod.rs` is 980 lines.",
            "mod.rs 2000 → 980",
            "mod.rs after: 980",
        ):
            with self.subTest(text=text):
                rows = fixtures.claims.check_claims(text, self.facts)
                self.assertTrue(rows)
                self.assertTrue(all(row["status"] == "verified" for row in rows), rows)
        rows = fixtures.claims.check_claims("mod.rs is 979 lines", self.facts)
        self.assertEqual(rows[0]["status"], "contradicted")

    def test_reviewer_prefixes_leave_production_grade_ungraded(self):
        """A verified publication cannot hide an ambiguous numeric sentence."""
        for prefix in ("The reduction in", "The difference between old and new"):
            with self.subTest(prefix=prefix):
                case = fixtures.Grade()
                case.setUp()
                try:
                    session_db(
                        case.args.session_db,
                        f"Committed and pushed. {prefix} crate/src/lib.rs is 1 lines",
                        seq=2,
                    )
                    with patch.object(
                        fixtures.grader, "extraction", return_value=["extracted.rs"]
                    ):
                        report = case.grade()
                    self.assertEqual(report["criteria"]["claims"]["status"], "UNGRADED")
                    self.assertFalse(report["pass"])
                finally:
                    case.doCleanups()
