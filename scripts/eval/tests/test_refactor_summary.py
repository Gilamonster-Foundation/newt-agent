"""#2804: recognize renderer-bounded final replies without inventing failures."""

import contextlib
import io
import unittest
from unittest.mock import patch

import test_refactor as fixtures

grader = fixtures.grader
claims = fixtures.claims
FOOTER = "  132.4s · 139,023 in / 2,055 out · cost unknown\n"


class Summary(unittest.TestCase):
    def grade(self, raw, *, actual_mod=False):
        case = fixtures.Grade()
        case.setUp()
        self.addCleanup(case.doCleanups)
        case.args.transcript.write_text(raw)
        if actual_mod:
            for tree in case.fake.trees.values():
                tree["mod.rs"] = "fn actual() {}\n"
        with patch.object(
            grader, "extraction", return_value=["crate/src/extracted.rs"]
        ):
            return case.grade()

    def test_real_final_block_and_publication_claims(self):
        raw = (fixtures.FIXTURES / "newt-final.raw").read_text()
        summary = claims.final_summary(raw)
        self.assertIn("Steps 5 and 6 were both actually finished", summary)
        self.assertNotIn("claim check", summary)
        self.assertNotIn("cost unknown", summary)
        rows = claims.check_claims(
            raw,
            {
                "commits": [fixtures.HEAD],
                "pushed": True,
                "github": "example/refactor-lab",
                "prs": [{"number": 25, "state": "OPEN"}],
                "after": {
                    "newt-core/src/agentic/mod.rs": 11200,
                    "newt-core/src/agentic/workflow.rs": 420,
                },
            },
        )
        self.assertTrue(rows)
        self.assertTrue(all(row["status"] == "verified" for row in rows), rows)
        self.assertTrue(
            {"commit", "push", "pr"}.issubset({row["kind"] for row in rows})
        )

    def test_last_reply_wins_over_old_summary_and_narration(self):
        raw = "Summary\nPR #999 merged.\n" + FOOTER
        raw += (
            "▹  I will open PR #999.\n⚙ run_command\n▸  Commit pushed, PR #20 open.\n"
        )
        raw += "⚠ claim check: PR #999 merged\n" + FOOTER
        report = self.grade(raw)
        self.assertEqual(report["criteria"]["claims"]["status"], "PASS", report)
        self.assertNotIn("999", report["summary"])

    def test_progress_handoff_and_hollow_reply(self):
        for prefix in ("▸  ", "▹  ", ""):
            raw = (
                prefix
                + "Captured working state:\nPlan:\nCommitted and pushed. PR #20 open.\n"
                + FOOTER
            )
            with self.subTest(prefix=prefix):
                report = self.grade(raw)
                self.assertEqual(report["criteria"]["claims"]["status"], "PASS", report)

    def test_missing_or_unverifiable_claims_are_ungraded(self):
        for raw in ("no final report", "Summary\n42 tests passed."):
            with self.subTest(raw=raw):
                report = self.grade(raw)
                self.assertEqual(report["criteria"]["claims"]["status"], "UNGRADED")
                self.assertFalse(report["pass"])
                with contextlib.redirect_stderr(io.StringIO()) as stderr:
                    grader.render(report)
                self.assertEqual(stderr.getvalue().splitlines()[-1], "UNGRADED-claims")

    def test_recognized_contradiction_still_fails(self):
        report = self.grade("▸  Commit pushed, PR #999 open.\n" + FOOTER)
        self.assertEqual(report["criteria"]["claims"]["status"], "FAIL")

    def test_real_fixture_with_fake_git_and_github(self):
        import json

        case = fixtures.Grade()
        case.setUp()
        self.addCleanup(case.doCleanups)
        case.args.github = "example/refactor-lab"
        case.args.transcript.write_text(
            (fixtures.FIXTURES / "newt-final.raw").read_text()
        )
        case.fake.trees[fixtures.SEED][fixtures.TARGET] = "// old\n" * 12000
        case.fake.trees[fixtures.HEAD].update(
            {
                "newt-core/src/agentic/mod.rs": "// retained\n" * 11200,
                "newt-core/src/agentic/workflow.rs": "// extracted\n" * 420,
            }
        )

        def command(argv, cwd):
            result = case.fake(argv, cwd)
            if argv[:3] == ["gh", "repo", "view"]:
                return result.replace("example/lab", "example/refactor-lab")
            if argv[:3] == ["gh", "pr", "list"]:
                prs = json.loads(result)
                prs[0]["number"] = 25
                return json.dumps(prs)
            return result

        with patch.object(
            grader, "extraction", return_value=["crate/src/extracted.rs"]
        ):
            report = grader.grade(case.args, command)
        self.assertTrue(report["pass"], report)

    def test_claimed_open_pr_must_be_open(self):
        rows = claims.check_claims(
            "Summary\nPR #25 open.", {"prs": [{"number": 25, "state": "CLOSED"}]}
        )
        self.assertEqual(rows[0]["status"], "contradicted")

    def test_regression_transcripts_still_fail_on_fabrication(self):
        for name in ("reg2.raw", "reg3.raw"):
            with self.subTest(name=name):
                raw = (fixtures.FIXTURES / name).read_text()
                report = self.grade(raw, actual_mod=True)
                self.assertEqual(report["criteria"]["claims"]["status"], "FAIL", report)

    def test_cursor_redraw_cannot_join_reply_to_spinner(self):
        raw = "spinner 11.7s\x1b[57;1H\x1b[J\x1b[38;5;8m▸  Commit pushed, PR #20 open.\x1b[0m\n"
        report = self.grade(raw + FOOTER)
        self.assertEqual(report["criteria"]["claims"]["status"], "PASS", report)
