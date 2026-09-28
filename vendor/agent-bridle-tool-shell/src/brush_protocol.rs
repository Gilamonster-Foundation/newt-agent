//! Framed, private Brush-worker result protocol (ABES).
//!
//! The authenticated authority request travels over the inherited private
//! control socket on stdin. **This** protocol is the worker's observable result
//! channel on stdout: live `started`/`stdout`/`stderr`/`denied`/`truncated`
//! frames followed by exactly one terminal frame. It carries no authority.
//!
//! # One output history
//!
//! The terminal frame deliberately does **not** repeat stdout/stderr. The
//! supervisor accumulates the live stream — already bounded — and that
//! accumulation *is* the captured transcript; the terminal carries only exit
//! status, denials, error text, and the worker's exact drop accounting. Two
//! copies would be two truths, and a protocol bug could then display one live
//! transcript to an observer and return a different final transcript to the
//! caller. There is no second copy to disagree with.
//!
//! # What this channel is and is not
//!
//! It is a **versioned, sequenced, length-bounded, integrity-checked** result
//! channel: a truncated, reordered, oversized, or unknown frame fails closed.
//! It is *not* cryptographically authenticated — nothing here signs or MACs a
//! frame, and the channel's trustworthiness rests entirely on it being a pipe
//! from a process this supervisor spawned through the trusted-worker funnel.
//! Calling it "authenticated" would claim a property it does not have.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use agent_bridle_core::{Denial, DroppedEvidence, ToolError, ToolResult};
use serde::{Deserialize, Serialize};

const MAGIC: [u8; 4] = *b"ABES";
const VERSION: u8 = 1;
const HEADER_LEN: usize = 18;
const STARTED: u8 = 1;
const STDOUT: u8 = 2;
const STDERR: u8 = 3;
const DENIED: u8 = 4;
const TERMINAL: u8 = 5;
const TRUNCATED: u8 = 6;

// ── Repository-defined hard ceilings ────────────────────────────────────────
//
// These do NOT derive from `max_output`. A configured output budget is a
// caller/config input, and deriving every cap from it by multiplication means a
// large configured budget silently buys a proportionally larger attack surface
// — and an absurd one either overflows or, with saturating arithmetic, pins
// every cap to `usize::MAX`, i.e. no cap at all. The ceilings below bound the
// protocol independently; `max_output` may only make the caps *smaller*.

/// The largest payload any single frame may declare.
pub(crate) const MAX_FRAME_PAYLOAD: usize = 1024 * 1024;
/// The largest total stream (headers + payloads) the supervisor will read.
pub(crate) const MAX_STREAM_BYTES: usize = 256 * 1024 * 1024;
/// The largest `max_output` a caller may configure. Rejected — not clamped — so
/// a configured budget always describes what is actually enforced.
pub(crate) const MAX_CONFIGURED_OUTPUT: usize = 64 * 1024 * 1024;
/// The largest chunk the worker packs into one output frame.
pub(crate) const MAX_EMIT_CHUNK: usize = 64 * 1024;
/// Headroom over `max_output` for framing overhead, denials, and the terminal.
/// Ample (a JSON denial is a few hundred bytes) without making the total-stream
/// cap so large that it stops being a meaningful bound for a small budget.
const PROTOCOL_SLACK: usize = 1024 * 1024;
/// How much more total stream than `max_output` a well-behaved worker may use.
const TOTAL_STREAM_FACTOR: usize = 4;

/// The terminal record. Carries no stdout/stderr — see the module docs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkerResponse {
    pub(crate) exit_code: i32,
    pub(crate) denials: Vec<Denial>,
    pub(crate) error: Option<String>,
    /// Exact accounting of output the worker itself could not stream, so a
    /// truncated transcript is never silently short.
    #[serde(default)]
    pub(crate) dropped: DroppedEvidence,
}

impl WorkerResponse {
    pub(crate) fn failure(message: impl Into<String>) -> Self {
        Self {
            exit_code: 126,
            denials: Vec::new(),
            error: Some(message.into()),
            dropped: DroppedEvidence::default(),
        }
    }
}

/// Everything one worker run produced: the terminal record plus THE captured
/// transcript, accumulated from the live stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkerOutcome {
    pub(crate) response: WorkerResponse,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    /// What the SUPERVISOR dropped while accumulating, on top of whatever the
    /// worker reported it could not send.
    pub(crate) dropped: DroppedEvidence,
}

/// Validated stream bounds for one worker run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StreamLimits {
    max_payload: usize,
    max_total: usize,
    max_capture: usize,
}

