//! The backend-neutral managed execution lifecycle (#370, RFC 5a).
//!
//! Bridle already had an audited spawn funnel (`ConfinedCommand`) and a
//! presentation-only shell output observer, but no authoritative execution
//! contract: `Tool::invoke` returns a final value, and a raw confined child is
//! a substrate rather than a lifecycle. This module is that contract.
//!
//! # The shape of it
//!
//! - [`ExecutionRequest`] — mechanism inputs only. Executable, argv, cwd,
//!   explicit env, stdin, and physical limits. **No authority**: no caveats, no
//!   grant, no fence. Also no shell-engine selection — how a command line is
//!   interpreted is a leaf-tool decision, not an execution-location one.
//! - [`ExecutionEvent`] — `Accepted`, `Started`, `Stdout`, `Stderr`,
//!   `OutputTruncated`, `Denial`, and exactly one terminal (`Exited`, `Denied`,
//!   or `Failed`), under one strictly increasing per-execution sequence.
//! - [`ExecutionHandle`] — the consumer's stream plus idempotent `wait`,
//!   `cancel`, and `kill`.
//! - [`LocalExecutionBackend`] — the real implementation, which starts nothing
//!   without a [`crate::ToolContext`] and reaches the child only through
//!   `ConfinedCommand`'s admission → sandbox → `verify_applied` funnel.
//!
//! # Authority
//!
//! Starting an execution *requires* a `ToolContext`. There is no new
//! `ToolContext` constructor and no path to a child that bypasses
//! `ConfinedCommand::spawn`, `AdmittedFence::admit`, or `verify_applied`. The
//! identity of the fence that was admitted and applied is carried into the
//! `Started` event and the final evidence as [`FenceEvidence`] — copied out of
//! the funnel, never synthesized here.
//!
//! # Location
//!
//! The contract is backend-neutral so that a remote provider can implement it,
//! but only Local exists. A remote fence requires the sandbox-grain
//! identity/provenance binding of RFC 5b before it can exist at all, so there
//! is deliberately no remote variant to select and `ConfinedCommand` carries no
//! execution-location axis to route on.

mod contract;
mod local;
mod stream;
mod tree;

pub use contract::{
    DroppedEvidence, ExecutionEvent, ExecutionEventKind, ExecutionId, ExecutionLimits,
    ExecutionRequest, ExecutionStdin, ExecutionTerminal, ExitDisposition, ExitEvidence,
    FenceEvidence,
};
pub use contract::{
    MAX_ARGV_ENTRIES, MAX_ARG_BYTES, MAX_ENV_ENTRIES, MAX_OUTPUT_CHUNK_BYTES_CEILING,
    MAX_QUEUED_EVENTS_CEILING, MAX_QUEUED_OUTPUT_BYTES_CEILING, MAX_STDIN_BYTES_CEILING,
};
pub use local::{local_tree_containment, LocalExecutionBackend, LocalTreeContainment};
pub use stream::{
    execution_stream, ExecutionControl, ExecutionEmit, ExecutionEventSink, ExecutionHandle,
    OutputStream,
};

#[cfg(test)]
mod tests;
