//! The backend-neutral execution request/event/evidence vocabulary (#370).
//!
//! Nothing in this module carries authority. An [`ExecutionRequest`] names only
//! *mechanism inputs* — what to run, where, with which explicit environment,
//! which stdin bytes, and which physical resource bounds. The authority a
//! started execution is confined by comes from the [`crate::ToolContext`] the
//! caller must present at start time, never from the request.
//!
//! Every transport-facing type here is serializable, and every variable-sized
//! field deserializes through an explicit bound so a hostile peer cannot make a
//! receiver allocate without limit (the #372 `ProxyFinalEvidence` precedent).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::de::{self, Deserializer, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};

use crate::{AdmittedFenceId, Denial, SandboxKind, ToolError, ToolResult};

// ── Repository-defined hard ceilings ────────────────────────────────────────
//
// These are NOT defaults; they are the upper bounds a *configured* limit may
// not exceed. A caller (or a hostile peer supplying a serialized request) that
// asks for more is refused outright rather than silently clamped, so a limit
// can never be read back as larger than what is physically enforced.

/// The most events an execution stream may be configured to hold.
pub const MAX_QUEUED_EVENTS_CEILING: usize = 65_536;
/// The most queued output bytes an execution stream may be configured to hold.
pub const MAX_QUEUED_OUTPUT_BYTES_CEILING: usize = 64 * 1024 * 1024;
/// The largest single stdout/stderr chunk an execution may be configured to
/// deliver. Also the deserialization bound on one output event.
pub const MAX_OUTPUT_CHUNK_BYTES_CEILING: usize = 1024 * 1024;
/// The most stdin bytes an [`ExecutionRequest`] may carry.
pub const MAX_STDIN_BYTES_CEILING: usize = 16 * 1024 * 1024;
/// The most argv entries an [`ExecutionRequest`] may carry.
pub const MAX_ARGV_ENTRIES: usize = 4096;
/// The longest single argv entry, executable name, or environment value.
pub const MAX_ARG_BYTES: usize = 128 * 1024;
/// The most explicit environment entries an [`ExecutionRequest`] may carry.
pub const MAX_ENV_ENTRIES: usize = 4096;
/// The longest graceful-cancellation grace period that may be configured.
pub const MAX_GRACE: Duration = Duration::from_secs(600);

/// A process-unique identifier for one execution.
///
/// Unique within the emitting process only; it is a correlation key for the
/// event stream, never an authority or a cross-host identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExecutionId(pub u64);

impl ExecutionId {
    /// Allocate the next process-unique execution id.
    #[must_use]
    pub fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

impl std::fmt::Display for ExecutionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "exec-{}", self.0)
    }
}

/// What the child's stdin is connected to.
///
/// There is deliberately no `Inherit`: handing a confined child the parent's
/// stdin would delegate an ambient object capability the grant never named.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStdin {
    /// stdin is connected to an immediately-closed pipe (reads see EOF).
    #[default]
    Null,
    /// stdin receives exactly these bytes, then EOF.
    Bytes(#[serde(deserialize_with = "bounded_stdin")] Vec<u8>),
}

/// Physical bounds on one execution's buffering and lifecycle timing.
///
/// Every field is validated against a repository ceiling at construction, so a
/// configured limit is always a limit the stream physically enforces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "RawExecutionLimits", try_from = "RawExecutionLimits")]
pub struct ExecutionLimits {
    max_queued_events: usize,
    max_queued_output_bytes: usize,
    max_output_chunk_bytes: usize,
    cancel_grace: Duration,
    reap_grace: Duration,
}

/// The wire shape [`ExecutionLimits`] round-trips through, so that *every*
/// deserialized limit re-runs [`ExecutionLimits::new`]'s ceiling and overflow
/// checks. A hostile peer cannot hand a receiver a limit the stream will not
/// actually enforce.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct RawExecutionLimits {
    max_queued_events: usize,
    max_queued_output_bytes: usize,
    max_output_chunk_bytes: usize,
    cancel_grace_ms: u64,
    reap_grace_ms: u64,
}

