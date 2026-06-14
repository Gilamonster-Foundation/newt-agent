//! Error type for the Phase 21 `newt-data` engine.
//!
//! See [`docs/design/centaur-data-scientist.md`](../../../docs/design/centaur-data-scientist.md):
//! the engine is headless and surfaces every failure as a typed [`DataError`]
//! so the later MCP adapter (step 21.2) and PyO3 submodule (step 21.6) can map
//! each variant to an honest, machine-readable error without string-sniffing.

use thiserror::Error;

/// Everything that can go wrong inside the `newt-data` SQLite engine.
///
/// The three `#[from]` variants wrap the upstream errors verbatim so the
/// underlying cause is never lost; the remaining variants are the engine's own
/// invariant violations (Phase 21 — identifier safety, table existence, and
/// the empty-CSV guard described in
/// [`docs/design/centaur-data-scientist.md`](../../../docs/design/centaur-data-scientist.md)).
#[derive(Debug, Error)]
pub enum DataError {
    /// A SQLite operation failed (open, pragma, prepare, step, bind, …).
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// The CSV reader failed to parse the input (malformed record, unequal
    /// field counts under `flexible = false`, encoding, …).
    #[error("csv error: {0}")]
    Csv(#[from] csv::Error),

    /// A filesystem operation failed (creating the parent dir, opening the
    /// CSV path, …).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// A table identifier supplied to [`ingest_csv`](crate::DataStore::ingest_csv)
    /// is not a safe SQLite identifier: empty, or containing a NUL byte or a
    /// double-quote. Defense in depth on top of the always-on
    /// double-quote quoting (Phase 21 — identifier safety is a review focus).
    #[error("invalid SQL identifier `{0}`: table names may not be empty or contain NUL / double-quote characters")]
    InvalidIdentifier(String),

    /// A table referenced by [`summarize`](crate::DataStore::summarize) does
    /// not exist in the database.
    #[error("no such table: `{0}`")]
    NoSuchTable(String),

    /// The CSV had no header row, so no columns could be created.
    #[error("empty csv: no header row")]
    EmptyCsv,

    /// An `.ipynb` notebook (Phase 21.4 — [`crate::notebook`]) could not be
    /// read, parsed, or validated: the file is not valid JSON, its top-level
    /// value is not an object, its `nbformat` is not `4`, or it is otherwise not
    /// a well-formed nbformat-4 document. The message names the offending file
    /// and the specific reason so the MCP adapter surfaces an honest, in-band
    /// tool error the model can read (never a panic). See
    /// [`docs/design/centaur-data-scientist.md`](../../../docs/design/centaur-data-scientist.md)
    /// §4.1, the notebook-artifact bullet.
    #[error("invalid notebook: {0}")]
    InvalidNotebook(String),
}

/// Crate-wide result alias over [`DataError`].
pub type Result<T> = std::result::Result<T, DataError>;
