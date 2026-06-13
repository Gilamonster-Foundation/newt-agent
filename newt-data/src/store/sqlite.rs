//! The SQLite [`DataStore`] backend (Phase 21).
//!
//! The first (and, until step 21.8's DuckDB backend, only) implementation of
//! the [`DataStore`] seam. It reuses the family's bundled `rusqlite 0.31`
//! (MSRV 1.75) and the same WAL-with-DELETE-fallback connection setup as the
//! conversation store ([`newt_core::store`]) so it behaves identically on NFS
//! homes. The data database is **separate** from the conversation store
//! (`<workspace>/.newt-data/data.db`), never entangled — see
//! [`docs/design/centaur-data-scientist.md`](../../../../docs/design/centaur-data-scientist.md).
//!
//! [`newt_core::store`]: https://docs.rs/newt-core

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::types::ValueRef;
use rusqlite::Connection;

use crate::error::Result;
use crate::ingest::quote_ident;
use crate::store::{DataStore, IngestReport, QueryResult, TableInfo, TableSummary};
use crate::{ingest, summary};

/// The directory created under a workspace to hold the data database
/// (`<workspace>/.newt-data/data.db`). Kept separate from `~/.newt` so the DS
/// store never touches the conversation database.
const DATA_DIR: &str = ".newt-data";

/// The data database file name under [`DATA_DIR`].
const DB_FILE: &str = "data.db";

