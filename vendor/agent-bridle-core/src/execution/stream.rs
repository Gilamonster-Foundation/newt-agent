//! The physically-bounded execution event stream (#370 "bounded buffering").
//!
//! The queue is a `VecDeque` behind one mutex, bounded by **both** a queued
//! event count and a queued output-byte total. It is deliberately not an
//! `mpsc::channel()` behind logical counters: an unbounded channel with a
//! byte counter still lets a stalled consumer accumulate unbounded allocations
//! in the channel itself, so the "limit" would describe a number rather than
//! memory.
//!
//! Three properties make the bound safe rather than lossy-by-surprise:
//!
//! 1. **Producers never block.** A drainer that cannot queue keeps reading the
//!    OS pipe, so a stalled consumer can never deadlock the child by filling
//!    its pipe buffer.
//! 2. **Drops are counted exactly.** Every byte read but not queued is added to
//!    [`DroppedEvidence`], per stream, and surfaced both as an
//!    `OutputTruncated` event and in the terminal record.
//! 3. **The terminal is out of band.** It lives in a reserved slot that the
//!    count/byte bounds do not govern, so a full output queue can never lose
//!    the lifecycle result.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use super::contract::{
    DroppedEvidence, ExecutionEvent, ExecutionEventKind, ExecutionId, ExecutionLimits,
    ExecutionTerminal, FenceEvidence,
};
use crate::{Denial, ToolError, ToolResult};

/// Which pipe an output chunk came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStream {
    /// The child's stdout.
    Stdout,
    /// The child's stderr.
    Stderr,
}

/// What the bounded queue did with an emitted event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionEmit {
    /// The event was queued for the consumer.
    Delivered,
    /// The event exceeded a physical bound and was counted as dropped. The
    /// producer must continue draining regardless.
    Dropped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Accepted,
    Started,
    Terminal,
}

struct QueueState {
    events: VecDeque<ExecutionEvent>,
    queued_output_bytes: usize,
    dropped: DroppedEvidence,
    /// The totals the most recent `OutputTruncated` notice reported. Anything
    /// dropped beyond this is still unreported on the stream.
    reported_dropped: DroppedEvidence,
    /// A reserved slot for the FINAL truncation notice, delivered just before
    /// the terminal. Without it, a consumer that stalls until the child exits
    /// would never be told: the queue stays full for the whole run, so the
    /// mid-stream notice never finds room, and the stream would end with the
    /// drops recorded only in the terminal evidence.
    final_notice: Option<ExecutionEvent>,
    /// The reserved terminal slot — never subject to the count/byte bounds.
    terminal: Option<ExecutionEvent>,
    next_sequence: u64,
    phase: Phase,
    consumer_alive: bool,
}

/// The producer half of one execution's bounded event stream.
///
/// Cloneable: the stdout drainer, the stderr drainer, and the owning reaper all
/// hold one. The single mutex inside is the global sequencer — the order events
/// acquire it *is* the observed cross-stream order.
#[derive(Clone)]
pub struct ExecutionEventSink {
    inner: Arc<StreamInner>,
}

struct StreamInner {
    id: ExecutionId,
    limits: ExecutionLimits,
    state: Mutex<QueueState>,
    ready: Condvar,
}

impl ExecutionEventSink {
    fn lock(&self) -> MutexGuard<'_, QueueState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Whether a consumer still holds the handle.
    #[must_use]
    pub fn consumer_alive(&self) -> bool {
        self.lock().consumer_alive
    }

    /// Emit the pre-spawn `Accepted` event. Exactly once, before any spawn.
    pub fn accepted(&self) -> ToolResult<ExecutionEmit> {
        let mut state = self.lock();
        if state.phase != Phase::Accepted || state.next_sequence != 0 {
            return Err(protocol("accepted must be the first event"));
        }
        Ok(self.push(&mut state, ExecutionEventKind::Accepted))
    }

    /// Emit `Started` — only legal once a real process tree exists.
    pub fn started(&self, pid: u32, fence: FenceEvidence) -> ToolResult<ExecutionEmit> {
        let mut state = self.lock();
        if state.phase != Phase::Accepted {
            return Err(protocol("started must follow accepted exactly once"));
        }
        state.phase = Phase::Started;
        Ok(self.push(&mut state, ExecutionEventKind::Started { pid, fence }))
    }

