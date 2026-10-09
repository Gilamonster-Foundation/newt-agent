"""Build SQLite fixtures from the production conversation/turn table DDL.

These fixtures exercise role separation and ordering, not cryptographic chain
verification; hash columns are placeholders, never manufactured identities.
"""

from pathlib import Path
import sqlite3


def session_db(
    path,
    assistant,
    *,
    conversation="run",
    seq=1,
    writer="writer",
    events="[]",
    user="refactor",
):
    schema = (
        Path(__file__).resolve().parents[3] / "newt-core/src/store/schema.rs"
    ).read_text()
    ddl = (
        "CREATE TABLE IF NOT EXISTS conversations"
        + schema.split('"CREATE TABLE IF NOT EXISTS conversations', 1)[1].split(
            "-- Immutable prompt receipts", 1
        )[0]
    )
    with sqlite3.connect(path) as connection:
        connection.executescript(ddl)
        connection.execute(
            "INSERT OR IGNORE INTO conversations "
            "(id,title,workspace_path,workspace_key,writer_fingerprint,activity_tick,tip_hash,started_at_claim,updated_at_claim) "
            "VALUES (?, 'fixture', '.', 'fixture', ?, 1, 'fixture-unverified', 0, 0)",
            (conversation, writer),
        )
        connection.execute(
            "INSERT INTO turns (conversation_id,writer_fingerprint,seq,prev_hash,user,assistant,events,ts_claim) "
            "VALUES (?,?,?,'fixture-unverified',?,?,?,0)",
            (conversation, writer, seq, user, assistant, events),
        )
    return path
