//! Live-kernel co-pilot transport (Phase 21.3).
//!
//! The [Centaur Data Scientist](../../../../docs/design/centaur-data-scientist.md)
//! step 21.3: the agent attaches to the human's **already-running** Jupyter
//! server and runs cells, reading back stdout/stderr, rich `execute_result` /
//! `display_data` payloads, and PNG plots — every action visible and reviewable
//! (AI-use type 3, the human stays on top).
//!
//! ## Scope (21.3 only)
//!
//! `kernel_attach` + `run_cell`. Notebook persistence is 21.4, dataframe
//! introspection is 21.5, interrupt/restart is 21.7. This module ships:
//!
//! - the output value types ([`CellRun`] and friends), serde-serializable so the
//!   MCP adapter can ferry a run summary across the JSON-RPC boundary;
//! - the [`KernelClient`] trait — the seam an MCP handler (or a test mock kernel)
//!   drives, identical in spirit to the [`DataStore`](crate::DataStore) seam;
//! - the **pure iopub accumulator** ([`Accumulator`]) — the testable heart: it
//!   folds a sequence of Jupyter iopub messages into a [`CellRun`] with no I/O
//!   beyond a single injected PNG sink, so the protocol logic is unit-tested
//!   against captured message fixtures without a live kernel.
//!
//! The thin REST + websocket implementation lives in [`rest`]; it parses bytes
//! off the wire and feeds them straight into the [`Accumulator`], so all the
//! interesting logic stays here and stays pure.
//!
//! ## Transport (Option A1, pure-Rust — no libpython)
//!
//! The client talks to the Jupyter **Server** REST API (`/api/kernels`) plus the
//! per-kernel **channels websocket** (`/api/kernels/<id>/channels`). No ZMQ, no
//! HMAC, no embedded interpreter — the server stays a lean Rust binary (the
//! rejected Option B linked libpython at runtime). PyO3 is orthogonal: it exposes
//! Rust *into* the notebook, it does not drive the kernel.
//!
//! [`DataStore`]: crate::DataStore
//! [`CellRun`]: crate::kernel::CellRun
//! [`Accumulator`]: crate::kernel::Accumulator
//! [`KernelClient`]: crate::kernel::KernelClient
//! [`rest`]: crate::kernel::rest

pub mod rest;

use std::path::{Path, PathBuf};

use serde::Serialize;

/// One MIME-typed text payload from an `execute_result` or `display_data` bundle
/// (e.g. `text/plain` for a repr, `text/html` for a styled table). Binary image
/// payloads are **not** carried here — they are decoded to disk and recorded as
/// an [`ImageOutput`] (Centaur principle: never inline a plot blob).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DisplayItem {
    /// The MIME type of the payload (`text/plain`, `text/html`, …).
    pub mime: String,
    /// The textual payload for that MIME type.
    pub text: String,
}

/// A PNG plot decoded off an output bundle and written to the plots directory.
///
/// The bytes are **never** inlined into the run summary — only the on-disk path
/// plus an honest size description travel back to the model (rich rendering is
/// deferred to gilamonster). See [`docs/design/centaur-data-scientist.md`](../../../../docs/design/centaur-data-scientist.md).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImageOutput {
    /// Filesystem path of the written image (`<plots>/cell-<n>-<uuid>.png`).
    pub path: PathBuf,
    /// The MIME type of the decoded image (`image/png`).
    pub mime: String,
    /// Pixel width, if the bundle carried `image/png` `metadata.width`.
    pub width: Option<u64>,
    /// Pixel height, if the bundle carried `image/png` `metadata.height`.
    pub height: Option<u64>,
}

/// A Python exception raised while executing a cell (the iopub `error` message).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KernelError {
    /// The exception class name (`NameError`, `ValueError`, …).
    pub ename: String,
    /// The exception value / message.
    pub evalue: String,
    /// The formatted traceback, one entry per line (may carry ANSI colour).
    pub traceback: Vec<String>,
}

/// The full result of running one cell: the folded view of every iopub message
/// the kernel emitted for our `execute_request`, up to its terminating
/// `status: idle`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CellRun {
    /// Concatenated `stream` output on the `stdout` channel.
    pub stdout: String,
    /// Concatenated `stream` output on the `stderr` channel.
    pub stderr: String,
    /// Non-image text payloads from `execute_result` / `display_data` bundles.
    pub results: Vec<DisplayItem>,
    /// PNG plots decoded to disk (path + honest size; never inlined).
    pub images: Vec<ImageOutput>,
    /// The exception, if the cell raised one (`error` message).
    pub error: Option<KernelError>,
    /// The kernel's `execution_count` for this cell, if it reported one.
    pub execution_count: Option<i64>,
}

impl CellRun {
    /// `true` if the cell raised an exception.
    pub fn failed(&self) -> bool {
        self.error.is_some()
    }
}