    /// Emit an ordered output chunk. Never blocks; a chunk that does not fit is
    /// counted exactly and dropped.
    pub fn output(&self, stream: OutputStream, bytes: &[u8]) -> ToolResult<ExecutionEmit> {
        if bytes.is_empty() {
            return Ok(ExecutionEmit::Delivered);
        }
        let mut state = self.lock();
        if state.phase != Phase::Started {
            return Err(protocol("output requires a prior started event"));
        }
        // A chunk longer than the configured chunk bound is a producer bug: the
        // drainer reads in chunk-sized reads. Refuse rather than silently split.
        if bytes.len() > self.inner.limits.max_output_chunk_bytes() {
            return Err(protocol("output chunk exceeds the configured chunk bound"));
        }
        let fits = state.events.len() < self.inner.limits.max_queued_events()
            && state
                .queued_output_bytes
                .checked_add(bytes.len())
                .is_some_and(|total| total <= self.inner.limits.max_queued_output_bytes());
        if !fits {
            self.record_drop(&mut state, stream, bytes.len());
            return Ok(ExecutionEmit::Dropped);
        }
        self.flush_truncation_notice(&mut state);
        // Re-check after the notice consumed a slot; the notice must never push
        // the queue past its bound.
        if state.events.len() >= self.inner.limits.max_queued_events() {
            self.record_drop(&mut state, stream, bytes.len());
            return Ok(ExecutionEmit::Dropped);
        }
        state.queued_output_bytes += bytes.len();
        let kind = match stream {
            OutputStream::Stdout => ExecutionEventKind::Stdout(bytes.to_vec()),
            OutputStream::Stderr => ExecutionEventKind::Stderr(bytes.to_vec()),
        };
        Ok(self.push(&mut state, kind))
    }

    /// Emit a structured, non-terminal denial observed during execution.
    pub fn denial(&self, denial: Denial) -> ToolResult<ExecutionEmit> {
        let mut state = self.lock();
        if state.phase == Phase::Terminal {
            return Err(protocol("no event may follow the terminal"));
        }
        if state.events.len() >= self.inner.limits.max_queued_events() {
            state.dropped.events = state.dropped.events.saturating_add(1);
            return Ok(ExecutionEmit::Dropped);
        }
        Ok(self.push(&mut state, ExecutionEventKind::Denial(denial)))
    }

    /// Publish the unique terminal record into its reserved slot.
    ///
    /// The caller is responsible for having already reached quiescence: the
    /// process tree reaped, both pipes at EOF, and any proxy joined. This
    /// method only enforces that it happens exactly once and last.
    pub fn publish_terminal(&self, terminal: ExecutionTerminal) -> ToolResult<()> {
        let mut state = self.lock();
        if state.phase == Phase::Terminal {
            return Err(protocol("terminal must be published exactly once"));
        }
        if state.phase == Phase::Accepted && terminal.is_exited() {
            return Err(protocol("an exited terminal requires a started event"));
        }
        state.phase = Phase::Terminal;
        // Guaranteed reporting: if anything was dropped that no notice covered,
        // the exact totals go out in a reserved slot immediately before the
        // terminal — so an observer is always told, even when the queue was
        // full for the entire run.
        if state.dropped != state.reported_dropped {
            state.reported_dropped = state.dropped;
            let dropped = state.dropped;
            let sequence = state.next_sequence;
            state.next_sequence = sequence.saturating_add(1);
            state.final_notice = Some(ExecutionEvent {
                execution: self.inner.id,
                sequence,
                kind: ExecutionEventKind::OutputTruncated(dropped),
            });
        }
        let sequence = state.next_sequence;
        state.next_sequence = sequence.saturating_add(1);
        let kind = match terminal {
            ExecutionTerminal::Exited(evidence) => ExecutionEventKind::Exited(evidence),
            ExecutionTerminal::Denied { denial } => ExecutionEventKind::Denied { denial },
            ExecutionTerminal::Failed { message } => ExecutionEventKind::Failed { message },
        };
        state.terminal = Some(ExecutionEvent {
            execution: self.inner.id,
            sequence,
            kind,
        });
        self.inner.ready.notify_all();
        Ok(())
    }

    /// The exact drop totals observed so far.
    #[must_use]
    pub fn dropped(&self) -> DroppedEvidence {
        self.lock().dropped
    }

    fn record_drop(&self, state: &mut QueueState, stream: OutputStream, len: usize) {
        let len = len as u64;
        match stream {
            OutputStream::Stdout => {
                state.dropped.stdout_bytes = state.dropped.stdout_bytes.saturating_add(len);
            }
            OutputStream::Stderr => {
                state.dropped.stderr_bytes = state.dropped.stderr_bytes.saturating_add(len);
            }
        }
        state.dropped.events = state.dropped.events.saturating_add(1);
    }

    /// Queue a pending `OutputTruncated` notice if there is room for it.
    fn flush_truncation_notice(&self, state: &mut QueueState) {
        if state.dropped == state.reported_dropped {
            return;
        }
        if state.events.len() >= self.inner.limits.max_queued_events() {
            return;
        }
        state.reported_dropped = state.dropped;
        let dropped = state.dropped;
        self.push(state, ExecutionEventKind::OutputTruncated(dropped));
    }

    fn push(&self, state: &mut QueueState, kind: ExecutionEventKind) -> ExecutionEmit {
        let sequence = state.next_sequence;
        state.next_sequence = sequence.saturating_add(1);
        state.events.push_back(ExecutionEvent {
            execution: self.inner.id,
            sequence,
            kind,
        });
        self.inner.ready.notify_all();
        ExecutionEmit::Delivered
    }
}

fn protocol(message: &str) -> ToolError {
    ToolError::denied(format!("invalid execution event sequence: {message}"))
}