impl TryFrom<RawExecutionLimits> for ExecutionLimits {
    type Error = String;

    fn try_from(raw: RawExecutionLimits) -> Result<Self, Self::Error> {
        Self::new(
            raw.max_queued_events,
            raw.max_queued_output_bytes,
            raw.max_output_chunk_bytes,
            Duration::from_millis(raw.cancel_grace_ms),
            Duration::from_millis(raw.reap_grace_ms),
        )
        .map_err(|e| e.to_string())
    }
}

impl From<ExecutionLimits> for RawExecutionLimits {
    fn from(limits: ExecutionLimits) -> Self {
        Self {
            max_queued_events: limits.max_queued_events,
            max_queued_output_bytes: limits.max_queued_output_bytes,
            max_output_chunk_bytes: limits.max_output_chunk_bytes,
            // Saturating, not wrapping: a grace period is ceiling-bounded at
            // construction, so this cannot lose information for any value that
            // `ExecutionLimits::new` admitted.
            cancel_grace_ms: u64::try_from(limits.cancel_grace.as_millis()).unwrap_or(u64::MAX),
            reap_grace_ms: u64::try_from(limits.reap_grace.as_millis()).unwrap_or(u64::MAX),
        }
    }
}

impl ExecutionLimits {
    /// Construct validated execution limits.
    ///
    /// Refuses a zero or above-ceiling value on any axis, and refuses a
    /// configuration whose worst-case queued footprint
    /// (`max_queued_events * max_output_chunk_bytes`) overflows `usize` — the
    /// multiplication-derived cap must be computed with checked arithmetic
    /// rather than wrapping into a small, apparently-safe number.
    pub fn new(
        max_queued_events: usize,
        max_queued_output_bytes: usize,
        max_output_chunk_bytes: usize,
        cancel_grace: Duration,
        reap_grace: Duration,
    ) -> ToolResult<Self> {
        check_bound(
            "max_queued_events",
            max_queued_events,
            MAX_QUEUED_EVENTS_CEILING,
        )?;
        check_bound(
            "max_queued_output_bytes",
            max_queued_output_bytes,
            MAX_QUEUED_OUTPUT_BYTES_CEILING,
        )?;
        check_bound(
            "max_output_chunk_bytes",
            max_output_chunk_bytes,
            MAX_OUTPUT_CHUNK_BYTES_CEILING,
        )?;
        if cancel_grace > MAX_GRACE || reap_grace > MAX_GRACE {
            return Err(ToolError::denied(format!(
                "execution grace periods exceed the {MAX_GRACE:?} ceiling"
            )));
        }
        // Checked, not wrapping: the worst-case footprint must be representable
        // before it is used to reason about memory.
        max_queued_events
            .checked_mul(max_output_chunk_bytes)
            .ok_or_else(|| {
                ToolError::denied(
                    "execution limits overflow: max_queued_events * max_output_chunk_bytes",
                )
            })?;
        Ok(Self {
            max_queued_events,
            max_queued_output_bytes,
            max_output_chunk_bytes,
            cancel_grace,
            reap_grace,
        })
    }

    /// Maximum non-terminal events held for a consumer.
    #[must_use]
    pub fn max_queued_events(&self) -> usize {
        self.max_queued_events
    }

    /// Maximum queued stdout+stderr bytes held for a consumer.
    #[must_use]
    pub fn max_queued_output_bytes(&self) -> usize {
        self.max_queued_output_bytes
    }

    /// Maximum bytes carried by one stdout/stderr event.
    #[must_use]
    pub fn max_output_chunk_bytes(&self) -> usize {
        self.max_output_chunk_bytes
    }

    /// How long [`crate::ExecutionHandle::cancel`] waits before escalating a
    /// graceful tree termination to a forced one.
    #[must_use]
    pub fn cancel_grace(&self) -> Duration {
        self.cancel_grace
    }

