//! nbformat `.ipynb` notebook persistence (Phase 21.4).
//!
//! The [Centaur Data Scientist](../../../docs/design/centaur-data-scientist.md)
//! §4.1 *notebook-artifact* bullet: the human notebook must stay a **faithful,
//! reviewable, git-diffable artifact** of the agent's work. Where 21.3 ran cells
//! on a live kernel and reported the outputs *into chat*, 21.4 leaves a durable
//! record *on disk* — `run_cell(persist_to=…)` appends each executed cell (source
//! + its outputs) to a real `.ipynb` so a human can open it in JupyterLab, see
//! exactly what ran, and review the diff in a pull request.
//!
//! ## What this module is (and is not)
//!
//! This is a **pure `serde_json` `.ipynb` manipulator** — it has no kernel, HTTP,
//! or websocket dependency, so it is a *normal* module (not behind the `kernel`
//! feature). An `.ipynb` is just JSON in the
//! [nbformat v4](https://nbformat.readthedocs.io/en/latest/format_description.html)
//! schema:
//!
//! ```json
//! { "cells": [ … ], "metadata": { … }, "nbformat": 4, "nbformat_minor": 5 }
//! ```
//!
//! The three public operations mirror the three Phase 21.4 MCP tools:
//!
//! - [`read_notebook`] → `notebook_read`: a reviewable summary of every cell.
//! - [`insert_cell`] → `notebook_insert_cell`: **proposes** a cell (it does *not*
//!   execute it — a code cell goes in with `execution_count: null, outputs: []`).
//! - [`persist_cell`] → `notebook_persist_executed_cell`: appends a code cell
//!   carrying its source **and** already-nbformat-shaped outputs. This is the
//!   low-level primitive `run_cell(persist_to)` calls after a successful run.
//!
//! ## Atomicity (the load-bearing review contract)
//!
//! Every write goes through [`atomic_write`]: serialize to a temp file in the
//! **same directory** as the target, then `rename` it over the target. A rename
//! within one filesystem is atomic, so a reader (a human's JupyterLab autosave
//! watcher, a `git diff`) never observes a half-written notebook, and a
//! serialization failure leaves the original untouched — the same discipline the
//! conversation store uses for `data.db`. The notebook is an artifact under
//! review; a torn write would be a corrupt artifact.
//!
//! ## Errors
//!
//! Every fallible operation returns [`crate::Result`] over [`crate::DataError`]:
//! a missing file on read, a non-nbformat-4 or corrupt file, or an I/O failure
//! is a clean typed error — never a panic.

use std::path::Path;

use serde_json::{json, Map, Value};

use crate::{DataError, Result};

/// The nbformat major version this module reads and writes (`nbformat: 4`).
const NBFORMAT_MAJOR: i64 = 4;

/// The `nbformat_minor` we stamp onto notebooks we create (4.5 is the modern
/// default — it adds per-cell `id`s, which we mint as fresh UUIDs).
const NBFORMAT_MINOR: i64 = 5;

/// One cell's reviewable summary, returned by [`read_notebook`].
///
/// Deliberately flat and serde-serializable so the MCP `notebook_read` handler
/// can ferry the `Vec<CellInfo>` straight across the JSON-RPC boundary as pretty
/// JSON — a human-scannable table of what the notebook holds.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CellInfo {
    /// Zero-based position of the cell in the notebook's `cells` array.
    pub index: usize,
    /// The cell kind: `code`, `markdown`, or `raw`.
    pub cell_type: String,
    /// The cell's `source`, joined into one string (nbformat stores source as
    /// either a single string or an array of line-strings; both fold to this).
    pub source: String,
    /// `true` if a code cell has at least one entry in its `outputs` array.
    pub has_output: bool,
}

/// The kind of cell [`insert_cell`] builds. A `code` cell gets the executable
/// scaffolding (`execution_count: null`, `outputs: []`); a `markdown` or `raw`
/// cell carries only `source`. Anything the caller passes that is not `markdown`
/// or `raw` is treated as `code` (the safe, executable default).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellType {
    /// An executable code cell (`execution_count: null`, empty `outputs`).
    Code,
    /// A markdown prose cell.
    Markdown,
    /// A raw (verbatim) cell.
    Raw,
}

