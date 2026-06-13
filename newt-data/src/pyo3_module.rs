//! Python bindings for `newt-data` — the Phase 21 Centaur data-science engine.
//!
//! Compiled only when the `pyo3` cargo feature is on (turned on solely by the
//! umbrella `newt-agent-py` crate); default builds of `newt-data` stay
//! Python-free. Registered as the `newt_data` submodule of the umbrella so a
//! human notebook cell can call the fast Rust DS helpers directly (the Phase 21
//! "use PyO3 to create data-science tools" thesis — see
//! [`docs/design/centaur-data-scientist.md`](../../../docs/design/centaur-data-scientist.md)).
//!
//! The wrappers are deliberately **thin**: every bit of logic (CSV ingest, dtype
//! inference, honest truncation, pandas-faithful `describe`) lives in
//! `newt-data` and is tested by step 21.1. This module only converts at the
//! Python boundary and maps [`DataError`] to a single [`PyDataError`] exception.
//!
//! All three surfaces are sync (no inference, no kernel); `pyo3-async-runtimes`
//! is not needed here.

use crate::error::DataError;
use crate::store::ColumnDtype;
use crate::{ColumnSummary, DataStore, IngestReport, NumericDescribe, SqliteBackend};
use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyModule};
use std::path::PathBuf;

create_exception!(_newt_agent, PyDataError, PyException);

/// Map any [`DataError`] to the single Python-facing [`PyDataError`]. The
/// `Display` form preserves the typed variant's message (sqlite / csv / io /
/// invalid-identifier / no-such-table / empty-csv), so a notebook user reads an
/// honest, specific cause without the engine leaking a Rust type.
fn data_err_to_py(e: DataError) -> PyErr {
    PyDataError::new_err(e.to_string())
}

// ---- IngestReport ----

/// What [`load_csv_to_sqlite`] did: the table it (re)created plus the inferred
/// per-column schema. Mirrors [`IngestReport`]; frozen because ingest reports
/// are immutable facts.
#[pyclass(
    name = "IngestReport",
    module = "newt_agent._newt_agent.data",
    frozen,
    skip_from_py_object
)]
#[derive(Clone)]
pub struct PyIngestReport {
    pub inner: IngestReport,
}

#[pymethods]
impl PyIngestReport {
    /// The table the CSV was loaded into (dropped + recreated).
    #[getter]
    fn table(&self) -> &str {
        &self.inner.table
    }

    /// Number of data rows inserted (header excluded).
    #[getter]
    fn row_count(&self) -> u64 {
        self.inner.row_count
    }

    /// One [`PyColumnInfo`] per CSV column, in header order.
    #[getter]
    fn columns(&self) -> Vec<PyColumnInfo> {
        self.inner
            .columns
            .iter()
            .map(|c| PyColumnInfo {
                name: c.name.clone(),
                dtype: c.dtype,
                null_count: c.null_count,
            })
            .collect()
    }

    /// The recorded source path (`path.display()`), if any.
    #[getter]
    fn source(&self) -> Option<&str> {
        self.inner.source.as_deref()
    }

    fn __repr__(&self) -> String {
        format!(
            "IngestReport(table='{}', row_count={}, columns={})",
            self.inner.table,
            self.inner.row_count,
            self.inner.columns.len(),
        )
    }
}

/// One column's inferred schema, exposed inside a [`PyIngestReport`].
#[pyclass(
    name = "ColumnInfo",
    module = "newt_agent._newt_agent.data",
    frozen,
    skip_from_py_object
)]
#[derive(Clone)]
pub struct PyColumnInfo {
    name: String,
    dtype: ColumnDtype,
    null_count: u64,
}

#[pymethods]
impl PyColumnInfo {
    /// The literal column name (verbatim from the CSV header).
    #[getter]
    fn name(&self) -> &str {
        &self.name
    }

    /// The inferred SQLite dtype as a lowercase string: `"integer"`,
    /// `"real"`, or `"text"` (matches the serde `rename_all = "lowercase"`).
    #[getter]
    fn dtype(&self) -> &'static str {
        dtype_str(self.dtype)
    }

    /// How many cells in this column were empty (stored as `NULL`).
    #[getter]
    fn null_count(&self) -> u64 {
        self.null_count
    }

    fn __repr__(&self) -> String {
        format!(
            "ColumnInfo(name='{}', dtype='{}', null_count={})",
            self.name,
            dtype_str(self.dtype),
            self.null_count,
        )
    }
}

/// The lowercase dtype label, identical to the serde representation and to
/// `ColumnDtype`'s SQLite declared type lower-cased — the form a notebook user
/// cross-checks against pandas.
fn dtype_str(dtype: ColumnDtype) -> &'static str {
    match dtype {
        ColumnDtype::Integer => "integer",
        ColumnDtype::Real => "real",
        ColumnDtype::Text => "text",
    }
}

