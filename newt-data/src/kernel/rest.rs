//! The pure-Rust Jupyter REST + websocket [`KernelClient`] (Phase 21.3, Option A1).
//!
//! [`RestKernelClient`] drives the human's **already-running** Jupyter Server
//! over two well-trodden surfaces — no ZMQ, no HMAC, no embedded libpython
//! (the rejected Option B). See
//! [`docs/design/centaur-data-scientist.md`](../../../../docs/design/centaur-data-scientist.md).
//!
//! 1. **REST discovery** ([`reqwest`]). [`RestKernelClient::connect`] either
//!    adopts a caller-supplied `kernel_id` or `GET`s `/api/kernels` and reuses
//!    the first running kernel, starting one with `POST /api/kernels` only when
//!    none exists. The Jupyter token is sent as an `Authorization: token <tok>`
//!    header (and mirrored as a `?token=` query on the websocket URL, which some
//!    proxies require).
//! 2. **Kernel channels websocket** ([`tokio_tungstenite`]). [`run_cell`] opens
//!    `ws(s)://…/api/kernels/<id>/channels`, sends one Jupyter `execute_request`,
//!    and reads iopub messages — feeding each straight into the **pure**
//!    [`Accumulator`] — until a `status: idle` whose `parent_header.msg_id`
//!    matches our request. A per-request timeout bounds the read. If the socket
//!    instead **closes before** that terminating idle, the run is truncated and
//!    [`run_cell`] returns `Err` (a protocol failure, not a partial success) —
//!    so a dropped kernel surfaces honestly through the MCP in-band `isError`
//!    path rather than presenting truncated output as a finished cell.
//!
//! All the output-folding logic lives in the pure [`Accumulator`]; this module is
//! deliberately thin (connect, frame, match the parent msg_id, map transport
//! failures to `anyhow`). [`run_cell`] returns `Ok(CellRun)` even when the *cell*
//! raised — a Python exception is data ([`CellRun::error`]), not a transport
//! fault; only connection/protocol failures are `Err`.
//!
//! [`Accumulator`]: super::Accumulator
//! [`CellRun`]: super::CellRun
//! [`KernelClient`]: super::KernelClient
//! [`run_cell`]: RestKernelClient::run_cell
//! [`CellRun::error`]: super::CellRun::error

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message;

use super::{Accumulator, CellRun, DirPngSink, KernelClient};

/// Default per-cell read timeout. A cell that produces no terminating
/// `status: idle` within this window fails with a timeout error rather than
/// hanging the agent. Generous enough for a slow plot render; bounded so a
/// wedged kernel surfaces honestly.
const DEFAULT_RUN_TIMEOUT: Duration = Duration::from_secs(120);

/// A live-kernel client over the Jupyter Server REST API + kernel channels
/// websocket (Phase 21.3, Option A1).
///
/// Cheap to hold: it stores the resolved websocket URL, the data directory the
/// PNGs are written under, and the per-run timeout. Each [`run_cell`](Self::run_cell)
/// opens a fresh websocket (the MVP keeps no persistent socket — reconnect
/// hardening is 21.7).
///
/// Deliberately **not** `Debug`: `ws_url` carries the `?token=…` query, and a
/// derived `Debug` would leak that secret into any log line or test panic. The
/// status fields a caller needs are exposed through [`kernel_id`](Self::kernel_id)
/// and [`base_url`](Self::base_url) instead.
pub struct RestKernelClient {
    /// The fully-resolved kernel channels websocket URL, token query included:
    /// `ws(s)://host/api/kernels/<id>/channels?token=<tok>`.
    ws_url: String,
    /// The kernel id this client is bound to.
    kernel_id: String,
    /// The Jupyter Server base URL the client connected to (for status reports).
    base_url: String,
    /// Directory the decoded PNG plots are written under (`<data_dir>/plots`).
    plots_dir: PathBuf,
    /// Per-cell read timeout.
    timeout: Duration,
    /// A stable session id used in every `execute_request` header.
    session: String,
}