/// A sink for a decoded PNG: given the cell's `execution_count` (or `None`) and
/// the raw image bytes, persist it and return the path it was written to.
///
/// Injected into the [`Accumulator`] so the protocol-folding logic stays pure
/// and unit-testable — tests pass a sink that writes into a `tempfile::TempDir`
/// (or captures the bytes), production passes [`DirPngSink`] over the real
/// `.newt-data/plots/` directory.
///
/// `Send` is a supertrait so the [`Accumulator`] (which holds `&mut dyn PngSink`)
/// can be held across the `.await` points inside the websocket read loop — the
/// `async_trait` [`KernelClient::run_cell`] future must be `Send`.
pub trait PngSink: Send {
    /// Persist `bytes` (a decoded PNG) for the cell numbered `execution_count`,
    /// returning the path written.
    fn write_png(&mut self, execution_count: Option<i64>, bytes: &[u8]) -> anyhow::Result<PathBuf>;
}

/// The production [`PngSink`]: writes `cell-<n>-<uuid>.png` under a plots
/// directory (creating it if needed). `<n>` is the cell's `execution_count`
/// (or `0` when the kernel did not report one); `<uuid>` is a fresh v4 UUID so
/// repeated runs of the same cell never clobber one another.
#[cfg(feature = "kernel")]
pub struct DirPngSink {
    dir: PathBuf,
}

#[cfg(feature = "kernel")]
impl DirPngSink {
    /// A sink that writes PNGs into `dir` (created on first write).
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }
}

#[cfg(feature = "kernel")]
impl PngSink for DirPngSink {
    fn write_png(&mut self, execution_count: Option<i64>, bytes: &[u8]) -> anyhow::Result<PathBuf> {
        std::fs::create_dir_all(&self.dir)?;
        let n = execution_count.unwrap_or(0);
        let name = format!("cell-{n}-{}.png", uuid::Uuid::new_v4());
        let path = self.dir.join(name);
        std::fs::write(&path, bytes)?;
        Ok(path)
    }
}

/// The MIME type of a PNG image bundle entry.
pub const MIME_PNG: &str = "image/png";

/// A fold of Jupyter iopub messages into a [`CellRun`] — the **pure, testable
/// heart** of the live-kernel transport (Phase 21.3).
///
/// The accumulator owns no sockets and does no network I/O. It is fed
/// `serde_json::Value` iopub messages one at a time via [`Accumulator::feed`]
/// (the transport in [`rest`] reads bytes off the websocket and calls this); the
/// only side effect is the injected [`PngSink`], so the entire protocol-folding
/// surface is unit-tested against captured message fixtures.
///
/// ## What it folds (the iopub `msg_type`s)
///
/// - `stream` → appends `content.text` to [`CellRun::stdout`] or `stderr` by
///   `content.name`.
/// - `execute_result` / `display_data` → splits `content.data`: `image/png`
///   (base64) is decoded and handed to the [`PngSink`] (recorded as an
///   [`ImageOutput`], never inlined); every other MIME entry becomes a
///   [`DisplayItem`]. `execute_result` also records `content.execution_count`.
/// - `error` → records [`CellRun::error`] from `ename` / `evalue` / `traceback`.
/// - `status` → when `content.execution_state == "idle"`, marks the run
///   terminated (see [`Accumulator::is_idle`]); the transport stops reading.
///
/// Any other `msg_type` (e.g. `execute_input`) is ignored — it carries no output.
pub struct Accumulator<'a> {
    run: CellRun,
    sink: &'a mut dyn PngSink,
    idle: bool,
}

impl<'a> Accumulator<'a> {
    /// Start a fresh accumulator writing PNGs through `sink`.
    pub fn new(sink: &'a mut dyn PngSink) -> Self {
        Self {
            run: CellRun::default(),
            sink,
            idle: false,
        }
    }

    /// `true` once an `idle` `status` message has been folded — the signal that
    /// the cell finished and the transport may stop reading iopub. (Matching the
    /// `idle` to *our* request's `msg_id` is the transport's job, in [`rest`];
    /// the accumulator only sees messages already filtered to our cell.)
    pub fn is_idle(&self) -> bool {
        self.idle
    }

    /// Fold one iopub message (a `serde_json::Value` with `header.msg_type` and
    /// `content`) into the accumulating [`CellRun`].
    ///
    /// Unknown or malformed messages are folded as no-ops (a kernel may emit
    /// vendor message types we do not model); a PNG that fails to decode or
    /// write surfaces as an `Err` so the transport can report it honestly.
    pub fn feed(&mut self, msg: &serde_json::Value) -> anyhow::Result<()> {
        let msg_type = msg
            .get("header")
            .and_then(|h| h.get("msg_type"))
            .and_then(|t| t.as_str())
            .or_else(|| msg.get("msg_type").and_then(|t| t.as_str()))
            .unwrap_or("");
        let content = msg.get("content").unwrap_or(&serde_json::Value::Null);

        match msg_type {
            "stream" => self.fold_stream(content),
            "execute_result" => self.fold_output_bundle(content, true)?,
            "display_data" => self.fold_output_bundle(content, false)?,
            "error" => self.fold_error(content),
            "status" => self.fold_status(content),
            _ => {}
        }
        Ok(())
    }