/// SQLite-backed data engine (see module docs).
///
/// Cheap to clone: clones share one connection behind a mutex, matching the
/// conversation store. All methods take `&self`.
#[derive(Debug, Clone)]
pub struct SqliteBackend {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteBackend {
    /// Open (creating if needed) the data database at `path`.
    ///
    /// Creates the parent directory, opens the connection, applies
    /// `journal_mode=WAL` (+ `synchronous=NORMAL`) with a `journal_mode=DELETE`
    /// fallback on the known network-filesystem failure modes — exactly like
    /// [`newt_core::store`](crate::store::sqlite) — and ensures the
    /// `__datasets` metadata table exists.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        apply_journal_mode(&conn)?;
        ensure_metadata_table(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Open an in-memory database with the `__datasets` table — used by tests
    /// (no WAL on `:memory:`; the metadata table is created the same way).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        ensure_metadata_table(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// The default data-database path for a workspace:
    /// `<workspace>/.newt-data/data.db`.
    pub fn default_db_path(workspace: &Path) -> PathBuf {
        workspace.join(DATA_DIR).join(DB_FILE)
    }

    /// Lock the shared connection, recovering from a poisoned mutex (a panic in
    /// another thread leaves the connection itself usable — transactions roll
    /// back), matching the conversation store's policy.
    fn lock_conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl DataStore for SqliteBackend {
    fn ingest_csv(&self, path: &Path, table: &str) -> Result<IngestReport> {
        let conn = self.lock_conn();
        ingest::ingest_csv(&conn, path, table)
    }

    fn query(&self, sql: &str, row_cap: usize) -> Result<QueryResult> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(sql)?;
        let columns: Vec<String> = stmt.column_names().iter().map(|c| c.to_string()).collect();

        // A non-row statement (INSERT/UPDATE/CREATE/…): execute it via the same
        // prepared statement and report an empty, untruncated result.
        if stmt.column_count() == 0 {
            stmt.raw_execute()?;
            return Ok(QueryResult {
                columns,
                rows: Vec::new(),
                returned: 0,
                truncated: false,
            });
        }

        let mut sql_rows = stmt.query([])?;
        let mut rows: Vec<Vec<serde_json::Value>> = Vec::new();
        let mut truncated = false;
        while let Some(row) = sql_rows.next()? {
            if rows.len() >= row_cap {
                // One row past the cap exists → mark truncated, drop it, stop.
                truncated = true;
                break;
            }
            let mut out_row = Vec::with_capacity(columns.len());
            for i in 0..columns.len() {
                out_row.push(value_ref_to_json(row.get_ref(i)?));
            }
            rows.push(out_row);
        }

        let returned = rows.len();
        Ok(QueryResult {
            columns,
            rows,
            returned,
            truncated,
        })
    }

    fn summarize(&self, table: &str) -> Result<TableSummary> {
        let conn = self.lock_conn();
        summary::summarize(&conn, table)
    }

    fn list_tables(&self) -> Result<Vec<TableInfo>> {
        let conn = self.lock_conn();
        let names: Vec<String> = {
            let mut stmt = conn.prepare(
                "SELECT name FROM sqlite_master
                  WHERE type = 'table'
                    AND name NOT LIKE 'sqlite_%'
                    AND name != '__datasets'
                  ORDER BY name",
            )?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };

        let mut infos = Vec::with_capacity(names.len());
        for name in names {
            let quoted = quote_ident(&name);
            let row_count: u64 =
                conn.query_row(&format!("SELECT COUNT(*) FROM {quoted}"), [], |row| {
                    row.get::<_, i64>(0).map(|n| n.max(0) as u64)
                })?;
            let source: Option<String> = conn
                .query_row(
                    "SELECT source FROM __datasets WHERE table_name = ?1",
                    [&name],
                    |row| row.get::<_, Option<String>>(0),
                )
                .ok()
                .flatten();
            infos.push(TableInfo {
                table: name,
                row_count,
                source,
            });
        }
        Ok(infos)
    }

    fn backend_name(&self) -> &'static str {
        "sqlite"
    }
}

/// Apply `journal_mode=WAL` (+ `synchronous=NORMAL`) with a
/// `journal_mode=DELETE` fallback on the SQLite errors WAL is known to produce
/// on network filesystems (NFS homes) — the same policy as
/// [`newt_core::store`]. Any other error propagates.
fn apply_journal_mode(conn: &Connection) -> Result<()> {
    let wal: rusqlite::Result<String> =
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0));
    match wal {
        // Assert the pragma actually took (it has documented silent-no-op
        // cases): NORMAL is only safe under WAL.
        Ok(mode) if mode.eq_ignore_ascii_case("wal") => {
            conn.pragma_update(None, "synchronous", "NORMAL")?;
            Ok(())
        }
        Ok(mode) => {
            tracing::warn!(%mode, "journal_mode=WAL did not take; keeping synchronous=FULL");
            Ok(())
        }
        Err(e) if wal_fallback_eligible(&e.to_string()) => {
            tracing::warn!(
                error = %e,
                "SQLite refused WAL (network filesystem?); data.db is running on the \
                 slower journal_mode=DELETE fallback"
            );
            conn.pragma_update(None, "journal_mode", "DELETE")?;
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

/// `true` for the SQLite error texts WAL produces on filesystems without
/// shared-memory mmap / POSIX lock support (NFS homes), where the store should
/// fall back to `journal_mode=DELETE` rather than fail to open. Mirrors the
/// conversation store's predicate.
fn wal_fallback_eligible(error_text: &str) -> bool {
    let lower = error_text.to_lowercase();
    lower.contains("locking protocol") || lower.contains("disk i/o error")
}

/// Create the engine's metadata table if absent. One row per ingested table,
/// recording the source path, row count, and ingest time (Phase 21).
fn ensure_metadata_table(conn: &Connection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS __datasets (
             table_name  TEXT PRIMARY KEY,
             source      TEXT,
             row_count   INTEGER NOT NULL,
             ingested_at INTEGER NOT NULL
         )",
        [],
    )?;
    Ok(())
}

/// Map a SQLite [`ValueRef`] to a [`serde_json::Value`] (Phase 21).
///
/// `NULL` → `Null`, `INTEGER` → `Number`, `REAL` → `Number` (via
/// [`serde_json::Number::from_f64`]), `TEXT` → `String`, and `BLOB` → a base64
/// `String` so binary survives the JSON boundary.
///
/// ## Non-finite reals are lossy at this boundary (Phase 21 contract)
///
/// JSON has no representation for `NaN` / `±Infinity`, so
/// [`serde_json::Number::from_f64`] returns `None` for any non-finite float and
/// this function emits `Null`. This is a **reachable, lossy** case, not merely
/// defensive: CSV ingest infers a column as [`Real`](crate::ColumnDtype::Real)
/// whenever every cell parses as `f64`, and Rust's `f64` parser accepts the
/// tokens `inf`, `-inf`, `nan`, `infinity` (case-insensitively) as well as
/// magnitudes that overflow to `±inf` (e.g. `1e9999`). Such a cell is stored as
/// `REAL` by [`ingest`](crate::ingest) and then degrades to JSON `Null` on
/// query — the original token is not preserved. (This differs from pandas,
/// which parses the same tokens to float but *keeps* the `inf`/`NaN` value.)
/// The behavior is pinned by `value_ref_real_non_finite_becomes_null` and the
/// end-to-end `query_non_finite_real_cell_is_null` regression test.
fn value_ref_to_json(value: ValueRef<'_>) -> serde_json::Value {
    use serde_json::Value;
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::Number(i.into()),
        ValueRef::Real(f) => serde_json::Number::from_f64(f)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        ValueRef::Text(bytes) => Value::String(String::from_utf8_lossy(bytes).into_owned()),
        ValueRef::Blob(bytes) => Value::String(base64_encode(bytes)),
    }
}

