//! CSV → SQLite-table ingest (Phase 21).
//!
//! Free functions over a `&rusqlite::Connection`, called by
//! [`SqliteBackend::ingest_csv`](crate::SqliteBackend). The contract is
//! [`docs/design/centaur-data-scientist.md`](../../../docs/design/centaur-data-scientist.md):
//! small datasets (≤ a few MB) read fully into memory, per-column dtype
//! inferred from every non-empty cell, and **every identifier quoted** before
//! it reaches SQL.
//!
//! # Identifier safety (review focus, Phase 21)
//!
//! There is exactly one path from a string to a SQL identifier:
//! [`quote_ident`]. It wraps the identifier in double-quotes and doubles any
//! embedded double-quote — the SQLite-documented way to make *any* string a
//! literal identifier, so a CSV header like `weird "name` round-trips as that
//! literal column name instead of breaking out into SQL. Table names get a
//! second, stricter gate ([`validate_table_name`]): they are rejected
//! ([`DataError::InvalidIdentifier`]) if empty or carrying a NUL / double-quote
//! — defense in depth, because a table name is also echoed into the
//! `__datasets` metadata and the `DROP TABLE` statement. Header column names
//! are quoted but **never** rejected, so punctuation and spaces survive as
//! literal names.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

use crate::error::{DataError, Result};
use crate::store::{ColumnDtype, ColumnInfo, IngestReport};

/// Quote `ident` as a SQLite identifier: wrap in double-quotes and double any
/// embedded double-quote (`a"b` → `"a""b"`).
///
/// THE single choke point for identifier interpolation (Phase 21 — identifier
/// safety). Used for every table and column name everywhere in the crate; no
/// header or table string is ever interpolated into SQL unquoted.
pub fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Reject a table name that is not a safe identifier even after quoting:
/// empty, or containing a NUL byte or a double-quote (Phase 21 — defense in
/// depth). Column names are deliberately *not* run through this — they are
/// quoted and kept verbatim.
fn validate_table_name(table: &str) -> Result<()> {
    if table.is_empty() || table.contains('"') || table.contains('\0') {
        return Err(DataError::InvalidIdentifier(table.to_string()));
    }
    Ok(())
}

/// One column's inference state while scanning the CSV (Phase 21).
///
/// Inference is monotone widening: a column starts as a candidate
/// [`ColumnDtype::Integer`], widens to [`ColumnDtype::Real`] on the first
/// non-empty cell that is not an `i64` but is an `f64`, and widens to
/// [`ColumnDtype::Text`] on the first cell that is neither. Empty cells are
/// `NULL` and never force `Text`.
#[derive(Debug)]
struct ColumnInference {
    all_int: bool,
    all_real: bool,
    null_count: u64,
}

impl ColumnInference {
    fn new() -> Self {
        Self {
            all_int: true,
            all_real: true,
            null_count: 0,
        }
    }

    /// Fold one cell into the inference state.
    fn observe(&mut self, cell: &str) {
        if cell.is_empty() {
            self.null_count += 1;
            return;
        }
        if self.all_int && cell.parse::<i64>().is_err() {
            self.all_int = false;
        }
        if self.all_real && cell.parse::<f64>().is_err() {
            self.all_real = false;
        }
    }

    /// The final dtype: integer if every non-empty cell parsed as `i64`, else
    /// real if every non-empty cell parsed as `f64`, else text.
    fn dtype(&self) -> ColumnDtype {
        if self.all_int {
            ColumnDtype::Integer
        } else if self.all_real {
            ColumnDtype::Real
        } else {
            ColumnDtype::Text
        }
    }
}