    /// How long the owner waits for the pipes to reach EOF after the direct
    /// child is reaped, before force-terminating surviving descendants that
    /// still hold a writer open.
    #[must_use]
    pub fn reap_grace(&self) -> Duration {
        self.reap_grace
    }
}

impl Default for ExecutionLimits {
    fn default() -> Self {
        Self::new(
            4096,
            8 * 1024 * 1024,
            64 * 1024,
            Duration::from_secs(5),
            Duration::from_secs(2),
        )
        .expect("the default execution limits are within the repository ceilings")
    }
}

fn check_bound(field: &str, value: usize, ceiling: usize) -> ToolResult<()> {
    if value == 0 {
        return Err(ToolError::denied(format!(
            "{field} must be greater than zero"
        )));
    }
    if value > ceiling {
        return Err(ToolError::denied(format!(
            "{field} = {value} exceeds the repository ceiling of {ceiling}"
        )));
    }
    Ok(())
}

/// The mechanism inputs for one execution.
///
/// Carries no authority: no caveats, no grant, no fence, and no shell-engine
/// selection. How a command line is *interpreted* is a leaf-tool decision; this
/// type describes an already-planned executable and argv.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionRequest {
    /// The program to execute. Subject to the caller's exec admission at start.
    pub executable: String,
    /// Arguments after `argv[0]`.
    #[serde(default, deserialize_with = "bounded_argv")]
    pub argv: Vec<String>,
    /// Working directory, or the caller's when `None`.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// The explicit environment. The confined spawn clears the ambient
    /// environment; only these entries cross the boundary.
    #[serde(default, deserialize_with = "bounded_env")]
    pub env: Vec<(String, String)>,
    /// What stdin is connected to.
    #[serde(default)]
    pub stdin: ExecutionStdin,
    /// Physical buffering and lifecycle bounds.
    #[serde(default)]
    pub limits: ExecutionLimits,
}

impl ExecutionRequest {
    /// Build a request for `executable` with default limits and no stdin.
    #[must_use]
    pub fn new(executable: impl Into<String>) -> Self {
        Self {
            executable: executable.into(),
            argv: Vec::new(),
            cwd: None,
            env: Vec::new(),
            stdin: ExecutionStdin::Null,
            limits: ExecutionLimits::default(),
        }
    }

    /// Append one argument.
    #[must_use]
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.argv.push(arg.into());
        self
    }

    /// Append several arguments.
    #[must_use]
    pub fn args<I: IntoIterator<Item = S>, S: Into<String>>(mut self, args: I) -> Self {
        self.argv.extend(args.into_iter().map(Into::into));
        self
    }

    /// Set the working directory.
    #[must_use]
    pub fn cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    /// Add one explicit environment entry.
    #[must_use]
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Set the stdin source.
    #[must_use]
    pub fn stdin(mut self, stdin: ExecutionStdin) -> Self {
        self.stdin = stdin;
        self
    }

    /// Set the physical limits.
    #[must_use]
    pub fn limits(mut self, limits: ExecutionLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Refuse a request whose in-memory shape already exceeds a hard ceiling.
    ///
    /// Applied by a backend before it does any work, so the same bound holds
    /// for a locally-built request and for one that arrived over a transport.
    pub fn validate(&self) -> ToolResult<()> {
        if self.executable.is_empty() {
            return Err(ToolError::denied("execution request names no executable"));
        }
        if self.executable.len() > MAX_ARG_BYTES {
            return Err(ToolError::denied(
                "execution executable exceeds the argument bound",
            ));
        }
        if self.argv.len() > MAX_ARGV_ENTRIES {
            return Err(ToolError::denied(
                "execution argv exceeds the argument-count bound",
            ));
        }
        if self.argv.iter().any(|a| a.len() > MAX_ARG_BYTES) {
            return Err(ToolError::denied(
                "an execution argument exceeds the argument bound",
            ));
        }
        if self.env.len() > MAX_ENV_ENTRIES {
            return Err(ToolError::denied(
                "execution env exceeds the entry-count bound",
            ));
        }
        if self
            .env
            .iter()
            .any(|(k, v)| k.len() > MAX_ARG_BYTES || v.len() > MAX_ARG_BYTES)
        {
            return Err(ToolError::denied(
                "an execution env entry exceeds the argument bound",
            ));
        }
        if let ExecutionStdin::Bytes(bytes) = &self.stdin {
            if bytes.len() > MAX_STDIN_BYTES_CEILING {
                return Err(ToolError::denied("execution stdin exceeds the stdin bound"));
            }
        }
        Ok(())
    }
}