// ---- load_csv_to_sqlite ----

/// Load `csv_path` into `table` of the on-disk SQLite database at `db_path`,
/// dropping and recreating the table. Returns the [`PyIngestReport`].
///
/// Thin wrapper over [`SqliteBackend::open`] + [`DataStore::ingest_csv`]; all
/// dtype inference and identifier safety live in `newt-data` (step 21.1).
#[pyfunction]
#[pyo3(name = "load_csv_to_sqlite")]
fn py_load_csv_to_sqlite(
    csv_path: PathBuf,
    db_path: PathBuf,
    table: &str,
) -> PyResult<PyIngestReport> {
    let store = SqliteBackend::open(&db_path).map_err(data_err_to_py)?;
    let report = store.ingest_csv(&csv_path, table).map_err(data_err_to_py)?;
    Ok(PyIngestReport { inner: report })
}

// ---- query ----

/// The default row cap for [`query`] when the caller passes none. Generous so a
/// notebook query "just works" on a typical EDA dataset, while still bounding an
/// accidental `SELECT *` over a huge table.
const DEFAULT_ROW_CAP: usize = 100_000;

/// Run `sql` against the database at `db_path` and return up to `row_cap` rows
/// (default [`DEFAULT_ROW_CAP`]) as a Python `list[dict]` — one dict per row,
/// keyed by column name.
///
/// Cell values map honestly: SQL `NULL` → `None`, `INTEGER` → `int`,
/// `REAL` → `float`, `TEXT`/`BLOB` → `str`. The int↔float distinction is
/// preserved (an integer never widens to a Python `float`). A `0`/`1` produced
/// by a SQL boolean expression is an `INTEGER` in SQLite and so surfaces as an
/// `int`; genuine Python `bool` only appears if the underlying JSON value is a
/// JSON boolean.
///
/// The engine's honest `truncated` flag is not surfaced here (this helper hands
/// the notebook the rows directly); use the MCP `sql_query` tool when the
/// truncation signal matters.
#[pyfunction]
#[pyo3(name = "query", signature = (db_path, sql, row_cap=None))]
fn py_query<'py>(
    py: Python<'py>,
    db_path: PathBuf,
    sql: &str,
    row_cap: Option<usize>,
) -> PyResult<Bound<'py, PyList>> {
    let store = SqliteBackend::open(&db_path).map_err(data_err_to_py)?;
    let cap = row_cap.unwrap_or(DEFAULT_ROW_CAP);
    let result = store.query(sql, cap).map_err(data_err_to_py)?;

    let rows = PyList::empty(py);
    for row in &result.rows {
        let d = PyDict::new(py);
        for (col, cell) in result.columns.iter().zip(row.iter()) {
            d.set_item(col, json_cell_to_py(py, cell)?)?;
        }
        rows.append(d)?;
    }
    Ok(rows)
}

/// Convert one [`serde_json::Value`] cell from a [`QueryResult`] row into a
/// Python object **without a lossy int→float coercion**.
///
/// `Null` → `None`; a JSON integer → Python `int` (i64 or u64, never via
/// `f64`); a JSON non-integer number → Python `float`; a JSON string → `str`;
/// a JSON bool → `bool`. Arrays/objects cannot occur in a SQL row cell, but are
/// mapped to their JSON string form defensively rather than dropped.
///
/// [`QueryResult`]: crate::QueryResult
fn json_cell_to_py<'py>(py: Python<'py>, cell: &serde_json::Value) -> PyResult<Bound<'py, PyAny>> {
    use serde_json::Value;
    let obj = match cell {
        Value::Null => py.None().into_bound(py),
        Value::Bool(b) => b.into_pyobject(py)?.to_owned().into_any(),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.into_pyobject(py)?.into_any()
            } else if let Some(u) = n.as_u64() {
                u.into_pyobject(py)?.into_any()
            } else if let Some(f) = n.as_f64() {
                f.into_pyobject(py)?.into_any()
            } else {
                py.None().into_bound(py)
            }
        }
        Value::String(s) => s.into_pyobject(py)?.into_any(),
        // A SQL row cell is always a scalar; these arms are defensive only.
        other => other.to_string().into_pyobject(py)?.into_any(),
    };
    Ok(obj)
}

// ---- summarize ----

