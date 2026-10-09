"""Read assistant-owned text from newt's conversation store, never screen output."""

from contextlib import closing
from pathlib import Path
import sqlite3


def final_assistant(path: Path, conversation_id: str | None = None) -> str:
    """Read a short, consistent read-only snapshot; preserve live WAL visibility.

    Schema: newt-core/src/store/schema.rs conversations/turns. Store ordering is
    writer_fingerprint + seq (turn_chain.rs), never ts_claim. Multiple
    writers have incomparable clocks, so decline rather than invent chronology.
    Tool output belongs to events; user and assistant are separate text columns.
    This reader trusts the operator-selected store; it does not verify its chain.
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
                "SELECT DISTINCT writer_fingerprint FROM turns WHERE conversation_id=? LIMIT 2",
                (conversation_id,),
            ).fetchall()
            if len(writers) != 1:
                raise ValueError(
                    "selected conversation has no turns or ambiguous writer ordering"
                )
            row = db.execute(
                "SELECT assistant FROM turns WHERE conversation_id=? AND writer_fingerprint=? ORDER BY seq DESC LIMIT 1",
                (conversation_id, writers[0][0]),
            ).fetchone()
            if row is None or not isinstance(row[0], str) or not row[0].strip():
                raise ValueError(
                    "final stored assistant message is empty or unavailable"
                )
            return row[0]
    except sqlite3.Error as exc:
        raise ValueError(f"session DB unavailable or unsupported: {exc}") from exc
