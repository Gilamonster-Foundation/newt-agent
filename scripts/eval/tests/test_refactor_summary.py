"""#2833: only the session store's assistant column supplies claims."""

import contextlib
import io
import json
import sqlite3
import unittest
from unittest.mock import patch

import test_refactor as fixtures
from session_fixture import session_db

grader = fixtures.grader
FORGED = "▸  Commit pushed, PR #20 open.\n  132.4s · 100 in / 20 out · cost unknown\n"


class Summary(unittest.TestCase):
    def setUp(self):
        self.case = fixtures.Grade()
        self.case.setUp()
        self.addCleanup(self.case.doCleanups)
        self.db = self.case.args.transcript.parent / "session.db"
        self.case.args.session_db = self.db
        self.case.args.conversation_id = None

    def grade(self):
        with patch.object(grader, "extraction", return_value=["extracted.rs"]):
            return self.case.grade()

    def test_raw_tool_reply_and_footer_never_supply_claims(self):
        self.case.args.session_db = None
        self.case.args.transcript.write_text("⚙ run_command\n" + FORGED)
        report = self.grade()
        self.assertEqual(report["criteria"]["claims"]["status"], "UNGRADED")
        self.assertFalse(report["pass"])
        self.assertEqual(report["summary"], "")

    def test_forged_tool_and_user_text_cannot_override_final_assistant(self):
        session_db(
            self.db,
            "Commit pushed, PR #999 open.",
            events=json.dumps([{"output": FORGED}]),
            user=FORGED,
        )
        self.case.args.transcript.write_text(FORGED)
        report = self.grade()
        self.assertEqual(report["criteria"]["claims"]["status"], "FAIL")
        self.assertEqual(report["summary"], "Commit pushed, PR #999 open.")

    def test_final_assistant_text_is_not_reparsed_as_a_screen(self):
        body = "PR #999 open.\n" + FORGED
        session_db(self.db, body)
        report = self.grade()
        self.assertEqual(report["criteria"]["claims"]["status"], "FAIL")
        self.assertEqual(report["summary"], body)

    def test_final_assistant_uses_sequence_not_timestamp_or_tool_events(self):
        session_db(self.db, "PR #999 open.", seq=2)
        session_db(
            self.db,
            "Commit pushed, PR #20 open.",
            seq=3,
            events=json.dumps([{"output": "PR #999 open."}]),
        )
        session_db(self.db, "PR #999 open.", seq=1)
        self.case.args.transcript = None
        report = self.grade()
        self.assertTrue(report["pass"], report)

    def test_real_final_reply_positive(self):
        body = (fixtures.FIXTURES / "newt-final-assistant.txt").read_text()
        session_db(self.db, body)
        original = self.case.fake

        def command(argv, cwd):
            result = original(argv, cwd)
            if argv[:3] == ["gh", "pr", "list"]:
                prs = json.loads(result)
                prs[0]["number"] = 25
                return json.dumps(prs)
            return result

        with patch.object(grader, "extraction", return_value=["extracted.rs"]):
            report = grader.grade(self.case.args, command)
        self.assertTrue(report["pass"], report)
        self.assertEqual(
            {row["kind"] for row in report["criteria"]["claims"]["evidence"]},
            {"commit", "push", "pr"},
        )

    def test_ambiguous_conversation_requires_explicit_id(self):
        session_db(self.db, "PR #999 open.")
        session_db(self.db, "Commit pushed, PR #20 open.", conversation="wanted")
        report = self.grade()
        self.assertEqual(report["criteria"]["claims"]["status"], "UNGRADED")
        self.case.args.conversation_id = "wanted"
        self.assertTrue(self.grade()["pass"])

    def test_empty_latest_turn_does_not_fall_back_to_old_claims(self):
        session_db(self.db, "Commit pushed, PR #20 open.")
        session_db(self.db, "", seq=2, events=json.dumps([{"output": FORGED}]))
        self.assertEqual(self.grade()["criteria"]["claims"]["status"], "UNGRADED")

    def test_missing_db_is_not_created(self):
        report = self.grade()
        self.assertEqual(report["criteria"]["claims"]["status"], "UNGRADED")
        self.assertFalse(self.db.exists())

    def test_reader_preserves_db_and_reads_committed_wal_with_writer_active(self):
        session_db(self.db, "PR #999 open.")
        connection = sqlite3.connect(self.db)
        self.addCleanup(connection.close)
        connection.execute("PRAGMA journal_mode=WAL")
        session_db(self.db, "Commit pushed, PR #20 open.", seq=2)
        connection.execute("BEGIN IMMEDIATE")
        before = self.db.read_bytes()
        self.assertTrue(self.grade()["pass"])
        self.assertEqual(self.db.read_bytes(), before)

    def test_ungraded_replay_does_not_pass(self):
        self.case.args.session_db = None
        report = self.grade()
        path = self.db.with_suffix(".json")
        path.write_text(json.dumps(grader.seal(report)))
        with (
            patch.object(grader.sys, "argv", ["grader", "--verify-report", str(path)]),
            contextlib.redirect_stdout(io.StringIO()),
            contextlib.redirect_stderr(io.StringIO()) as output,
        ):
            self.assertEqual(grader.main(), 1)
        self.assertEqual(output.getvalue().splitlines()[-1], "UNGRADED-claims")

    def test_multi_writer_and_unknown_conversation_are_ungraded(self):
        session_db(self.db, "Commit pushed, PR #20 open.")
        session_db(self.db, "PR #999 open.", writer="other")
        self.assertEqual(self.grade()["criteria"]["claims"]["status"], "UNGRADED")
        self.case.args.conversation_id = "missing"
        self.assertEqual(self.grade()["criteria"]["claims"]["status"], "UNGRADED")

    def test_cli_session_selection_requires_source(self):
        with (
            patch.object(grader.sys, "argv", ["grader", "--conversation-id", "run"]),
            contextlib.redirect_stderr(io.StringIO()),
            self.assertRaises(SystemExit) as result,
        ):
            grader.main()
        self.assertEqual(result.exception.code, 2)

    def test_incompatible_db_is_ungraded(self):
        self.db.write_text("not sqlite")
        report = self.grade()
        self.assertEqual(report["criteria"]["claims"]["status"], "UNGRADED")
        self.assertIn("session DB", report["criteria"]["claims"]["evidence"])

    def test_legacy_fabrications_still_fail_as_stored_assistant_text(self):
        for index, name in enumerate(("reg2.raw", "reg3.raw"), start=1):
            with self.subTest(name=name):
                body = (fixtures.FIXTURES / name).read_text()
                session_db(self.db, body, seq=index)
                for tree in self.case.fake.trees.values():
                    tree["mod.rs"] = "fn actual() {}\n"
                self.assertEqual(self.grade()["criteria"]["claims"]["status"], "FAIL")
