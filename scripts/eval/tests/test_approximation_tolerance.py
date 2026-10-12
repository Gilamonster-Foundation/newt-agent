"""Report stage 2 and the grader share an inclusive observed-relative tolerance."""

import json
from pathlib import Path
import unittest

import test_refactor as fixtures


class ApproximationTolerance(unittest.TestCase):
    def test_shared_policy_and_boundaries(self):
        # Pin the operator policy in both runtimes' boundary regressions.
        self.assertEqual(fixtures.claims.APPROXIMATION_DIVISOR, 10)
        # Rust's golden bridge emits the actual production divisor, not a copy.
        bridge = json.loads(
            (
                Path(__file__).parent / "fixtures/refactor/composed-report.json"
            ).read_text()
        )
        self.assertEqual(
            fixtures.claims.APPROXIMATION_DIVISOR, bridge["approximation_divisor"]
        )
        for observed in (0, 100, 11726):
            facts = {
                "before": {"src/mod.rs": observed},
                "after": {"src/mod.rs": observed},
            }
            for claimed in (0, 89, 90, 100, 110, 111, 3800, 4400, 11700):
                rows = fixtures.claims.check_claims(
                    f"src/mod.rs (~{claimed} lines)", facts
                )
                self.assertEqual(len(rows), 1)
                expected = (
                    "verified"
                    if claimed == observed
                    else (
                        "unverifiable"
                        if abs(claimed - observed) * 10 <= abs(observed)
                        else "contradicted"
                    )
                )
                self.assertEqual(rows[0]["status"], expected, (observed, claimed, rows))
