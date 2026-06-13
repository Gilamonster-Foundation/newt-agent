//! The [`DataStore`] trait and its result types (Phase 21).
//!
//! These types are the engine's public contract — the seam the later MCP
//! adapter (step 21.2) serializes over JSON-RPC and the PyO3 submodule (step
//! 21.6) hands to a notebook. Per
//! [`docs/design/centaur-data-scientist.md`](../../../docs/design/centaur-data-scientist.md)
//! the trait is the **DuckDB-later seam**: swapping the SQLite backend for a
//! DuckDB one (step 21.8) must never change a single signature here.
//!
//! Every result type derives `serde` `Serialize` / `Deserialize` so the MCP
//! adapter can return it as a JSON-RPC tool result with no hand mapping. The
//! types are deliberately honest (Phase 21 — "the gates must be honest"):
//! [`QueryResult`] carries a `truncated` flag but **never a fabricated total
//! row count**, and [`NumericDescribe`] follows pandas `.describe()` semantics
//! exactly (sample std, linear-interpolation quartiles) rather than an
//! approximation.

pub mod sqlite;

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::Result;

/// The SQLite storage class a column is materialized as.
///
/// Inference is "smallest type that fits every non-empty cell" (Phase 21,
/// [`crate::ingest`]): all-integer → [`ColumnDtype::Integer`], else all-numeric
/// → [`ColumnDtype::Real`], else [`ColumnDtype::Text`]. The [`Display`] form is
/// the SQLite declared type used in `CREATE TABLE`.
///
/// [`Display`]: std::fmt::Display
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ColumnDtype {
    /// 64-bit signed integer (`INTEGER`).
    Integer,
    /// 64-bit IEEE-754 float (`REAL`).
    Real,
    /// UTF-8 text (`TEXT`).
    Text,
}

impl ColumnDtype {
    /// The SQLite declared type written into `CREATE TABLE` for this dtype.
    ///
    /// Identical to the [`Display`](std::fmt::Display) form; provided as an
    /// explicit, self-documenting name at the SQL call sites.
    pub fn sqlite_decl_type(self) -> &'static str {
        match self {
            Self::Integer => "INTEGER",
            Self::Real => "REAL",
            Self::Text => "TEXT",
        }
    }

    /// Map a SQLite `PRAGMA table_info` declared type back to a [`ColumnDtype`].
    ///
    /// Case-insensitive on the leading token: `INTEGER` → [`ColumnDtype::Integer`],
    /// `REAL` (or any `REA…`/`FLOA…`/`DOUB…` numeric affinity) → [`ColumnDtype::Real`],
    /// everything else → [`ColumnDtype::Text`]. Used by [`DataStore::summarize`]
    /// to recover the dtype of an already-materialized table.
    pub fn from_decl_type(decl: &str) -> Self {
        let upper = decl.trim().to_ascii_uppercase();
        if upper.starts_with("INT") {
            Self::Integer
        } else if upper.starts_with("REA") || upper.starts_with("FLOA") || upper.starts_with("DOUB")
        {
            Self::Real
        } else {
            Self::Text
        }
    }
}

impl std::fmt::Display for ColumnDtype {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.sqlite_decl_type())
    }
}

/// Per-column schema fact reported by [`DataStore::ingest_csv`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnInfo {
    /// The literal column name (taken verbatim from the CSV header — spaces
    /// and punctuation survive because identifiers are quoted, never rejected).
    pub name: String,
    /// The inferred storage dtype.
    pub dtype: ColumnDtype,
    /// How many cells in this column were empty (stored as `NULL`).
    pub null_count: u64,
}

/// What [`DataStore::ingest_csv`] did: the table it (re)created and the schema
/// it inferred.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestReport {
    /// The table name the CSV was loaded into (dropped + recreated).
    pub table: String,
    /// Number of data rows inserted (header excluded).
    pub row_count: u64,
    /// One [`ColumnInfo`] per CSV column, in header order.
    pub columns: Vec<ColumnInfo>,
    /// The source path (`path.display()`), recorded in the `__datasets`
    /// metadata table; `None` only on stores that never recorded one.
    pub source: Option<String>,
}

/// The result of a [`DataStore::query`] — a row set capped at `row_cap`.
///
/// **Honesty (Phase 21):** there is no fabricated grand total. `returned` is
/// exactly how many rows are in `rows`, and `truncated` is `true` **iff** at
/// least one more row existed past the cap (detected by reading one extra row
/// and discarding it). Callers learn "there is more" without the engine ever
/// inventing a number it did not count.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryResult {
    /// Column names, in SELECT order (empty for a non-row statement).
    pub columns: Vec<String>,
    /// The returned rows; each inner vec is one row aligned to `columns`.
    /// Cell values are JSON: `null`, number, string, or a base64 string for
    /// `BLOB` columns.
    pub rows: Vec<Vec<serde_json::Value>>,
    /// `rows.len()` — the count actually returned.
    pub returned: usize,
    /// `true` iff more rows existed beyond `row_cap` (none of which are in
    /// `rows`).
    pub truncated: bool,
}