impl CellType {
    /// Map a free-form string (an MCP argument) to a [`CellType`], defaulting to
    /// [`CellType::Code`] for anything unrecognized — the executable default the
    /// agent reaches for most.
    pub fn from_str_lenient(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "markdown" | "md" => Self::Markdown,
            "raw" => Self::Raw,
            _ => Self::Code,
        }
    }

    /// The nbformat `cell_type` string this maps to.
    fn as_nb_str(self) -> &'static str {
        match self {
            Self::Code => "code",
            Self::Markdown => "markdown",
            Self::Raw => "raw",
        }
    }
}

/// Read and parse the `.ipynb` at `path`, returning a [`CellInfo`] summary of
/// every cell (Phase 21.4 `notebook_read`).
///
/// A missing file, a file that is not valid JSON, or a JSON document whose
/// `nbformat` is not `4` is a clean [`DataError`] — never a panic. (A genuinely
/// empty `cells` array is a *valid* notebook and yields an empty `Vec`.)
pub fn read_notebook(path: &Path) -> Result<Vec<CellInfo>> {
    let nb = load_notebook(path)?;
    let cells = nb.get("cells").and_then(Value::as_array);
    let cells = match cells {
        Some(c) => c,
        // nbformat requires a `cells` array; its absence is a malformed notebook.
        None => {
            return Err(DataError::InvalidNotebook(format!(
                "{}: nbformat document has no `cells` array",
                path.display()
            )))
        }
    };

    Ok(cells
        .iter()
        .enumerate()
        .map(|(index, cell)| {
            let cell_type = cell
                .get("cell_type")
                .and_then(Value::as_str)
                .unwrap_or("code")
                .to_string();
            let source = join_source(cell.get("source"));
            let has_output = cell
                .get("outputs")
                .and_then(Value::as_array)
                .map(|o| !o.is_empty())
                .unwrap_or(false);
            CellInfo {
                index,
                cell_type,
                source,
                has_output,
            }
        })
        .collect())
}

/// Build a fresh nbformat cell from `source` and insert it into the notebook at
/// `path`, **proposing** it without executing (Phase 21.4 `notebook_insert_cell`).
///
/// `index` is the zero-based position to insert at; `None` appends. An index past
/// the end clamps to an append (so the cell is never dropped). A `code` cell is
/// inserted with `execution_count: null` and `outputs: []` — it has *not* run,
/// and a reviewer can tell at a glance. If `path` does not yet exist, a minimal
/// valid nbformat-4 notebook is created first. The write is [atomic](atomic_write).
///
/// Returns the index the cell actually landed at.
pub fn insert_cell(
    path: &Path,
    source: &str,
    index: Option<usize>,
    cell_type: CellType,
) -> Result<usize> {
    let mut nb = load_or_create(path)?;
    let cell = build_cell(source, cell_type, None, &[]);
    let at = insert_into_cells(&mut nb, cell, index)?;
    atomic_write(path, &nb)?;
    Ok(at)
}

/// Append a **code** cell carrying `source` and the given already-nbformat-shaped
/// `outputs` to the notebook at `path` (Phase 21.4 `notebook_persist_executed_cell`).
///
/// This is the low-level primitive `run_cell(persist_to)` calls after a
/// successful run: the `outputs` are the converted [`CellRun`](crate::kernel::CellRun)
/// (stream/execute_result/display_data/error Values) so the persisted notebook
/// renders exactly what the cell produced — a faithful artifact. If `path` does
/// not exist, a minimal nbformat-4 notebook is created first. The write is
/// [atomic](atomic_write).
///
/// Returns the appended index.
pub fn persist_cell(path: &Path, source: &str, outputs: Vec<Value>) -> Result<usize> {
    let mut nb = load_or_create(path)?;
    let cell = build_cell(source, CellType::Code, None, &outputs);
    let at = insert_into_cells(&mut nb, cell, None)?;
    atomic_write(path, &nb)?;
    Ok(at)
}