/// Derive the stream bounds, refusing an unreasonable configuration *before*
/// any multiplication-based cap is computed.
pub(crate) fn stream_limits(max_output: usize) -> ToolResult<StreamLimits> {
    if max_output == 0 {
        return Err(protocol_error("max_output must be greater than zero"));
    }
    if max_output > MAX_CONFIGURED_OUTPUT {
        return Err(protocol_error(format!(
            "max_output {max_output} exceeds the {MAX_CONFIGURED_OUTPUT}-byte repository ceiling"
        )));
    }
    // Checked, never saturating: saturation would turn an absurd configuration
    // into `usize::MAX` — a cap that can never be exceeded, i.e. silently no cap.
    let max_total = max_output
        .checked_mul(TOTAL_STREAM_FACTOR)
        .and_then(|scaled| scaled.checked_add(PROTOCOL_SLACK))
        .ok_or_else(|| protocol_error("stream byte cap overflowed"))?
        .min(MAX_STREAM_BYTES);
    Ok(StreamLimits {
        // A frame cap is a protocol property, not a budget property.
        max_payload: MAX_FRAME_PAYLOAD,
        max_total,
        max_capture: max_output,
    })
}

// ── worker side ─────────────────────────────────────────────────────────────

struct EmitterState<W: Write> {
    writer: W,
    sequence: u64,
    started: bool,
    terminal: bool,
    dropped: DroppedEvidence,
    stdout_budget: usize,
    stderr_budget: usize,
    reported: DroppedEvidence,
}

/// The worker's sequenced, bounded frame emitter.
///
/// Shared by the Brush engine's output callback and the worker's terminal path,
/// so one monotonic sequence covers every frame and exactly one terminal can be
/// written. Bytes beyond the configured budget are counted exactly rather than
/// flagged, and the running totals are announced on the stream as `truncated`
/// frames as well as carried in the terminal.
pub(crate) struct WorkerEmitter<W: Write + Send + 'static> {
    inner: Arc<Mutex<EmitterState<W>>>,
}

