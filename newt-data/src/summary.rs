//! describe / summarize logic (Phase 21).
//!
//! Free functions over a `&rusqlite::Connection`, called by
//! [`SqliteBackend::summarize`](crate::SqliteBackend). The numeric statistics
//! reproduce pandas `.describe()` exactly so a notebook user can cross-check
//! the agent's output against their own dataframe
//! ([`docs/design/centaur-data-scientist.md`](../../../docs/design/centaur-data-scientist.md)):
//! the **sample** standard deviation (`ddof = 1`) and **linear-interpolation**
//! quartiles (the numpy/pandas default).

use rusqlite::Connection;

use crate::error::{DataError, Result};
use crate::ingest::quote_ident;
use crate::store::{ColumnDtype, ColumnSummary, NumericDescribe, TableSummary};

/// Summarize `table`: schema, null/distinct counts, and pandas-style describe
/// for numeric columns. [`DataError::NoSuchTable`] if it does not exist.
pub fn summarize(conn: &Connection, table: &str) -> Result<TableSummary> {
    let quoted = quote_ident(table);

    // PRAGMA table_info yields (name, declared type) per column. An empty
    // result means either no such table, or a table with zero columns — the
    // former is the error we report, distinguished by sqlite_master.
    let columns: Vec<(String, String)> = {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({quoted})"))?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    if columns.is_empty() && !table_exists(conn, table)? {
        return Err(DataError::NoSuchTable(table.to_string()));
    }

    let row_count: u64 = conn.query_row(&format!("SELECT COUNT(*) FROM {quoted}"), [], |row| {
        row.get::<_, i64>(0).map(|n| n.max(0) as u64)
    })?;

    let mut summaries = Vec::with_capacity(columns.len());
    for (name, decl) in &columns {
        let dtype = ColumnDtype::from_decl_type(decl);
        let qcol = quote_ident(name);

        let non_null: i64 =
            conn.query_row(&format!("SELECT COUNT({qcol}) FROM {quoted}"), [], |row| {
                row.get(0)
            })?;
        let distinct_count: i64 = conn.query_row(
            &format!("SELECT COUNT(DISTINCT {qcol}) FROM {quoted}"),
            [],
            |row| row.get(0),
        )?;
        let null_count = row_count.saturating_sub(non_null.max(0) as u64);

        let numeric = match dtype {
            ColumnDtype::Integer | ColumnDtype::Real => {
                let values = collect_numeric(conn, &quoted, &qcol)?;
                Some(describe(&values))
            }
            ColumnDtype::Text => None,
        };

        summaries.push(ColumnSummary {
            name: name.clone(),
            dtype,
            null_count,
            distinct_count: distinct_count.max(0) as u64,
            numeric,
        });
    }

    Ok(TableSummary {
        table: table.to_string(),
        row_count,
        columns: summaries,
    })
}

/// `true` iff `table` is a real table (or view) in `sqlite_master`. Lets
/// [`summarize`] tell "no such table" from "table with zero columns".
fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type IN ('table','view') AND name = ?1",
        [table],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// Pull a numeric column's non-null values, ascending — the sorted input the
