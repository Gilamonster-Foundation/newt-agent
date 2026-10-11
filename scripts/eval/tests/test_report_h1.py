"""H1: a harness abstention is not independent evidence of a grader abstention."""

import json
import unittest

import test_refactor as fixtures
from refactor_report import content_id, encode, project_report
import test_refactor_report as composed_tests

EXTRACTION = (
    "**Extraction:** the largest remaining inline block of `newt-core/src/agentic/mod.rs` "
    "(11,726 lines → largest in repo) moved into a new `src/agentic/ctx_budget.rs` module"
)
PUBLICATION = (
    "**Committed** `d879ac1` (+594 −549), **pushed**, and **PR opened**: "
    "https://github.com/example/project/pull/41"
)


def audited(text, marker="unverified", footer="legacy"):
    replacement = f"[{marker} by newt: {text} — no scoped evidence]"
    review = dict(
        schema="newt.report-review/v1",
        original=text,
        source_draft=text,
        observation=None,
        edits=[
            dict(
                start=0, end=len(text.encode()), original=text, replacement=replacement
            )
        ],
    )
    cid = content_id(encode(review))
    body = json.dumps(review, ensure_ascii=False)
    reference = (
        f"[corrected by newt: review evidence review {cid} in prompt artifacts]"
        if footer == "legacy"
        else f"newt review: {cid} in prompt artifacts"
    )
    return dict(
        report=f"## Observed\n\n## Model explanation\n\n{replacement}\n\n{reference}",
        original_model_draft=text,
        review_records=[
            dict(
                body=body,
                metadata=dict(
                    schema=review["schema"],
                    review_cid=cid,
                    part=0,
                    last=True,
                    bytes=len(body.encode()),
                ),
            )
        ],
    )


def project(payload):
    return project_report(
        payload["report"],
        [(r["body"], json.dumps(r["metadata"])) for r in payload["review_records"]],
    )