/// Exact accounting of what a bounded stream could not deliver.
///
/// Deliberately not a boolean: a consumer that stalls must be able to say how
/// many bytes of each stream, and how many events, it did not see.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DroppedEvidence {
    /// Stdout bytes read from the OS pipe but never queued.
    pub stdout_bytes: u64,
    /// Stderr bytes read from the OS pipe but never queued.
    pub stderr_bytes: u64,
    /// Events (of any kind) that could not be queued.
    pub events: u64,
}

impl DroppedEvidence {
    /// Whether anything at all was omitted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.stdout_bytes == 0 && self.stderr_bytes == 0 && self.events == 0
    }
}

/// What the admitted fence proved about a started execution.
///
/// Every field is copied out of the object the spawn funnel actually admitted
/// and applied — nothing here is synthesized by the execution layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FenceEvidence {
    /// The content-addressed identity of the fence admitted for this spawn.
    pub fence_id: AdmittedFenceId,
    /// The OS confinement actually in force around the child.
    pub sandbox_kind: SandboxKind,
    /// Whether the child's egress was fenced through the loopback proxy.
    pub egress_proxied: bool,
    /// Actual verified NamedRoot body; absent for the ordinary operation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admitted: Option<Box<crate::AdmittedFenceBody>>,
}

impl FenceEvidence {
    /// Check content integrity and binding to the trusted invocation's root and
    /// effective authority. A local CID/ExecutionId is not sender authentication.
    pub fn verify_named_root(
        &self,
        expected_root: &str,
        effective: &crate::Caveats,
        protected_roots: &std::collections::BTreeSet<String>,
    ) -> crate::ToolResult<&crate::AdmittedFenceBody> {
        let body = self.admitted.as_ref().ok_or_else(|| {
            crate::ToolError::denied("named-root evidence lacks its admitted body")
        })?;
        if body.fence_id()? != self.fence_id
            || body.mechanism.kind() != self.sandbox_kind
            || self.egress_proxied
        {
            return Err(crate::ToolError::denied(
                "named-root body does not match its fence CID or actual backend",
            ));
        }
        body.verify(expected_root, effective, protected_roots)?;
        Ok(body)
    }
}

/// How the process tree reached its terminal state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitDisposition {
    /// The direct child exited on its own and no descendant had to be reaped.
    Natural,
    /// A graceful cancellation request terminated the tree.
    Cancelled,
    /// A forced kill terminated the tree.
    Killed,
    /// The direct child exited, but surviving descendants still held an output
    /// writer open and were force-terminated so the stream could reach EOF.
    DescendantsReaped,
}

/// The terminal record for one execution. Exactly one is produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionTerminal {
    /// The process tree ran and is fully terminated, reaped, drained, and (if
    /// a proxy was in force) quiesced.
    Exited(Box<ExitEvidence>),
    /// Bridle refused the execution. No process tree was ever acquired.
    Denied {
        /// The structured refusal.
        denial: Denial,
    },
    /// The execution mechanism failed, independently of a policy denial.
    ///
    /// A proxy that could not be finalized lands here: an execution whose
    /// egress evidence is not quiescent is a failure, never an apparently
    /// successful exit.
    Failed {
        /// Bounded, operator-safe failure text.
        #[serde(deserialize_with = "bounded_message")]
        message: String,
    },
}