    /// Consume the accumulator, returning the folded [`CellRun`].
    pub fn finish(self) -> CellRun {
        self.run
    }

    fn fold_stream(&mut self, content: &serde_json::Value) {
        let text = content.get("text").and_then(|t| t.as_str()).unwrap_or("");
        match content.get("name").and_then(|n| n.as_str()) {
            Some("stderr") => self.run.stderr.push_str(text),
            // `stdout`, an unknown stream name, or a missing name all default to
            // stdout — a kernel only ever names these two streams.
            _ => self.run.stdout.push_str(text),
        }
    }

    fn fold_output_bundle(
        &mut self,
        content: &serde_json::Value,
        is_execute_result: bool,
    ) -> anyhow::Result<()> {
        if is_execute_result {
            if let Some(n) = content.get("execution_count").and_then(|c| c.as_i64()) {
                self.run.execution_count = Some(n);
            }
        }

        let data = match content.get("data").and_then(|d| d.as_object()) {
            Some(obj) => obj,
            None => return Ok(()),
        };

        // Decode a PNG (if present) once, recording the size from the bundle's
        // metadata when the kernel reported it.
        if let Some(png_b64) = data.get(MIME_PNG).and_then(|v| v.as_str()) {
            let bytes = decode_png_base64(png_b64)?;
            let (width, height) = png_dimensions_from_metadata(content);
            let path = self
                .sink
                .write_png(self.run.execution_count, &bytes)
                .map_err(|e| anyhow::anyhow!("failed to write PNG plot: {e}"))?;
            self.run.images.push(ImageOutput {
                path,
                mime: MIME_PNG.to_string(),
                width,
                height,
            });
        }

        // Every non-image MIME entry becomes a text DisplayItem (sorted so the
        // result order is deterministic for tests and review).
        let mut items: Vec<DisplayItem> = data
            .iter()
            .filter(|(mime, _)| mime.as_str() != MIME_PNG)
            .filter_map(|(mime, value)| {
                display_text(value).map(|text| DisplayItem {
                    mime: mime.clone(),
                    text,
                })
            })
            .collect();
        items.sort_by(|a, b| a.mime.cmp(&b.mime));
        self.run.results.extend(items);
        Ok(())
    }

    fn fold_error(&mut self, content: &serde_json::Value) {
        let ename = content
            .get("ename")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let evalue = content
            .get("evalue")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let traceback = content
            .get("traceback")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|line| line.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        self.run.error = Some(KernelError {
            ename,
            evalue,
            traceback,
        });
    }

    fn fold_status(&mut self, content: &serde_json::Value) {
        if content
            .get("execution_state")
            .and_then(|s| s.as_str())
            .map(|s| s == "idle")
            .unwrap_or(false)
        {
            self.idle = true;
        }
    }
}

/// Render a `data`-bundle MIME value as text. Jupyter sends `text/plain` and
/// `text/html` as either a single string or an array of strings (one per line);
/// fold both shapes into one string. A non-string value is JSON-serialized so
/// nothing is silently dropped.
fn display_text(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(lines) => Some(
            lines
                .iter()
                .filter_map(|l| l.as_str())
                .collect::<Vec<_>>()
                .concat(),
        ),
        serde_json::Value::Null => None,
        other => Some(other.to_string()),
    }
}

/// Pull `(width, height)` out of an output bundle's
/// `metadata["image/png"]{width,height}`, if the kernel reported them (matplotlib
/// does). Both are optional and independent.
fn png_dimensions_from_metadata(content: &serde_json::Value) -> (Option<u64>, Option<u64>) {
    let meta = content
        .get("metadata")
        .and_then(|m| m.get(MIME_PNG))
        .or_else(|| content.get("metadata"));
    let width = meta.and_then(|m| m.get("width")).and_then(|w| w.as_u64());
    let height = meta.and_then(|m| m.get("height")).and_then(|h| h.as_u64());
    (width, height)
}

/// Decode a base64 `image/png` payload into raw PNG bytes.
///
/// Jupyter base64-encodes binary bundle entries and may wrap them with embedded
/// newlines (the classic notebook does); strip whitespace before decoding. Kept
/// here (not in [`rest`]) so it is unit-tested at the pure level.
fn decode_png_base64(b64: &str) -> anyhow::Result<Vec<u8>> {
    let cleaned: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
    base64_decode(&cleaned).map_err(|e| anyhow::anyhow!("invalid base64 in image/png payload: {e}"))
}