// ── pure helpers (no I/O — unit-testable in isolation) ──────────────────────

/// Parse raw `.ipynb` bytes into a notebook [`Value`], validating that it is an
/// nbformat-4 JSON object. Pulled out (no filesystem touch) so the parse +
/// validation rules are unit-testable against in-memory strings.
fn parse_notebook(bytes: &[u8], origin: &str) -> Result<Value> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|e| DataError::InvalidNotebook(format!("{origin}: not valid JSON: {e}")))?;
    if !value.is_object() {
        return Err(DataError::InvalidNotebook(format!(
            "{origin}: top-level value is not a JSON object"
        )));
    }
    match value.get("nbformat").and_then(Value::as_i64) {
        Some(NBFORMAT_MAJOR) => Ok(value),
        Some(other) => Err(DataError::InvalidNotebook(format!(
            "{origin}: nbformat {other} is unsupported (this build reads nbformat {NBFORMAT_MAJOR})"
        ))),
        None => Err(DataError::InvalidNotebook(format!(
            "{origin}: missing or non-integer `nbformat` field"
        ))),
    }
}

/// A minimal but fully valid empty nbformat-4 notebook (`cells: []`). Used when
/// `persist_to` / `insert_cell` targets a path that does not yet exist.
fn empty_notebook() -> Value {
    json!({
        "cells": [],
        "metadata": {},
        "nbformat": NBFORMAT_MAJOR,
        "nbformat_minor": NBFORMAT_MINOR,
    })
}

/// Build a fresh nbformat cell.
///
/// A `code` cell carries `execution_count` (the supplied value, or `null` when
/// `None`) and an `outputs` array (the supplied outputs, or empty); markdown and
/// raw cells carry neither (those fields are invalid for them in nbformat). Every
/// cell gets a fresh `id` (nbformat 4.5) and an empty `metadata` object, and the
/// `source` is stored as a single string (a valid nbformat shape — readers accept
/// both a string and a line-array).
fn build_cell(
    source: &str,
    cell_type: CellType,
    execution_count: Option<i64>,
    outputs: &[Value],
) -> Value {
    let mut cell = Map::new();
    cell.insert("cell_type".into(), json!(cell_type.as_nb_str()));
    cell.insert("id".into(), json!(new_cell_id()));
    cell.insert("metadata".into(), json!({}));
    cell.insert("source".into(), json!(source));
    if matches!(cell_type, CellType::Code) {
        cell.insert(
            "execution_count".into(),
            execution_count.map(|n| json!(n)).unwrap_or(Value::Null),
        );
        cell.insert("outputs".into(), Value::Array(outputs.to_vec()));
    }
    Value::Object(cell)
}

/// Insert `cell` into the notebook's `cells` array at `index` (or append when
/// `None`); an out-of-range index clamps to an append. Returns the landed index.
///
/// Errors only if the document has no `cells` array (a malformed notebook that
/// slipped past validation) — which cannot happen for a notebook this module
/// created or loaded through [`load_or_create`].
fn insert_into_cells(nb: &mut Value, cell: Value, index: Option<usize>) -> Result<usize> {
    let cells = nb
        .get_mut("cells")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| {
            DataError::InvalidNotebook("notebook has no `cells` array to insert into".to_string())
        })?;
    let at = match index {
        Some(i) if i < cells.len() => i,
        // None, or an index at/past the end, appends (never drops the cell).
        _ => cells.len(),
    };
    cells.insert(at, cell);
    Ok(at)
}

/// Join an nbformat `source` field (a string, an array of line-strings, or
/// absent) into one string. nbformat stores source either way; readers must
/// accept both, so we normalize on read.
fn join_source(source: Option<&Value>) -> String {
    match source {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(lines)) => lines
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .concat(),
        _ => String::new(),
    }
}

/// A fresh nbformat 4.5 cell `id` (a v4 UUID string). Stamped on every cell we
/// build so the notebook conforms to nbformat 4.5 and cell ids stay unique.
fn new_cell_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

// ── filesystem helpers ──────────────────────────────────────────────────────

