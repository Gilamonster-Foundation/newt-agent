"""Counted E2 regressions: attached sizes, ordered counts, and signed net delta."""

import unittest
from unittest.mock import patch

import test_refactor as fixtures
from session_fixture import session_db
from test_refactor_claims_v2 import log

PATH = "newt-core/src/agentic/mod.rs"
NEW = "newt-core/src/agentic/responses.rs"
# Relevant actual assistant text; publication and local-workspace details omitted.
SIZE = "The largest Rust file in the repo was `newt-core/src/agentic/mod.rs` (11,726 lines)"
MODULE = (
    "- New `newt-core/src/agentic/responses.rs` (254 lines) with 10 pure fs-free tests"
)
PAIR = "- `mod.rs`: 11,726 → 11,632 lines (net −106)"


class SentenceClaims(unittest.TestCase):
    def setUp(self):
        self.facts = {
            "before": {PATH: 11726, "other/mod.rs": 100},
            "after": {PATH: 11632, NEW: 254, "other/mod.rs": 100},
            "changed": [PATH, NEW],
        }

    def rows(self, text):
        return fixtures.claims.check_claims(text, self.facts)

    def test_actual_e2_parenthetical_sizes(self):
        rows = self.rows(SIZE)
        self.assertEqual([r["status"] for r in rows], ["verified"])
        rows = self.rows(MODULE)
        self.assertEqual(
            [r["status"] for r in rows if r["kind"] == "lines"], ["verified"]
        )
        self.assertEqual(
            [r["status"] for r in rows if r["kind"] == "tests"], ["unverifiable"]
        )

    def test_unnamed_test_inventory_does_not_borrow_a_run_count(self):
        rows = fixtures.claims.check_claims(MODULE, self.facts, log(10))
        self.assertEqual(
            [r["status"] for r in rows if r["kind"] == "tests"], ["unverifiable"]
        )

    def test_actual_pair_verifies_but_net_deletions_are_not_net_change(self):
        rows = self.rows(PAIR)
        self.assertEqual(
            [r["status"] for r in rows if r["kind"] == "lines"],
            ["verified", "verified"],
        )
        delta = next(r for r in rows if r["kind"] == "line_delta")
        self.assertEqual(delta["status"], "contradicted")
        self.assertEqual(delta["actual"], -94)

    def test_wrong_parenthetical_and_pair_are_contradicted(self):
        for text in (
            SIZE.replace("11,726", "11,725"),
            PAIR.replace("11,632", "11,630"),
        ):
            self.assertTrue(any(r["status"] == "contradicted" for r in self.rows(text)))

    def test_signed_delta_and_ordered_pair_controls(self):
        for arrow in ("→", "->"):
            for tail in ("", " (net −94)", " (net -94)", " net -94"):
                text = f"The result is `mod.rs`: 11,726 {arrow} 11,632 lines{tail}"
                self.assertTrue(
                    all(r["status"] == "verified" for r in self.rows(text)), text
                )
        self.facts["after"][PATH] = 11820
        self.assertTrue(
            all(
                r["status"] == "verified"
                for r in self.rows("mod.rs: 11726 → 11820 lines (net +94)")
            )
        )

    def test_ambiguous_changed_basename_never_picks_one(self):
        self.facts["changed"].append("other/mod.rs")
        rows = self.rows(PAIR)
        self.assertTrue(rows)
        self.assertTrue(all(r["status"] == "unverifiable" for r in rows), rows)
        self.assertEqual(self.rows(SIZE)[0]["status"], "verified")

    def test_unparsed_comparison_vocabulary_still_fails_closed(self):
        for text in (
            "The reduction in mod.rs (11,632 lines)",
            "The difference between old and new mod.rs (11,632 lines)",
            "mod.rs: 11726 → 11632 lines, reduced by roughly 106",
            "mod.rs: 11726 → 11632 lines (net approximately -94)",
        ):
            rows = self.rows(text)
            self.assertTrue(rows)
            self.assertTrue(all(r["status"] == "unverifiable" for r in rows), rows)

    def test_partial_numeric_units_do_not_verify_a_pair_prefix(self):
        for suffix in ("11632.5 lines", "11632 thousand lines", "11632percent"):
            rows = self.rows(f"mod.rs: 11726 → {suffix}")
            self.assertTrue(rows)
            self.assertTrue(all(r["status"] == "unverifiable" for r in rows), rows)

    def test_multiple_paths_keep_their_own_sizes_and_deltas(self):
        rows = self.rows(f"{PATH} (11726 lines) and {NEW} (255 lines)")
        self.assertEqual([r["status"] for r in rows], ["verified", "contradicted"])
        rows = self.rows(
            PAIR.replace("−106", "−94") + " and other/mod.rs: 100 → 100 lines (net +1)"
        )
        self.assertEqual(
            [r["actual"] for r in rows if r["kind"] == "line_delta"], [-94, 0]
        )
        self.assertEqual(rows[-1]["status"], "contradicted")

    def test_pair_without_measurable_before_cannot_prove_delta(self):
        self.facts["before"].pop(PATH)
        rows = self.rows(PAIR)
        self.assertEqual(
            next(r for r in rows if r["kind"] == "line_delta")["status"], "unverifiable"
        )

    def test_production_grade_uses_changed_paths_and_preserves_fail_and_ungraded(self):
        for text, status in (
            ("lib.rs: 10 → 1 lines (net −8)", "FAIL"),
            (
                "New crate/src/extracted.rs (1 lines) with 10 pure fs-free tests",
                "UNGRADED",
            ),
            ("The original crate/src/lib.rs (10 lines)", "PASS"),
        ):
            with self.subTest(text=text):
                case = fixtures.Grade()
                case.setUp()
                try:
                    for tree in case.fake.trees.values():
                        tree["other/lib.rs"] = "unchanged\n"
                    session_db(case.args.session_db, text, seq=2)
                    with patch.object(
                        fixtures.grader, "extraction", return_value=["extracted.rs"]
                    ):
                        report = case.grade()
                    self.assertEqual(
                        report["criteria"]["claims"]["status"], status, report
                    )
                    self.assertEqual(report["pass"], status == "PASS")
                finally:
                    case.doCleanups()