/// Compute schema + per-column statistics for `table` in the database at
/// `db_path`, returned as a Python `dict`.
///
/// Shape: `{"table": str, "row_count": int, "columns": [ {col}, … ]}` where
/// each `{col}` carries `name`, `dtype`, `null_count`, `distinct_count`, and —
/// for numeric columns only — a nested `describe` dict with pandas-faithful
/// `count` / `mean` / `std` / `min` / `q25` / `q50` / `q75` / `max`. A `TEXT`
/// column has no `describe` key.
///
/// Thin wrapper over [`DataStore::summarize`]; the statistics themselves are
/// computed (and pinned against pandas) in `newt-data` (step 21.1).
#[pyfunction]
#[pyo3(name = "summarize")]
fn py_summarize<'py>(
    py: Python<'py>,
    db_path: PathBuf,
    table: &str,
) -> PyResult<Bound<'py, PyDict>> {
    let store = SqliteBackend::open(&db_path).map_err(data_err_to_py)?;
    let summary = store.summarize(table).map_err(data_err_to_py)?;

    let out = PyDict::new(py);
    out.set_item("table", &summary.table)?;
    out.set_item("row_count", summary.row_count)?;
    let cols = PyList::empty(py);
    for col in &summary.columns {
        cols.append(column_summary_to_py(py, col)?)?;
    }
    out.set_item("columns", cols)?;
    Ok(out)
}

/// Build the per-column dict for [`py_summarize`], with the nested numeric
/// `describe` dict present only for numeric columns.
fn column_summary_to_py<'py>(py: Python<'py>, col: &ColumnSummary) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    d.set_item("name", &col.name)?;
    d.set_item("dtype", dtype_str(col.dtype))?;
    d.set_item("null_count", col.null_count)?;
    d.set_item("distinct_count", col.distinct_count)?;
    if let Some(n) = col.numeric {
        d.set_item("describe", numeric_describe_to_py(py, n)?)?;
    }
    Ok(d)
}

/// Build the pandas-style `describe` dict for a numeric column.
fn numeric_describe_to_py<'py>(
    py: Python<'py>,
    n: NumericDescribe,
) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    d.set_item("count", n.count)?;
    d.set_item("mean", n.mean)?;
    d.set_item("std", n.std)?;
    d.set_item("min", n.min)?;
    d.set_item("q25", n.q25)?;
    d.set_item("q50", n.q50)?;
    d.set_item("q75", n.q75)?;
    d.set_item("max", n.max)?;
    Ok(d)
}

/// Register the `data` submodule on the parent `_newt_agent` module.
pub fn register(py: Python<'_>, parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let m = PyModule::new(py, "data")?;
    m.add_class::<PyIngestReport>()?;
    m.add_class::<PyColumnInfo>()?;
    m.add_function(wrap_pyfunction!(py_load_csv_to_sqlite, &m)?)?;
    m.add_function(wrap_pyfunction!(py_query, &m)?)?;
    m.add_function(wrap_pyfunction!(py_summarize, &m)?)?;
    m.add("DataError", py.get_type::<PyDataError>())?;
    parent.add_submodule(&m)?;
    Ok(())
}

// These tests embed a CPython interpreter (`Python::attach`) and so must link
// libpython. That only works when `newt-data` is built standalone — never under
// the umbrella's `extension-module` build. They are therefore gated behind the
// dedicated `embed-tests` feature (NOT `pyo3`), so `cargo test --workspace`
// never compiles them. Run with: `cargo test -p newt-data --features
// pyo3,embed-tests` (PYO3_PYTHON pointing at a shared-libpython interpreter).
#[cfg(all(test, feature = "embed-tests"))]
mod tests {
    use super::*;
    use std::io::Write;

    const FIXTURE_CSV: &str = "id,label,score\n\
        1,alpha,1.0\n\
        2,bravo,2.0\n\
        3,charlie,3.0\n\
        4,delta,4.0\n\
        5,echo,\n";

    /// Materialize the fixture CSV under `dir` and return its path.
    fn write_fixture(dir: &std::path::Path) -> PathBuf {
        let path = dir.join("metrics.csv");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(FIXTURE_CSV.as_bytes()).unwrap();
        f.flush().unwrap();
        path
    }

