"""Read the final composed operator report from newt's structured outcome store."""

from __future__ import annotations

from contextlib import closing
from pathlib import Path
import sqlite3
import json

from refactor_report import project_report


def final_report(path: Path, conversation_id: str | None = None) -> dict:
    """Read one consistent read-only snapshot, including live committed WAL rows.

    Select the latest prompt by writer/sequence, then its completed outcome.
    Missing/incomplete outcomes never fall back to raw assistant text or an
    older successful prompt. The operator-selected database remains the trust
    boundary; this reader validates the review CID, not the whole store chain.
    """
    try:
        with closing(
            sqlite3.connect(path.resolve().as_uri() + "?mode=ro", uri=True, timeout=0)
        ) as db:
            db.execute("PRAGMA query_only=ON")
            db.execute("BEGIN")
            if conversation_id is None:
                conversations = db.execute(
                    "SELECT id FROM conversations LIMIT 2"
                ).fetchall()
                if len(conversations) != 1:
                    raise ValueError(
                        "session DB must have one conversation; select --conversation-id"
                    )
                conversation_id = conversations[0][0]
            if not db.execute(
                "SELECT 1 FROM conversations WHERE id=?", (conversation_id,)
            ).fetchone():
                raise ValueError("selected conversation does not exist")
            writers = db.execute(
                "SELECT DISTINCT writer_fingerprint FROM prompt_receipts WHERE conversation_id=? LIMIT 2",
                (conversation_id,),
            ).fetchall()
            if len(writers) != 1:
                raise ValueError(
                    "selected conversation has no prompts or ambiguous writer ordering"
                )
            prompt = db.execute(
                "SELECT id FROM prompt_receipts WHERE conversation_id=? AND writer_fingerprint=? ORDER BY seq DESC LIMIT 1",
                (conversation_id, writers[0][0]),
            ).fetchone()[0]
            outcome = db.execute(
                "SELECT seq,body,metadata FROM prompt_artifacts WHERE conversation_id=? AND prompt_id=? "
                "AND kind='turn_outcome' ORDER BY seq DESC LIMIT 1",
                (conversation_id, prompt),
            ).fetchone()
            if (
                outcome is None
                or not isinstance(outcome[1], str)
                or not outcome[1].strip()
                or not isinstance(json.loads(outcome[2]).get("reply_digest"), str)
            ):
                raise ValueError(
                    "final composed turn-outcome report is empty or unavailable"
                )
            records = db.execute(
                "SELECT body,metadata FROM prompt_artifacts WHERE conversation_id=? AND prompt_id=? "
                "AND kind='turn_outcome' AND seq<? ORDER BY seq",
                (conversation_id, prompt, outcome[0]),
            ).fetchall()
            return project_report(outcome[1], records)
    except (sqlite3.Error, KeyError, TypeError, UnicodeError) as exc:
        raise ValueError(f"session DB unavailable or unsupported: {exc}") from exc