impl RestKernelClient {
    /// Connect to a running Jupyter Server and bind to a kernel.
    ///
    /// `base_url` is the server root (e.g. `http://127.0.0.1:8888`); `token` is
    /// the Jupyter token (often required); `kernel_id` adopts a specific kernel,
    /// or — when `None` — the first running kernel is reused, or a new one is
    /// started. `plots_dir` is where [`run_cell`](Self::run_cell) writes decoded
    /// PNGs (typically `<workspace>/.newt-data/plots`).
    ///
    /// Returns an `Err` for any REST failure (unreachable server, auth refused,
    /// no kernelspec) so [`kernel_attach`] can surface it in-band.
    ///
    /// [`kernel_attach`]: super
    pub async fn connect(
        base_url: &str,
        token: Option<&str>,
        kernel_id: Option<&str>,
        plots_dir: PathBuf,
    ) -> anyhow::Result<Self> {
        let base = base_url.trim_end_matches('/').to_string();
        let http = reqwest::Client::builder()
            .build()
            .context("building HTTP client for Jupyter REST")?;

        let kernel_id = match kernel_id {
            Some(id) => id.to_string(),
            None => resolve_or_start_kernel(&http, &base, token).await?,
        };

        let ws_url = channels_ws_url(&base, &kernel_id, token)?;

        Ok(Self {
            ws_url,
            kernel_id,
            base_url: base,
            plots_dir,
            timeout: DEFAULT_RUN_TIMEOUT,
            session: uuid::Uuid::new_v4().to_string(),
        })
    }

    /// The kernel id this client is bound to (reported by `kernel_attach`).
    pub fn kernel_id(&self) -> &str {
        &self.kernel_id
    }

    /// The Jupyter Server base URL (reported by `kernel_attach`).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Override the per-cell read timeout (used by tests to keep the mock-ws run
    /// snappy; production keeps the default `DEFAULT_RUN_TIMEOUT`).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The implementation behind the [`KernelClient::run_cell`] trait method,
    /// split out so it is directly callable (and testable) without the
    /// `async_trait` indirection.
    async fn run_cell_impl(&self, code: &str) -> anyhow::Result<CellRun> {
        let request = self.ws_url.clone();
        let (mut ws, _resp) = tokio_tungstenite::connect_async(&request)
            .await
            .with_context(|| {
                format!(
                    "connecting to kernel channels websocket at {}",
                    self.base_url
                )
            })?;

        let msg_id = uuid::Uuid::new_v4().to_string();
        let execute = execute_request_message(&msg_id, &self.session, code);
        ws.send(Message::text(execute.to_string()))
            .await
            .context("sending execute_request to kernel")?;

        let mut sink = DirPngSink::new(self.plots_dir.clone());
        let mut acc = Accumulator::new(&mut sink);

        let read = async {
            while let Some(frame) = ws.next().await {
                let frame = frame.context("reading kernel websocket frame")?;
                // A Close frame is the kernel ending the channels socket. Break
                // on it directly rather than waiting for the *subsequent*
                // `ws.next()` to return `None`: an abruptly-closed socket
                // surfaces that follow-up read differently across platforms
                // (clean `None` vs a `ConnectionReset` error vs a stall on
                // Windows), which made the closed-before-idle path
                // non-deterministic. The Close frame itself is delivered
                // identically everywhere; the post-loop `is_idle` check then
                // bails if the run was truncated.
                if frame.is_close() {
                    break;
                }
                let text = match message_text(frame) {
                    Some(t) => t,
                    // Non-text frames (ping/pong/binary) carry no iopub.
                    None => continue,
                };
                let msg: Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    // A frame we cannot parse is skipped, not fatal — a kernel
                    // may interleave control chatter we do not model.
                    Err(_) => continue,
                };

                // Only fold messages on the iopub channel that are replies to
                // *our* execute_request (parent_header.msg_id matches). This is
                // the protocol-correct filter the pure accumulator relies on.
                if channel_of(&msg) == Some("iopub") && parent_msg_id(&msg) == Some(msg_id.as_str())
                {
                    acc.feed(&msg)?;
                    if acc.is_idle() {
                        break;
                    }
                }
            }

            // The `while` loop exits either because we saw `status: idle` (and
            // `break`'d — the success path) or because the websocket stream
            // ended first (`ws.next()` returned `None`). The latter is a
            // protocol failure: the kernel channels socket closed before the
            // cell finished, so whatever the accumulator holds is a *truncated*
            // run. Bailing here maps it to a transport `Err` — which the MCP
            // `run_cell` handler surfaces as an in-band `isError` tool error —
            // rather than letting a partial `CellRun` masquerade as a successful
            // run (Phase 21.3; see docs/design/centaur-data-scientist.md's
            // in-band-error contract: a dropped socket is a failure, not data).
            if !acc.is_idle() {
                bail!(
                    "kernel channels websocket closed before the cell finished \
                     (no status: idle); the run is truncated"
                );
            }
            Ok::<(), anyhow::Error>(())
        };

