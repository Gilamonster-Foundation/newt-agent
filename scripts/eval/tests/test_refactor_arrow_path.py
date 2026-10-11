"""Relocation arrows must not turn attached absolute sizes into numeric pairs."""

import unittest

import test_refactor as fixtures

PATH = "newt-core/src/agentic/openai_wire.rs"
CLAIM = (
    "- **Extraction:** `openai_chat_wire_messages` (pure Chat Completions "
    "message-wiring) → new `newt-core/src/agentic/openai_wire.rs` "
    "(124 lines) with a mocked test suite"
)


class ArrowPath(unittest.TestCase):
    facts = {"before": {}, "after": {PATH: 124}, "changed": [PATH]}

    def statuses(self, text, facts=None):
        rows = fixtures.claims.check_claims(
            text, self.facts if facts is None else facts
        )
        self.assertTrue(rows)
        return [row["status"] for row in rows]

    def test_exact_counted_claim_and_wrong_size(self):
        """The counted extraction was incorrectly rejected as a comparison."""
        self.assertEqual(self.statuses(CLAIM), ["verified"])
        self.assertEqual(self.statuses(CLAIM.replace("124", "125")), ["contradicted"])

    def test_relocation_spelling_and_unqualified_before_size(self):
        for arrow in ("→", "->"):
            for prefix in ("", "new "):
                for path in (PATH, f"`{PATH}`"):
                    with self.subTest(arrow=arrow, prefix=prefix, path=path):
                        text = f"helper {arrow} {prefix}{path} (124 lines)"
                        self.assertEqual(self.statuses(text), ["verified"])
                        self.assertEqual(
                            self.statuses(
                                text, {"before": {PATH: 124}, "after": {PATH: 120}}
                            ),
                            ["verified"],
                        )

    def test_numeric_pairs_still_have_ordered_before_after_counts(self):
        facts = {"before": {PATH: 130}, "after": {PATH: 124}}
        for arrow in ("→", "->"):
            self.assertEqual(
                self.statuses(f"{PATH}: 130 {arrow} 124 lines", facts),
                ["verified", "verified"],
            )
            self.assertEqual(
                self.statuses(f"{PATH}: 124 {arrow} 130 lines", facts),
                ["contradicted", "contradicted"],
            )

    def test_relocation_does_not_hide_numeric_or_comparison_residue(self):
        for suffix in (
            " and 999 lines",
            " with 999",
            " with 3 mocked tests",
            " with fewer dependencies",
            " reduced by 4",
            " and 130 → 124 lines",
        ):
            with self.subTest(suffix=suffix):
                self.assertIn("unverifiable", self.statuses(CLAIM + suffix))
        self.assertEqual(
            self.statuses(f"helper → approximately {PATH} (124 lines)"),
            ["unverifiable"],
        )