// Hand-written: `derive(Clone)` would require `W: Clone`, but cloning an
// emitter shares the one `Arc`-held writer and sequence — which is the whole
// point, since every clone must contribute to the SAME frame sequence.
impl<W: Write + Send + 'static> Clone for WorkerEmitter<W> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<W: Write + Send + 'static> WorkerEmitter<W> {
    pub(crate) fn new(writer: W, limits: StreamLimits) -> Self {
        Self {
            inner: Arc::new(Mutex::new(EmitterState {
                writer,
                sequence: 0,
                started: false,
                terminal: false,
                dropped: DroppedEvidence::default(),
                stdout_budget: limits.max_capture,
                stderr_budget: limits.max_capture,
                reported: DroppedEvidence::default(),
            })),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, EmitterState<W>> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn started(&self) -> Result<(), String> {
        let mut state = self.lock();
        if state.started || state.sequence != 0 {
            return Err("started must be the first frame, exactly once".to_string());
        }
        state.started = true;
        emit(&mut state, STARTED, &[])
    }

    /// Stream an output chunk, splitting it into frames and counting anything
    /// beyond the budget exactly. Never emits before `started`.
    pub(crate) fn output(&self, stream: crate::ShellOutputStream, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let mut state = self.lock();
        if !state.started || state.terminal {
            return;
        }
        let stdout = matches!(stream, crate::ShellOutputStream::Stdout);
        let budget = if stdout {
            &mut state.stdout_budget
        } else {
            &mut state.stderr_budget
        };
        let sendable = bytes.len().min(*budget);
        let omitted = bytes.len() - sendable;
        *budget -= sendable;
        if omitted > 0 {
            let omitted = omitted as u64;
            if stdout {
                state.dropped.stdout_bytes = state.dropped.stdout_bytes.saturating_add(omitted);
            } else {
                state.dropped.stderr_bytes = state.dropped.stderr_bytes.saturating_add(omitted);
            }
            state.dropped.events = state.dropped.events.saturating_add(1);
        }
        let kind = if stdout { STDOUT } else { STDERR };
        for chunk in bytes[..sendable].chunks(MAX_EMIT_CHUNK) {
            if emit(&mut state, kind, chunk).is_err() {
                return;
            }
        }
        if state.dropped != state.reported {
            state.reported = state.dropped;
            let payload = match serde_json::to_vec(&state.dropped) {
                Ok(payload) => payload,
                Err(_) => return,
            };
            let _ = emit(&mut state, TRUNCATED, &payload);
        }
    }

    /// Record that output was omitted for reasons the emitter cannot see — a
    /// drain the run had to detach. The byte count is unknowable, so only the
    /// event is counted; a fabricated byte total would be worse than none.
    pub(crate) fn note_omitted_stream(&self) {
        let mut state = self.lock();
        state.dropped.events = state.dropped.events.saturating_add(1);
    }

    pub(crate) fn denial(&self, denial: &Denial) -> Result<(), String> {
        let mut state = self.lock();
        if !state.started || state.terminal {
            return Err("a denial frame requires started and precedes terminal".to_string());
        }
        let payload = serde_json::to_vec(denial)
            .map_err(|error| format!("serialize worker denial: {error}"))?;
        emit(&mut state, DENIED, &payload)
    }

    /// Write the unique terminal frame, stamping the emitter's own exact drop
    /// accounting onto it.
    pub(crate) fn terminal(&self, mut response: WorkerResponse) -> Result<(), String> {
        let mut state = self.lock();
        if state.terminal {
            return Err("terminal must be written exactly once".to_string());
        }
        if !state.started && response.error.is_none() {
            return Err("a pre-start terminal must be an explicit worker error".to_string());
        }
        state.terminal = true;
        response.dropped = state.dropped;
        let payload = serde_json::to_vec(&response)
            .map_err(|error| format!("serialize worker terminal: {error}"))?;
        emit(&mut state, TERMINAL, &payload)
    }
}

fn emit<W: Write>(state: &mut EmitterState<W>, kind: u8, payload: &[u8]) -> Result<(), String> {
    if payload.len() > MAX_FRAME_PAYLOAD {
        return Err("worker frame payload exceeds the repository ceiling".to_string());
    }
    let len = u32::try_from(payload.len()).map_err(|_| "frame payload exceeds u32".to_string())?;
    let sequence = state.sequence;
    state.sequence = sequence
        .checked_add(1)
        .ok_or_else(|| "worker frame sequence overflowed".to_string())?;
    let mut header = [0_u8; HEADER_LEN];
    header[..4].copy_from_slice(&MAGIC);
    header[4] = VERSION;
    header[5] = kind;
    header[6..14].copy_from_slice(&sequence.to_le_bytes());
    header[14..18].copy_from_slice(&len.to_le_bytes());
    state
        .writer
        .write_all(&header)
        .and_then(|()| state.writer.write_all(payload))
        .and_then(|()| state.writer.flush())
        .map_err(|error| format!("write brush worker frame: {error}"))
}

/// Write a standalone terminal frame at sequence 0.
///
/// Only for the pre-authentication failure path, where no emitter exists
/// because no run ever started.
pub(crate) fn write_pre_start_terminal(
    writer: &mut impl Write,
    response: &WorkerResponse,
) -> Result<(), String> {
    let mut state = EmitterState {
        writer,
        sequence: 0,
        started: false,
        terminal: true,
        dropped: DroppedEvidence::default(),
        stdout_budget: 0,
        stderr_budget: 0,
        reported: DroppedEvidence::default(),
    };
    let payload = serde_json::to_vec(response)
        .map_err(|error| format!("serialize worker terminal: {error}"))?;
    emit(&mut state, TERMINAL, &payload)
}

// ── supervisor side ─────────────────────────────────────────────────────────

/// Read one complete worker stream, fail-closed on anything malformed.
///
/// `on_output` sees each live chunk as it arrives (presentation only). The
/// returned [`WorkerOutcome`] carries the accumulated transcript, which is the
/// single authoritative output history.
pub(crate) fn read_stream(
    mut reader: impl Read,
    limits: StreamLimits,
    mut on_output: impl FnMut(crate::ShellOutputStream, &[u8]),
) -> ToolResult<WorkerOutcome> {
    let mut expected_sequence = 0_u64;
    let mut total = 0_usize;
    let mut started = false;
    let mut streamed_denials = Vec::new();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut dropped = DroppedEvidence::default();

    loop {
        let Some((kind, sequence, payload)) = read_frame(&mut reader, limits.max_payload)? else {
            return Err(protocol_error(
                "worker stream ended before its terminal result",
            ));
        };
        total = total
            .checked_add(HEADER_LEN)
            .and_then(|value| value.checked_add(payload.len()))
            .ok_or_else(|| protocol_error("worker stream byte count overflowed"))?;
        if total > limits.max_total {
            return Err(protocol_error("worker stream exceeded its total byte cap"));
        }
        // Contiguous, not merely increasing: a gap or a duplicate is a protocol
        // error, so a dropped or replayed frame cannot pass unnoticed.
        if sequence != expected_sequence {
            return Err(protocol_error(format!(
                "expected sequence {expected_sequence}, received {sequence}"
            )));
        }
        expected_sequence = expected_sequence
            .checked_add(1)
            .ok_or_else(|| protocol_error("worker sequence overflowed"))?;

        match kind {
            STARTED => {
                if started || sequence != 0 || !payload.is_empty() {
                    return Err(protocol_error("invalid or duplicate started frame"));
                }
                started = true;
            }
            STDOUT | STDERR => {
                if !started {
                    return Err(protocol_error("output arrived before started"));
                }
                let stream = if kind == STDOUT {
                    crate::ShellOutputStream::Stdout
                } else {
                    crate::ShellOutputStream::Stderr
                };
                on_output(stream, &payload);
                let (buffer, counter) = if kind == STDOUT {
                    (&mut stdout, &mut dropped.stdout_bytes)
                } else {
                    (&mut stderr, &mut dropped.stderr_bytes)
                };
                let room = limits.max_capture.saturating_sub(buffer.len());
                let kept = payload.len().min(room);
                buffer.extend_from_slice(&payload[..kept]);
                if kept < payload.len() {
                    *counter = counter.saturating_add((payload.len() - kept) as u64);
                    dropped.events = dropped.events.saturating_add(1);
                }
            }
            DENIED => {
                if !started {
                    return Err(protocol_error("denial arrived before started"));
                }
                streamed_denials.push(
                    serde_json::from_slice::<Denial>(&payload)
                        .map_err(|error| protocol_error(format!("invalid denial: {error}")))?,
                );
            }
            TRUNCATED => {
                if !started {
                    return Err(protocol_error("truncation notice arrived before started"));
                }
                serde_json::from_slice::<DroppedEvidence>(&payload).map_err(|error| {
                    protocol_error(format!("invalid truncation notice: {error}"))
                })?;
            }
            TERMINAL => {
                let response = serde_json::from_slice::<WorkerResponse>(&payload)
                    .map_err(|error| protocol_error(format!("invalid terminal: {error}")))?;
                if !started && response.error.is_none() {
                    return Err(protocol_error(
                        "a pre-start terminal must be an explicit worker error",
                    ));
                }
                if started && response.denials != streamed_denials {
                    return Err(protocol_error(
                        "streamed denials do not match the terminal result",
                    ));
                }
                ensure_eof(&mut reader)?;
                return Ok(WorkerOutcome {
                    response,
                    stdout,
                    stderr,
                    dropped,
                });
            }
            _ => return Err(protocol_error(format!("unknown frame kind {kind}"))),
        }
    }
}

fn read_frame(
    reader: &mut impl Read,
    max_payload: usize,
) -> ToolResult<Option<(u8, u64, Vec<u8>)>> {
    let mut header = [0_u8; HEADER_LEN];
    let mut read = 0;
    while read < header.len() {
        match reader.read(&mut header[read..]) {
            Ok(0) if read == 0 => return Ok(None),
            Ok(0) => return Err(protocol_error("truncated frame header")),
            Ok(n) => read += n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(ToolError::from(error)),
        }
    }
    if header[..4] != MAGIC {
        return Err(protocol_error("frame magic mismatch"));
    }
    if header[4] != VERSION {
        return Err(protocol_error(format!(
            "unsupported frame version {}",
            header[4]
        )));
    }
    let sequence = u64::from_le_bytes(header[6..14].try_into().expect("fixed sequence field"));
    let len = u32::from_le_bytes(header[14..18].try_into().expect("fixed length field")) as usize;
    // Checked BEFORE the allocation: a hostile declared length must never size
    // a buffer.
    if len > max_payload {
        return Err(protocol_error("frame payload exceeded its cap"));
    }
    let mut payload = vec![0_u8; len];
    reader
        .read_exact(&mut payload)
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::UnexpectedEof => protocol_error("truncated frame payload"),
            _ => ToolError::from(error),
        })?;
    Ok(Some((header[5], sequence, payload)))
}

fn ensure_eof(reader: &mut impl Read) -> ToolResult<()> {
    let mut byte = [0_u8; 1];
    loop {
        match reader.read(&mut byte) {
            Ok(0) => return Ok(()),
            Ok(_) => return Err(protocol_error("bytes followed the terminal frame")),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(ToolError::from(error)),
        }
    }
}

fn protocol_error(message: impl std::fmt::Display) -> ToolError {
    ToolError::denied(format!("invalid brush worker event stream: {message}"))
}

#[cfg(test)]
mod tests;
