"""Smoke tests for newt_agent.data — the Phase 21 SQLite EDA submodule.

Exercises the PyO3 bridge end to end: load a CSV into a SQLite DB, query it
back as a list of dicts, and summarize it. Hermetic — every path lives under a
fresh ``tempfile.TemporaryDirectory``.
"""

from __future__ import annotations

import tempfile
from pathlib import Path

import pytest

import newt_agent.data as data

# id (integer) / label (text) / score (real, with one empty cell → NULL).
FIXTURE_CSV = (
    "id,label,score\n"
    "1,alpha,1.0\n"
    "2,bravo,2.0\n"
    "3,charlie,3.0\n"
    "4,delta,4.0\n"
    "5,echo,\n"
)


def _write_fixture(d: str) -> str:
    path = Path(d) / "metrics.csv"
    path.write_text(FIXTURE_CSV)
    return str(path)


def test_load_csv_to_sqlite_reports_schema() -> None:
    with tempfile.TemporaryDirectory() as d:
        csv_path = _write_fixture(d)
        db_path = str(Path(d) / "data.db")

        report = data.load_csv_to_sqlite(csv_path, db_path, "metrics")
        assert report.table == "metrics"
        assert report.row_count == 5
        assert report.source is not None

        cols = {c.name: c for c in report.columns}
        assert set(cols) == {"id", "label", "score"}
        assert cols["id"].dtype == "integer"
        assert cols["id"].null_count == 0
        assert cols["label"].dtype == "text"
        assert cols["score"].dtype == "real"
        # Exactly one empty score cell (row 5).
        assert cols["score"].null_count == 1
        assert Path(db_path).exists()


def test_query_returns_rows_as_dicts_with_honest_types() -> None:
    with tempfile.TemporaryDirectory() as d:
        csv_path = _write_fixture(d)
        db_path = str(Path(d) / "data.db")
        data.load_csv_to_sqlite(csv_path, db_path, "metrics")

        rows = data.query(db_path, "SELECT id, label, score FROM metrics ORDER BY id")
        assert isinstance(rows, list)
        assert len(rows) == 5

        first = rows[0]
        assert isinstance(first, dict)
        assert set(first) == {"id", "label", "score"}
        # int stays int (no lossy widening to float).
        assert first["id"] == 1
        assert isinstance(first["id"], int) and not isinstance(first["id"], bool)
        # real is a float.
        assert isinstance(first["score"], float)
        assert first["score"] == pytest.approx(1.0)
        # text is a str.
        assert first["label"] == "alpha"

        # Row 5 has a NULL score → None.
        assert rows[4]["score"] is None


def test_query_row_cap_limits_rows() -> None:
    with tempfile.TemporaryDirectory() as d:
        csv_path = _write_fixture(d)
        db_path = str(Path(d) / "data.db")
        data.load_csv_to_sqlite(csv_path, db_path, "metrics")

        capped = data.query(db_path, "SELECT * FROM metrics", 2)
        assert len(capped) == 2


def test_summarize_returns_describe_for_numeric_columns() -> None:
    with tempfile.TemporaryDirectory() as d:
        csv_path = _write_fixture(d)
        db_path = str(Path(d) / "data.db")
        data.load_csv_to_sqlite(csv_path, db_path, "metrics")

        summary = data.summarize(db_path, "metrics")
        assert isinstance(summary, dict)
        assert summary["table"] == "metrics"
        assert summary["row_count"] == 5

        cols = {c["name"]: c for c in summary["columns"]}
        assert set(cols) == {"id", "label", "score"}

        score = cols["score"]
        assert score["dtype"] == "real"
        assert score["null_count"] == 1
        assert score["distinct_count"] == 4
        # pandas .describe() of [1, 2, 3, 4].
        describe = score["describe"]
        assert describe["count"] == 4
        assert describe["mean"] == pytest.approx(2.5)
        assert describe["std"] == pytest.approx(1.2909944487358056)
        assert describe["min"] == pytest.approx(1.0)
        assert describe["q25"] == pytest.approx(1.75)
        assert describe["q50"] == pytest.approx(2.5)
        assert describe["q75"] == pytest.approx(3.25)
        assert describe["max"] == pytest.approx(4.0)

        # A TEXT column has no describe key.
        assert "describe" not in cols["label"]


def test_bad_sql_raises_data_error() -> None:
    with tempfile.TemporaryDirectory() as d:
        csv_path = _write_fixture(d)
        db_path = str(Path(d) / "data.db")
        data.load_csv_to_sqlite(csv_path, db_path, "metrics")
        with pytest.raises(Exception):
            data.query(db_path, "SELECT * FROM no_such_table")


def test_summarize_missing_table_raises() -> None:
    with tempfile.TemporaryDirectory() as d:
        db_path = str(Path(d) / "data.db")
        # Opening creates the DB; summarizing a table that was never ingested
        # raises the data error.
        with pytest.raises(Exception):
            data.summarize(db_path, "nope")


def test_data_error_exported() -> None:
    assert hasattr(data, "DataError")


def test_umbrella_exposes_data(newt) -> None:
    assert hasattr(newt, "data")
    assert newt.data is data