/// Load + parse the notebook at `path`, surfacing a missing file as a clean
/// [`DataError`] (not a panic). Used by [`read_notebook`], which must *not*
/// silently create a notebook for a path the human did not ask to read.
fn load_notebook(path: &Path) -> Result<Value> {
    let bytes = std::fs::read(path)?;
    parse_notebook(&bytes, &path.display().to_string())
}

/// Load the notebook at `path`, or return a fresh empty nbformat-4 notebook if
/// the file does not exist (so `persist_to` / `insert_cell` work on a brand-new
/// path). A file that *exists* but is corrupt or non-nbformat-4 is still a clean
/// error — only `NotFound` falls through to the empty notebook.
fn load_or_create(path: &Path) -> Result<Value> {
    match std::fs::read(path) {
        Ok(bytes) => parse_notebook(&bytes, &path.display().to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(empty_notebook()),
        Err(e) => Err(DataError::Io(e)),
    }
}

/// Serialize `nb` to `path` **atomically**: write a temp file in the same
/// directory, then `rename` it over the target.
///
/// Same-directory placement guarantees the `rename` is within one filesystem (so
/// it is atomic and cannot fail with a cross-device error), and a serialization
/// or partial-write failure leaves the original notebook untouched — a reader
/// never sees a half-written `.ipynb`. This mirrors the conversation store's
/// durable-write discipline (Phase 21.4 — the notebook is an artifact under
/// review; a torn write would be a corrupt artifact).
fn atomic_write(path: &Path, nb: &Value) -> Result<()> {
    // Pretty JSON with a trailing newline: nbformat tools and JupyterLab write
    // pretty-printed notebooks, and a trailing newline keeps `git diff` clean.
    let mut serialized = serde_json::to_vec_pretty(nb)
        .map_err(|e| DataError::InvalidNotebook(format!("failed to serialize notebook: {e}")))?;
    serialized.push(b'\n');

    let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
    if let Some(dir) = dir {
        std::fs::create_dir_all(dir)?;
    }

    // Temp file in the SAME directory as the target so the rename stays
    // intra-filesystem. `NamedTempFile::persist` does the atomic rename.
    let mut tmp = match dir {
        Some(dir) => tempfile::NamedTempFile::new_in(dir)?,
        None => tempfile::NamedTempFile::new_in(".")?,
    };
    use std::io::Write as _;
    tmp.write_all(&serialized)?;
    tmp.flush()?;
    tmp.persist(path)
        .map_err(|e| DataError::Io(e.error))
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A 2-cell nbformat-4 notebook fixture: a markdown cell and a code cell
    /// (the code cell carries one stream output, so `has_output` is true).
    fn two_cell_ipynb() -> String {
        serde_json::to_string_pretty(&json!({
            "cells": [
                {
                    "cell_type": "markdown",
                    "metadata": {},
                    "source": "# Title\n"
                },
                {
                    "cell_type": "code",
                    "execution_count": 1,
                    "metadata": {},
                    "source": ["print(", "'hi')"],
                    "outputs": [
                        { "output_type": "stream", "name": "stdout", "text": "hi\n" }
                    ]
                }
            ],
            "metadata": {},
            "nbformat": 4,
            "nbformat_minor": 5
        }))
        .unwrap()
    }

    /// Write `contents` to `path.ipynb` under a fresh tempdir; returns the dir
    /// (kept alive by the caller) and the file path.
    fn write_fixture(contents: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nb.ipynb");
        std::fs::write(&path, contents).unwrap();
        (dir, path)
    }

    // ── read ────────────────────────────────────────────────────────────────

    #[test]
    fn read_two_cell_notebook_summarizes_each_cell() {
        let (_dir, path) = write_fixture(&two_cell_ipynb());
        let cells = read_notebook(&path).unwrap();
        assert_eq!(cells.len(), 2);

        assert_eq!(cells[0].index, 0);
        assert_eq!(cells[0].cell_type, "markdown");
        assert_eq!(cells[0].source, "# Title\n");
        assert!(!cells[0].has_output, "markdown has no outputs");

        assert_eq!(cells[1].index, 1);
        assert_eq!(cells[1].cell_type, "code");
        // The line-array source is joined.
        assert_eq!(cells[1].source, "print('hi')");
        assert!(cells[1].has_output, "the code cell has a stream output");
    }

    #[test]
    fn read_missing_file_is_clean_error_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.ipynb");
        let err = read_notebook(&path).unwrap_err();
        assert!(matches!(err, DataError::Io(_)), "got {err:?}");
    }

    #[test]
    fn read_corrupt_json_is_invalid_notebook_error() {
        let (_dir, path) = write_fixture("{ this is not json ");
        let err = read_notebook(&path).unwrap_err();
        assert!(matches!(err, DataError::InvalidNotebook(_)), "got {err:?}");
    }

    #[test]
    fn read_non_nbformat4_is_invalid_notebook_error() {
        // Valid JSON, valid-looking notebook, but nbformat 3.
        let nb = json!({ "cells": [], "metadata": {}, "nbformat": 3, "nbformat_minor": 0 });
        let (_dir, path) = write_fixture(&nb.to_string());
        let err = read_notebook(&path).unwrap_err();
        match err {
            DataError::InvalidNotebook(msg) => assert!(msg.contains("nbformat 3"), "{msg}"),
            other => panic!("expected InvalidNotebook, got {other:?}"),
        }
    }

    #[test]
    fn read_missing_nbformat_field_is_error() {
        let nb = json!({ "cells": [], "metadata": {} });
        let (_dir, path) = write_fixture(&nb.to_string());
        let err = read_notebook(&path).unwrap_err();
        assert!(matches!(err, DataError::InvalidNotebook(_)), "got {err:?}");
    }

    #[test]
    fn read_nbformat4_without_cells_array_is_error() {
        let nb = json!({ "metadata": {}, "nbformat": 4, "nbformat_minor": 5 });
        let (_dir, path) = write_fixture(&nb.to_string());
        let err = read_notebook(&path).unwrap_err();
        match err {
            DataError::InvalidNotebook(msg) => assert!(msg.contains("cells"), "{msg}"),
            other => panic!("expected InvalidNotebook, got {other:?}"),
        }
    }

    // ── insert ────────────────────────────────────────────────────────────────

    #[test]
    fn insert_at_index_zero_prepends() {
        let (_dir, path) = write_fixture(&two_cell_ipynb());
        let at = insert_cell(&path, "x = 1", Some(0), CellType::Code).unwrap();
        assert_eq!(at, 0);
        let cells = read_notebook(&path).unwrap();
        assert_eq!(cells.len(), 3);
        assert_eq!(cells[0].cell_type, "code");
        assert_eq!(cells[0].source, "x = 1");
        // The proposed cell has NOT run: no outputs.
        assert!(!cells[0].has_output);
        // The original first cell is now at index 1.
        assert_eq!(cells[1].cell_type, "markdown");
    }

    #[test]
    fn insert_in_the_middle_lands_between() {
        let (_dir, path) = write_fixture(&two_cell_ipynb());
        let at = insert_cell(&path, "# note", Some(1), CellType::Markdown).unwrap();
        assert_eq!(at, 1);
        let cells = read_notebook(&path).unwrap();
        assert_eq!(cells.len(), 3);
        assert_eq!(cells[1].cell_type, "markdown");
        assert_eq!(cells[1].source, "# note");
    }

    #[test]
    fn insert_none_appends() {
        let (_dir, path) = write_fixture(&two_cell_ipynb());
        let at = insert_cell(&path, "y = 2", None, CellType::Code).unwrap();
        assert_eq!(at, 2, "append lands at the end");
        let cells = read_notebook(&path).unwrap();
        assert_eq!(cells.len(), 3);
        assert_eq!(cells[2].source, "y = 2");
    }

    #[test]
    fn insert_index_past_end_clamps_to_append() {
        let (_dir, path) = write_fixture(&two_cell_ipynb());
        let at = insert_cell(&path, "z = 3", Some(999), CellType::Code).unwrap();
        assert_eq!(at, 2, "an out-of-range index clamps to an append");
        let cells = read_notebook(&path).unwrap();
        assert_eq!(cells.len(), 3);
        assert_eq!(cells[2].source, "z = 3");
    }

    #[test]
    fn insert_creates_notebook_on_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        // A nested path that does not exist yet: the parent dir is created too.
        let path = dir.path().join("sub").join("fresh.ipynb");
        let at = insert_cell(&path, "first()", None, CellType::Code).unwrap();
        assert_eq!(at, 0);
        let cells = read_notebook(&path).unwrap();
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].source, "first()");
        assert_eq!(cells[0].cell_type, "code");
    }

    #[test]
    fn insert_code_cell_has_nbformat_code_scaffolding() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nb.ipynb");
        insert_cell(&path, "code()", None, CellType::Code).unwrap();
        // Inspect the raw JSON: a code cell must carry execution_count:null + [].
        let raw: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let cell = &raw["cells"][0];
        assert_eq!(cell["cell_type"], "code");
        assert!(
            cell["execution_count"].is_null(),
            "proposed cell has not run"
        );
        assert_eq!(cell["outputs"], json!([]));
        assert!(cell["id"].is_string(), "nbformat 4.5 cell id");
    }

    #[test]
    fn insert_markdown_cell_omits_code_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nb.ipynb");
        insert_cell(&path, "# heading", None, CellType::Markdown).unwrap();
        let raw: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let cell = &raw["cells"][0];
        assert_eq!(cell["cell_type"], "markdown");
        // execution_count / outputs are invalid on markdown cells — absent.
        assert!(cell.get("execution_count").is_none());
        assert!(cell.get("outputs").is_none());
    }

    // ── persist ─────────────────────────────────────────────────────────────

    #[test]
    fn persist_cell_appends_code_cell_with_source_and_outputs() {
        let (_dir, path) = write_fixture(&two_cell_ipynb());
        let outputs = vec![
            json!({ "output_type": "stream", "name": "stdout", "text": "42\n" }),
            json!({
                "output_type": "execute_result",
                "data": { "text/plain": "42" },
                "metadata": {},
                "execution_count": 7
            }),
        ];
        let at = persist_cell(&path, "print(42); 42", outputs.clone()).unwrap();
        assert_eq!(at, 2, "persist appends after the two fixture cells");

        // Round-trip through read for the summary view.
        let cells = read_notebook(&path).unwrap();
        assert_eq!(cells.len(), 3);
        assert_eq!(cells[2].cell_type, "code");
        assert_eq!(cells[2].source, "print(42); 42");
        assert!(
            cells[2].has_output,
            "persisted outputs make has_output true"
        );

        // Inspect the raw cell: the outputs match the nbformat we passed in.
        let raw: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let cell = &raw["cells"][2];
        assert_eq!(cell["cell_type"], "code");
        assert_eq!(cell["outputs"], json!(outputs));
        assert!(
            cell["execution_count"].is_null(),
            "persist leaves the cell-level execution_count null; the count lives \
             inside the execute_result output"
        );
    }

    #[test]
    fn persist_cell_creates_notebook_on_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.ipynb");
        let at = persist_cell(&path, "1 + 1", vec![]).unwrap();
        assert_eq!(at, 0);
        let cells = read_notebook(&path).unwrap();
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].source, "1 + 1");
        // No outputs passed → empty outputs array, has_output false.
        assert!(!cells[0].has_output);
    }

    // ── round-trip + atomicity ──────────────────────────────────────────────

    #[test]
    fn round_trip_read_insert_read_is_stable() {
        let (_dir, path) = write_fixture(&two_cell_ipynb());
        let before = read_notebook(&path).unwrap();
        assert_eq!(before.len(), 2);
        insert_cell(&path, "mid()", Some(1), CellType::Code).unwrap();
        let after = read_notebook(&path).unwrap();
        assert_eq!(after.len(), 3);
        // The two original cells survive unchanged around the insert.
        assert_eq!(after[0], before[0]);
        assert_eq!(after[0].index, 0);
        assert_eq!(after[2].source, before[1].source);
    }

    #[test]
    fn written_notebook_is_valid_nbformat4_and_reparses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nb.ipynb");
        persist_cell(&path, "x", vec![]).unwrap();
        let raw: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(raw["nbformat"], NBFORMAT_MAJOR);
        assert_eq!(raw["nbformat_minor"], NBFORMAT_MINOR);
        assert!(raw["cells"].is_array());
        assert!(raw["metadata"].is_object());
        // And the parse-validation accepts what we wrote.
        assert!(parse_notebook(&std::fs::read(&path).unwrap(), "nb").is_ok());
    }

    #[test]
    fn written_notebook_ends_with_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nb.ipynb");
        persist_cell(&path, "x", vec![]).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            *bytes.last().unwrap(),
            b'\n',
            "trailing newline keeps git diffs clean"
        );
    }

    #[test]
    fn atomic_write_leaves_no_partial_file_and_no_stray_temp() {
        // A successful write leaves exactly one file (the notebook) in the dir —
        // the temp file was renamed away, not left behind.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nb.ipynb");
        persist_cell(&path, "a", vec![]).unwrap();
        persist_cell(&path, "b", vec![]).unwrap();
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries.len(), 1, "only the notebook remains: {entries:?}");
        assert_eq!(entries[0], std::ffi::OsStr::new("nb.ipynb"));
    }

    #[test]
    fn atomic_write_does_not_clobber_original_on_existing_corrupt_target() {
        // If the target exists but is corrupt, load_or_create errors BEFORE any
        // write — the corrupt file is left exactly as it was (not truncated).
        let (_dir, path) = write_fixture("{ broken json");
        let err = persist_cell(&path, "x", vec![]).unwrap_err();
        assert!(matches!(err, DataError::InvalidNotebook(_)), "got {err:?}");
        // The original bytes are untouched.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ broken json");
    }

    // ── pure-helper unit tests ──────────────────────────────────────────────

    #[test]
    fn join_source_handles_string_array_and_absent() {
        assert_eq!(join_source(Some(&json!("a\nb"))), "a\nb");
        assert_eq!(join_source(Some(&json!(["a", "b", "c"]))), "abc");
        assert_eq!(join_source(None), "");
        assert_eq!(
            join_source(Some(&json!(42))),
            "",
            "non-string/array → empty"
        );
    }

    #[test]
    fn cell_type_from_str_is_lenient() {
        assert_eq!(CellType::from_str_lenient("code"), CellType::Code);
        assert_eq!(CellType::from_str_lenient("markdown"), CellType::Markdown);
        assert_eq!(CellType::from_str_lenient("MD"), CellType::Markdown);
        assert_eq!(CellType::from_str_lenient(" Raw "), CellType::Raw);
        // Anything unrecognized → Code (the executable default).
        assert_eq!(CellType::from_str_lenient("gibberish"), CellType::Code);
        assert_eq!(CellType::from_str_lenient(""), CellType::Code);
    }

    #[test]
    fn build_cell_code_vs_markdown_shape() {
        let code = build_cell("c()", CellType::Code, Some(3), &[json!({"o": 1})]);
        assert_eq!(code["cell_type"], "code");
        assert_eq!(code["execution_count"], 3);
        assert_eq!(code["outputs"], json!([{"o": 1}]));

        let md = build_cell("# h", CellType::Markdown, None, &[]);
        assert_eq!(md["cell_type"], "markdown");
        assert!(md.get("execution_count").is_none());
        assert!(md.get("outputs").is_none());
    }

    #[test]
    fn parse_notebook_rejects_non_object_top_level() {
        let err = parse_notebook(b"[1, 2, 3]", "arr").unwrap_err();
        assert!(matches!(err, DataError::InvalidNotebook(_)));
    }

    #[test]
    fn insert_into_cells_errors_without_cells_array() {
        let mut nb = json!({ "nbformat": 4 });
        let err = insert_into_cells(&mut nb, json!({}), None).unwrap_err();
        assert!(matches!(err, DataError::InvalidNotebook(_)));
    }
}
