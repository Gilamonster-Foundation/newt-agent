//! # newt-data — the Phase 21 Centaur data-science engine
//!
//! A **headless** Rust library: the SQLite data engine behind the
//! [Centaur Data Scientist](../../../docs/design/centaur-data-scientist.md)
//! capability. No MCP, no JSON-RPC, no PyO3, no Jupyter kernel — those arrive
//! in later steps (21.2 MCP server, 21.3+ kernel, 21.6 PyO3 submodule). This
//! crate is *only* the engine where ~all the coverage is earned: pure
//! functions over an in-memory or on-disk SQLite database.
//!
//! ## The shape
//!
//! - [`DataStore`] is the trait the whole capability is built on — the
//!   **DuckDB-later seam** (step 21.8 swaps the backend without touching a
//!   signature). It exposes CSV ingest, ad-hoc SQL query, schema/statistics
//!   summary, and table listing.
//! - [`SqliteBackend`] is the first implementation: bundled `rusqlite 0.31`
//!   (MSRV 1.75), WAL-with-DELETE-fallback like the conversation store, and a
//!   separate database at `<workspace>/.newt-data/data.db`.
//!
//! ## Design contracts honored here (Phase 21)
//!
//! - **Identifier safety** — every table/column name reaches SQL only through
//!   [`ingest::quote_ident`]; table names are additionally validated. A CSV
//!   header like `weird "name` round-trips as a literal column name; it cannot
//!   inject SQL.
//! - **Honest query results** — [`QueryResult::truncated`] is set by reading
//!   one row past the cap, never by fabricating a total.
//! - **pandas-faithful statistics** — [`NumericDescribe`] reproduces
//!   `.describe()` exactly: sample std (`ddof = 1`), linear-interpolation
//!   quartiles.
//!
//! ## Example
//!
//! ```no_run
//! use std::path::Path;
//! use newt_data::{DataStore, SqliteBackend};
//!
//! # fn main() -> anyhow::Result<()> {
//! let store = SqliteBackend::open_in_memory()?;
//! let report = store.ingest_csv(Path::new("sales.csv"), "sales")?;
//! println!("loaded {} rows into {}", report.row_count, report.table);
//!
//! let result = store.query("SELECT * FROM sales", 100)?;
//! if result.truncated {
//!     println!("(showing first {} rows; more exist)", result.returned);
//! }
//!
//! let summary = store.summarize("sales")?;
//! for col in &summary.columns {
//!     println!("{}: {} ({} nulls)", col.name, col.dtype, col.null_count);
//! }
//! # Ok(())
//! # }
//! ```

pub mod error;
pub mod ingest;
pub mod store;
pub mod summary;