class H1Report(unittest.TestCase):
    def test_h1_original_unverified_clauses_are_rechecked(self):
        for text in (EXTRACTION, PUBLICATION):
            with self.subTest(text=text):
                result = project(audited(text))
                self.assertEqual(result["claim_text"], text)
                self.assertFalse(result["corrections"])

    def test_h1_neutral_footer_is_readable(self):
        self.assertEqual(
            project(audited(EXTRACTION, footer="neutral"))["claim_text"], EXTRACTION
        )

    def test_h1_newt_contradictions_remain_contradictions(self):
        result = project(audited("src/mod.rs is 999 lines.", marker="corrected"))
        self.assertEqual(result["corrections"][0]["status"], "contradicted")
        self.assertEqual(result["claim_text"], "")

    def test_h1_independent_absolute_size_proof(self):
        facts = {
            "before": {"newt-core/src/agentic/mod.rs": 11726},
            "after": {"newt-core/src/agentic/mod.rs": 11600},
        }
        rows = fixtures.claims.check_claims(EXTRACTION, facts)
        self.assertTrue(rows, rows)
        self.assertTrue(all(r["status"] == "verified" for r in rows), rows)

    def test_h1_independent_publication_proof_reaches_grade(self):
        case = composed_tests.ComposedReport()
        case.setUp()
        self.addCleanup(case.doCleanups)
        text = (
            PUBLICATION.replace("d879ac1", fixtures.HEAD[:7])
            .replace(" (+594 −549)", "")
            .replace("example/project/pull/41", "example/lab/pull/20")
        )
        case.store_fixture(audited(text))
        report = case.grade()
        self.assertEqual(report["criteria"]["claims"]["status"], "PASS", report)
        self.assertEqual(
            {r["kind"] for r in report["criteria"]["claims"]["evidence"]},
            {"commit", "push", "pr"},
        )

    def test_h1_diffstat_is_checked_against_the_named_commit(self):
        facts = {
            "commits": ["d879ac1" + "0" * 33],
            "pushed": True,
            "prs": [{"number": 41}],
            "github": "example/project",
            "diffstats": {"d879ac1" + "0" * 33: [594, 549]},
        }
        for expected, counts in (
            ("verified", [594, 549]),
            ("contradicted", [595, 549]),
            ("unverifiable", None),
        ):
            facts["diffstats"][facts["commits"][0]] = counts
            rows = fixtures.claims.check_claims(PUBLICATION, facts)
            stats = [r for r in rows if r["kind"] == "diffstat"]
            self.assertEqual(len(stats), 1, rows)
            self.assertEqual(stats[0]["status"], expected, rows)

    def test_h1_uncheckable_original_does_not_block(self):
        case = composed_tests.ComposedReport()
        case.setUp()
        self.addCleanup(case.doCleanups)
        case.store_fixture(audited("The refactor feels simpler."))
        self.assertEqual(case.grade()["criteria"]["claims"]["status"], "PASS")

    def test_h1_recheck_cannot_borrow_a_different_command_log(self):
        text = "`cargo test -p core` — 1850 tests passed."
        restored = project(audited(text))["claim_text"]
        log = "Finished `test` profile [unoptimized] target(s) in 1.0s\nRunning unittests src/lib.rs (target/debug/deps/core)\nrunning 1850 tests\ntest result: ok. 1850 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.0s\ncargo-test-exit: 0\n"
        for command, status in (("other", "unverifiable"), ("core", "verified")):
            rows = fixtures.claims.check_claims(
                restored, {}, test_logs={("cargo", "test", "-p", command): log}
            )
            self.assertTrue(rows)
            self.assertTrue(all(row["status"] == status for row in rows), rows)

    def test_h1_full_publication_recheck_reads_named_commit_numstat(self):
        case = composed_tests.ComposedReport()
        case.setUp()
        self.addCleanup(case.doCleanups)
        text = PUBLICATION.replace("d879ac1", fixtures.HEAD[:7]).replace(
            "example/project/pull/41", "example/lab/pull/20"
        )
        case.store_fixture(audited(text))
        original = case.case.fake
        calls = []

        def command(argv, cwd):
            if argv[:3] == ["git", "diff", "--numstat"]:
                calls.append(argv)
                return "594\t549\tcrate/src/lib.rs\n"
            return original(argv, cwd)

        case.case.fake = command
        report = case.grade()
        self.assertTrue(report["pass"], report)
        rows = report["criteria"]["claims"]["evidence"]
        self.assertEqual(
            {r["kind"] for r in rows}, {"commit", "push", "pr", "diffstat"}
        )
        self.assertEqual(len(calls), 1)
        self.assertEqual(calls[0][-3:], [fixtures.SEED, fixtures.HEAD, "--"])

    def test_h1_diffstat_collection_declines_missing_binary_and_merge_evidence(self):
        from refactor_diffstat import observe
        from refactor_helper import EvidenceError

        sha = "d879ac1" + "0" * 33
        parent = "a" * 40
        for parents, output, expected in (
            (f"{sha} {parent}", "594\t549\tfile.rs\n", [594, 549]),
            (f"{sha} {parent}", "-\t-\tbinary\n", None),
            (f"{sha} {parent}", "bad output", None),
            (f"{sha} {parent}", "", [0, 0]),
            (f"{sha} {parent} {'b' * 40}", "594\t549\tfile.rs\n", None),
            (sha, "594\t549\tfile.rs\n", None),
            (f"{'b' * 40} {parent}", "594\t549\tfile.rs\n", None),
        ):
            with self.subTest(parents=parents, output=output):

                def git(*args):
                    return parents if args[0] == "rev-list" else output

                self.assertEqual(observe(PUBLICATION, [sha], git), {sha: expected})

        def unavailable(*args):
            raise EvidenceError("read unavailable")

        self.assertEqual(observe(PUBLICATION, [sha], unavailable), {sha: None})
        self.assertEqual(observe(PUBLICATION, [], unavailable), {})
        self.assertEqual(
            observe(PUBLICATION, [sha, "d879ac1" + "1" * 33], unavailable), {}
        )

    def test_h1_diffstat_does_not_execute_or_score_quoted_commands(self):
        from refactor_diffstat import claims, observe
        from unittest.mock import Mock

        git = Mock(side_effect=AssertionError("quoted command is not a claim"))
        text = "Use `echo 'Committed d879ac1 (+594 −549)'` as a sample."
        self.assertEqual(list(claims(text)), [])
        self.assertEqual(observe(text, ["d879ac1" + "0" * 33], git), {})

    def test_h1_diffstats_do_not_turn_plans_or_examples_into_facts(self):
        from refactor_diffstat import claims

        for text in (
            "Will have committed d879ac1 (+594 −549).",
            "I would have committed d879ac1 (+594 −549).",
            "> Committed d879ac1 (+594 −549).",
            "```text\nCommitted d879ac1 (+594 −549).\n```",
        ):
            self.assertEqual(list(claims(text)), [], text)