/// Backend-owned lifecycle controls for one execution.
///
/// `wait` must be idempotent and return byte-identical terminal evidence to
/// every caller. `abandon` must leave nothing detached.
pub trait ExecutionControl: Send + Sync + 'static {
    /// Block until the unique terminal record exists, then return a clone of
    /// it. Independent of event consumption.
    fn wait(&self) -> ToolResult<ExecutionTerminal>;

    /// Request graceful termination of the whole process tree, escalating to a
    /// forced kill after the configured grace period.
    fn cancel(&self) -> ToolResult<()>;

    /// Immediately and forcibly terminate the whole process tree.
    fn kill(&self) -> ToolResult<()>;

    /// The consumer dropped its handle before observing the terminal.
    ///
    /// Must terminate and join the tree rather than detaching it.
    fn abandon(&self);
}

/// Consumer-owned event stream and lifecycle handle for one execution.
///
/// Dropping this handle before the terminal has been observed is a fail-closed
/// operation: the backend terminates the process tree and joins it, so a
/// forgotten execution can never leave a detached child or grandchild behind.
pub struct ExecutionHandle {
    id: ExecutionId,
    inner: Arc<StreamInner>,
    control: Arc<dyn ExecutionControl>,
    terminal_observed: bool,
}

impl ExecutionHandle {
    /// The id correlating every event on this stream.
    #[must_use]
    pub fn id(&self) -> ExecutionId {
        self.id
    }

    /// The physical bounds this stream enforces.
    #[must_use]
    pub fn limits(&self) -> ExecutionLimits {
        self.inner.limits
    }

    /// Block until the next event. Returns `None` once the terminal has been
    /// observed; the terminal is always the last event returned.
    pub fn next_event(&mut self) -> Option<ExecutionEvent> {
        if self.terminal_observed {
            return None;
        }
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if let Some(event) = state.events.pop_front() {
                self.release(&mut state, &event);
                return Some(event);
            }
            // Only once the queued events are exhausted do the reserved slots
            // become visible — so the terminal can never be observed before
            // output that was actually delivered, and the final truncation
            // notice always precedes it.
            if let Some(event) = state.final_notice.take() {
                return Some(event);
            }
            if let Some(event) = state.terminal.take() {
                self.terminal_observed = true;
                return Some(event);
            }
            state = self
                .inner
                .ready
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// Take the next event without blocking.
    pub fn try_next_event(&mut self) -> Option<ExecutionEvent> {
        if self.terminal_observed {
            return None;
        }
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(event) = state.events.pop_front() {
            self.release(&mut state, &event);
            return Some(event);
        }
        if let Some(event) = state.final_notice.take() {
            return Some(event);
        }
        if let Some(event) = state.terminal.take() {
            self.terminal_observed = true;
            return Some(event);
        }
        None
    }

    fn release(&self, state: &mut QueueState, event: &ExecutionEvent) {
        let freed = match &event.kind {
            ExecutionEventKind::Stdout(b) | ExecutionEventKind::Stderr(b) => b.len(),
            _ => 0,
        };
        state.queued_output_bytes = state.queued_output_bytes.saturating_sub(freed);
    }

    /// Wait for the terminal record, independently of event consumption.
    ///
    /// Idempotent: every call returns the same cached evidence.
    pub fn wait(&self) -> ToolResult<ExecutionTerminal> {
        self.control.wait()
    }

    /// Request graceful termination of the process tree, escalating to a
    /// forced kill after the configured grace period. Idempotent.
    pub fn cancel(&self) -> ToolResult<()> {
        self.control.cancel()
    }

    /// Forcibly terminate the whole process tree now. Idempotent.
    pub fn kill(&self) -> ToolResult<()> {
        self.control.kill()
    }
}

impl Drop for ExecutionHandle {
    fn drop(&mut self) {
        {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.consumer_alive = false;
            // A terminal already sitting in the reserved slot means the
            // execution reached quiescence on its own; nothing to tear down.
            if state.terminal.is_some() {
                self.terminal_observed = true;
            }
        }
        if !self.terminal_observed {
            self.control.abandon();
        }
    }
}

/// Create the bounded producer/consumer pair for one execution.
#[must_use]
pub fn execution_stream(
    id: ExecutionId,
    limits: ExecutionLimits,
    control: Arc<dyn ExecutionControl>,
) -> (ExecutionEventSink, ExecutionHandle) {
    let inner = Arc::new(StreamInner {
        id,
        limits,
        state: Mutex::new(QueueState {
            events: VecDeque::new(),
            queued_output_bytes: 0,
            dropped: DroppedEvidence::default(),
            reported_dropped: DroppedEvidence::default(),
            final_notice: None,
            terminal: None,
            next_sequence: 0,
            phase: Phase::Accepted,
            consumer_alive: true,
        }),
        ready: Condvar::new(),
    });
    let sink = ExecutionEventSink {
        inner: Arc::clone(&inner),
    };
    let handle = ExecutionHandle {
        id,
        inner,
        control,
        terminal_observed: false,
    };
    (sink, handle)
}