pub use error::{DataError, Result};
pub use store::sqlite::SqliteBackend;
pub use store::{
    ColumnDtype, ColumnInfo, ColumnSummary, DataStore, IngestReport, NumericDescribe, QueryResult,
    TableInfo, TableSummary,
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    const EPS: f64 = 1e-9;

    /// Fixture: id (Integer), label (Text), score (Real). The score column
    /// holds 1.0..=4.0 over four rows plus one fifth row with an EMPTY score
    /// (the null test). The non-null scores [1,2,3,4] give the checkable
    /// pandas describe in [`super::summary`].
    const FIXTURE_CSV: &str = "id,label,score\n\
        1,alpha,1.0\n\
        2,bravo,2.0\n\
        3,charlie,3.0\n\
        4,delta,4.0\n\
        5,echo,\n";

    /// Write `contents` to a NamedTempFile and return it (kept alive by the
    /// caller so the path stays valid).
    fn csv_file(contents: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    fn ingested() -> (SqliteBackend, NamedTempFile, IngestReport) {
        let store = SqliteBackend::open_in_memory().unwrap();
        let f = csv_file(FIXTURE_CSV);
        let report = store.ingest_csv(f.path(), "metrics").unwrap();
        (store, f, report)
    }

    #[test]
    fn ingest_infers_dtypes_and_counts_nulls() {
        let (_store, _f, report) = ingested();
        assert_eq!(report.table, "metrics");
        assert_eq!(report.row_count, 5);
        assert_eq!(report.columns.len(), 3);

        let id = &report.columns[0];
        assert_eq!(id.name, "id");
        assert_eq!(id.dtype, ColumnDtype::Integer);
        assert_eq!(id.null_count, 0);

        let label = &report.columns[1];
        assert_eq!(label.name, "label");
        assert_eq!(label.dtype, ColumnDtype::Text);
        assert_eq!(label.null_count, 0);

        let score = &report.columns[2];
        assert_eq!(score.name, "score");
        assert_eq!(score.dtype, ColumnDtype::Real);
        // Exactly one empty score cell (row 5).
        assert_eq!(score.null_count, 1);

        assert!(report.source.is_some());
    }

    #[test]
    fn summarize_describe_matches_pandas() {
        let (store, _f, _report) = ingested();
        let summary = store.summarize("metrics").unwrap();
        assert_eq!(summary.table, "metrics");
        assert_eq!(summary.row_count, 5);

        let score = summary.columns.iter().find(|c| c.name == "score").unwrap();
        assert_eq!(score.dtype, ColumnDtype::Real);
        assert_eq!(score.null_count, 1);
        // 4 distinct non-null scores.
        assert_eq!(score.distinct_count, 4);

        let d = score.numeric.expect("score is numeric");
        // pandas .describe() of [1,2,3,4].
        assert_eq!(d.count, 4);
        assert!((d.mean - 2.5).abs() < EPS);
        assert!((d.std - 1.290_994_448_735_805_6).abs() < EPS);
        assert!((d.min - 1.0).abs() < EPS);
        assert!((d.q25 - 1.75).abs() < EPS);
        assert!((d.q50 - 2.5).abs() < EPS);
        assert!((d.q75 - 3.25).abs() < EPS);
        assert!((d.max - 4.0).abs() < EPS);

        // The id column is also numeric; the label column is not.
        let id = summary.columns.iter().find(|c| c.name == "id").unwrap();
        assert!(id.numeric.is_some());
        let label = summary.columns.iter().find(|c| c.name == "label").unwrap();
        assert!(label.numeric.is_none());
        assert_eq!(label.distinct_count, 5);
    }

    #[test]
    fn summarize_missing_table_errors() {
        let store = SqliteBackend::open_in_memory().unwrap();
        assert!(matches!(
            store.summarize("nope"),
            Err(DataError::NoSuchTable(_))
        ));
    }

    #[test]
    fn query_truncation_is_honest() {
        let (store, _f, _report) = ingested();
        // 5 rows; cap of 2 → returns 2, truncated true.
        let capped = store.query("SELECT * FROM metrics", 2).unwrap();
        assert_eq!(capped.returned, 2);
        assert_eq!(capped.rows.len(), 2);
        assert!(capped.truncated);
        assert_eq!(capped.columns, vec!["id", "label", "score"]);

        // A cap larger than the table → no truncation.
        let full = store.query("SELECT * FROM metrics", 100).unwrap();
        assert_eq!(full.returned, 5);
        assert!(!full.truncated);

        // cap 0 → 0 rows, but truncated true (rows exist).
        let zero = store.query("SELECT * FROM metrics", 0).unwrap();
        assert_eq!(zero.returned, 0);
        assert!(zero.truncated);

        // cap 0 over an empty result → not truncated.
        let empty = store
            .query("SELECT * FROM metrics WHERE id < 0", 0)
            .unwrap();
        assert_eq!(empty.returned, 0);
        assert!(!empty.truncated);
    }

    #[test]
    fn query_maps_cell_types_to_json() {
        let (store, _f, _report) = ingested();
        let r = store
            .query("SELECT id, label, score FROM metrics ORDER BY id", 10)
            .unwrap();
        // Row 1: integer, text, real.
        assert_eq!(r.rows[0][0], serde_json::json!(1));
        assert_eq!(r.rows[0][1], serde_json::json!("alpha"));
        assert_eq!(r.rows[0][2], serde_json::json!(1.0));
        // Row 5 has a NULL score.
        assert_eq!(r.rows[4][2], serde_json::Value::Null);
    }

    /// End-to-end regression for the lossy non-finite boundary (Phase 21 —
    /// see [`store::sqlite`]'s `value_ref_to_json` contract). A CSV cell that is
    /// literally `inf` / `-inf` / `nan` parses as `f64`, so the column infers
    /// [`ColumnDtype::Real`] and the value is stored as `REAL`. JSON has no
    /// representation for a non-finite float, so on query each such cell comes
    /// back as `Null` — the original token is *not* preserved. This pins the
    /// behavior so a future change (e.g. emitting the strings `"Infinity"` /
    /// `"NaN"`) is a conscious, test-updating decision rather than a silent one.
    #[test]
    fn query_non_finite_real_cell_is_null() {
        // Mixed finite + non-finite so the column still infers Real (every
        // non-empty cell parses as f64). `1e9999` overflows f64 to +inf.
        let csv = "v\n\
            1.0\n\
            inf\n\
            -inf\n\
            nan\n\
            1e9999\n";
        let store = SqliteBackend::open_in_memory().unwrap();
        let f = csv_file(csv);
        let report = store.ingest_csv(f.path(), "edge").unwrap();
        assert_eq!(report.row_count, 5);
        // The whole column is inferred Real, not Text — these tokens parse.
        assert_eq!(report.columns[0].dtype, ColumnDtype::Real);
        assert_eq!(report.columns[0].null_count, 0);

        let r = store
            .query("SELECT v FROM edge ORDER BY rowid", 10)
            .unwrap();
        // The one finite value survives as a JSON number.
        assert_eq!(r.rows[0][0], serde_json::json!(1.0));
        // inf, -inf, nan, and the overflowed 1e9999 all degrade to JSON null.
        for row in &r.rows[1..] {
            assert_eq!(
                row[0],
                serde_json::Value::Null,
                "non-finite real cell should query back as JSON null"
            );
        }
    }

    #[test]
    fn query_non_row_statement_returns_empty() {
        let (store, _f, _report) = ingested();
        let r = store.query("DELETE FROM metrics WHERE id = 5", 10).unwrap();
        assert_eq!(r.returned, 0);
        assert!(!r.truncated);
        assert!(r.rows.is_empty());
        // The delete really happened.
        let after = store.query("SELECT * FROM metrics", 100).unwrap();
        assert_eq!(after.returned, 4);
    }

    #[test]
    fn identifier_safety_round_trips_weird_headers() {
        // A header with an embedded double-quote and a space-containing column.
        let csv = "id,weird \"name,has space\n\
            1,x,y\n\
            2,z,w\n";
        let store = SqliteBackend::open_in_memory().unwrap();
        let f = csv_file(csv);
        let report = store.ingest_csv(f.path(), "quirky").unwrap();
        assert_eq!(report.row_count, 2);

        let names: Vec<&str> = report.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["id", "weird \"name", "has space"]);

        // Those literal column names are queryable through the same quoting.
        let r = store
            .query(
                "SELECT \"weird \"\"name\", \"has space\" FROM quirky ORDER BY id",
                10,
            )
            .unwrap();
        assert_eq!(r.returned, 2);
        assert_eq!(r.rows[0][0], serde_json::json!("x"));
        assert_eq!(r.rows[0][1], serde_json::json!("y"));

        // summarize survives the weird names too.
        let summary = store.summarize("quirky").unwrap();
        let weird = summary
            .columns
            .iter()
            .find(|c| c.name == "weird \"name")
            .unwrap();
        assert_eq!(weird.dtype, ColumnDtype::Text);
    }

    #[test]
    fn ingest_rejects_table_name_with_double_quote() {
        let store = SqliteBackend::open_in_memory().unwrap();
        let f = csv_file(FIXTURE_CSV);
        let err = store.ingest_csv(f.path(), "bad\"name").unwrap_err();
        assert!(matches!(err, DataError::InvalidIdentifier(_)));
    }

    #[test]
    fn ingest_empty_csv_errors() {
        let store = SqliteBackend::open_in_memory().unwrap();
        // No header row at all.
        let f = csv_file("");
        let err = store.ingest_csv(f.path(), "empty").unwrap_err();
        assert!(matches!(err, DataError::EmptyCsv));
    }

    #[test]
    fn list_tables_reports_rows_and_source() {
        let (store, f, _report) = ingested();
        let tables = store.list_tables().unwrap();
        assert_eq!(tables.len(), 1);
        let t = &tables[0];
        assert_eq!(t.table, "metrics");
        assert_eq!(t.row_count, 5);
        // The recorded source is the fixture's path.
        assert_eq!(
            t.source.as_deref(),
            Some(f.path().display().to_string().as_str())
        );
    }

    #[test]
    fn list_tables_excludes_metadata_table() {
        let store = SqliteBackend::open_in_memory().unwrap();
        // __datasets must never show up as a user table.
        assert!(store.list_tables().unwrap().is_empty());
    }

    #[test]
    fn reingest_replaces_table() {
        let store = SqliteBackend::open_in_memory().unwrap();
        let f1 = csv_file(FIXTURE_CSV);
        store.ingest_csv(f1.path(), "metrics").unwrap();

        // Re-ingest a smaller CSV into the same table name.
        let f2 = csv_file("id\n1\n2\n");
        let report = store.ingest_csv(f2.path(), "metrics").unwrap();
        assert_eq!(report.row_count, 2);
        assert_eq!(report.columns.len(), 1);

        let tables = store.list_tables().unwrap();
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].row_count, 2);
    }

    #[test]
    fn backend_name_is_sqlite() {
        let store = SqliteBackend::open_in_memory().unwrap();
        assert_eq!(store.backend_name(), "sqlite");
    }

    #[test]
    fn datastore_is_object_safe() {
        let backend = SqliteBackend::open_in_memory().unwrap();
        let _: &dyn DataStore = &backend;
    }

    #[test]
    fn open_on_disk_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = SqliteBackend::default_db_path(dir.path());
        let store = SqliteBackend::open(&path).unwrap();
        let f = csv_file(FIXTURE_CSV);
        store.ingest_csv(f.path(), "metrics").unwrap();
        drop(store);

        // Reopen the same file: the data persisted.
        let reopened = SqliteBackend::open(&path).unwrap();
        let tables = reopened.list_tables().unwrap();
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].row_count, 5);
    }

    #[test]
    fn types_serialize_round_trip() {
        // The public types are serde-stable (the MCP adapter relies on this).
        let (store, _f, report) = ingested();
        let json = serde_json::to_string(&report).unwrap();
        let back: IngestReport = serde_json::from_str(&json).unwrap();
        assert_eq!(report, back);

        let summary = store.summarize("metrics").unwrap();
        let json = serde_json::to_string(&summary).unwrap();
        let back: TableSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(summary, back);
    }
}