        match tokio::time::timeout(self.timeout, read).await {
            Ok(result) => result?,
            Err(_) => bail!(
                "timed out after {:?} waiting for kernel to finish the cell (no status: idle)",
                self.timeout
            ),
        }

        // Best-effort close; ignore errors (we already have the result).
        let _ = ws.close(None).await;
        Ok(acc.finish())
    }
}

#[async_trait::async_trait]
impl KernelClient for RestKernelClient {
    async fn run_cell(&self, code: &str) -> anyhow::Result<CellRun> {
        self.run_cell_impl(code).await
    }
}

// ── REST discovery helpers ───────────────────────────────────────────────────

/// Adopt the first running kernel, or start a new one. Pulled out so the policy
/// (reuse-then-create) is one place.
async fn resolve_or_start_kernel(
    http: &reqwest::Client,
    base: &str,
    token: Option<&str>,
) -> anyhow::Result<String> {
    let list_url = format!("{base}/api/kernels");
    let resp = with_token(http.get(&list_url), token)
        .send()
        .await
        .with_context(|| format!("GET {list_url}"))?;
    if !resp.status().is_success() {
        bail!(
            "Jupyter REST GET /api/kernels failed with HTTP {} (is the token correct?)",
            resp.status()
        );
    }
    let kernels: Value = resp
        .json()
        .await
        .context("parsing /api/kernels response as JSON")?;
    if let Some(id) = kernels
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(|k| k.get("id"))
        .and_then(|id| id.as_str())
    {
        return Ok(id.to_string());
    }

    // No running kernel — start one with the server's default kernelspec.
    let start_url = format!("{base}/api/kernels");
    let resp = with_token(http.post(&start_url), token)
        .json(&serde_json::json!({}))
        .send()
        .await
        .with_context(|| format!("POST {start_url}"))?;
    if !resp.status().is_success() {
        bail!(
            "Jupyter REST POST /api/kernels failed with HTTP {} (no kernelspec available?)",
            resp.status()
        );
    }
    let started: Value = resp
        .json()
        .await
        .context("parsing POST /api/kernels response as JSON")?;
    started
        .get("id")
        .and_then(|id| id.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("POST /api/kernels response carried no kernel id"))
}

/// Attach the Jupyter token to a request as an `Authorization: token <tok>`
/// header (the canonical scheme). A `None` token leaves the request unauthed
/// (a server launched with `--ServerApp.token=''`).
fn with_token(builder: reqwest::RequestBuilder, token: Option<&str>) -> reqwest::RequestBuilder {
    match token {
        Some(tok) if !tok.is_empty() => builder.header("Authorization", format!("token {tok}")),
        _ => builder,
    }
}

/// Build the kernel channels websocket URL from the HTTP base URL, swapping the
/// scheme (`http`→`ws`, `https`→`wss`) and appending `?token=<tok>` when a token
/// is present (some proxies authenticate the websocket only via the query).
fn channels_ws_url(base: &str, kernel_id: &str, token: Option<&str>) -> anyhow::Result<String> {
    let parsed = url::Url::parse(base).with_context(|| format!("parsing base URL {base}"))?;
    let ws_scheme = match parsed.scheme() {
        "https" => "wss",
        "http" => "ws",
        other => bail!("unsupported Jupyter base-URL scheme {other:?} (expected http/https)"),
    };
    let authority = parsed
        .host_str()
        .ok_or_else(|| anyhow!("base URL {base} has no host"))?;
    let port = parsed.port().map(|p| format!(":{p}")).unwrap_or_default();
    let path = parsed.path().trim_end_matches('/');
    let mut ws_url =
        format!("{ws_scheme}://{authority}{port}{path}/api/kernels/{kernel_id}/channels");
    if let Some(tok) = token {
        if !tok.is_empty() {
            ws_url.push_str(&format!("?token={tok}"));
        }
    }
    Ok(ws_url)
}