/// Standard base64 (RFC 4648) decode. A tiny self-contained decoder mirroring the
/// engine's `base64_encode` so the pure accumulator carries no base64 dependency
/// of its own at the *parse* layer (this crate deliberately does **not** depend on
/// the `base64` crate at all — keeping the decode here self-contained keeps the
/// accumulator's unit tests buildable on the same minimal surface).
fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    fn val(c: u8) -> Result<u8, String> {
        match c {
            b'A'..=b'Z' => Ok(c - b'A'),
            b'a'..=b'z' => Ok(c - b'a' + 26),
            b'0'..=b'9' => Ok(c - b'0' + 52),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => Err(format!("invalid base64 byte: 0x{c:02x}")),
        }
    }
    let bytes = s.as_bytes();
    if bytes.len() % 4 != 0 {
        return Err(format!(
            "base64 length {} is not a multiple of 4",
            bytes.len()
        ));
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        let pad = chunk.iter().rev().take_while(|&&b| b == b'=').count();
        let b0 = val(chunk[0])?;
        let b1 = val(chunk[1])?;
        let b2 = if pad >= 2 { 0 } else { val(chunk[2])? };
        let b3 = if pad >= 1 { 0 } else { val(chunk[3])? };
        out.push((b0 << 2) | (b1 >> 4));
        if pad < 2 {
            out.push((b1 << 4) | (b2 >> 2));
        }
        if pad < 1 {
            out.push((b2 << 6) | b3);
        }
    }
    Ok(out)
}

/// The agent's client to a live Jupyter kernel (Phase 21.3).
///
/// The seam the MCP `run_cell` handler drives — and the seam a test mock kernel
/// substitutes so the handler logic is exercised without a live kernel. The only
/// operation in 21.3 scope is [`run_cell`](Self::run_cell); interrupt/restart
/// arrive in 21.7 behind the same trait.
///
/// `Send` (not `Sync`): the MCP server stores the active client behind a
/// `tokio::Mutex`, which serializes access, so the implementation only needs to
/// move across `.await` points, not be shared by reference.
#[cfg(feature = "kernel")]
#[async_trait::async_trait]
pub trait KernelClient: Send {
    /// Execute `code` on the kernel and return the folded [`CellRun`] (every
    /// iopub message up to the terminating `status: idle`). Transport or
    /// protocol failures are `Err`; a cell that *raises* is `Ok` with
    /// [`CellRun::error`] set (the exception is data, not a transport fault).
    async fn run_cell(&self, code: &str) -> anyhow::Result<CellRun>;
}

/// Resolve the plots directory under a data directory: `<data_dir>/plots`. The
/// caller passes the `.newt-data` directory (the parent of `data.db`), keeping
/// plots alongside the data store. Pulled out so the path policy is one place.
pub fn plots_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("plots")
}

/// Convert a [`CellRun`] into the [nbformat](https://nbformat.readthedocs.io)
/// `outputs` array a persisted code cell carries (Phase 21.4).
///
/// This is the bridge between the live-kernel transport (21.3) and the on-disk
/// notebook artifact (21.4): `run_cell(persist_to=…)` runs a cell, folds the
/// iopub stream into a [`CellRun`], then hands that `CellRun` here to produce the
/// `Vec<serde_json::Value>` outputs that [`crate::notebook::persist_cell`] appends.
/// The persisted notebook then renders **exactly** what the cell produced — a
/// faithful, reviewable artifact (see
/// [`docs/design/centaur-data-scientist.md`](../../../../docs/design/centaur-data-scientist.md)
/// §4.1, the notebook-artifact bullet).
///
/// The mapping follows the nbformat v4 output-type schema:
///
/// - `stdout` / `stderr` → one `stream` output each (`{ output_type, name, text }`),
///   emitted only when non-empty so an output-free cell stays clean.
/// - each text [`DisplayItem`] → an `execute_result`
///   (`{ output_type, data: { <mime>: text }, metadata: {}, execution_count }`).
/// - each [`ImageOutput`] → a `display_data` carrying the PNG **re-read from disk
///   and base64-encoded** into `data["image/png"]` (with `metadata["image/png"]`
///   width/height when known), so the notebook actually renders the plot rather
///   than pointing at a path. A PNG that cannot be read is **skipped** with a
///   `tracing::warn` — the persist is still total (no panic, no aborted write).
/// - a [`KernelError`] → one `error` output (`{ output_type, ename, evalue,
///   traceback }`).
///
/// Output ordering is stdout, stderr, text results, images, then error — a
/// natural reading order for a reviewer scanning the cell.
pub fn cell_run_to_nb_outputs(run: &CellRun) -> Vec<serde_json::Value> {
    let mut outputs = Vec::new();

    if !run.stdout.is_empty() {
        outputs.push(serde_json::json!({
            "output_type": "stream",
            "name": "stdout",
            "text": run.stdout,
        }));
    }
    if !run.stderr.is_empty() {
        outputs.push(serde_json::json!({
            "output_type": "stream",
            "name": "stderr",
            "text": run.stderr,
        }));
    }

    for item in &run.results {
        outputs.push(serde_json::json!({
            "output_type": "execute_result",
            "data": { item.mime.clone(): item.text },
            "metadata": {},
            "execution_count": run.execution_count,
        }));
    }

    for img in &run.images {
        match std::fs::read(&img.path) {
            Ok(bytes) => {
                let mut data = serde_json::Map::new();
                data.insert(
                    MIME_PNG.to_string(),
                    serde_json::json!(base64_encode(&bytes)),
                );
                let mut png_meta = serde_json::Map::new();
                if let Some(w) = img.width {
                    png_meta.insert("width".to_string(), serde_json::json!(w));
                }
                if let Some(h) = img.height {
                    png_meta.insert("height".to_string(), serde_json::json!(h));
                }
                outputs.push(serde_json::json!({
                    "output_type": "display_data",
                    "data": serde_json::Value::Object(data),
                    "metadata": { MIME_PNG: serde_json::Value::Object(png_meta) },
                }));
            }
            // Total, not fatal: a missing/unreadable plot file is skipped (the
            // run already happened; the notebook stays writable) and logged so
            // the gap is visible rather than silent.
            Err(e) => {
                tracing::warn!(
                    path = %img.path.display(),
                    error = %e,
                    "cell_run_to_nb_outputs: skipping unreadable PNG plot when persisting cell"
                );
            }
        }
    }

    if let Some(err) = &run.error {
        outputs.push(serde_json::json!({
            "output_type": "error",
            "ename": err.ename,
            "evalue": err.evalue,
            "traceback": err.traceback,
        }));
    }

    outputs
}