    /// End-to-end over the GIL: load a CSV, query it back as list[dict], and
    /// summarize — asserting the returned Python objects mirror the engine.
    /// Hermetic: both the CSV and the DB live under a fresh tempdir.
    #[test]
    fn load_query_summarize_round_trip() {
        Python::initialize();
        Python::attach(|py| {
            let dir = tempfile::tempdir().unwrap();
            let csv_path = write_fixture(dir.path());
            let db_path = dir.path().join("data.db");

            // load_csv_to_sqlite → PyIngestReport
            let report =
                py_load_csv_to_sqlite(csv_path.clone(), db_path.clone(), "metrics").unwrap();
            assert_eq!(report.table(), "metrics");
            assert_eq!(report.row_count(), 5);
            let cols = report.columns();
            assert_eq!(cols.len(), 3);
            assert_eq!(cols[0].name(), "id");
            assert_eq!(cols[0].dtype(), "integer");
            assert_eq!(cols[0].null_count(), 0);
            assert_eq!(cols[2].dtype(), "real");
            assert_eq!(cols[2].null_count(), 1);
            assert!(report.source().is_some());
            assert!(report.__repr__().contains("metrics"));
            assert!(cols[0].__repr__().contains("id"));

            // query → list[dict] with honest int/float/None/str typing.
            let rows = py_query(
                py,
                db_path.clone(),
                "SELECT id, label, score FROM metrics ORDER BY id",
                None,
            )
            .unwrap();
            assert_eq!(rows.len(), 5);

            let first = rows.get_item(0).unwrap();
            let first: &Bound<'_, PyDict> = first.cast().unwrap();
            // id is an int (NOT a float) — no lossy widening.
            let id_obj = first.get_item("id").unwrap().unwrap();
            assert!(id_obj.is_instance_of::<pyo3::types::PyInt>());
            let id_val: i64 = id_obj.extract().unwrap();
            assert_eq!(id_val, 1);
            // score is a float.
            let score_obj = first.get_item("score").unwrap().unwrap();
            assert!(score_obj.is_instance_of::<pyo3::types::PyFloat>());
            let score_val: f64 = score_obj.extract().unwrap();
            assert!((score_val - 1.0).abs() < 1e-9);
            // label is a str.
            let label_val: String = first.get_item("label").unwrap().unwrap().extract().unwrap();
            assert_eq!(label_val, "alpha");

            // Row 5 has a NULL score → None.
            let fifth = rows.get_item(4).unwrap();
            let fifth: &Bound<'_, PyDict> = fifth.cast().unwrap();
            assert!(fifth.get_item("score").unwrap().unwrap().is_none());

            // summarize → dict with nested describe for numeric columns.
            let summary = py_summarize(py, db_path.clone(), "metrics").unwrap();
            let row_count: u64 = summary
                .get_item("row_count")
                .unwrap()
                .unwrap()
                .extract()
                .unwrap();
            assert_eq!(row_count, 5);
            let cols_any = summary.get_item("columns").unwrap().unwrap();
            let cols_list: &Bound<'_, PyList> = cols_any.cast().unwrap();
            assert_eq!(cols_list.len(), 3);

            // The score column carries a pandas-faithful describe dict.
            let mut saw_score = false;
            for item in cols_list.iter() {
                let c: &Bound<'_, PyDict> = item.cast().unwrap();
                let name: String = c.get_item("name").unwrap().unwrap().extract().unwrap();
                if name == "score" {
                    saw_score = true;
                    let dtype: String = c.get_item("dtype").unwrap().unwrap().extract().unwrap();
                    assert_eq!(dtype, "real");
                    let null_count: u64 = c
                        .get_item("null_count")
                        .unwrap()
                        .unwrap()
                        .extract()
                        .unwrap();
                    assert_eq!(null_count, 1);
                    let describe = c.get_item("describe").unwrap().unwrap();
                    let describe: &Bound<'_, PyDict> = describe.cast().unwrap();
                    let mean: f64 = describe
                        .get_item("mean")
                        .unwrap()
                        .unwrap()
                        .extract()
                        .unwrap();
                    assert!((mean - 2.5).abs() < 1e-9);
                    let std: f64 = describe
                        .get_item("std")
                        .unwrap()
                        .unwrap()
                        .extract()
                        .unwrap();
                    assert!((std - 1.290_994_448_735_805_6).abs() < 1e-9);
                } else if name == "label" {
                    // A TEXT column has no describe key.
                    let c2: &Bound<'_, PyDict> = item.cast().unwrap();
                    assert!(c2.get_item("describe").unwrap().is_none());
                }
            }
            assert!(saw_score, "summary should include the score column");
        });
    }

    /// A DataError (bad SQL → sqlite error) surfaces as PyDataError, not a
    /// transport panic.
    #[test]
    fn bad_sql_raises_data_error() {
        Python::initialize();
        Python::attach(|py| {
            let dir = tempfile::tempdir().unwrap();
            let csv_path = write_fixture(dir.path());
            let db_path = dir.path().join("data.db");
            py_load_csv_to_sqlite(csv_path, db_path.clone(), "metrics").unwrap();

            let err = py_query(py, db_path, "SELECT * FROM no_such_table", None).unwrap_err();
            assert!(err.is_instance_of::<PyDataError>(py));
        });
    }

    /// summarize of a missing table maps NoSuchTable → PyDataError.
    #[test]
    fn summarize_missing_table_raises() {
        Python::initialize();
        Python::attach(|py| {
            let dir = tempfile::tempdir().unwrap();
            let db_path = dir.path().join("data.db");
            // Open creates an empty DB; summarizing a nonexistent table errors.
            let err = py_summarize(py, db_path, "nope").unwrap_err();
            assert!(err.is_instance_of::<PyDataError>(py));
        });
    }
}