// ── websocket message helpers ────────────────────────────────────────────────

/// Build a Jupyter `execute_request` message envelope (the shell-channel
/// message; the kernel-channels websocket multiplexes channels by a top-level
/// `channel` field). `msg_id` ties the reply iopub stream back to this request;
/// `session` is the stable per-client session id.
fn execute_request_message(msg_id: &str, session: &str, code: &str) -> Value {
    serde_json::json!({
        "channel": "shell",
        "header": {
            "msg_id": msg_id,
            "session": session,
            "username": "newt",
            "msg_type": "execute_request",
            "version": "5.3"
        },
        "parent_header": {},
        "metadata": {},
        "content": {
            "code": code,
            "silent": false,
            "store_history": true,
            "user_expressions": {},
            "allow_stdin": false,
            "stop_on_error": true
        }
    })
}

/// Extract the textual payload of a websocket frame, or `None` for non-text
/// frames (ping/pong/binary/close) which carry no iopub message.
fn message_text(msg: Message) -> Option<String> {
    match msg {
        // In tungstenite 0.24, `Message::Text` wraps a `String` directly.
        Message::Text(t) => Some(t),
        _ => None,
    }
}

/// The top-level `channel` field of a kernel-channels websocket message
/// (`"iopub"`, `"shell"`, …), if present.
fn channel_of(msg: &Value) -> Option<&str> {
    msg.get("channel").and_then(|c| c.as_str())
}

/// The `parent_header.msg_id` of a kernel message — the id of the request this
/// message is a reply to. Used to filter iopub to *our* `execute_request`.
fn parent_msg_id(msg: &Value) -> Option<&str> {
    msg.get("parent_header")
        .and_then(|p| p.get("msg_id"))
        .and_then(|id| id.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channels_ws_url_swaps_http_to_ws_and_appends_token() {
        let url = channels_ws_url("http://127.0.0.1:8888", "abc123", Some("sekret")).unwrap();
        assert_eq!(
            url,
            "ws://127.0.0.1:8888/api/kernels/abc123/channels?token=sekret"
        );
    }

    #[test]
    fn channels_ws_url_swaps_https_to_wss() {
        let url = channels_ws_url("https://lab.home.lan", "k", None).unwrap();
        assert_eq!(url, "wss://lab.home.lan/api/kernels/k/channels");
    }

    #[test]
    fn channels_ws_url_preserves_base_path_prefix() {
        // A JupyterHub-style base path (`/user/alice`) must be preserved.
        let url = channels_ws_url("http://hub:8000/user/alice/", "k", Some("t")).unwrap();
        assert_eq!(
            url,
            "ws://hub:8000/user/alice/api/kernels/k/channels?token=t"
        );
    }

    #[test]
    fn channels_ws_url_rejects_non_http_scheme() {
        assert!(channels_ws_url("ftp://nope", "k", None).is_err());
    }

    #[test]
    fn execute_request_carries_code_and_msg_id() {
        let msg = execute_request_message("mid-1", "sess-1", "print(1)");
        assert_eq!(msg["channel"], "shell");
        assert_eq!(msg["header"]["msg_id"], "mid-1");
        assert_eq!(msg["header"]["session"], "sess-1");
        assert_eq!(msg["header"]["msg_type"], "execute_request");
        assert_eq!(msg["content"]["code"], "print(1)");
        assert_eq!(msg["content"]["silent"], false);
    }

    #[test]
    fn channel_and_parent_msg_id_extract() {
        let msg = serde_json::json!({
            "channel": "iopub",
            "parent_header": { "msg_id": "mid-1" }
        });
        assert_eq!(channel_of(&msg), Some("iopub"));
        assert_eq!(parent_msg_id(&msg), Some("mid-1"));

        let bare = serde_json::json!({});
        assert_eq!(channel_of(&bare), None);
        assert_eq!(parent_msg_id(&bare), None);
    }

    #[test]
    fn message_text_only_for_text_frames() {
        assert_eq!(
            message_text(Message::text("hi".to_string())),
            Some("hi".to_string())
        );
        assert_eq!(message_text(Message::Ping(Default::default())), None);
    }
}