/// Standard base64 (RFC 4648, with `=` padding) of `bytes` — the inverse of the
/// accumulator's [`base64_decode`]. Kept self-contained (mirroring the engine's
/// `sqlite::base64_encode`) so the nbformat `image/png` re-encode carries no new
/// dependency at this layer.
fn base64_encode(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(A[(b0 >> 2) as usize] as char);
        out.push(A[(((b0 & 0b11) << 4) | (b1 >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            A[(((b1 & 0b1111) << 2) | (b2 >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            A[(b2 & 0b111111) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Shared record of `(execution_count, bytes)` writes the [`RecordingSink`]
    /// appends to. Aliased so the type stays readable (and clippy-clean).
    type Writes = Arc<Mutex<Vec<(Option<i64>, Vec<u8>)>>>;

    /// A test PNG sink that records every write `(execution_count, bytes)` and
    /// returns a synthetic path, with no filesystem touch — for the pure-fold
    /// tests that only assert *what* was decoded. `Arc<Mutex<…>>` (not
    /// `Rc<RefCell<…>>`) so the sink stays `Send`, as the [`PngSink`] supertrait
    /// now requires.
    struct RecordingSink {
        writes: Writes,
    }
    impl PngSink for RecordingSink {
        fn write_png(
            &mut self,
            execution_count: Option<i64>,
            bytes: &[u8],
        ) -> anyhow::Result<PathBuf> {
            self.writes
                .lock()
                .unwrap()
                .push((execution_count, bytes.to_vec()));
            Ok(PathBuf::from(format!(
                "/plots/cell-{}-fixed.png",
                execution_count.unwrap_or(0)
            )))
        }
    }

    fn header(msg_type: &str) -> serde_json::Value {
        serde_json::json!({ "msg_type": msg_type })
    }

    /// `base64` of a 4-byte "PNG\x89" sentinel so the decode + write path is
    /// exercised with bytes we can assert on. (Not a real PNG; the accumulator
    /// never parses image internals, only ferries the decoded bytes.)
    fn png_b64() -> (String, Vec<u8>) {
        let raw = vec![0x89u8, b'P', b'N', b'G'];
        // Standard base64 of [0x89,0x50,0x4e,0x47] = "iVBORw==" ... compute it.
        let encoded = encode_for_test(&raw);
        (encoded, raw)
    }

    /// Local base64 encoder for the test fixture (RFC 4648).
    fn encode_for_test(bytes: &[u8]) -> String {
        const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b0 = chunk[0];
            let b1 = chunk.get(1).copied().unwrap_or(0);
            let b2 = chunk.get(2).copied().unwrap_or(0);
            out.push(A[(b0 >> 2) as usize] as char);
            out.push(A[(((b0 & 0b11) << 4) | (b1 >> 4)) as usize] as char);
            out.push(if chunk.len() > 1 {
                A[(((b1 & 0b1111) << 2) | (b2 >> 6)) as usize] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                A[(b2 & 0b111111) as usize] as char
            } else {
                '='
            });
        }
        out
    }

    #[test]
    fn base64_decode_round_trips_rfc_vectors() {
        // RFC 4648 vectors (the inverse of the engine's encoder).
        assert_eq!(base64_decode("").unwrap(), b"");
        assert_eq!(base64_decode("Zg==").unwrap(), b"f");
        assert_eq!(base64_decode("Zm8=").unwrap(), b"fo");
        assert_eq!(base64_decode("Zm9v").unwrap(), b"foo");
        assert_eq!(base64_decode("Zm9vYg==").unwrap(), b"foob");
        assert_eq!(base64_decode("Zm9vYmE=").unwrap(), b"fooba");
        assert_eq!(base64_decode("Zm9vYmFy").unwrap(), b"foobar");
    }

    #[test]
    fn base64_decode_rejects_bad_input() {
        assert!(base64_decode("Zg=").is_err()); // not a multiple of 4
        assert!(base64_decode("Z!==").is_err()); // illegal byte
    }

    #[test]
    fn decode_png_base64_strips_embedded_newlines() {
        let (b64, raw) = png_b64();
        // The classic notebook wraps base64 at 76 cols with '\n'; emulate it.
        let wrapped = format!("{}\n{}", &b64[..4], &b64[4..]);
        assert_eq!(decode_png_base64(&wrapped).unwrap(), raw);
    }

    #[test]
    fn fold_stream_splits_stdout_and_stderr() {
        let mut sink = RecordingSink {
            writes: Arc::new(Mutex::new(Vec::new())),
        };
        let mut acc = Accumulator::new(&mut sink);
        acc.feed(&serde_json::json!({
            "header": header("stream"),
            "content": { "name": "stdout", "text": "hello " }
        }))
        .unwrap();
        acc.feed(&serde_json::json!({
            "header": header("stream"),
            "content": { "name": "stdout", "text": "world\n" }
        }))
        .unwrap();
        acc.feed(&serde_json::json!({
            "header": header("stream"),
            "content": { "name": "stderr", "text": "a warning\n" }
        }))
        .unwrap();
        let run = acc.finish();
        assert_eq!(run.stdout, "hello world\n");
        assert_eq!(run.stderr, "a warning\n");
    }

    #[test]
    fn fold_execute_result_records_text_and_execution_count() {
        let mut sink = RecordingSink {
            writes: Arc::new(Mutex::new(Vec::new())),
        };
        let mut acc = Accumulator::new(&mut sink);
        acc.feed(&serde_json::json!({
            "header": header("execute_result"),
            "content": {
                "execution_count": 7,
                "data": {
                    "text/plain": "42",
                    "text/html": ["<b>", "42", "</b>"]
                }
            }
        }))
        .unwrap();
        let run = acc.finish();
        assert_eq!(run.execution_count, Some(7));
        // Sorted by MIME: text/html before text/plain.
        assert_eq!(
            run.results,
            vec![
                DisplayItem {
                    mime: "text/html".into(),
                    text: "<b>42</b>".into()
                },
                DisplayItem {
                    mime: "text/plain".into(),
                    text: "42".into()
                },
            ]
        );
        assert!(run.images.is_empty());
    }

    #[test]
    fn fold_display_data_with_png_writes_to_sink_never_inlines() {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let mut sink = RecordingSink {
            writes: writes.clone(),
        };
        let (b64, raw) = png_b64();
        let mut acc = Accumulator::new(&mut sink);
        // Establish an execution_count first (via an execute_result), then a
        // display_data carrying the PNG plus a text/plain fallback.
        acc.feed(&serde_json::json!({
            "header": header("execute_result"),
            "content": { "execution_count": 3, "data": { "text/plain": "<Figure>" } }
        }))
        .unwrap();
        acc.feed(&serde_json::json!({
            "header": header("display_data"),
            "content": {
                "data": { "image/png": b64, "text/plain": "<Figure size 640x480>" },
                "metadata": { "image/png": { "width": 640, "height": 480 } }
            }
        }))
        .unwrap();
        let run = acc.finish();

        // The PNG went to the sink with the right bytes and execution_count.
        let w = writes.lock().unwrap();
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].0, Some(3));
        assert_eq!(w[0].1, raw);

        // It is recorded as an ImageOutput with size from metadata — and NOT as
        // a DisplayItem (never inlined).
        assert_eq!(run.images.len(), 1);
        assert_eq!(run.images[0].mime, MIME_PNG);
        assert_eq!(run.images[0].width, Some(640));
        assert_eq!(run.images[0].height, Some(480));
        assert!(run.results.iter().all(|d| d.mime != MIME_PNG));
        // The two text/plain payloads survive as DisplayItems.
        assert_eq!(
            run.results
                .iter()
                .filter(|d| d.mime == "text/plain")
                .count(),
            2
        );
    }

    #[test]
    fn fold_error_records_exception() {
        let mut sink = RecordingSink {
            writes: Arc::new(Mutex::new(Vec::new())),
        };
        let mut acc = Accumulator::new(&mut sink);
        acc.feed(&serde_json::json!({
            "header": header("error"),
            "content": {
                "ename": "NameError",
                "evalue": "name 'foo' is not defined",
                "traceback": ["Traceback...", "NameError: name 'foo' is not defined"]
            }
        }))
        .unwrap();
        let run = acc.finish();
        assert!(run.failed());
        let err = run.error.unwrap();
        assert_eq!(err.ename, "NameError");
        assert_eq!(err.evalue, "name 'foo' is not defined");
        assert_eq!(err.traceback.len(), 2);
    }

    #[test]
    fn fold_status_idle_terminates() {
        let mut sink = RecordingSink {
            writes: Arc::new(Mutex::new(Vec::new())),
        };
        let mut acc = Accumulator::new(&mut sink);
        assert!(!acc.is_idle());
        acc.feed(&serde_json::json!({
            "header": header("status"),
            "content": { "execution_state": "busy" }
        }))
        .unwrap();
        assert!(!acc.is_idle(), "busy must not terminate");
        acc.feed(&serde_json::json!({
            "header": header("status"),
            "content": { "execution_state": "idle" }
        }))
        .unwrap();
        assert!(acc.is_idle(), "idle must terminate");
    }

    #[test]
    fn unknown_msg_type_is_noop() {
        let mut sink = RecordingSink {
            writes: Arc::new(Mutex::new(Vec::new())),
        };
        let mut acc = Accumulator::new(&mut sink);
        acc.feed(&serde_json::json!({
            "header": header("execute_input"),
            "content": { "code": "1+1", "execution_count": 1 }
        }))
        .unwrap();
        let run = acc.finish();
        assert_eq!(run, CellRun::default());
    }

    #[test]
    fn full_sequence_folds_to_cellrun() {
        // A realistic captured iopub sequence: status busy, stream, an
        // execute_result, then status idle.
        let mut sink = RecordingSink {
            writes: Arc::new(Mutex::new(Vec::new())),
        };
        let mut acc = Accumulator::new(&mut sink);
        for msg in [
            serde_json::json!({ "header": header("status"), "content": { "execution_state": "busy" } }),
            serde_json::json!({ "header": header("stream"), "content": { "name": "stdout", "text": "ok\n" } }),
            serde_json::json!({ "header": header("execute_result"), "content": { "execution_count": 1, "data": { "text/plain": "2" } } }),
            serde_json::json!({ "header": header("status"), "content": { "execution_state": "idle" } }),
        ] {
            acc.feed(&msg).unwrap();
        }
        assert!(acc.is_idle());
        let run = acc.finish();
        assert_eq!(run.stdout, "ok\n");
        assert_eq!(run.execution_count, Some(1));
        assert_eq!(
            run.results,
            vec![DisplayItem {
                mime: "text/plain".into(),
                text: "2".into()
            }]
        );
        assert!(!run.failed());
    }

    #[test]
    fn cellrun_serializes_stably() {
        let run = CellRun {
            stdout: "hi".into(),
            stderr: String::new(),
            results: vec![DisplayItem {
                mime: "text/plain".into(),
                text: "1".into(),
            }],
            images: vec![ImageOutput {
                path: PathBuf::from("/plots/cell-1-x.png"),
                mime: MIME_PNG.into(),
                width: Some(640),
                height: Some(480),
            }],
            error: None,
            execution_count: Some(1),
        };
        let json = serde_json::to_value(&run).unwrap();
        assert_eq!(json["stdout"], "hi");
        assert_eq!(json["results"][0]["mime"], "text/plain");
        assert_eq!(json["images"][0]["width"], 640);
        assert_eq!(json["execution_count"], 1);
    }

    #[test]
    fn plots_dir_is_under_data_dir() {
        assert_eq!(
            plots_dir(Path::new("/ws/.newt-data")),
            Path::new("/ws/.newt-data/plots")
        );
    }

    #[test]
    fn png_written_to_tempdir_has_exact_bytes() {
        // The file-write path (RecordingSink avoids disk; here we exercise a
        // real on-disk sink via a closure-backed PngSink over a tempdir).
        let dir = tempfile::tempdir().unwrap();
        struct DiskSink {
            dir: PathBuf,
            last: Option<PathBuf>,
        }
        impl PngSink for DiskSink {
            fn write_png(&mut self, n: Option<i64>, bytes: &[u8]) -> anyhow::Result<PathBuf> {
                std::fs::create_dir_all(&self.dir)?;
                let p = self.dir.join(format!("cell-{}-test.png", n.unwrap_or(0)));
                std::fs::write(&p, bytes)?;
                self.last = Some(p.clone());
                Ok(p)
            }
        }
        let mut sink = DiskSink {
            dir: dir.path().join("plots"),
            last: None,
        };
        let (b64, raw) = png_b64();
        {
            let mut acc = Accumulator::new(&mut sink);
            acc.feed(&serde_json::json!({
                "header": header("display_data"),
                "content": { "data": { "image/png": b64 } }
            }))
            .unwrap();
            let run = acc.finish();
            assert_eq!(run.images.len(), 1);
        }
        let written = sink.last.unwrap();
        assert!(written.exists());
        assert_eq!(std::fs::read(&written).unwrap(), raw);
    }

    // ── cell_run_to_nb_outputs (Phase 21.4 bridge to the notebook artifact) ──

    #[test]
    fn base64_encode_matches_rfc_vectors_and_round_trips() {
        // The encoder is the inverse of the accumulator's base64_decode.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        // Round-trips with the decoder for arbitrary bytes.
        let bytes = vec![0x89u8, b'P', b'N', b'G', 0x00, 0xff, 0x10];
        assert_eq!(base64_decode(&base64_encode(&bytes)).unwrap(), bytes);
    }

    #[test]
    fn cell_run_to_nb_outputs_maps_streams_results_and_error() {
        let run = CellRun {
            stdout: "out\n".into(),
            stderr: "warn\n".into(),
            results: vec![DisplayItem {
                mime: "text/plain".into(),
                text: "42".into(),
            }],
            images: vec![],
            error: Some(KernelError {
                ename: "ValueError".into(),
                evalue: "bad".into(),
                traceback: vec!["Traceback...".into(), "ValueError: bad".into()],
            }),
            execution_count: Some(9),
        };
        let outputs = cell_run_to_nb_outputs(&run);
        // stdout, stderr, one execute_result, one error → 4 outputs in order.
        assert_eq!(outputs.len(), 4);
        assert_eq!(outputs[0]["output_type"], "stream");
        assert_eq!(outputs[0]["name"], "stdout");
        assert_eq!(outputs[0]["text"], "out\n");
        assert_eq!(outputs[1]["name"], "stderr");
        assert_eq!(outputs[2]["output_type"], "execute_result");
        assert_eq!(outputs[2]["data"]["text/plain"], "42");
        assert_eq!(outputs[2]["execution_count"], 9);
        assert_eq!(outputs[2]["metadata"], serde_json::json!({}));
        assert_eq!(outputs[3]["output_type"], "error");
        assert_eq!(outputs[3]["ename"], "ValueError");
        assert_eq!(outputs[3]["evalue"], "bad");
        assert_eq!(outputs[3]["traceback"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn cell_run_to_nb_outputs_skips_empty_streams() {
        // A cell with only a result emits exactly one output (no empty streams).
        let run = CellRun {
            results: vec![DisplayItem {
                mime: "text/html".into(),
                text: "<b>1</b>".into(),
            }],
            execution_count: Some(1),
            ..Default::default()
        };
        let outputs = cell_run_to_nb_outputs(&run);
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0]["output_type"], "execute_result");
        assert_eq!(outputs[0]["data"]["text/html"], "<b>1</b>");
    }

    #[test]
    fn cell_run_to_nb_outputs_rereads_png_into_display_data_base64() {
        // Write a real (sentinel) PNG to disk, point an ImageOutput at it, and
        // assert the display_data carries the base64 of those exact bytes so the
        // persisted notebook RENDERS the plot (faithful artifact).
        let dir = tempfile::tempdir().unwrap();
        let png_path = dir.path().join("cell-5-x.png");
        let raw = vec![0x89u8, b'P', b'N', b'G', 0x0d, 0x0a];
        std::fs::write(&png_path, &raw).unwrap();

        let run = CellRun {
            images: vec![ImageOutput {
                path: png_path,
                mime: MIME_PNG.into(),
                width: Some(640),
                height: Some(480),
            }],
            execution_count: Some(5),
            ..Default::default()
        };
        let outputs = cell_run_to_nb_outputs(&run);
        assert_eq!(outputs.len(), 1);
        let out = &outputs[0];
        assert_eq!(out["output_type"], "display_data");
        // The bytes were re-read and base64-encoded — decoding gives them back.
        let b64 = out["data"][MIME_PNG].as_str().unwrap();
        assert_eq!(base64_decode(b64).unwrap(), raw);
        // Size metadata travels under metadata["image/png"].
        assert_eq!(out["metadata"][MIME_PNG]["width"], 640);
        assert_eq!(out["metadata"][MIME_PNG]["height"], 480);
    }

    #[test]
    fn cell_run_to_nb_outputs_skips_missing_png_without_panic() {
        // A plot whose file is gone is skipped (total, not fatal) — no panic, no
        // output emitted for it.
        let run = CellRun {
            images: vec![ImageOutput {
                path: PathBuf::from("/no/such/plot/cell-1-gone.png"),
                mime: MIME_PNG.into(),
                width: None,
                height: None,
            }],
            ..Default::default()
        };
        let outputs = cell_run_to_nb_outputs(&run);
        assert!(
            outputs.is_empty(),
            "an unreadable PNG is skipped, not panicked on"
        );
    }

    #[test]
    fn cell_run_to_nb_outputs_png_without_dimensions_omits_size_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let png_path = dir.path().join("p.png");
        std::fs::write(&png_path, b"\x89PNG").unwrap();
        let run = CellRun {
            images: vec![ImageOutput {
                path: png_path,
                mime: MIME_PNG.into(),
                width: None,
                height: None,
            }],
            ..Default::default()
        };
        let outputs = cell_run_to_nb_outputs(&run);
        assert_eq!(outputs.len(), 1);
        // metadata["image/png"] is present but empty (no width/height keys).
        assert_eq!(outputs[0]["metadata"][MIME_PNG], serde_json::json!({}));
    }

    #[test]
    fn cell_run_to_nb_outputs_empty_run_is_empty() {
        assert!(cell_run_to_nb_outputs(&CellRun::default()).is_empty());
    }
}
