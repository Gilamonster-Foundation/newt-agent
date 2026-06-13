//! # newt-mcp-data — the Phase 21 Centaur data-science MCP server
//!
//! A **thin stdio MCP server** that exposes the headless [`newt_data`] SQLite
//! engine — and, since Phase 21.3, the human's live Jupyter kernel — as MCP
//! tools, the shippable slices of the
//! [Centaur Data Scientist](../../../docs/design/centaur-data-scientist.md)
//! capability. All the logic lives in `newt-data`; this crate is a
//! dependency-light JSON-RPC adapter (no inference, no confined shell, no
//! capability leash — pure data, plus a websocket to the human's own kernel).
//!
//! ## Tool surface
//!
//! Tools are registered with bare names; the MCP client namespaces them as
//! `data__*` when this server is configured under the name `"data"`. The SQL EDA
//! tools (21.2):
//!
//! - `sql_ingest_csv` — ingest a CSV into a dtype-inferred SQLite table.
//! - `sql_query` — run a read-or-write SQL statement (honest `truncated` flag).
//! - `sql_summarize` — schema / dtypes / null+distinct counts / pandas describe.
//! - `sql_list_tables` — list ingested tables with row counts and CSV sources.
//!
//! Plus the Phase 21.3 live-kernel co-pilot (`docs/design/centaur-data-scientist.md`):
//!
//! - `kernel_attach` — attach to the human's already-running Jupyter server.
//! - `run_cell` — run a cell, reading back stdout/stderr, rich results, and PNG
//!   plots (written to `<data-dir>/.newt-data/plots/`, reported as a path + an
//!   honest size summary — never inlined).
//!
//! Every tool returns the MCP content envelope; **any** failure (bad SQL, no
//! such table, a missing argument) comes back as an in-band MCP tool error
//! (`isError: true`) the model can read and recover from — never a `-32603`
//! transport fault. This is the same in-band-error discipline `newt-mcp-server`
//! applies to `shell_run` (see [`handlers`]).
//!
//! ## Usage — wire it into newt chat with one config line
//!
//! Add an `[[mcp_servers]]` entry to `~/.newt/config.toml`:
//!
//! ```toml
//! [[mcp_servers]]
//! name = "data"
//! command = "newt-mcp-data"
//! ```
//!
//! The agent then discovers and routes to `data__sql_query`,
//! `data__sql_ingest_csv`, `data__sql_summarize`, and `data__sql_list_tables`
//! through the existing MCP discovery→connect→route chain — **zero** newt-core /
//! newt-tui / agentic-loop changes. The data database lives at
//! `<workspace>/.newt-data/data.db` (override with the `NEWT_DATA_DB`
//! environment variable), separate from the conversation store.

use std::path::PathBuf;
use std::sync::Arc;

use newt_data::SqliteBackend;

pub mod handlers;
pub mod server;

/// Environment variable that, when set, overrides the data-database path.
///
/// Used by the MCP launcher (and the integration smoke test) to point the
/// server at a throwaway file instead of the workspace's `.newt-data/data.db`.
const DB_PATH_ENV: &str = "NEWT_DATA_DB";

/// Run the data MCP server over stdin/stdout.
///
/// Resolves the data-database path — `NEWT_DATA_DB` if set, else
/// [`SqliteBackend::default_db_path`] under the current working directory —
/// opens the [`SqliteBackend`], builds an [`server::McpServer`], registers the
/// SQL **and** live-kernel handlers (over the shared store + a fresh, empty
/// kernel session), and serves the JSON-RPC wire on stdio.
pub async fn run_stdio() -> anyhow::Result<()> {
    let store = open_store()?;
    let plots_dir = resolve_plots_dir();
    let session = handlers::new_kernel_session();
    let mut server = server::McpServer::new();
    handlers::register_handlers(&mut server, store, session, plots_dir);
    server.run_stdio().await
}

/// The directory the data database lives in (`NEWT_DATA_DB`'s parent, else
/// `<cwd>/.newt-data`) — the data engine's home and the parent of `plots/`.
/// Pulled out so the policy is unit-testable.
fn data_dir() -> PathBuf {
    match std::env::var_os(DB_PATH_ENV) {
        Some(path) => PathBuf::from(path)
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(".")),
        None => std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(".newt-data"),
    }
}

/// The directory `run_cell` writes decoded PNG plots into: `<data-dir>/plots`
/// (Phase 21.3). With `NEWT_DATA_DB=/x/.newt-data/data.db` that is
/// `/x/.newt-data/plots`; with no override it is `<cwd>/.newt-data/plots`.
fn resolve_plots_dir() -> PathBuf {
    newt_data::kernel::plots_dir(&data_dir())
}

/// Open the [`SqliteBackend`] at the resolved data-database path.
///
/// Honors `NEWT_DATA_DB` (an explicit on-disk path); otherwise opens
/// `<cwd>/.newt-data/data.db`. Pulled out of [`run_stdio`] so the path-resolution
/// policy is unit-testable without standing up the stdio loop.
fn open_store() -> anyhow::Result<Arc<SqliteBackend>> {
    let db_path = match std::env::var_os(DB_PATH_ENV) {
        Some(path) => std::path::PathBuf::from(path),
        None => SqliteBackend::default_db_path(&std::env::current_dir()?),
    };
    tracing::info!(db = %db_path.display(), "newt-mcp-data: opening data store");
    let store = SqliteBackend::open(&db_path)?;
    Ok(Arc::new(store))
}

#[cfg(test)]
mod tests {
    use super::*;
    use newt_data::DataStore;
    use std::sync::Mutex;

    /// `NEWT_DATA_DB` and the process-wide current directory are global mutable
    /// state; serialize the two tests that touch them so they cannot race when
    /// cargo runs the lib-test binary multi-threaded.
    static ENV_GUARD: Mutex<()> = Mutex::new(());

    /// `NEWT_DATA_DB`, when set, opens exactly that file (and creates its parent
    /// directory). Verified by listing through the freshly opened store.
    #[test]
    fn open_store_honors_env_override() {
        let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("nested").join("custom.db");
        std::env::set_var(DB_PATH_ENV, &db);
        let store = open_store().unwrap();
        std::env::remove_var(DB_PATH_ENV);
        // A fresh store has the metadata table but no user tables.
        assert!(store.list_tables().unwrap().is_empty());
        assert_eq!(store.backend_name(), "sqlite");
        assert!(
            db.exists(),
            "the env-specified db file should have been created"
        );
    }

    /// With no override, [`open_store`] resolves under the current directory's
    /// `.newt-data/data.db`. We point cwd at a tempdir so the test never writes
    /// into the real workspace.
    #[test]
    fn open_store_defaults_under_cwd() {
        let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        // Guard against another test having set the env var.
        std::env::remove_var(DB_PATH_ENV);
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        let result = open_store();

        // Restore cwd before asserting so a failure can't leave the process in
        // the tempdir (which is about to be deleted).
        std::env::set_current_dir(&prev).unwrap();

        let store = result.unwrap();
        assert!(store.list_tables().unwrap().is_empty());
        assert!(
            dir.path().join(".newt-data").join("data.db").exists(),
            "default path should be <cwd>/.newt-data/data.db"
        );
    }
}
