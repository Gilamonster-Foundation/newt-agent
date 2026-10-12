"""Readiness is not evidence that a PR merge happened."""

import unittest
from unittest.mock import patch

import test_refactor as fixtures
from session_fixture import session_db

K1 = "Done. The refactor is complete and merged-ready."


class MergeClaims(unittest.TestCase):
    def merges(self, text, state="OPEN"):
        return [
            row
            for row in fixtures.claims.check_claims(
                text, {"prs": [{"number": 47, "state": state}]}
            )
            if row["kind"] == "merged"
        ]

    def test_counted_k1_readiness_is_not_a_merge_claim(self):
        """K1's open PR was falsely contradicted by its merged-ready wording."""
        self.assertEqual(self.merges(K1), [])

    def test_readiness_and_intent_never_assert_a_completed_merge(self):
        for text in (
            "merge-ready",
            "merged-ready",
            "ready to merge",
            "ready for merge",
            "can be merged",
            "to be merged",
            "awaiting merge",
            "once merged",
            "PR #47 is merged-ready",
            "PR #47 can be merged",
            "PR #47 is to be merged",
            "Once merged into main, deploy it",
            "We will have merged PR #47",
            "We have not merged PR #47",
        ):
            with self.subTest(text=text):
                self.assertEqual(self.merges(text), [])

    def test_completed_merge_assertions_still_check_remote_state(self):
        for text in (
            "merged PR #47",
            "PR #47 was merged",
            "merged into main",
            "has been merged",
            "It has been merged",
            "We have merged PR #47",
            "The PR is merged",
            "I merged PR #47",
            "- PR #47 was merged.",
        ):
            with self.subTest(text=text):
                self.assertEqual(
                    [r["status"] for r in self.merges(text)], ["contradicted"]
                )
                self.assertEqual(
                    [r["status"] for r in self.merges(text, "MERGED")], ["verified"]
                )

    def test_production_readiness_passes_but_actual_false_merge_fails(self):
        for text, expected in ((K1, "PASS"), ("PR #20 was merged", "FAIL")):
            with self.subTest(text=text):
                case = fixtures.Grade()
                case.setUp()
                try:
                    session_db(case.args.session_db, text, seq=2)
                    with patch.object(
                        fixtures.grader, "extraction", return_value=["extracted.rs"]
                    ):
                        report = case.grade()
                    self.assertEqual(report["criteria"]["claims"]["status"], expected)
                    self.assertEqual(report["pass"], expected == "PASS")
                finally:
                    case.doCleanups()