/// Standard base64 (RFC 4648, with `=` padding) of `bytes`.
///
/// A tiny self-contained encoder so `newt-data` needs no `base64` dependency
/// just to ferry the rare `BLOB` cell across the JSON boundary (Phase 21 — the
/// dependency budget is deliberately minimal; every new dep must compile on
/// MSRV 1.75).
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0b11) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(((b1 & 0b1111) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(b2 & 0b111111) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        // RFC 4648 test vectors.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn default_db_path_is_under_newt_data() {
        let p = SqliteBackend::default_db_path(Path::new("/tmp/ws"));
        assert_eq!(p, Path::new("/tmp/ws/.newt-data/data.db"));
    }

    #[test]
    fn wal_fallback_predicate() {
        assert!(wal_fallback_eligible("disk I/O error"));
        assert!(wal_fallback_eligible("locking protocol"));
        assert!(!wal_fallback_eligible("syntax error"));
    }

    #[test]
    fn open_creates_parent_and_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = SqliteBackend::default_db_path(dir.path());
        let backend = SqliteBackend::open(&path).unwrap();
        assert!(path.exists());
        // __datasets exists and is queryable; no user tables yet.
        assert_eq!(backend.list_tables().unwrap().len(), 0);
    }

    #[test]
    fn value_ref_blob_becomes_base64() {
        assert_eq!(
            value_ref_to_json(ValueRef::Blob(b"foobar")),
            serde_json::Value::String("Zm9vYmFy".to_string())
        );
        assert_eq!(value_ref_to_json(ValueRef::Null), serde_json::Value::Null);
    }

    #[test]
    fn value_ref_real_finite_becomes_number() {
        assert_eq!(
            value_ref_to_json(ValueRef::Real(1.5)),
            serde_json::json!(1.5)
        );
    }

    /// Pins the lossy non-finite boundary documented on [`value_ref_to_json`]:
    /// JSON cannot represent `NaN` / `±Infinity`, so every non-finite `REAL`
    /// degrades to `Null` rather than erroring. A future change to the
    /// `from_f64` handling (e.g. preserving these as strings) must update this
    /// test deliberately.
    #[test]
    fn value_ref_real_non_finite_becomes_null() {
        for f in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(
                value_ref_to_json(ValueRef::Real(f)),
                serde_json::Value::Null,
                "non-finite real {f} should degrade to JSON null"
            );
        }
    }
}