/// Load `path` into `table` on `conn`, dropping and recreating the table, and
/// upsert the `__datasets` metadata row. See the module docs and
/// [`docs/design/centaur-data-scientist.md`](../../../docs/design/centaur-data-scientist.md).
pub fn ingest_csv(conn: &Connection, path: &Path, table: &str) -> Result<IngestReport> {
    validate_table_name(table)?;

    let mut reader = csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(false)
        .from_path(path)?;

    let headers: Vec<String> = reader.headers()?.iter().map(|h| h.to_string()).collect();
    if headers.is_empty() {
        return Err(DataError::EmptyCsv);
    }

    // Datasets are small (design doc: ≤ a few MB) — read fully into memory so
    // dtype inference can see every cell before a single row is inserted.
    let records: Vec<csv::StringRecord> = reader.records().collect::<csv::Result<Vec<_>>>()?;

    let mut inference: Vec<ColumnInference> =
        (0..headers.len()).map(|_| ColumnInference::new()).collect();
    for record in &records {
        for (col, inf) in inference.iter_mut().enumerate() {
            // A short/ragged record is impossible under `flexible(false)`, but
            // an out-of-range index would still be UB-free here: treat a
            // missing cell as empty rather than panic.
            inf.observe(record.get(col).unwrap_or(""));
        }
    }

    let dtypes: Vec<ColumnDtype> = inference.iter().map(ColumnInference::dtype).collect();

    let quoted_table = quote_ident(table);
    let column_defs: Vec<String> = headers
        .iter()
        .zip(&dtypes)
        .map(|(name, dtype)| format!("{} {}", quote_ident(name), dtype.sqlite_decl_type()))
        .collect();

    conn.execute(&format!("DROP TABLE IF EXISTS {quoted_table}"), [])?;
    conn.execute(
        &format!("CREATE TABLE {quoted_table} ({})", column_defs.join(", ")),
        [],
    )?;

    // One transaction, one prepared statement: bulk insert.
    let placeholders = (1..=headers.len())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let quoted_cols = headers
        .iter()
        .map(|h| quote_ident(h))
        .collect::<Vec<_>>()
        .join(", ");
    let insert_sql = format!("INSERT INTO {quoted_table} ({quoted_cols}) VALUES ({placeholders})");

    let tx = conn.unchecked_transaction()?;
    {
        let mut stmt = tx.prepare(&insert_sql)?;
        for record in &records {
            let mut params: Vec<rusqlite::types::Value> = Vec::with_capacity(headers.len());
            for (col, dtype) in dtypes.iter().enumerate() {
                let cell = record.get(col).unwrap_or("");
                params.push(cell_to_value(cell, *dtype));
            }
            let param_refs: Vec<&dyn rusqlite::ToSql> =
                params.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
            stmt.execute(rusqlite::params_from_iter(param_refs))?;
        }
    }

    let row_count = records.len() as u64;
    let source = path.display().to_string();
    let ingested_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    tx.execute(
        "INSERT INTO __datasets (table_name, source, row_count, ingested_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(table_name) DO UPDATE SET
             source = excluded.source,
             row_count = excluded.row_count,
             ingested_at = excluded.ingested_at",
        rusqlite::params![table, source, row_count as i64, ingested_at],
    )?;
    tx.commit()?;

    let columns = headers
        .iter()
        .zip(&dtypes)
        .zip(&inference)
        .map(|((name, dtype), inf)| ColumnInfo {
            name: name.clone(),
            dtype: *dtype,
            null_count: inf.null_count,
        })
        .collect();

    Ok(IngestReport {
        table: table.to_string(),
        row_count,
        columns,
        source: Some(source),
    })
}

/// Bind one CSV cell as the SQLite value its inferred dtype calls for.
///
/// Empty → `NULL`. Otherwise typed per `dtype`. Inference guarantees a
/// non-empty cell parses for its dtype, but a parse failure here cannot panic:
/// it falls back to `NULL` with a `tracing::warn!` (Phase 21 — never insert
/// garbage, never abort the whole ingest on one surprising cell).
fn cell_to_value(cell: &str, dtype: ColumnDtype) -> rusqlite::types::Value {
    use rusqlite::types::Value;
    if cell.is_empty() {
        return Value::Null;
    }
    match dtype {
        ColumnDtype::Integer => match cell.parse::<i64>() {
            Ok(v) => Value::Integer(v),
            Err(e) => {
                tracing::warn!(cell, error = %e, "integer cell failed to parse; storing NULL");
                Value::Null
            }
        },
        ColumnDtype::Real => match cell.parse::<f64>() {
            Ok(v) => Value::Real(v),
            Err(e) => {
                tracing::warn!(cell, error = %e, "real cell failed to parse; storing NULL");
                Value::Null
            }
        },
        ColumnDtype::Text => Value::Text(cell.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_ident_doubles_embedded_quotes() {
        assert_eq!(quote_ident("plain"), "\"plain\"");
        assert_eq!(quote_ident("with space"), "\"with space\"");
        assert_eq!(quote_ident("weird \"name"), "\"weird \"\"name\"");
        // A classic injection attempt becomes a single literal identifier.
        assert_eq!(
            quote_ident("x\"); DROP TABLE t;--"),
            "\"x\"\"); DROP TABLE t;--\""
        );
    }

    #[test]
    fn validate_table_name_rejects_unsafe() {
        assert!(validate_table_name("ok_table").is_ok());
        assert!(matches!(
            validate_table_name(""),
            Err(DataError::InvalidIdentifier(_))
        ));
        assert!(matches!(
            validate_table_name("a\"b"),
            Err(DataError::InvalidIdentifier(_))
        ));
        assert!(matches!(
            validate_table_name("a\0b"),
            Err(DataError::InvalidIdentifier(_))
        ));
    }

    #[test]
    fn inference_widens_int_to_real_to_text() {
        let mut inf = ColumnInference::new();
        inf.observe("1");
        assert_eq!(inf.dtype(), ColumnDtype::Integer);
        inf.observe("2.5");
        assert_eq!(inf.dtype(), ColumnDtype::Real);
        inf.observe("hello");
        assert_eq!(inf.dtype(), ColumnDtype::Text);
    }

    #[test]
    fn empty_cells_are_nulls_not_text() {
        let mut inf = ColumnInference::new();
        inf.observe("1");
        inf.observe("");
        inf.observe("2");
        assert_eq!(inf.dtype(), ColumnDtype::Integer);
        assert_eq!(inf.null_count, 1);
    }

    #[test]
    fn cell_to_value_maps_types_and_nulls() {
        use rusqlite::types::Value;
        assert!(matches!(
            cell_to_value("", ColumnDtype::Integer),
            Value::Null
        ));
        assert!(matches!(
            cell_to_value("42", ColumnDtype::Integer),
            Value::Integer(42)
        ));
        assert!(matches!(
            cell_to_value("1.5", ColumnDtype::Real),
            Value::Real(_)
        ));
        assert!(matches!(
            cell_to_value("hi", ColumnDtype::Text),
            Value::Text(_)
        ));
    }
}