/// quartile interpolation needs.
fn collect_numeric(conn: &Connection, quoted_table: &str, quoted_col: &str) -> Result<Vec<f64>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {quoted_col} FROM {quoted_table} WHERE {quoted_col} IS NOT NULL ORDER BY {quoted_col}"
    ))?;
    let rows = stmt.query_map([], |row| row.get::<_, f64>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// pandas `.describe()` over a **sorted, non-null** `f64` slice (Phase 21).
///
/// `mean`; sample `std` (`ddof = 1`, `0.0` when `count < 2`); `min`/`max`;
/// and `q25`/`q50`/`q75` by [`quantile_sorted`] linear interpolation. An empty
/// slice yields all-zero stats with `count = 0` (no `NaN` over the wire).
pub fn describe(sorted: &[f64]) -> NumericDescribe {
    let count = sorted.len() as u64;
    if sorted.is_empty() {
        return NumericDescribe {
            count: 0,
            mean: 0.0,
            std: 0.0,
            min: 0.0,
            q25: 0.0,
            q50: 0.0,
            q75: 0.0,
            max: 0.0,
        };
    }
    let n = sorted.len();
    let sum: f64 = sorted.iter().sum();
    let mean = sum / n as f64;
    // Sample variance: divide by (n - 1). Undefined for n == 1 → std 0.0.
    let std = if n < 2 {
        0.0
    } else {
        let ss: f64 = sorted.iter().map(|v| (v - mean).powi(2)).sum();
        (ss / (n as f64 - 1.0)).sqrt()
    };
    NumericDescribe {
        count,
        mean,
        std,
        // `sorted` is ascending, so the ends are min/max.
        min: sorted[0],
        q25: quantile_sorted(sorted, 0.25),
        q50: quantile_sorted(sorted, 0.50),
        q75: quantile_sorted(sorted, 0.75),
        max: sorted[n - 1],
    }
}

/// The `q`-quantile of a **sorted** slice by linear interpolation — the
/// numpy/pandas default (`interpolation="linear"`): `pos = (n - 1) * q`,
/// `lower = floor(pos)`, `frac = pos - lower`,
/// `val = v[lower] + frac * (v[lower + 1] - v[lower])`.
///
/// `q` is assumed in `[0, 1]`; an empty slice returns `0.0`.
pub fn quantile_sorted(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let n = sorted.len();
    if n == 1 {
        return sorted[0];
    }
    let pos = (n as f64 - 1.0) * q;
    let lower = pos.floor() as usize;
    let frac = pos - lower as f64;
    if lower + 1 < n {
        sorted[lower] + frac * (sorted[lower + 1] - sorted[lower])
    } else {
        // pos == n - 1 exactly (q == 1.0): the top element, no successor.
        sorted[n - 1]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f64 = 1e-9;

    #[test]
    fn describe_matches_pandas_1_2_3_4() {
        // pandas .describe() of [1,2,3,4]: count 4, mean 2.5,
        // std 1.2909944487358056 (ddof=1), min 1, 25% 1.75, 50% 2.5,
        // 75% 3.25, max 4.
        let d = describe(&[1.0, 2.0, 3.0, 4.0]);
        assert_eq!(d.count, 4);
        assert!((d.mean - 2.5).abs() < EPS);
        assert!((d.std - 1.290_994_448_735_805_6).abs() < EPS);
        assert!((d.min - 1.0).abs() < EPS);
        assert!((d.q25 - 1.75).abs() < EPS);
        assert!((d.q50 - 2.5).abs() < EPS);
        assert!((d.q75 - 3.25).abs() < EPS);
        assert!((d.max - 4.0).abs() < EPS);
    }

    #[test]
    fn describe_single_value_has_zero_std() {
        let d = describe(&[7.0]);
        assert_eq!(d.count, 1);
        assert!((d.mean - 7.0).abs() < EPS);
        assert_eq!(d.std, 0.0);
        assert!((d.q25 - 7.0).abs() < EPS);
        assert!((d.q50 - 7.0).abs() < EPS);
        assert!((d.q75 - 7.0).abs() < EPS);
    }

    #[test]
    fn describe_empty_is_all_zero() {
        let d = describe(&[]);
        assert_eq!(d.count, 0);
        assert_eq!(d.mean, 0.0);
        assert_eq!(d.std, 0.0);
        assert_eq!(d.max, 0.0);
    }

    #[test]
    fn quantile_interpolates_linearly() {
        let v = [10.0, 20.0, 30.0, 40.0, 50.0];
        assert!((quantile_sorted(&v, 0.0) - 10.0).abs() < EPS);
        assert!((quantile_sorted(&v, 1.0) - 50.0).abs() < EPS);
        // pos = 4 * 0.25 = 1.0 -> exactly v[1]
        assert!((quantile_sorted(&v, 0.25) - 20.0).abs() < EPS);
        // pos = 4 * 0.1 = 0.4 -> 10 + 0.4*(20-10) = 14
        assert!((quantile_sorted(&v, 0.1) - 14.0).abs() < EPS);
    }
}
