//! Private, single-request protocol for the carried Brush worker.
//!
//! This entrypoint is reached only after [`agent_bridle_core::SandboxedWorker`]
//! has created a fresh process through the L3-aware spawn funnel. When the
//! effective caveats engage a native backend, the worker inherits that boundary;
//! otherwise its result honestly reports no L3. It accepts one bounded JSON
//! request on stdin and emits a framed execution-event stream on stdout.

use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use agent_bridle_core::{Gate, Scope, Tool, ToolContext, ToolResult, TrustedWorkerRequest};
use serde::{Deserialize, Serialize};

use crate::brush_protocol::{
    stream_limits, write_pre_start_terminal, WorkerEmitter, WorkerResponse,
};
use crate::brush_shell::run_in_brush;
use crate::caveat_interceptor::{CaveatInterceptor, DenialSink};
use crate::output_observer::OutputEmitter;

pub(crate) const WORKER_FLAG: &str = "--agent-bridle-worker";
pub(crate) const WORKER_KIND: &str = "brush";
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) type WorkerEndpoint = std::os::unix::net::UnixStream;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) type WorkerEndpoint = ();
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkerPayload {
    cmd: String,
    cwd: Option<String>,
    path: String,
    env: BTreeMap<String, String>,
    max_output: usize,
    #[serde(default)]
    pub(crate) broker: bool,
}

impl WorkerPayload {
    pub(crate) fn new(
        cmd: String,
        cwd: Option<String>,
        path: String,
        env: BTreeMap<String, String>,
        max_output: usize,
    ) -> Self {
        Self {
            cmd,
            cwd,
            path,
            env,
            max_output,
            broker: false,
        }
    }
}

/// Serve exactly one Brush request and return the process exit status.
pub(crate) fn main() -> i32 {
    match receive_request() {
        Ok((payload, cx, endpoint)) => run(payload, cx, endpoint),
        Err(error) => {
            let response = WorkerResponse::failure(error);
            match write_pre_start_terminal(&mut std::io::stdout().lock(), &response) {
                Ok(()) => 126,
                Err(error) => {
                    eprintln!("brush worker terminal frame failed: {error}");
                    125
                }
            }
        }
    }
}

fn receive_request() -> Result<(WorkerPayload, ToolContext, Option<WorkerEndpoint>), String> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let (request, endpoint): (TrustedWorkerRequest<WorkerPayload>, _) =
        crate::private_control::receive_worker_request_with_endpoint(|payload: &WorkerPayload| {
            payload.broker
        })
        .map_err(|error| format!("trusted worker authentication failed: {error}"))?;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let request: TrustedWorkerRequest<WorkerPayload> =
        crate::private_control::receive_worker_request()
            .map_err(|error| format!("trusted worker authentication failed: {error}"))?;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let endpoint = None;
    if !request.has_supported_version() {
        return Err("unsupported worker protocol version".to_string());
    }
    let (nonce, caveats, strength_floor, payload) = request.into_parts();
    let expected = std::env::var("AGENT_BRIDLE_WORKER_NONCE")
        .map_err(|_| "worker nonce is absent".to_string())?;
    if nonce != expected {
        return Err("worker nonce mismatch".to_string());
    }

    let generation = match &caveats.valid_for_generation {
        Scope::All => 0,
        Scope::Only(values) => values
            .iter()
            .copied()
            .next()
            .ok_or_else(|| "worker grant has no valid generation".to_string())?,
    };
    let tool = WorkerTool;
    let cx = Gate::new(generation)
        // The delegated floor is per-axis (`EnforcementFloor`), re-applied
        // faithfully so the worker's own confinement matches the supervisor's.
        .with_enforcement_floor(strength_floor)
        .authorize(&tool, &caveats)
        .map_err(|error| format!("worker authorization failed: {error}"))?;
    Ok((payload, cx, endpoint))
}

fn run(request: WorkerPayload, cx: ToolContext, endpoint: Option<WorkerEndpoint>) -> i32 {
    // Bounds are validated BEFORE anything is emitted: an unreasonable
    // `max_output` is refused rather than silently turned into an unbounded cap
    // by saturating arithmetic.
    let limits = match stream_limits(request.max_output) {
        Ok(limits) => limits,
        Err(error) => {
            let response = WorkerResponse::failure(error.to_string());
            let _ = write_pre_start_terminal(&mut std::io::stdout().lock(), &response);
            return 126;
        }
    };

    let sink: DenialSink = Arc::new(Mutex::new(Vec::new()));
    let cancel = Arc::new(AtomicBool::new(false));
    let interceptor =
        CaveatInterceptor::new(cx, Arc::clone(&sink)).with_cancel(Arc::clone(&cancel));
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let interceptor = match endpoint {
        Some(endpoint) => {
            interceptor.with_broker(Arc::new(crate::broker_runtime::WorkerBroker::new(endpoint)))
        }
        None => interceptor,
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let _ = endpoint;

    // One emitter owns the whole frame sequence, so the shell engine's live
    // output and the terminal cannot interleave out of order or race a second
    // terminal. It writes straight to stdout under its own lock — there is no
    // intermediate queue to grow, and the pipe is the backpressure.
    let emitter = WorkerEmitter::new(std::io::stdout(), limits);
    if let Err(error) = emitter.started() {
        eprintln!("brush worker started frame failed: {error}");
        return 125;
    }

    let output = {
        let emitter = emitter.clone();
        OutputEmitter::to_worker_channel(Arc::new(move |stream, chunk: &[u8]| {
            emitter.output(stream, chunk);
        }))
    };

    let result = run_in_brush(
        request.cmd,
        request.cwd,
        request.path,
        request.env,
        interceptor,
        request.max_output,
        output,
    );

    let denials = sink
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    for denial in &denials {
        if let Err(error) = emitter.denial(denial) {
            eprintln!("brush worker denial frame failed: {error}");
            return 125;
        }
    }

    // The terminal carries status, denials, error text, and the emitter's exact
    // drop accounting — NOT a second copy of stdout/stderr. The supervisor's
    // accumulation of the frames above is the one output history.
    let (response, process_code) = match result {
        Ok(captured) => {
            // A detached drain means output was produced that reached neither
            // the stream nor any capture. The byte count is unknowable, so
            // record the omission as an event rather than invent a number —
            // the terminal must not present a short transcript as complete.
            if captured.stdout_detached {
                emitter.note_omitted_stream();
            }
            if captured.stderr_detached {
                emitter.note_omitted_stream();
            }
            (
                WorkerResponse {
                    exit_code: captured.exit_code,
                    denials,
                    error: None,
                    dropped: Default::default(),
                },
                0,
            )
        }
        Err(error) => (
            WorkerResponse {
                exit_code: 126,
                denials,
                error: Some(error.to_string()),
                dropped: Default::default(),
            },
            126,
        ),
    };
    match emitter.terminal(response) {
        Ok(()) => process_code,
        Err(error) => {
            eprintln!("brush worker terminal frame failed: {error}");
            125
        }
    }
}

struct WorkerTool;

#[async_trait::async_trait]
impl Tool for WorkerTool {
    fn name(&self) -> &str {
        "brush-worker"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({})
    }

    async fn invoke(
        &self,
        _args: serde_json::Value,
        _cx: &ToolContext,
    ) -> ToolResult<serde_json::Value> {
        unreachable!("worker tool is used only to mint its context")
    }
}
