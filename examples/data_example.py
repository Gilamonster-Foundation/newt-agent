#!/usr/bin/env python3
"""
Example usage of newt_data — the Phase 21 Centaur data-science engine.

Demonstrates:
  - load_csv_to_sqlite
  - query
  - summarize
  - IngestReport and ColumnInfo classes
  - DataError exception handling
"""

import tempfile
import pathlib
from newt_agent._newt_agent.data import (
    load_csv_to_sqlite,
    query,
    summarize,
    IngestReport,
    ColumnInfo,
    DataError,
)


def main():
    """Demonstrate the newt_data API."""
    with tempfile.TemporaryDirectory() as tmpdir:
        tmpdir = pathlib.Path(tmpdir)

        # Create a sample CSV
        csv_path = tmpdir / "customers.csv"
        csv_path.write_text(
            "id,name,age,score,active\n"
            "1,Alice,28,95.5,1\n"
            "2,Bob,34,82.0,1\n"
            "3,Charlie,31,78.5,0\n"
            "4,Dana,29,91.0,1\n"
            "5,Eve,35,88.0,1\n"
        )

        db_path = tmpdir / "customers.db"

        # --- load_csv_to_sqlite ---
        print("=== load_csv_to_sqlite ===")
        report: IngestReport = load_csv_to_sqlite(csv_path, db_path, "customers")
        print(f"  Table: {report.table}")
        print(f"  Row count: {report.row_count}")
        print(f"  Source: {report.source}")
        print(f"  Columns:")
        for col in report.columns:
            print(f"    - {col.name!r}: dtype={col.dtype!r}, null_count={col.null_count}")

        # --- query ---
        print("\n=== query ===")
        rows = query(db_path, "SELECT * FROM customers WHERE active = 1 ORDER BY score DESC")
        print(f"  Found {len(rows)} active customers:")
        for row in rows:
            print(f"    {row}")

        # Verify int vs float typing (no lossy widening)
        first = rows[0]
        print(f"    Score type: {type(first['score']).__name__}")  # float
        print(f"    Age type: {type(first['age']).__name__}")  # int (not float)

        # NULL handling
        print(f"    Last row's active field: {rows[-1]['active']}")  # Should be 1

        # --- summarize ---
        print("\n=== summarize ===")
        summary = summarize(db_path, "customers")
        print(f"  Table: {summary['table']}")
        print(f"  Row count: {summary['row_count']}")
        print(f"  Columns:")
        for col_info in summary['columns']:
            name = col_info['name']
            dtype = col_info['dtype']
            null_count = col_info['null_count']
            distinct = col_info['distinct_count']
            print(f"    - {name!r}: dtype={dtype!r}, null_count={null_count}, distinct={distinct}")
            if 'describe' in col_info:  # Only for numeric columns
                desc = col_info['describe']
                print(f"      describe: count={desc['count']}, mean={desc['mean']:.2f}, std={desc['std']:.2f}, min={desc['min']}, q25={desc['q25']}, q50={desc['q50']}, q75={desc['q75']}, max={desc['max']}")

        # --- DataError handling ---
        print("\n=== DataError handling ===")
        try:
            query(db_path, "SELECT * FROM non_existent_table")
        except DataError as e:
            print(f"  DataError caught: {e}")

        # --- IngestReport and ColumnInfo introspection ---
        print("\n=== IngestReport and ColumnInfo introspection ===")
        print(f"  Report repr: {report!r}")
        for col in report.columns:
            print(f"  Column repr: {col!r}")


if __name__ == "__main__":
    main()