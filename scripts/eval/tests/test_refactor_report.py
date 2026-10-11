"""Stage 2: score the composed operator report, never the original model draft."""

import sqlite3
import unittest
from unittest.mock import patch

import test_refactor as fixtures
from session_fixture import session_db


class ComposedReport(unittest.TestCase):
    def setUp(self):
        self.case = fixtures.Grade()
        self.case.setUp()
        self.addCleanup(self.case.doCleanups)
        self.db = self.case.args.transcript.parent / "composed.db"
        self.case.args.session_db = self.db
        self.case.args.conversation_id = None

    def grade(self):
        with patch.object(fixtures.grader, "extraction", return_value=["extracted.rs"]):
            return self.case.grade()

    def test_missing_composed_artifact_cannot_fall_back_to_raw_assistant(self):
        session_db(self.db, "Commit pushed, PR #20 open.")
        with sqlite3.connect(self.db) as db:
            db.execute("DROP TABLE IF EXISTS prompt_artifacts")
        report = self.grade()
        self.assertEqual(report["criteria"]["claims"]["status"], "UNGRADED")
        self.assertFalse(report["pass"])

    def fixture(self):
        import json

        return json.loads((fixtures.FIXTURES / "composed-report.json").read_text())

    def store_fixture(self, payload, **kwargs):
        session_db(
            self.db,
            "PR #999 open.",
            report=payload["report"],
            review_records=payload["review_records"],
            **kwargs,
        )

    def test_real_rust_composed_report_is_scored_original_is_audit_only(self):
        payload = self.fixture()
        self.store_fixture(payload)
        report = self.grade()
        self.assertTrue(report["pass"], report)
        self.assertEqual(report["summary"], payload["report"])
        self.assertEqual(
            report["original_model_draft"], payload["original_model_draft"]
        )
        rows = report["criteria"]["claims"]["evidence"]
        self.assertEqual(len(rows), 1, rows)
        self.assertEqual(rows[0]["kind"], "harness_correction")
        self.assertEqual(rows[0]["status"], "verified")
        self.assertIn("1850 passed", rows[0]["actual"])
        self.assertNotIn("1617", str(rows))
        self.assertNotIn("3,800", str(rows))
        self.assertNotIn("999", str(rows))
        # PR #2855 R2: independently reviewed claims share one physical line.
        self.assertIn("Finished refactor. [corrected by newt:", report["summary"])
        self.assertIn("; [unverified by newt: PR #999", report["summary"])
        self.assertTrue(report["review_cid"])

    def test_only_unverified_spans_cannot_make_claims_pass(self):
        import json
        from refactor_report import content_id, encode

        payload = self.fixture()
        record = payload["review_records"][0]
        review = json.loads(record["body"])
        before = review["edits"][0]["replacement"]
        after = "[unverified by newt: named invocation lacks complete totals]"
        review["edits"][0]["replacement"] = after
        old_cid = record["metadata"]["review_cid"]
        new_cid = content_id(encode(review))
        record["body"] = json.dumps(review, ensure_ascii=False)
        record["metadata"].update(
            review_cid=new_cid, bytes=len(record["body"].encode())
        )
        payload["report"] = (
            payload["report"].replace(before, after).replace(old_cid, new_cid)
        )
        self.store_fixture(payload)
        report = self.grade()
        self.assertEqual(report["criteria"]["claims"]["status"], "UNGRADED", report)
        self.assertFalse(report["pass"])
        self.assertEqual(
            report["original_model_draft"], payload["original_model_draft"]
        )

    def test_modified_report_or_audit_is_not_verified(self):
        import json

        for index, change in enumerate(("report", "audit"), start=1):
            payload = self.fixture()
            if change == "report":
                payload["report"] = payload["report"].replace(
                    "1850 passed", "9999 passed"
                )
            else:
                record = payload["review_records"][0]
                body = json.loads(record["body"])
                body["source_draft"] = "forged original"
                record["body"] = json.dumps(body)
                record["metadata"]["bytes"] = len(record["body"].encode())
            self.store_fixture(payload, seq=index)
            report = self.grade()
            self.assertEqual(report["criteria"]["claims"]["status"], "UNGRADED", report)
            self.assertFalse(report["pass"])

    def test_missing_review_cannot_borrow_another_conversations_audit(self):
        payload = self.fixture()
        self.store_fixture(payload, conversation="other")
        session_db(self.db, "PR #20 open.", report=payload["report"])
        self.case.args.conversation_id = "run"
        report = self.grade()
        self.assertEqual(report["criteria"]["claims"]["status"], "UNGRADED")
        self.assertIn(
            "review evidence missing", report["criteria"]["claims"]["evidence"]
        )

    def test_latest_incomplete_prompt_does_not_borrow_an_old_report(self):
        self.store_fixture(self.fixture())
        session_db(self.db, "Commit pushed, PR #20 open.", seq=2)
        with sqlite3.connect(self.db) as db:
            db.execute(
                "DELETE FROM prompt_artifacts WHERE prompt_id='prompt:run:writer:2'"
            )
        report = self.grade()
        self.assertEqual(report["criteria"]["claims"]["status"], "UNGRADED")

    def test_latest_review_without_outcome_is_incomplete(self):
        self.store_fixture(self.fixture())
        with sqlite3.connect(self.db) as db:
            db.execute(
                "UPDATE prompt_artifacts SET metadata='{}' WHERE seq=(SELECT MAX(seq) FROM prompt_artifacts)"
            )
        self.assertEqual(self.grade()["criteria"]["claims"]["status"], "UNGRADED")

    def test_chunked_review_requires_all_parts_in_order(self):
        payload = self.fixture()
        record = payload["review_records"][0]
        body, metadata = record["body"], record["metadata"]
        split = len(body) // 2
        payload["review_records"] = [
            {"body": body[:split], "metadata": dict(metadata, part=0, last=False)},
            {"body": body[split:], "metadata": dict(metadata, part=1, last=True)},
        ]
        self.store_fixture(payload)
        self.assertTrue(self.grade()["pass"])
        with sqlite3.connect(self.db) as db:
            db.execute(
                "DELETE FROM prompt_artifacts WHERE json_extract(metadata,'$.part')=0"
            )
        self.assertEqual(self.grade()["criteria"]["claims"]["status"], "UNGRADED")

    def test_truncated_operator_report_is_ungraded(self):
        payload = self.fixture()
        payload["report"] += "\n[Observed report excerpt: artifact body limit reached.]"
        self.store_fixture(payload)
        self.assertEqual(self.grade()["criteria"]["claims"]["status"], "UNGRADED")