impl ExecutionTerminal {
    /// The exit evidence when this execution actually ran to completion.
    #[must_use]
    pub fn exit_evidence(&self) -> Option<&ExitEvidence> {
        match self {
            Self::Exited(evidence) => Some(evidence),
            _ => None,
        }
    }

    /// Whether this terminal reports a completed process tree.
    #[must_use]
    pub fn is_exited(&self) -> bool {
        matches!(self, Self::Exited(_))
    }
}

/// Quiescent final evidence for an execution that ran.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitEvidence {
    /// Platform exit code, or `None` for a signal-only exit.
    pub code: Option<i32>,
    /// The terminating signal on Unix, when the child died by signal.
    pub signal: Option<i32>,
    /// How the tree reached this state.
    pub disposition: ExitDisposition,
    /// The fence this execution actually ran under.
    pub fence: FenceEvidence,
    /// Exact accounting of anything the bounded stream omitted.
    pub dropped: DroppedEvidence,
    /// Frozen egress evidence from [`crate::ProxyHandle::shutdown_and_join`],
    /// present only when a proxy fenced this execution. Its presence here means
    /// the proxy was joined to quiescence *before* this terminal was published.
    pub proxy: Option<crate::net_proxy::ProxyFinalEvidence>,
}

/// One item in the backend-neutral execution event stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionEvent {
    /// Which execution produced this event.
    pub execution: ExecutionId,
    /// Strictly increasing, contiguous over the events actually delivered on
    /// this stream. Events the bounded queue could not accept are counted in
    /// [`DroppedEvidence`] rather than consuming a sequence number, so a gap in
    /// this sequence is a protocol error, not backpressure.
    pub sequence: u64,
    /// The payload.
    pub kind: ExecutionEventKind,
}

impl ExecutionEvent {
    /// Whether this event is the stream's unique terminal.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.kind,
            ExecutionEventKind::Exited(_)
                | ExecutionEventKind::Denied { .. }
                | ExecutionEventKind::Failed { .. }
        )
    }
}

/// The payload of an [`ExecutionEvent`].
///
/// `Accepted` is emitted before any spawn is attempted. `Started` is emitted
/// only once a real process tree has been acquired, and carries the fence the
/// spawn funnel actually admitted. Exactly one of `Exited`, `Denied`, or
/// `Failed` ever appears, and it is always the last event on the stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionEventKind {
    /// The backend accepted a well-formed request and is about to seek
    /// admission for it. No process exists yet.
    Accepted,
    /// A real process tree was acquired under a proven fence.
    Started {
        /// The direct child's pid.
        pid: u32,
        /// The fence admitted and applied for this spawn.
        fence: FenceEvidence,
    },
    /// An ordered stdout chunk. Chunks may split UTF-8 code points; per-fd read
    /// order is preserved.
    Stdout(#[serde(deserialize_with = "bounded_chunk")] Vec<u8>),
    /// An ordered stderr chunk.
    Stderr(#[serde(deserialize_with = "bounded_chunk")] Vec<u8>),
    /// The bounded stream omitted output or events. Carries the exact running
    /// totals at the moment it was emitted; the terminal carries the final
    /// totals, which are authoritative.
    OutputTruncated(DroppedEvidence),
    /// A structured, non-terminal denial observed during execution.
    Denial(Denial),
    /// Terminal: the process tree completed, drained, and quiesced.
    Exited(Box<ExitEvidence>),
    /// Terminal: refused before a process tree existed.
    Denied {
        /// The structured refusal.
        denial: Denial,
    },
    /// Terminal: the mechanism failed.
    Failed {
        /// Bounded, operator-safe failure text.
        #[serde(deserialize_with = "bounded_message")]
        message: String,
    },
}

// ── Hostile-deserialization bounds ──────────────────────────────────────────

fn bounded_chunk<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
    let bytes = serde_bytes_vec(d)?;
    if bytes.len() > MAX_OUTPUT_CHUNK_BYTES_CEILING {
        return Err(de::Error::custom(format!(
            "output chunk exceeds the {MAX_OUTPUT_CHUNK_BYTES_CEILING}-byte bound"
        )));
    }
    Ok(bytes)
}