/// pandas `.describe()` of a numeric column (Phase 21).
///
/// Semantics match pandas / numpy defaults exactly so a notebook user can
/// cross-check: `std` is the **sample** standard deviation (`ddof = 1`, and is
/// `0.0` when `count < 2`), and the quartiles use **linear interpolation** on
/// the sorted values (`pos = (n - 1) * q`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct NumericDescribe {
    /// Number of non-null values.
    pub count: u64,
    /// Arithmetic mean.
    pub mean: f64,
    /// Sample standard deviation (`ddof = 1`; `0.0` when `count < 2`).
    pub std: f64,
    /// Minimum.
    pub min: f64,
    /// 25th percentile (linear interpolation).
    pub q25: f64,
    /// 50th percentile / median (linear interpolation).
    pub q50: f64,
    /// 75th percentile (linear interpolation).
    pub q75: f64,
    /// Maximum.
    pub max: f64,
}

/// Per-column summary returned inside a [`TableSummary`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnSummary {
    /// Column name.
    pub name: String,
    /// The column's dtype (recovered from its SQLite declared type).
    pub dtype: ColumnDtype,
    /// Count of `NULL` cells (`row_count - COUNT(col)`).
    pub null_count: u64,
    /// Count of distinct non-null values (`COUNT(DISTINCT col)`).
    pub distinct_count: u64,
    /// pandas-style describe for numeric columns; `None` for `TEXT`.
    pub numeric: Option<NumericDescribe>,
}

/// A full [`DataStore::summarize`] result: schema + per-column statistics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TableSummary {
    /// The summarized table.
    pub table: String,
    /// Total row count.
    pub row_count: u64,
    /// One [`ColumnSummary`] per column, in schema order.
    pub columns: Vec<ColumnSummary>,
}

/// One entry from [`DataStore::list_tables`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableInfo {
    /// The (user-facing) table name.
    pub table: String,
    /// Current row count.
    pub row_count: u64,
    /// The source path recorded at ingest time, if any.
    pub source: Option<String>,
}

/// The headless data engine (Phase 21).
///
/// The seam every later step builds on: the MCP adapter (21.2) wraps a
/// `dyn DataStore`, and the DuckDB backend (21.8) implements this same trait so
/// no tool signature changes. `Send` is required so the trait object can move
/// across the MCP server's task boundary; methods take `&self` because the
/// SQLite backend shares one connection behind a mutex.
///
/// See [`docs/design/centaur-data-scientist.md`](../../../docs/design/centaur-data-scientist.md).
pub trait DataStore: Send {
    /// Load `path` (a CSV) into `table`, dropping and recreating the table.
    ///
    /// Headers become column names; per-column dtype is inferred from the data
    /// (Phase 21 — [`crate::ingest`]). Empty cells are `NULL`. `table` is
    /// validated as a safe identifier first ([`DataError::InvalidIdentifier`]);
    /// an empty file is [`DataError::EmptyCsv`].
    ///
    /// [`DataError::InvalidIdentifier`]: crate::DataError::InvalidIdentifier
    /// [`DataError::EmptyCsv`]: crate::DataError::EmptyCsv
    fn ingest_csv(&self, path: &Path, table: &str) -> Result<IngestReport>;

    /// Run `sql` and return up to `row_cap` rows with an honest `truncated`
    /// flag (Phase 21 — see [`QueryResult`]). `row_cap == 0` returns no rows
    /// and `truncated == true` iff the query produced any.
    fn query(&self, sql: &str, row_cap: usize) -> Result<QueryResult>;

    /// Compute schema + per-column statistics (pandas-style describe for
    /// numeric columns) for `table` ([`DataError::NoSuchTable`] if absent).
    ///
    /// [`DataError::NoSuchTable`]: crate::DataError::NoSuchTable
    fn summarize(&self, table: &str) -> Result<TableSummary>;

    /// List the user tables (excluding the engine's `__datasets` metadata and
    /// SQLite's internal tables) with their row counts and source paths.
    fn list_tables(&self) -> Result<Vec<TableInfo>>;

    /// A short, stable backend identifier (`"sqlite"` today; `"duckdb"` at
    /// step 21.8) for diagnostics and tool output.
    fn backend_name(&self) -> &'static str;
}
