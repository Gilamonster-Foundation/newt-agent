"""Build SQLite fixtures from the production conversation/turn table DDL.

These fixtures exercise role separation and ordering, not cryptographic chain
verification; hash columns are placeholders, never manufactured identities.
"""

from pathlib import Path
import sqlite3
import json


def session_db(
    path,
    assistant,
    *,
    conversation="run",
    seq=1,
    writer="writer",
    events="[]",
    user="refactor",
    report=None,
    review_records=(),
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
        for table in ("prompt_receipts", "prompt_artifacts"):
            statement = "CREATE TABLE IF NOT EXISTS " + table
            statement += schema.split(statement, 1)[1].split(");", 1)[0] + ");"
            connection.executescript(statement)
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
        prompt = f"prompt:{conversation}:{writer}:{seq}"
        connection.execute(
            "INSERT INTO prompt_receipts "
            "(id,conversation_id,writer_fingerprint,seq,root_prompt_id,origin,raw_text,model_text,raw_digest,model_digest,receipt_hash,ts_claim) "
            "VALUES (?,?,?,?,?,'operator',?,?,'fixture','fixture','fixture',0)",
            (prompt, conversation, writer, seq, prompt, user.encode(), user.encode()),
        )
        report = composed_report(assistant) if report is None else report
        records = list(review_records) + [
            {
                "body": report,
                "metadata": {
                    "reply_digest": "fixture-unverified",
                    "reply_bytes": len(report.encode()),
                },
            }
        ]
        last = connection.execute(
            "SELECT COALESCE(MAX(seq),0) FROM prompt_artifacts WHERE conversation_id=?",
            (conversation,),
        ).fetchone()[0]
        for index, record in enumerate(records, start=last + 1):
            connection.execute(
                "INSERT INTO prompt_artifacts "
                "(id,conversation_id,writer_fingerprint,seq,prev_hash,prompt_id,root_prompt_id,kind,relation,body,metadata,ts_claim,artifact_hash) "
                "VALUES (?,?,?,?,'fixture',?,?,'turn_outcome','derived_from',?,?,0,'fixture')",
                (
                    f"artifact:{conversation}:{index}",
                    conversation,
                    writer,
                    index,
                    prompt,
                    prompt,
                    record["body"],
                    json.dumps(record["metadata"]),
                ),
            )
    return path


def composed_report(prose):
    return (
        "## Observed\n\nFixture observations.\n\n## Model explanation\n\n" + prose
        if prose
        else ""
    )