fn bounded_stdin<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
    let bytes = serde_bytes_vec(d)?;
    if bytes.len() > MAX_STDIN_BYTES_CEILING {
        return Err(de::Error::custom(format!(
            "stdin exceeds the {MAX_STDIN_BYTES_CEILING}-byte bound"
        )));
    }
    Ok(bytes)
}

/// Deserialize a byte sequence, refusing an over-long one *as the offending
/// element is seen* rather than after materializing the whole hostile array.
fn serde_bytes_vec<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
    struct BoundedBytes;
    impl<'de> Visitor<'de> for BoundedBytes {
        type Value = Vec<u8>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "at most {MAX_STDIN_BYTES_CEILING} bytes")
        }

        fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<Self::Value, E> {
            if v.len() > MAX_STDIN_BYTES_CEILING {
                return Err(E::custom("byte payload exceeds the repository bound"));
            }
            Ok(v.to_vec())
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let hint = seq.size_hint().unwrap_or(0).min(MAX_STDIN_BYTES_CEILING);
            let mut out = Vec::with_capacity(hint);
            while let Some(byte) = seq.next_element::<u8>()? {
                if out.len() >= MAX_STDIN_BYTES_CEILING {
                    return Err(de::Error::custom(
                        "byte payload exceeds the repository bound",
                    ));
                }
                out.push(byte);
            }
            Ok(out)
        }
    }
    d.deserialize_byte_buf(BoundedBytes)
}

fn bounded_message<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let s = String::deserialize(d)?;
    if s.len() > MAX_ARG_BYTES {
        return Err(de::Error::custom(format!(
            "message exceeds the {MAX_ARG_BYTES}-byte bound"
        )));
    }
    Ok(s)
}

fn bounded_argv<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    bounded_strings(d, MAX_ARGV_ENTRIES, "argv")
}

fn bounded_strings<'de, D: Deserializer<'de>>(
    d: D,
    max: usize,
    what: &'static str,
) -> Result<Vec<String>, D::Error> {
    struct BoundedStrings(usize, &'static str);
    impl<'de> Visitor<'de> for BoundedStrings {
        type Value = Vec<String>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "at most {} {} entries", self.0, self.1)
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(self.0));
            while let Some(item) = seq.next_element::<String>()? {
                if out.len() >= self.0 {
                    return Err(de::Error::custom(format!(
                        "{} exceeds the {}-entry bound",
                        self.1, self.0
                    )));
                }
                if item.len() > MAX_ARG_BYTES {
                    return Err(de::Error::custom(format!(
                        "{} entry exceeds the {MAX_ARG_BYTES}-byte bound",
                        self.1
                    )));
                }
                out.push(item);
            }
            Ok(out)
        }
    }
    d.deserialize_seq(BoundedStrings(max, what))
}

fn bounded_env<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<(String, String)>, D::Error> {
    struct BoundedEnv;
    impl<'de> Visitor<'de> for BoundedEnv {
        type Value = Vec<(String, String)>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "at most {MAX_ENV_ENTRIES} environment entries")
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(MAX_ENV_ENTRIES));
            while let Some((k, v)) = seq.next_element::<(String, String)>()? {
                if out.len() >= MAX_ENV_ENTRIES {
                    return Err(de::Error::custom(format!(
                        "env exceeds the {MAX_ENV_ENTRIES}-entry bound"
                    )));
                }
                if k.len() > MAX_ARG_BYTES || v.len() > MAX_ARG_BYTES {
                    return Err(de::Error::custom(format!(
                        "env entry exceeds the {MAX_ARG_BYTES}-byte bound"
                    )));
                }
                out.push((k, v));
            }
            Ok(out)
        }
    }
    d.deserialize_seq(BoundedEnv)
}

// Model: gpt-6-astra | Harness: Codex 0.153.4 | Operator: Shawn Hartsock | Time: 22:29 UTC | Date: 2026-09-12
