//! Spawn an **arbitrary** child process confined by a [`ToolContext`]'s caveats.
//!
//! The in-process leash (L2) gates operations the bridle process can observe.
//! But a host often needs to launch a separate program — an MCP capability
//! server, a language runtime — and put *its own syscalls* under the leash. L2
//! cannot follow a child across a process boundary; that requires an available
//! native L3 backend ([`crate::sandbox`]).
//!
//! [`ConfinedCommand`] is that primitive. It is deliberately *not* a confused
//! deputy: the parent attenuates **before** the spawn (the child is never trusted
//! to confine itself), the environment is **cleared** so nothing ambient leaks
//! (only explicitly-granted vars reach the child — the external-boundary
//! invariant), and exec is admission-checked against the granted `exec` scope.
//!
//! Mechanism (mirrors [`crate::sandbox`]'s contract): thread-confining backends
//! such as Landlock are applied on a fresh throwaway thread immediately before
//! spawn; wrapper backends such as Seatbelt and AppContainer prefix the child
//! launch. In either case the confined child and its descendants inherit the
//! active OS boundary.
//!
//! Honesty & fail-closed: the achieved [`SandboxKind`] is returned on the
//! [`ConfinedChild`]. A restricted filesystem axis that no active backend can
//! kernel-enforce is **refused** rather than launched unconfined. Restricted
//! `exec` and `net` axes are checked against the principal's requested strength
//! floor because their kernel coverage differs by backend and scope. The
//! per-axis enforcement report is the authoritative statement of what held.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use std::sync::Arc;
use std::time::Duration;

use crate::{
    best_available_sandbox, effective_sandbox_kind, unenforceable_axis, AdmittedFence,
    AdmittedFenceBody, AdmittedFenceId, AxisEnforcement, BackendProjection, Caveats,
    ConfinementMechanism, EnforcementFloor, RuntimeClosure, SandboxKind, SandboxPolicy, Scope,
    ToolContext, ToolError, ToolResult,
};
use agent_mesh_protocol::Fingerprint;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// A spawned child together with the OS sandbox actually in force around it.
///
/// The caller owns `child` (it does its own `wait`/`kill`/pipe plumbing).
/// `sandbox_kind` is the honest record of what confinement was achieved —
/// [`SandboxKind::None`] means the leash on this child is advisory only.
#[derive(Debug)]
pub struct ConfinedChild {
    /// The spawned process.
    pub child: Child,
    /// The OS-level sandbox actually applied to the child.
    pub sandbox_kind: SandboxKind,
    /// The content-addressed identity of the fence this spawn was **admitted**
    /// under and that `verify_applied` confirmed at the admit→apply handoff.
    ///
    /// Carried out of the funnel rather than recomputed by a caller: an
    /// execution layer that wants to put fence identity in its evidence must
    /// report the object this spawn actually ran under, never one it derived
    /// for itself afterwards (#370 "preserve `AdmittedFenceId`").
    pub fence_id: AdmittedFenceId,
    /// The inspectable body verified at the actual NamedRoot launch boundary.
    pub admitted: Option<AdmittedFenceBody>,
}

/// A confined child together with **every** fence whose lifetime brackets it.
///
/// [`ConfinedChild`] hands back the process and the identity it was admitted
/// under, but a child under a proxied-net grant is also fenced by a live
/// loopback egress proxy whose own finalization produces the authoritative
/// egress evidence (#372/#374). A managed execution owner must hold that proxy
/// for exactly as long as the child and join it to quiescence before it may
/// call any result final — so the funnel hands the proxy out here rather than
/// dropping it on a detached teardown thread.
#[derive(Debug)]
pub struct ManagedSpawn {
    /// The spawned process.
    pub child: Child,
    /// The OS-level sandbox actually applied to the child.
    pub sandbox_kind: SandboxKind,
    /// The identity of the admitted, verified-applied fence.
    pub fence_id: AdmittedFenceId,
    /// Inspectable NamedRoot proof transported from the actual spawn.
    pub admitted: Option<AdmittedFenceBody>,
    /// The live egress proxy fencing this child's network, when the grant
    /// engaged one. The receiver owns it and **must** finalize it with
    /// [`crate::ProxyHandle::shutdown_and_join`] before publishing a terminal.
    pub proxy: Option<crate::net_proxy::ProxyHandle>,
}

/// A fixed internal worker together with its take-once parent control channel.
///
/// Unlike an ordinary [`ConfinedChild`], a trusted worker is launched with a
/// kernel object that model-selected commands do not receive. The worker
/// validates that channel and its peer before accepting any authority-bearing
/// request. The channel is private by default and can be taken only once by the
/// trusted supervisor.
#[derive(Debug)]
pub struct SandboxedWorkerChild {
    /// The spawned worker process.
    pub child: Child,
    /// The OS-level sandbox actually applied to the worker.
    pub sandbox_kind: SandboxKind,
    control: Option<TrustedWorkerControl>,
}

impl SandboxedWorkerChild {
    /// Authenticate the fixed worker and send one authority-bearing request.
    ///
    /// Core—not the caller—serializes the effective caveats, strength floor,
    /// and launch nonce captured by [`SandboxedWorker::spawn`]. `payload`
    /// contains only tool-specific, non-authority fields. The control endpoint
    /// is consumed and closed after this frame, so a launch can authorize at
    /// most one request.
    pub fn send_payload<T: Serialize>(&mut self, payload: &T, timeout: Duration) -> ToolResult<()> {
        let mut control = self
            .control
            .take()
            .ok_or_else(|| ToolError::denied("trusted worker request was already sent"))?;
        control.send(payload, self.child.id(), timeout)
    }

    /// Authenticate the worker and explicitly delegate one separate Unix control
    /// endpoint during its take-once bootstrap. The receiver must request this
    /// attachment in its authenticated tool payload and consume it before ACK.
    /// The initial authority channel is still closed after that ACK; it is never
    /// a broker request channel. Existing caveats and strength floor are unchanged.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub fn send_payload_with_control_channel<T: Serialize>(
        &mut self,
        payload: &T,
        channel: std::os::unix::net::UnixStream,
        timeout: Duration,
    ) -> ToolResult<()> {
        let mut control = self
            .control
            .take()
            .ok_or_else(|| ToolError::denied("trusted worker request was already sent"))?;
        control.send_with_channel(payload, self.child.id(), timeout, Some(&channel))
    }
}

/// The supervisor-owned end of a trusted worker's private control channel.
///
/// The stream and authority are intentionally private. Callers can only send a
/// non-authority payload through [`SandboxedWorkerChild::send_payload`].
#[derive(Debug)]
struct TrustedWorkerControl {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    stream: std::os::unix::net::UnixStream,
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    unavailable: (),
    nonce: String,
    caveats: Caveats,
    strength_floor: EnforcementFloor,
}

impl TrustedWorkerControl {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn send<T: Serialize>(
        &mut self,
        payload: &T,
        child_pid: u32,
        timeout: Duration,
    ) -> ToolResult<()> {
        self.send_with_channel(payload, child_pid, timeout, None)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn send_with_channel<T: Serialize>(
        &mut self,
        payload: &T,
        child_pid: u32,
        timeout: Duration,
        channel: Option<&std::os::unix::net::UnixStream>,
    ) -> ToolResult<()> {
        self.stream
            .set_read_timeout(Some(timeout))
            .map_err(ToolError::from)?;
        self.stream
            .set_write_timeout(Some(timeout))
            .map_err(ToolError::from)?;

        // Establish kernel peer metadata before the worker snapshots its
        // parent. On macOS LOCAL_PEERTOKEN is populated only after a write
        // from that peer; this fixed prelude carries no authority.
        self.stream
            .write_all(&TRUSTED_WORKER_BOOTSTRAP)
            .and_then(|()| self.stream.flush())
            .map_err(ToolError::from)?;
        let mut hello = [0_u8; TRUSTED_WORKER_HELLO_LEN];
        self.stream
            .read_exact(&mut hello)
            .map_err(ToolError::from)?;
        let (reported_pid, challenge) = decode_trusted_worker_hello(&hello)
            .map_err(|error| ToolError::denied(format!("invalid worker hello: {error}")))?;
        if reported_pid != child_pid {
            return Err(ToolError::denied(format!(
                "worker hello PID mismatch: spawned {child_pid}, reported {reported_pid}"
            )));
        }

        let request = TrustedWorkerRequest {
            version: TRUSTED_WORKER_PROTOCOL_VERSION,
            nonce: self.nonce.clone(),
            caveats: self.caveats.clone(),
            strength_floor: self.strength_floor,
            payload,
        };
        let body = serde_json::to_vec(&request)
            .map_err(|error| ToolError::denied(format!("encode worker request: {error}")))?;
        if body.len() > TRUSTED_WORKER_MAX_BODY {
            return Err(ToolError::denied(
                "trusted worker request exceeds its 1 MiB cap",
            ));
        }
        let header = encode_trusted_worker_frame_header(
            challenge,
            trusted_worker_frame_digest(&challenge, &body),
            body.len(),
        )
        .map_err(ToolError::denied)?;
        self.stream.write_all(&header).map_err(ToolError::from)?;
        self.stream.write_all(&body).map_err(ToolError::from)?;
        self.stream.flush().map_err(ToolError::from)?;
        if let Some(channel) = channel {
            crate::send_control_endpoint(&self.stream, channel)?;
        }
        let mut ack = [0_u8; TRUSTED_WORKER_ACK.len()];
        self.stream.read_exact(&mut ack).map_err(ToolError::from)?;
        if ack != TRUSTED_WORKER_ACK {
            return Err(ToolError::denied(
                "trusted worker returned an invalid authentication ACK",
            ));
        }
        // The control object is consumed immediately after this method. The
        // worker may close its endpoint as soon as it writes the ACK, so a
        // racing ENOTCONN here is not an authentication failure.
        let _ = self.stream.shutdown(std::net::Shutdown::Write);
        Ok(())
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn send<T: Serialize>(
        &mut self,
        payload: &T,
        child_pid: u32,
        timeout: Duration,
    ) -> ToolResult<()> {
        let _ = (
            &self.unavailable,
            &self.nonce,
            &self.caveats,
            self.strength_floor,
            payload,
            child_pid,
            timeout,
        );
        Err(ToolError::denied(
            "trusted worker control channels are unavailable on this platform",
        ))
    }
}

/// Version of the private trusted-worker authority envelope.
pub const TRUSTED_WORKER_PROTOCOL_VERSION: u8 = 2;
/// Maximum serialized trusted-worker request body.
pub const TRUSTED_WORKER_MAX_BODY: usize = 1024 * 1024;
/// Fixed, non-authority prelude used to establish kernel peer metadata.
pub const TRUSTED_WORKER_BOOTSTRAP: [u8; 8] = *b"ABTW-B1\0";
/// Fixed acknowledgement emitted only after a worker authenticates its frame.
pub const TRUSTED_WORKER_ACK: [u8; 8] = *b"ABTW-A1\0";
const TRUSTED_WORKER_HELLO_MAGIC: [u8; 8] = *b"ABTW-H1\0";
const TRUSTED_WORKER_FRAME_MAGIC: [u8; 8] = *b"ABTW-R1\0";
/// Exact byte length of a trusted-worker hello frame.
pub const TRUSTED_WORKER_HELLO_LEN: usize = 8 + 4 + 32;
/// Exact byte length of a trusted-worker response header.
pub const TRUSTED_WORKER_FRAME_HEADER_LEN: usize = 8 + 4 + 32 + 32;
const TRUSTED_WORKER_DIGEST_DOMAIN: &[u8] = b"agent-bridle/trusted-worker-frame/v1";

/// Core-owned authority envelope received by a fixed trusted worker.
///
/// `P` is tool-specific data only. Authority fields are captured from the
/// supervisor's minted [`ToolContext`] and cannot be supplied through
/// [`SandboxedWorkerChild::send_payload`].
#[derive(Debug, Serialize, Deserialize)]
pub struct TrustedWorkerRequest<P> {
    version: u8,
    nonce: String,
    caveats: Caveats,
    strength_floor: EnforcementFloor,
    payload: P,
}

impl<P> TrustedWorkerRequest<P> {
    /// Borrow the authenticated tool payload without changing its authority envelope.
    #[must_use]
    pub fn payload(&self) -> &P {
        &self.payload
    }

    /// Consume the envelope into its core-authenticated authority and payload.
    #[must_use]
    pub fn into_parts(self) -> (String, Caveats, EnforcementFloor, P) {
        (self.nonce, self.caveats, self.strength_floor, self.payload)
    }

    /// Whether the envelope uses the protocol version understood by this core.
    #[must_use]
    pub fn has_supported_version(&self) -> bool {
        self.version == TRUSTED_WORKER_PROTOCOL_VERSION
    }
}

/// Encode the child-to-supervisor hello that carries a fresh challenge.
#[must_use]
pub fn encode_trusted_worker_hello(child_pid: u32, challenge: [u8; 32]) -> [u8; 44] {
    let mut frame = [0_u8; TRUSTED_WORKER_HELLO_LEN];
    frame[..8].copy_from_slice(&TRUSTED_WORKER_HELLO_MAGIC);
    frame[8..12].copy_from_slice(&child_pid.to_le_bytes());
    frame[12..].copy_from_slice(&challenge);
    frame
}

/// Decode and validate a child-to-supervisor hello.
pub fn decode_trusted_worker_hello(frame: &[u8]) -> Result<(u32, [u8; 32]), String> {
    if frame.len() != TRUSTED_WORKER_HELLO_LEN || frame[..8] != TRUSTED_WORKER_HELLO_MAGIC {
        return Err("bad trusted-worker hello framing".to_string());
    }
    let pid = u32::from_le_bytes(
        frame[8..12]
            .try_into()
            .map_err(|_| "bad trusted-worker PID field")?,
    );
    let challenge = frame[12..]
        .try_into()
        .map_err(|_| "bad trusted-worker challenge field")?;
    Ok((pid, challenge))
}

/// Encode a supervisor-to-worker header binding challenge, body length, and
/// content digest.
pub fn encode_trusted_worker_frame_header(
    challenge: [u8; 32],
    digest: [u8; 32],
    body_len: usize,
) -> Result<[u8; TRUSTED_WORKER_FRAME_HEADER_LEN], String> {
    let body_len = u32::try_from(body_len)
        .map_err(|_| "trusted-worker request length does not fit its frame".to_string())?;
    let mut frame = [0_u8; TRUSTED_WORKER_FRAME_HEADER_LEN];
    frame[..8].copy_from_slice(&TRUSTED_WORKER_FRAME_MAGIC);
    frame[8..12].copy_from_slice(&body_len.to_le_bytes());
    frame[12..44].copy_from_slice(&challenge);
    frame[44..].copy_from_slice(&digest);
    Ok(frame)
}

/// Decode a supervisor-to-worker frame header.
pub fn decode_trusted_worker_frame_header(
    frame: &[u8],
) -> Result<(usize, [u8; 32], [u8; 32]), String> {
    if frame.len() != TRUSTED_WORKER_FRAME_HEADER_LEN || frame[..8] != TRUSTED_WORKER_FRAME_MAGIC {
        return Err("bad trusted-worker response framing".to_string());
    }
    let body_len = u32::from_le_bytes(
        frame[8..12]
            .try_into()
            .map_err(|_| "bad trusted-worker length field")?,
    ) as usize;
    if body_len > TRUSTED_WORKER_MAX_BODY {
        return Err("trusted-worker request exceeds its 1 MiB cap".to_string());
    }
    let challenge = frame[12..44]
        .try_into()
        .map_err(|_| "bad trusted-worker challenge field")?;
    let digest = frame[44..]
        .try_into()
        .map_err(|_| "bad trusted-worker digest field")?;
    Ok((body_len, challenge, digest))
}

/// Content digest binding one trusted-worker request to its fresh challenge.
#[must_use]
pub fn trusted_worker_frame_digest(challenge: &[u8; 32], body: &[u8]) -> [u8; 32] {
    let mut framed =
        Vec::with_capacity(TRUSTED_WORKER_DIGEST_DOMAIN.len() + challenge.len() + body.len());
    framed.extend_from_slice(TRUSTED_WORKER_DIGEST_DOMAIN);
    framed.extend_from_slice(challenge);
    framed.extend_from_slice(body);
    Fingerprint::of_bytes(&framed).0
}

/// Deserialize a verified trusted-worker request body.
pub fn decode_trusted_worker_request<P: DeserializeOwned>(
    body: &[u8],
) -> Result<TrustedWorkerRequest<P>, String> {
    serde_json::from_slice(body).map_err(|error| format!("invalid trusted-worker request: {error}"))
}

/// Builder for a subprocess confined by a [`ToolContext`].
///
/// Like [`std::process::Command`], but: the environment starts **empty** (only
/// vars added with [`ConfinedCommand::env`] reach the child), and
/// [`ConfinedCommand::spawn`] admission-checks `exec`, applies the OS sandbox,
/// and fails closed when a restricted axis cannot meet its required
/// enforcement floor.
///
/// ## Descriptor inheritance (agent-bridle#319)
///
/// Both **environment** and **file descriptors** are delegated explicitly. The
/// child's stdio (fds 0/1/2, including the trusted-worker control channel which
/// rides in as stdin) is deliberately delegated; every *ambient* descriptor the
/// parent left open (an already-open descriptor is itself an object capability)
/// is closed at `exec` via [`agent_bridle_fdguard::deny_inherited_fds`], which
/// installs an async-signal-safe pre-exec that marks every ambient descriptor
/// close-on-exec (preserving stdio, and preserving std's own exec-status pipe so
/// a failed `exec` is still reported). The `unsafe` pre-exec is encapsulated in
/// that crate so this crate stays `#![forbid(unsafe_code)]` (ADR 0026, slice-1
/// decision). Enforcement covers **Linux** (`close_range(2)`, kernel ≥ 5.11) and
/// **macOS** (an `fcntl(F_SETFD, FD_CLOEXEC)` sweep over a bound derived from the
/// kernel's own descriptor ceilings; a descriptor universe that cannot be bounded
/// **refuses the spawn** rather than being swept short — see the fdguard crate
/// docs for the derivation, the concurrency argument and the residual). On Windows the *confined* path
/// spawns via `agent-bridle-aclaunch`'s explicit
/// `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` (only delegated handles are inheritable);
/// a Windows spawn through this builder still relies on the handle-inheritance
/// convention. The fix fails closed: if the marking step errors, the spawn fails
/// and the child never runs.
#[derive(Debug)]
pub struct ConfinedCommand {
    program: String,
    args: Vec<OsString>,
    envs: Vec<(OsString, OsString)>,
    cwd: Option<PathBuf>,
    stdin: Option<Stdio>,
    stdout: Option<Stdio>,
    stderr: Option<Stdio>,
    /// Put the child in a fresh process group so a supervising caller can
    /// terminate the complete descendant tree at a timeout boundary.
    new_process_group: bool,
    /// Validated fixed-worker alias roots; only SandboxedWorker can set these.
    worker_read_resources: BTreeSet<String>,
    /// The sandbox mechanism config (read/exec allow-lists). Rides the builder —
    /// NOT the `ToolContext`, which carries only authority (I5-B, #144, ADR 0017
    /// D2). Defaults to today's built-in allow-lists.
    sandbox_policy: Arc<SandboxPolicy>,
    /// Explicit private-host approvals for [`spawn_tokio`](ConfinedCommand::spawn_tokio)'s
    /// egress proxy (#385/#386, forward-ported from the 0.7 line's `ac3d34a`).
    /// Empty by default; consulted only on the `spawn_tokio` path.
    #[cfg(all(unix, feature = "spawn-tokio"))]
    private_hosts: std::collections::HashSet<String>,
}

impl ConfinedCommand {
    /// Start building a confined spawn of `program` (no inherited environment).
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            envs: Vec::new(),
            cwd: None,
            #[cfg(all(unix, feature = "spawn-tokio"))]
            private_hosts: std::collections::HashSet::new(),
            stdin: None,
            stdout: None,
            stderr: None,
            new_process_group: false,
            worker_read_resources: BTreeSet::new(),
            sandbox_policy: Arc::new(SandboxPolicy::default()),
        }
    }

    /// Set the sandbox mechanism policy (read/exec allow-lists, ABI floors) the
    /// OS backend will enforce. The default is today's built-in allow-lists.
    #[must_use]
    pub fn sandbox_policy(mut self, policy: Arc<SandboxPolicy>) -> Self {
        self.sandbox_policy = policy;
        self
    }

    /// Append a single argument.
    #[must_use]
    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_os_string());
        self
    }

    /// Append several arguments.
    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|a| a.as_ref().to_os_string()));
        self
    }

    /// Grant one environment variable to the child. This is the **only** way an
    /// env var reaches the child — there is no ambient inheritance.
    #[must_use]
    pub fn env(mut self, key: impl AsRef<OsStr>, val: impl AsRef<OsStr>) -> Self {
        self.envs
            .push((key.as_ref().to_os_string(), val.as_ref().to_os_string()));
        self
    }

    /// Set the child's working directory.
    #[must_use]
    pub fn current_dir(mut self, dir: impl AsRef<Path>) -> Self {
        self.cwd = Some(dir.as_ref().to_path_buf());
        self
    }

    /// Configure the child's stdin (e.g. [`Stdio::piped`] for an MCP server).
    #[must_use]
    pub fn stdin(mut self, cfg: Stdio) -> Self {
        self.stdin = Some(cfg);
        self
    }

    /// Configure the child's stdout.
    #[must_use]
    pub fn stdout(mut self, cfg: Stdio) -> Self {
        self.stdout = Some(cfg);
        self
    }

    /// Configure the child's stderr.
    #[must_use]
    pub fn stderr(mut self, cfg: Stdio) -> Self {
        self.stderr = Some(cfg);
        self
    }

    /// Start the child as leader of a fresh process group.
    ///
    /// This is used by trusted worker supervisors that must terminate the
    /// worker and every descendant together. It is currently effective on
    /// Unix; other platforms retain their native child-process behavior.
    #[must_use]
    pub fn new_process_group(mut self) -> Self {
        self.new_process_group = true;
        self
    }

    /// Admission-check, confine, and spawn the child.
    ///
    /// Order: (1) `cx.check_exec(program)` — deny before doing anything; (2)
    /// derive the backend's per-axis enforcement and refuse if a restricted
    /// axis cannot meet its floor; (3) apply the selected thread- or
    /// wrapper-based sandbox, then spawn inside that boundary.
    pub fn spawn(self, cx: &ToolContext) -> ToolResult<ConfinedChild> {
        let effective = cx.caveats().clone();
        self.spawn_with_effective(cx, effective)
    }

    /// The spawn body, parameterized over the **effective** caveats the OS
    /// sandbox confines to (#257). `spawn` passes the context's caveats
    /// verbatim; the egress-proxy path (`spawn_tokio` under a proxied-net
    /// grant) passes [`crate::loopback_fenced_caveats`] — same fs/exec axes,
    /// `net` swapped for the loopback fence. The exec admission-check always
    /// runs against the REAL context (the fence never widens or narrows exec).
    fn spawn_with_effective(
        self,
        cx: &ToolContext,
        effective: Caveats,
    ) -> ToolResult<ConfinedChild> {
        self.spawn_authorized(cx, effective, SpawnAuthority::ModelSelected)
    }

    /// Shared spawn funnel for model-selected programs and fixed internal
    /// workers. The latter skips only the model-facing executable admission
    /// check; its executable and entrypoint are fixed by [`SandboxedWorker`].
    fn spawn_authorized(
        self,
        cx: &ToolContext,
        effective: Caveats,
        authority: SpawnAuthority,
    ) -> ToolResult<ConfinedChild> {
        // (1) Admission: model-selected programs must be in the exec grant.
        // A trusted worker transition is not model-selected; its fixed program
        // is added only to the mechanism policy below.
        if authority != SpawnAuthority::TrustedWorker {
            cx.check_exec(&self.program)?;
        }

        let sandbox = if authority == SpawnAuthority::NamedRoot {
            crate::sandbox::named_root_sandbox(&self.sandbox_policy)?
        } else {
            best_available_sandbox(&self.sandbox_policy)
        };
        let kind = sandbox.kind();
        // The kind that actually GOVERNS this spawn: the backend's kind only when
        // it will actually confine something (fs or net restricted), else `None`.
        // The fail-closed
        // check is decided against THIS, not the raw probe, so the check and the
        // routing cannot disagree (the adversarial-review fix: a raw
        // `enforcement_report` claim of fs→Kernel for a backend that is not
        // actually applied would otherwise pass a run the path executes
        // unconfined). Also the honest kind reported on the child (I9 / ADR 0006 D3).
        let reported_kind = effective_sandbox_kind(kind, &effective);
        // The witness the fail-closed check consumes is built from the SAME
        // mechanism that will govern this child: the reported backend kind AND the
        // child-network policy carried by `self.sandbox_policy` — the exact policy
        // `best_available_sandbox` above selected the backend from and that
        // installs the seccomp `DenyDirect` leg at apply time. So the net witness
        // (Landlock `net:none` = Kernel only under `DenyDirect`) cannot diverge
        // from the mechanism actually applied to the spawn.
        let mechanism = match sandbox.exec_boundary() {
            crate::ExecBoundary::ProcessTree => {
                ConfinementMechanism::new(reported_kind, self.sandbox_policy.child_network)
            }
            crate::ExecBoundary::NamedRoot => ConfinementMechanism::for_named_root(
                reported_kind,
                self.sandbox_policy.child_network,
            ),
        };

        // (2) The declared runtime closure — the ONLY door for authority beyond
        // the delegated grant. A fixed worker executable is an internal
        // transition, not authority delegated to the model: allowlist-based
        // kernel exec policies (Landlock/Seatbelt/rootfs/microVM) need its
        // exact path declared so the boundary can launch it; AppContainer must
        // instead preserve exec deny-all so its launcher applies the
        // child-process block — so it declares nothing. Neither changes the
        // reported/effective authority.
        let mut closure = trusted_worker_closure(kind, authority, &self.program)?;
        for root in &self.worker_read_resources {
            closure = closure.with_fs_read(root.clone())?;
        }

        // (3) THE admission (L2+L3+L4, one object). `AdmittedFence::admit`
        // derives the mechanism caveats (delegated ∪ declared closure) exactly
        // once, computes the L3 scope bound over the resolved-authority lattice
        // (an undeclared widening refuses as a Superset — the audit's bug
        // class), and checks the per-axis strength floor (L4). What it returns
        // is the object the sandbox applies below; nothing is re-derived after
        // admission (L2 non-equivocation).
        let admitted = if authority == SpawnAuthority::NamedRoot {
            AdmittedFence::admit_named_root(
                &effective,
                &self.program,
                &self.sandbox_policy.resolve_named_root_protected_roots()?,
                mechanism,
                cx.strength_floor(),
                |caveats| BackendProjection {
                    resolved: sandbox.resolved_authority(caveats),
                    runtime_closure: sandbox.runtime_closure(caveats),
                },
            )
        } else {
            AdmittedFence::admit(
                &effective,
                closure,
                mechanism,
                cx.strength_floor(),
                |mechanism_caveats| {
                    // Mechanism selection for the projection: the authority a spawn is
                    // bounded by is EITHER the OS sandbox OR, for a trusted worker, the
                    // in-process brush-ocap engine (a VERIFIED interceptor for exec/fs).
                    // Brush cannot mediate sockets or ambient IPC used by an external
                    // descendant, so the net axis must always come from the OS backend's
                    // conservative projection. This exemption is keyed on the TRUSTED-WORKER
                    // route, NOT on `SandboxKind::None`: an arbitrary Noop spawn is
                    // bounded by NOTHING and must fall through to the backend
                    // projection below (Noop ⇒ Unbounded ⇒ refuse a restricted axis).
                    if authority == SpawnAuthority::TrustedWorker {
                        // Brush bounds execution to the delegated grant; the fixed
                        // worker binary remains mechanism-only exec authority. The
                        // explicit read resource directories are visible in the OS
                        // fence, so both projection and closure must include them.
                        // Preserve the backend's net result: an Unknown/Unbounded native
                        // network boundary cannot borrow Brush's fs/exec interception.
                        let mut resolved = crate::ResolvedAuthority::from_delegated(&effective);
                        resolved.fs_read =
                            crate::ResolvedScope::from_scope(&mechanism_caveats.fs_read);
                        resolved.net = sandbox.resolved_authority(mechanism_caveats).net;
                        return BackendProjection {
                            resolved,
                            runtime_closure: crate::ResolvedAuthority {
                                fs_read: crate::ResolvedScope::concrete(
                                    self.worker_read_resources.iter().cloned(),
                                ),
                                ..crate::empty_closure()
                            },
                        };
                    }

                    // The backend's CONSERVATIVE projection of what it will actually
                    // install for these caveats (ruleset grain; the shared root-set
                    // derivation cannot independently drift from `apply`) plus the
                    // harness-added substrate the resolution rests on. This is the #317
                    // fix — admission now sees authority the ruleset installs beyond the
                    // grant (Landlock's `base_read` loader/library trees; a symlinked
                    // grant root that resolves `Unknown`) instead of the caveats-grain
                    // blind spot. A backend that enforces nothing (Noop ⇒ all-Unbounded)
                    // refuses a restricted axis here; the net axis resolves `Unknown`
                    // for a restricted grant (E3 io_uring — not yet provable) and so
                    // fails closed until the io_uring egress floor lands (PR-1). No
                    // caveats-grain net override: `resolved` is the honest projection.
                    BackendProjection {
                        resolved: sandbox.resolved_authority(mechanism_caveats),
                        runtime_closure: sandbox.runtime_closure(mechanism_caveats),
                    }
                },
            )
        }
        .map_err(|e| match e {
            ToolError::Denied { reason } => {
                ToolError::denied(format!("{reason} (program: {:?})", self.program))
            }
            other => other,
        })?;
        let mechanism_effective = admitted.mechanism_caveats().clone();

        // ASM-CID / L2 at runtime: the caveats we are about to compile+apply must
        // content-address to the fence admission stamped. Same object today, so
        // this holds by construction; it is the cryptographic backstop that a
        // future re-derivation between admit and apply (the #317 bug) cannot pass.
        if authority != SpawnAuthority::NamedRoot {
            admitted.verify_applied(&mechanism_effective)?;
        }

        // For a wrapper-based backend (Seatbelt/AppContainer) this is the argv
        // prefix that confines the child; empty for thread-confining backends
        // (Landlock, via `apply`) and Noop. Computed here so a fail-closed wrapper
        // error aborts *before* we spawn the thread. Built from the ADMITTED
        // caveats — the same object `apply` consumes on the spawn thread.
        let prefix = sandbox.command_prefix(&mechanism_effective)?;

        // (3) Apply the sandbox on a throwaway thread, then spawn on it so the
        //     child inherits the OS confinement — the per-thread, fork/exec-
        //     inherited Landlock domain or the selected process wrapper.
        let apply_policy = Arc::clone(&self.sandbox_policy);
        let Self {
            program,
            args,
            envs,
            cwd,
            stdin,
            stdout,
            stderr,
            new_process_group,
            // Already consumed above into `sandbox` via `best_available_sandbox`.
            sandbox_policy: _,
            worker_read_resources: _,
            // Consulted only on the `spawn_tokio` path, not this sync `spawn`.
            #[cfg(all(unix, feature = "spawn-tokio"))]
                private_hosts: _,
        } = self;

        let apply_admitted = admitted.clone();
        let spawned = std::thread::spawn(move || -> ToolResult<Child> {
            // Prepare the exact Command operand first. NamedRoot currently
            // supports only Landlock (no wrapper), so the actual program operand
            // is the root; unsupported wrappers refuse rather than guessing at
            // a second target string. Verification consumes fresh backend state.
            let (spawn_program, spawn_args) = wrap_argv(&prefix, &program, &args);

            // Wrap the child in the backend's command prefix when it confines via
            // a wrapper (Seatbelt, AppContainer); otherwise spawn the program directly.
            let mut cmd = Command::new(&spawn_program);
            cmd.args(&spawn_args);
            cmd.env_clear(); // no ambient environment crosses the boundary …
            for (k, v) in &envs {
                cmd.env(k, v); // … only the explicitly-granted vars.
            }
            if let Some(dir) = &cwd {
                cmd.current_dir(dir);
            }
            if let Some(cfg) = stdin {
                cmd.stdin(cfg);
            }
            if let Some(cfg) = stdout {
                cmd.stdout(cfg);
            }
            if let Some(cfg) = stderr {
                cmd.stderr(cfg);
            }
            #[cfg(unix)]
            if new_process_group {
                use std::os::unix::process::CommandExt;
                cmd.process_group(0);
            }
            #[cfg(not(unix))]
            let _ = new_process_group;
            // #319: close ambient descriptors the parent left open so they are not
            // inherited as un-delegated object capabilities. Encapsulated in
            // `agent-bridle-fdguard` (the single `unsafe` seam) so core stays
            // `forbid(unsafe_code)`. Enforced on Linux (close_range) and macOS
            // (planned FD_CLOEXEC sweep; an unbounded descriptor universe refuses
            // the spawn, #352); no-op elsewhere.
            #[cfg(unix)]
            agent_bridle_fdguard::deny_inherited_fds(&mut cmd);

            if authority == SpawnAuthority::NamedRoot {
                if !prefix.is_empty() || sandbox.exec_boundary() != crate::ExecBoundary::NamedRoot {
                    return Err(ToolError::denied("named-root actual launch requires its supported unwrapped native mechanism"));
                }
                apply_admitted.verify_named_root_applied(
                    &mechanism_effective,
                    cmd.get_program().to_str().ok_or_else(|| ToolError::denied("named-root launch operand is not UTF-8"))?,
                    &apply_policy.resolve_named_root_protected_roots()?,
                    mechanism,
                    BackendProjection {
                        resolved: sandbox.resolved_authority(&mechanism_effective),
                        runtime_closure: sandbox.runtime_closure(&mechanism_effective),
                    },
                )?;
            }
            // Thread-confining backends (Landlock): apply the sandbox on this
            // throwaway thread before the spawn so the child inherits the
            // Landlock domain. `apply` is fail-closed: if the kernel did not
            // actually enforce, it returns Err and we never spawn.
            //
            // Wrapper-based backends (Seatbelt, AppContainer): confinement is
            // achieved by the `command_prefix` wrapper — no per-thread state is
            // involved, and calling `apply` would be wrong (AppContainer fails
            // closed; Seatbelt is a no-op). Skip `apply` when the prefix is
            // non-empty.
            if prefix.is_empty() {
                sandbox.apply(&mechanism_effective)?;
            }

            cmd.spawn().map_err(ToolError::from)
        })
        .join()
        .map_err(|_| ToolError::denied("confined-spawn thread panicked before exec"))?;

        Ok(ConfinedChild {
            child: spawned?,
            sandbox_kind: reported_kind,
            // The identity of the object admission produced and `verify_applied`
            // confirmed above — copied out, never recomputed downstream.
            fence_id: admitted.fence_id().clone(),
            admitted: admitted.admitted_body().cloned(),
        })
    }

    /// Admission-check, confine, spawn, **and hand back every fence that
    /// brackets the child** — the entry point a managed execution owner uses.
    ///
    /// Identical confinement to [`Self::spawn`]: same exec admission, same
    /// `AdmittedFence::admit`, same `verify_applied`, same env scrub. It differs
    /// only in what it *returns*: the proxy handle stays owned by the caller (so
    /// the caller can join it to quiescence and put frozen egress evidence in a
    /// terminal record) instead of being torn down on a detached thread.
    ///
    /// This is the same egress-proxy decision `spawn_tokio` makes — one shared
    /// `egress_proxy_plan` call — so a managed execution and an async MCP child
    /// cannot disagree about when a fence engages.
    pub fn spawn_managed(self, cx: &ToolContext) -> ToolResult<ManagedSpawn> {
        self.spawn_managed_authorized(cx, SpawnAuthority::ModelSelected)
    }

    /// Exact named root admission with unchanged caveats and a confined tree.
    pub(crate) fn spawn_named_root_managed(self, cx: &ToolContext) -> ToolResult<ManagedSpawn> {
        self.spawn_managed_authorized(cx, SpawnAuthority::NamedRoot)
    }

    fn spawn_managed_authorized(
        mut self,
        cx: &ToolContext,
        authority: SpawnAuthority,
    ) -> ToolResult<ManagedSpawn> {
        let mut proxy = None;
        let mut effective = cx.caveats().clone();
        // NamedRoot preserves the effective caveats exactly. Unsupported native
        // network shapes refuse admission; no loopback rewrite changes its body.
        if authority != SpawnAuthority::NamedRoot {
            if let Some((hosts, fenced)) =
                crate::egress_proxy_plan(&effective, &self.sandbox_policy)
            {
                // Fail-closed: the grant calls for a fence + proxy; a proxy that
                // cannot bind must refuse the spawn, never run unfenced.
                let handle = crate::net_proxy::start_for_hosts(hosts).map_err(|e| {
                    ToolError::Exec(std::io::Error::other(format!(
                        "refusing to spawn {:?}: the egress proxy could not bind loopback ({e})",
                        self.program
                    )))
                })?;
                for (k, v) in handle.proxy_env() {
                    self = self.env(k, v);
                }
                proxy = Some(handle);
                effective = fenced;
            }
        }

        // A refusal here must not leave the proxy running: `proxy` drops on the
        // error path, and `ProxyHandle::Drop` force-closes and blocks until its
        // workers are done (#372), so a denied spawn leaks no listener.
        let ConfinedChild {
            child,
            sandbox_kind,
            fence_id,
            admitted,
        } = self.spawn_authorized(cx, effective, authority)?;

        Ok(ManagedSpawn {
            child,
            sandbox_kind,
            fence_id,
            admitted,
            proxy,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpawnAuthority {
    ModelSelected,
    TrustedWorker,
    NamedRoot,
}

/// A closed set of internal worker entrypoints. The caller cannot supply
/// arbitrary arguments: each kind maps to a fixed private protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustedWorkerKind {
    /// The carried Brush shell worker.
    Brush,
}

impl TrustedWorkerKind {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn args(self) -> [&'static str; 2] {
        match self {
            Self::Brush => ["--agent-bridle-worker", "brush"],
        }
    }
}

/// Builder for a fixed Agent Bridle worker born under the ordinary confinement
/// funnel. Unlike [`ConfinedCommand`], the executable is mechanism
/// configuration chosen by the trusted embedder and the entrypoint arguments
/// are fixed by [`TrustedWorkerKind`]; model-authored arguments never reach the
/// spawn boundary.
#[derive(Debug, Clone)]
pub struct SandboxedWorker {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    kind: TrustedWorkerKind,
    sandbox_policy: Arc<SandboxPolicy>,
    read_only_resources: Vec<PathBuf>,
}

impl SandboxedWorker {
    /// Configure the carried Brush worker at this process's fixed executable.
    ///
    /// The executable is intentionally not caller-selectable: trusted-worker
    /// admission bypasses the model-facing exec check, so accepting an arbitrary
    /// path here would turn the worker API into a generic confused deputy.
    #[must_use]
    pub fn brush() -> Self {
        Self {
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            kind: TrustedWorkerKind::Brush,
            sandbox_policy: Arc::new(SandboxPolicy::default()),
            read_only_resources: Vec::new(),
        }
    }

    /// Set the sandbox mechanism policy used by the shared spawn funnel.
    #[must_use]
    pub fn sandbox_policy(mut self, policy: Arc<SandboxPolicy>) -> Self {
        self.sandbox_policy = policy;
        self
    }

    /// Declare read-only directories of aliases for this fixed worker executable.
    ///
    /// Currently each directory must be empty or contain only symlinks resolving
    /// to the exact current executable: arbitrary files, subdirectories and other
    /// executables are refused. Roots must be outside all write authority and any
    /// configured protected-root inventory. An unrestricted or unresolved write
    /// grant refuses these resources. Validation occurs at spawn; the caller must
    /// retain the directories unchanged for the worker's lifetime. No execution
    /// authority or model-facing ToolContext grant is added by this declaration.
    #[must_use]
    pub fn read_only_resources(mut self, roots: Vec<PathBuf>) -> Self {
        self.read_only_resources = roots;
        self
    }

    /// Spawn the fixed worker with empty ambient environment, piped output, and
    /// a private authenticated-control transport in place of ordinary stdin.
    ///
    /// `nonce` binds the worker request carried over stdin to this launch. The
    /// worker is a fresh process-group leader on Unix so its supervisor can
    /// terminate the complete process tree on timeout. Other targets retain
    /// their native child-process behavior. The process-wide unbridled state is
    /// derived inside core; a caller cannot opt a single worker out of
    /// confinement.
    pub fn spawn(
        self,
        cx: &ToolContext,
        nonce: &str,
        cwd: &Path,
    ) -> ToolResult<SandboxedWorkerChild> {
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = (self, cx, nonce, cwd);
            Err(ToolError::denied(
                "refusing the Brush worker: this platform has no authenticated \
                 private-control transport",
            ))
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            self.spawn_supported(cx, nonce, cwd)
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn spawn_supported(
        self,
        cx: &ToolContext,
        nonce: &str,
        cwd: &Path,
    ) -> ToolResult<SandboxedWorkerChild> {
        cx.check_path_read(cwd)?;
        let request_caveats = cx.caveats().clone();
        let request_strength_floor = cx.strength_floor();
        let executable = std::env::current_exe()
            .and_then(std::fs::canonicalize)
            .map_err(|error| ToolError::denied(format!("worker executable is invalid: {error}")))?;
        let worker_read_resources = canonical_worker_read_resources(
            &self.read_only_resources,
            if crate::is_unbridled() {
                &Scope::All
            } else {
                &request_caveats.fs_write
            },
            &executable,
            &self.sandbox_policy,
        )?;
        let executable = executable.to_string_lossy().into_owned();
        let [flag, kind] = self.kind.args();
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let (control, child_control) = std::os::unix::net::UnixStream::pair().map_err(|error| {
            ToolError::denied(format!("create worker control channel: {error}"))
        })?;

        let mut command = ConfinedCommand::new(executable)
            .args([flag, kind])
            .env("AGENT_BRIDLE_WORKER_NONCE", nonce)
            .current_dir(cwd)
            .stdin(Stdio::from(std::os::fd::OwnedFd::from(child_control)))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .new_process_group()
            .sandbox_policy(self.sandbox_policy);
        command.worker_read_resources = worker_read_resources;

        let confined = if crate::is_unbridled() {
            command.spawn_authorized(cx, Caveats::top(), SpawnAuthority::TrustedWorker)
        } else {
            let effective = cx.caveats().clone();
            let available = best_available_sandbox(&command.sandbox_policy).kind();
            let reported = effective_sandbox_kind(available, &effective);
            #[cfg(not(target_os = "linux"))]
            let _ = reported;
            #[cfg(target_os = "linux")]
            if reported == SandboxKind::Landlock
                && crate::sandbox::restricts_fs(&effective)
                && !linux_user_namespaces_hardened()
            {
                return Err(ToolError::denied(
                    "refusing the Brush worker: Landlock filesystem confinement \
                     is not a complete boundary while unprivileged user namespaces \
                     remain available; disable them or add the namespace syscall backstop",
                ));
            }
            command.spawn_authorized(cx, effective, SpawnAuthority::TrustedWorker)
        }?;
        Ok(SandboxedWorkerChild {
            child: confined.child,
            sandbox_kind: confined.sandbox_kind,
            control: Some(TrustedWorkerControl {
                stream: control,
                nonce: nonce.to_string(),
                caveats: request_caveats,
                strength_floor: request_strength_floor,
            }),
        })
    }
}

/// Resolve narrowly declared worker aliases without trusting mutable child paths.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn canonical_worker_read_resources(
    resources: &[PathBuf],
    writable: &Scope<String>,
    executable: &Path,
    policy: &SandboxPolicy,
) -> ToolResult<BTreeSet<String>> {
    if resources.is_empty() {
        return Ok(BTreeSet::new());
    }
    let Scope::Only(write_grants) = writable else {
        return Err(ToolError::denied(
            "read-only worker resources require bounded write authority",
        ));
    };
    let write_roots = write_grants
        .iter()
        .map(|grant| {
            let path = Path::new(grant);
            if !path.is_absolute() {
                return Err(ToolError::denied(
                    "worker resource write grant is not absolute",
                ));
            }
            path.canonicalize().map_err(|error| {
                ToolError::denied(format!(
                    "cannot resolve worker resource write grant: {error}"
                ))
            })
        })
        .collect::<ToolResult<Vec<_>>>()?;
    let protected_roots = if policy.named_root_protected_roots.is_some() {
        policy.resolve_named_root_protected_roots()?
    } else {
        BTreeSet::new()
    };
    let mut roots = BTreeSet::new();
    for resource in resources {
        if !resource.is_absolute() {
            return Err(ToolError::denied(
                "worker resource directory must be absolute",
            ));
        }
        // A safe canonical target does not make a writable alias leading to it
        // safe: the child could replace that alias or relocate its parent.
        for ancestor in resource.ancestors() {
            let ancestor = ancestor.canonicalize().map_err(|error| {
                ToolError::denied(format!("cannot resolve worker resource ancestor: {error}"))
            })?;
            if write_roots.iter().any(|write| ancestor.starts_with(write)) {
                return Err(ToolError::denied(
                    "worker resource has a writable relocation ancestor",
                ));
            }
        }
        let root = resource.canonicalize().map_err(|error| {
            ToolError::denied(format!("cannot resolve worker resource directory: {error}"))
        })?;
        let root_text = root.to_str().ok_or_else(|| {
            ToolError::denied("worker resource directory is not representable exactly")
        })?;
        if !root.is_dir() || crate::admitted::entry_reaches_harness_private(root_text) {
            return Err(ToolError::denied(
                "worker resource must be a harness-disjoint directory",
            ));
        }
        for write_root in &write_roots {
            if root.starts_with(write_root) || write_root.starts_with(&root) {
                return Err(ToolError::denied(
                    "worker resource overlaps write authority or a writable relocation ancestor",
                ));
            }
        }
        for protected in &protected_roots {
            let protected = Path::new(protected);
            if root.starts_with(protected) || protected.starts_with(&root) {
                return Err(ToolError::denied(
                    "worker resource overlaps protected inventory",
                ));
            }
        }
        for entry in std::fs::read_dir(&root).map_err(|error| {
            ToolError::denied(format!("cannot inspect worker resource directory: {error}"))
        })? {
            let entry = entry.map_err(|error| {
                ToolError::denied(format!("cannot inspect worker resource entry: {error}"))
            })?;
            let file_type = entry.file_type().map_err(|error| {
                ToolError::denied(format!(
                    "cannot inspect worker resource entry type: {error}"
                ))
            })?;
            if !file_type.is_symlink()
                || !entry
                    .path()
                    .canonicalize()
                    .is_ok_and(|target| target == executable)
            {
                return Err(ToolError::denied(
                    "worker resources permit only flat aliases of the current executable",
                ));
            }
        }
        roots.insert(root_text.to_owned());
    }
    Ok(roots)
}

#[cfg(test)]
mod read_resource_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            loop {
                let path = std::env::temp_dir().join(format!(
                    "bridle-read-resources-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                match std::fs::create_dir(&path) {
                    Ok(()) => return Self(path.canonicalize().unwrap()),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => panic!("create test directory: {error}"),
                }
            }
        }
        fn dir(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            std::fs::create_dir_all(&path).unwrap();
            path
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn resolve(roots: &[PathBuf], writable: Scope<String>) -> ToolResult<BTreeSet<String>> {
        canonical_worker_read_resources(
            roots,
            &writable,
            &std::env::current_exe().unwrap().canonicalize().unwrap(),
            &SandboxPolicy::default(),
        )
    }
    fn only(path: &Path) -> Scope<String> {
        Scope::only([path.to_str().unwrap().to_owned()])
    }

    #[test]
    fn resource_declaration_defaults_empty_and_preserves_payload() {
        assert!(SandboxedWorker::brush().read_only_resources.is_empty());
        assert!(resolve(&[], Scope::All).unwrap().is_empty());
        let request = TrustedWorkerRequest {
            version: TRUSTED_WORKER_PROTOCOL_VERSION,
            nonce: "nonce".to_string(),
            caveats: Caveats::top(),
            strength_floor: EnforcementFloor::default(),
            payload: "data",
        };
        assert_eq!(request.payload(), &"data");
        assert_eq!(request.into_parts().3, "data");
    }

    #[test]
    fn read_resources_accept_disjoint_empty_roots_and_deduplicate() {
        let dir = TestDir::new();
        let resource = dir.dir("resource");
        let writable = dir.dir("writable");
        let resolved = resolve(&[resource.clone(), resource.clone()], only(&writable)).unwrap();
        assert_eq!(
            resolved,
            BTreeSet::from([resource.to_str().unwrap().to_owned()])
        );
    }

    #[test]
    fn read_resources_reject_write_overlap_relocation_and_unknown_grants() {
        let dir = TestDir::new();
        let resource = dir.dir("resource");
        for writable in [
            Scope::All,
            only(&resource),
            only(&dir.0),
            only(&resource.join("missing")),
            only(&dir.0.join("missing")),
        ] {
            assert!(resolve(std::slice::from_ref(&resource), writable).is_err());
        }
        let child = dir.dir("resource/child");
        assert!(resolve(&[resource], only(&child)).is_err());
    }

    #[test]
    fn read_resources_reject_files_nested_trees_and_private_roots() {
        let dir = TestDir::new();
        let resource = dir.dir("home-like");
        std::fs::write(resource.join("secret"), "private").unwrap();
        assert!(resolve(std::slice::from_ref(&resource), Scope::only([])).is_err());
        assert!(resolve(&[resource.join("secret")], Scope::only([])).is_err());
        assert!(resolve(&[dir.dir("private/.newt")], Scope::only([])).is_err());
        assert!(resolve(&[dir.0.join("private")], Scope::only([])).is_err());
        assert!(resolve(&[dir.0.join("missing")], Scope::only([])).is_err());
    }

    #[test]
    fn read_resources_reject_configured_protected_inventory_overlap() {
        let dir = TestDir::new();
        let resource = dir.dir("resource");
        for protected in [&dir.0, &resource, &resource.join("future-private")] {
            let policy = SandboxPolicy {
                named_root_protected_roots: Some(BTreeSet::from([protected
                    .to_str()
                    .unwrap()
                    .to_owned()])),
                ..SandboxPolicy::default()
            };
            assert!(canonical_worker_read_resources(
                std::slice::from_ref(&resource),
                &Scope::only([]),
                &std::env::current_exe().unwrap(),
                &policy
            )
            .is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn read_resources_reject_relocatable_alias_ancestors() {
        let dir = TestDir::new();
        let resource = dir.dir("resource");
        let writable = dir.dir("writable");
        let alias = writable.join("resource-alias");
        std::os::unix::fs::symlink(resource, &alias).unwrap();
        assert!(resolve(&[alias], only(&writable)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn read_resources_accept_only_current_worker_aliases_and_resolve_write_aliases() {
        let dir = TestDir::new();
        let resource = dir.dir("resource");
        let executable = std::env::current_exe().unwrap().canonicalize().unwrap();
        std::os::unix::fs::symlink(&executable, resource.join("helper")).unwrap();
        assert!(resolve(std::slice::from_ref(&resource), Scope::only([])).is_ok());
        let alias = dir.0.join("alias");
        std::os::unix::fs::symlink(&resource, &alias).unwrap();
        assert!(resolve(std::slice::from_ref(&resource), only(&alias)).is_err());
        let other = dir.0.join("other-executable");
        std::fs::write(&other, "other").unwrap();
        std::os::unix::fs::symlink(other, resource.join("wrong-helper")).unwrap();
        assert!(resolve(&[resource], Scope::only([])).is_err());
    }
}

/// The declared [`RuntimeClosure`] for a trusted worker transition — the only
/// door by which the worker's executable reaches the mechanism's allow-list
/// (it then flows through [`AdmittedFence::admit`]'s scope check like any
/// other closure entry; nothing widens the mechanism caveats silently).
///
/// Landlock, Seatbelt, and the identity-closing stronger tiers need the fixed
/// worker executable in their kernel execute allow-list so the boundary can
/// launch it — those declare it. AppContainer is different: its launcher
/// creates the worker as the initial confined process, and `exec: Only([])`
/// must remain empty so `--no-child-process` is attached to that worker.
/// Declaring the worker path there would silently turn deny-all into a
/// non-empty allow-list, disable the kernel child-process mitigation, and
/// leave an `exec → Kernel` report overclaiming — so it declares nothing.
///
/// A model-selected spawn declares nothing: the closure exists for internal
/// transitions only, never for authority the model chose.
///
/// This shapes mechanism configuration only; it never alters the effective
/// authority carried by `ToolContext` or the enforcement report.
fn trusted_worker_closure(
    kind: SandboxKind,
    authority: SpawnAuthority,
    program: &str,
) -> ToolResult<RuntimeClosure> {
    if authority != SpawnAuthority::TrustedWorker {
        return Ok(RuntimeClosure::empty());
    }
    match kind {
        SandboxKind::Landlock
        | SandboxKind::Seatbelt
        | SandboxKind::MinimalRootfs
        | SandboxKind::MicroVm => {
            RuntimeClosure::empty().with_exec(crate::admitted::canonical_closure_program(program)?)
        }
        SandboxKind::AppContainer | SandboxKind::None => Ok(RuntimeClosure::empty()),
    }
}

#[cfg(target_os = "linux")]
fn linux_user_namespaces_hardened() -> bool {
    fn sysctl_is(path: &str, expected: &str) -> bool {
        std::fs::read_to_string(path).is_ok_and(|value| value.trim() == expected)
    }

    sysctl_is("/proc/sys/kernel/unprivileged_userns_clone", "0")
        || sysctl_is("/proc/sys/user/max_user_namespaces", "0")
        || sysctl_is(
            "/proc/sys/kernel/apparmor_restrict_unprivileged_userns",
            "1",
        )
}

/// Spawn `program args` confined by `cx`, with the inherited stdio of the parent.
///
/// The convenience form of [`ConfinedCommand`]: `env_allow` is the child's
/// **entire** environment (nothing else is inherited). For piped stdio (an MCP
/// server), use [`ConfinedCommand`] directly.
pub fn spawn_confined_subprocess(
    program: &str,
    args: &[String],
    cx: &ToolContext,
    env_allow: &[(String, String)],
    cwd: Option<&Path>,
) -> ToolResult<ConfinedChild> {
    let mut cmd = ConfinedCommand::new(program).args(args);
    for (k, v) in env_allow {
        cmd = cmd.env(k, v);
    }
    if let Some(dir) = cwd {
        cmd = cmd.current_dir(dir);
    }
    cmd.spawn(cx)
}

// ── Async-host spawn (tokio pipe handles) ────────────────────────────────────
//
// `spawn` above returns a `std::process::Child` — the caller owns the pipe
// plumbing. An async host (an MCP-server **stdio** transport speaking JSON-RPC
// over the child's stdin/stdout) needs those pipes as tokio-native, reactor-
// registered handles, and it needs the child reaped when the transport drops.
// `spawn_tokio` is that async-facing sibling: the confinement is **identical**
// (it calls `spawn`, so the admission-check / OS-sandbox / env-scrub are the
// same audited path — the boundary is unchanged), only the returned handle
// types differ. Unix-only and gated on `spawn-tokio`, so core stays tokio-free
// by default (the confinement primitives themselves have no async dependency).
#[cfg(all(unix, feature = "spawn-tokio"))]
pub use tokio_spawn::ConfinedTokioChild;

#[cfg(all(unix, feature = "spawn-tokio"))]
mod tokio_spawn {
    use super::{ConfinedChild, ConfinedCommand, SandboxKind, ToolContext, ToolResult};
    use crate::net_proxy::ProxyHandle;
    use crate::{egress_proxy_plan, ToolError};
    use std::os::fd::OwnedFd;
    use std::process::Child;
    use tokio::net::unix::pipe;

    /// A confined child whose stdio is exposed as **tokio-native** pipe handles,
    /// for an async host (e.g. an MCP-server stdio transport). The async-facing
    /// sibling of [`ConfinedChild`](super::ConfinedChild): the confinement is
    /// identical (produced by [`ConfinedCommand::spawn`]), only the pipe types
    /// differ.
    ///
    /// **Kill-on-drop.** Dropping this SIGKILLs the child and reaps it on a
    /// detached thread — restoring the guarantee a host loses by moving off
    /// `tokio::process::Command::kill_on_drop(true)` onto the std child
    /// underneath (tokio's runtime reaper only tracks *its own* children, so the
    /// std child would otherwise linger as a zombie). Take the pipe ends with
    /// the `take_*` accessors; the child stays owned here so this value's
    /// lifetime governs the process.
    #[derive(Debug)]
    pub struct ConfinedTokioChild {
        /// The OS-level sandbox actually applied to the child — the honest record
        /// (mirrors [`ConfinedChild::sandbox_kind`](super::ConfinedChild)).
        pub sandbox_kind: SandboxKind,
        stdin: Option<pipe::Sender>,
        stdout: Option<pipe::Receiver>,
        stderr: Option<pipe::Receiver>,
        /// `Some` until dropped; owned so kill-on-drop governs the process.
        child: Option<Child>,
        /// The live egress proxy fencing this child's net (#257) — `Some` iff the
        /// grant was a general remote-host allow-list AND the loopback kernel
        /// fence engaged. Owned here so the proxy's lifetime brackets the
        /// child's: it is torn down after the child is killed on drop.
        proxy: Option<ProxyHandle>,
    }

    impl ConfinedTokioChild {
        /// Take the child's stdin pipe (writer). `None` if stdin was not
        /// [`piped`](std::process::Stdio::piped) or was already taken.
        pub fn take_stdin(&mut self) -> Option<pipe::Sender> {
            self.stdin.take()
        }

        /// Take the child's stdout pipe (reader). `None` if stdout was not piped
        /// or was already taken.
        pub fn take_stdout(&mut self) -> Option<pipe::Receiver> {
            self.stdout.take()
        }

        /// Take the child's stderr pipe (reader). `None` if stderr was not piped
        /// or was already taken.
        pub fn take_stderr(&mut self) -> Option<pipe::Receiver> {
            self.stderr.take()
        }

        /// Whether this child's egress is fenced through the loopback proxy
        /// (#257): kernel-fenced to loopback, per-host allow-list enforced by
        /// the proxy it is pointed at via `*_PROXY` env.
        pub fn egress_proxied(&self) -> bool {
            self.proxy.is_some()
        }

        /// The off-allow-list hosts the child tried to reach through the proxy
        /// (#196) — each was refused with 403. Empty when no proxy is in force
        /// or nothing was refused. The exfil-attempt signal a host surfaces as
        /// structured `net` denials.
        pub fn refused_hosts(&self) -> Vec<String> {
            self.proxy
                .as_ref()
                .map(ProxyHandle::refused_hosts)
                .unwrap_or_default()
        }
    }

    impl Drop for ConfinedTokioChild {
        fn drop(&mut self) {
            // Reinstate kill-on-drop. `spawn_tokio` hands back a std child, which
            // tokio's runtime reaper does NOT track — so kill it and `wait` on a
            // detached thread to avoid a zombie without blocking this (possibly
            // async) drop.
            if let Some(mut child) = self.child.take() {
                let _ = child.kill();
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
            }
            // #372: `ProxyHandle`'s own `Drop` now force-closes and BLOCKS
            // joining every connection worker (bounded by `CONN_TIMEOUT` each),
            // not just the accept thread — dropping it inline here could block
            // this (possibly async, possibly-on-the-Tokio-reactor) drop far
            // longer than before. Move the teardown to a detached thread, the
            // same fix already applied to the child reap above.
            if let Some(proxy) = self.proxy.take() {
                std::thread::spawn(move || drop(proxy));
            }
        }
    }

    impl ConfinedCommand {
        /// Approve exact names for RFC1918/ULA resolution by this command's
        /// `spawn_tokio` egress proxy (#385/#386, forward-ported from the 0.7
        /// line). The owning harness supplies these names after an explicit
        /// operator decision; server metadata is not authority. The context's
        /// ordinary net allow-list must independently permit them.
        ///
        /// Empty by default. This neither starts a proxy where no loopback fence
        /// exists nor changes synchronous `spawn` or any filesystem/exec caveat.
        /// No wildcard or global private-space approval is accepted.
        pub fn with_private_hosts(
            mut self,
            hosts: impl IntoIterator<Item = String>,
        ) -> std::io::Result<Self> {
            self.private_hosts = crate::net_proxy::canonical_private_hosts(hosts)?;
            Ok(self)
        }

        /// Admission-check, confine, and spawn the child — like
        /// [`spawn`](ConfinedCommand::spawn), but the stdio pipes are returned as
        /// **tokio-native** handles wrapped in a kill-on-drop
        /// [`ConfinedTokioChild`], for an async host (an MCP-server stdio
        /// transport).
        ///
        /// The confinement is exactly `spawn`'s (this delegates to it): the
        /// `exec` admission-check, the fail-closed refusal when a restricted fs
        /// axis cannot be kernel-enforced, the OS sandbox, and the env scrub all
        /// happen there. This method only converts the piped std handles into
        /// tokio pipe ends.
        ///
        /// Must be called from within a tokio runtime — the pipe handles register
        /// with the reactor. Unix-only; gated on the `spawn-tokio` feature.
        pub fn spawn_tokio(mut self, cx: &ToolContext) -> ToolResult<ConfinedTokioChild> {
            // #257 (Part A — Leg 4): under a general remote-host `net` grant,
            // fence the child's egress. `egress_proxy_plan` is the ONE shared
            // decision (also the shell engine's): engage only when the loopback
            // kernel fence is actually emittable on this host — a proxy a rogue
            // child can walk around is not confinement, so on fence-less hosts
            // (e.g. Landlock, which cannot address-fence) the wiring stays
            // INERT and the spawn proceeds exactly as before (net advisory,
            // ADR 0015 posture).
            let mut proxy = None;
            let mut effective = cx.caveats().clone();
            if let Some((hosts, fenced)) = egress_proxy_plan(&effective, &self.sandbox_policy) {
                // Fail-closed: the grant calls for a fence + proxy; a proxy
                // that cannot bind must refuse the spawn, never run unfenced.
                let handle = crate::net_proxy::start_with_private_hosts(
                    hosts,
                    self.private_hosts.iter().cloned(),
                    std::sync::Arc::new(crate::net_proxy::StdResolver),
                    std::sync::Arc::new(crate::net_proxy::NullSink),
                )
                .map_err(|e| {
                    ToolError::Exec(std::io::Error::other(format!(
                        "refusing to spawn {:?}: the egress proxy could not bind \
                         loopback ({e})",
                        self.program
                    )))
                })?;
                // Point the child at the proxy through the explicit env
                // grants (the only channel across the boundary).
                for (k, v) in handle.proxy_env() {
                    self = self.env(k, v);
                }
                proxy = Some(handle);
                effective = fenced;
            }

            let ConfinedChild {
                mut child,
                sandbox_kind,
                fence_id: _,
                admitted: _,
            } = self.spawn_with_effective(cx, effective)?;

            // Convert each *piped* std handle into a tokio pipe end.
            // `pipe::{Sender,Receiver}::from_owned_fd` set O_NONBLOCK and register
            // the fd with the reactor. The `OwnedFd` conversion moves ownership
            // out of the std `Child`, so each fd is closed exactly once (the tokio
            // end owns it; `Child` no longer does after `take`). A handle that was
            // not piped stays `None`. `?` maps the io error via `ToolError::from`.
            let stdin = child
                .stdin
                .take()
                .map(|h| pipe::Sender::from_owned_fd(OwnedFd::from(h)))
                .transpose()?;
            let stdout = child
                .stdout
                .take()
                .map(|h| pipe::Receiver::from_owned_fd(OwnedFd::from(h)))
                .transpose()?;
            let stderr = child
                .stderr
                .take()
                .map(|h| pipe::Receiver::from_owned_fd(OwnedFd::from(h)))
                .transpose()?;

            Ok(ConfinedTokioChild {
                sandbox_kind,
                stdin,
                stdout,
                stderr,
                child: Some(child),
                proxy,
            })
        }
    }
}

/// Prepend a backend command prefix (Seatbelt's `sandbox-exec -p <profile>`) to
/// a `(program, args)`, yielding the argv to actually spawn. An empty prefix is
/// the identity — thread-confining (Landlock) and Noop backends spawn the
/// program directly. Under Seatbelt the program should be an absolute path
/// (the environment is scrubbed, so `sandbox-exec` cannot resolve a bare name
/// via `PATH`).
fn wrap_argv(prefix: &[String], program: &str, args: &[OsString]) -> (OsString, Vec<OsString>) {
    if prefix.is_empty() {
        return (OsString::from(program), args.to_vec());
    }
    let mut argv: Vec<OsString> = prefix[1..].iter().map(OsString::from).collect();
    argv.push(OsString::from(program));
    argv.extend(args.iter().cloned());
    (OsString::from(&prefix[0]), argv)
}

/// Would confining this child be a *lie*? Decided against the **real** backend
/// `kind` (the probe the spawn actually confines through — not a stale gate
/// stamp; ADR 0012 D4) and the principal's `floor`.
///
/// Two parts:
/// 1. **The filesystem floor (always).** `fs_read` and `fs_write` *are*
///    kernel-enforceable (Landlock/Seatbelt/AppContainer); a restricted fs axis
///    the active backend cannot kernel-confine is a grant we cannot honor, so we
///    refuse regardless of strength. This keeps the ADR 0003 stub floor for
///    `fs_write` **and** extends it to `fs_read` — closing the spawn-boundary
///    fail-open ADR 0012 D4 found (a restricted `fs_read` was run unconfined
///    under `None` because the old check looked at `fs_write` only).
/// 2. **The strength floor (`exec`/`net`).** Coverage varies by backend and
///    scope, so those axes refuse when the principal's `floor` demands more than
///    the real report delivers (`fence_strength(report) < floor`). With the
///    default floor ([`AxisEnforcement::Advisory`]), an honestly reported weaker
///    axis may run; a strong principal (`floor = Kernel`) fails closed whenever
///    the active backend cannot kernel-confine it (ADR 0012 D3/D10).
#[must_use]
pub fn confinement_unenforceable(
    kind: SandboxKind,
    caveats: &Caveats,
    floor: AxisEnforcement,
) -> bool {
    // Back-compat scalar wrapper over the per-axis check: the historic scalar
    // floor `f` means filesystem=Kernel (always), exec=net=`f`
    // ([`EnforcementFloor::from_scalar`]). A confined executor should call
    // [`unenforceable_axis`] directly with [`EnforcementFloor::CONFINED`] so the
    // exec axis is accepted at the interceptor tier rather than forced to Kernel.
    unenforceable_axis(caveats, kind, EnforcementFloor::from_scalar(floor)).is_some()
}

// Async-path proof for `spawn_tokio`: the child's stdio survives the std→tokio
// pipe conversion (a JSON-RPC line round-trips), and kill-on-drop actually kills
// the child. Real-subprocess tests, matching this module's convention (the
// landlock/seatbelt child proofs above also spawn real programs).
#[cfg(all(unix, feature = "spawn-tokio", test))]
mod tokio_spawn_tests {
    use super::*;
    use crate::{Gate, Tool};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    fn ctx(granted: Caveats) -> ToolContext {
        struct AnyTool;
        #[async_trait::async_trait]
        impl Tool for AnyTool {
            fn name(&self) -> &str {
                "any"
            }
            fn schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            async fn invoke(
                &self,
                _a: serde_json::Value,
                _c: &ToolContext,
            ) -> ToolResult<serde_json::Value> {
                Ok(serde_json::Value::Null)
            }
        }
        Gate::new(0)
            .authorize(&AnyTool, &granted)
            .expect("authorize")
    }

    fn find_cat() -> Option<&'static str> {
        ["/usr/bin/cat", "/bin/cat"]
            .into_iter()
            .find(|p| Path::new(p).exists())
    }

    /// #385/#386: forward-port of `ac3d34a`'s `ConfinedCommand::with_private_hosts`
    /// acceptance test — empty by default, canonicalizes an exact approval, and
    /// rejects a wildcard.
    #[test]
    fn exact_private_hosts_builder_defaults_closed_and_validates_names() {
        let command = ConfinedCommand::new("cat");
        assert!(command.private_hosts.is_empty());
        let command = command
            .with_private_hosts(["SERVICE.TEST.".to_string()])
            .unwrap();
        assert_eq!(command.private_hosts, ["service.test".to_string()].into());
        assert!(ConfinedCommand::new("cat")
            .with_private_hosts(["*".to_string()])
            .is_err());
    }

    /// The MCP-transport use case: a newline-delimited JSON-RPC line written to
    /// the child's tokio stdin comes back on its tokio stdout (`cat` echoes),
    /// proving the std→tokio pipe conversion preserves a working duplex stream.
    #[tokio::test]
    async fn json_line_round_trips_over_tokio_pipes() {
        let Some(cat) = find_cat() else {
            eprintln!("skipping: no cat(1) found");
            return;
        };
        let cx = ctx(Caveats {
            exec: Scope::only(["cat".to_string()]),
            ..Caveats::top()
        });
        let mut child = ConfinedCommand::new(cat)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn_tokio(&cx)
            .expect("spawn_tokio cat");

        let mut stdin = child.take_stdin().expect("stdin piped");
        let stdout = child.take_stdout().expect("stdout piped");
        assert!(child.take_stdin().is_none(), "stdin taken once");

        let msg = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        stdin.write_all(msg.as_bytes()).await.expect("write");
        stdin.write_all(b"\n").await.expect("write nl");
        stdin.flush().await.expect("flush");

        let mut lines = BufReader::new(stdout).lines();
        let got = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
            .await
            .expect("recv did not time out")
            .expect("recv ok");
        assert_eq!(got.as_deref(), Some(msg));
    }

    /// Kill-on-drop: dropping the [`ConfinedTokioChild`] SIGKILLs the child, which
    /// closes its stdout write end — so the retained reader reaches EOF. `stdin`
    /// is held so `cat` cannot exit on its own from a stdin EOF; the only thing
    /// that ends it is the drop.
    #[tokio::test]
    async fn dropping_the_guard_kills_the_child() {
        let Some(cat) = find_cat() else {
            eprintln!("skipping: no cat(1) found");
            return;
        };
        let cx = ctx(Caveats {
            exec: Scope::only(["cat".to_string()]),
            ..Caveats::top()
        });
        let mut child = ConfinedCommand::new(cat)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn_tokio(&cx)
            .expect("spawn_tokio cat");

        let stdout = child.take_stdout().expect("stdout piped");
        // Hold stdin so `cat` does not exit from a stdin EOF — isolate the kill.
        let _stdin = child.take_stdin().expect("stdin piped");
        drop(child);

        let mut lines = BufReader::new(stdout).lines();
        let eof = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
            .await
            .expect("EOF did not time out")
            .expect("read ok");
        assert_eq!(
            eof, None,
            "kill-on-drop must terminate the child and close its stdout (EOF)"
        );
    }

    // ── #257: spawn_tokio's egress-proxy wiring ─────────────────────────────

    /// A remote-host `net` grant whose backend CANNOT address-fence it (e.g.
    /// Linux/Landlock) can no longer spawn "inert / advisory-net". That was the
    /// pre-#257 / ADR-0015 register-and-proceed posture; the conservative net
    /// projection (review #1) RETIRES it: the net axis resolves `Unknown` (E3
    /// io_uring not yet proven closed), so admission FAILS CLOSED until the
    /// io_uring egress floor lands (PR-1). Where the loopback fence WOULD engage
    /// (Seatbelt), the premise doesn't hold, so skip.
    #[tokio::test]
    async fn remote_net_grant_without_fence_backend_refuses() {
        let caveats = Caveats {
            exec: Scope::only(["true".to_string()]),
            net: Scope::only(["api.example.com".to_string()]),
            ..Caveats::top()
        };
        let plan_engages = crate::egress_proxy_plan(
            &caveats,
            &std::sync::Arc::new(crate::SandboxPolicy::default()),
        )
        .is_some();
        if plan_engages {
            eprintln!("skipping: this host CAN emit the loopback fence (engage path)");
            return;
        }
        let cx = ctx(caveats);
        let result = ConfinedCommand::new("true").spawn_tokio(&cx);
        assert!(
            matches!(result, Err(ToolError::Denied { .. })),
            "an un-bound-able remote net grant must fail closed (E3, until PR-1), not spawn inert"
        );
    }

    /// The engage path — INTEGRATION tier (real Seatbelt + real subprocess +
    /// the loopback proxy), so `#[ignore]`d out of the per-PR unit run; the
    /// deterministic engage proof lives in `net_proxy::tests` (the proxy 403s +
    /// records an off-list host with no subprocess) and the inert case above.
    /// Run on macOS with `--ignored`.
    ///
    /// Under a remote-host grant on a fence-capable host, a spawned `curl`
    /// (exec-scoped to itself — no shell re-exec) inherits the granted
    /// `*_PROXY` env and its CONNECT to an off-allow-list host is refused by
    /// the proxy (recorded in `refused_hosts()`) BEFORE any real dial — so this
    /// needs no network. Proves the full spawn_tokio ∘ fence ∘ proxy compose.
    #[cfg(all(target_os = "macos", feature = "macos-seatbelt"))]
    #[tokio::test]
    #[ignore = "integration: real Seatbelt fence + curl subprocess + loopback proxy"]
    async fn remote_net_grant_with_fence_spawns_proxied_and_refuses_off_list() {
        if !crate::seatbelt_is_supported() {
            eprintln!("skipping: /usr/bin/sandbox-exec unavailable");
            return;
        }
        let curl = "/usr/bin/curl"; // always present on macOS; no shell re-exec
        let caveats = Caveats {
            exec: Scope::only([curl.to_string()]),
            net: Scope::only(["api.example.com".to_string()]),
            ..Caveats::top()
        };
        let cx = ctx(caveats);
        // curl honors the lowercase `https_proxy` the proxy env grant sets; the
        // off-list CONNECT is refused at the allow-list (403) before any dial.
        let child = ConfinedCommand::new(curl)
            .arg("-s")
            .arg("-m")
            .arg("5")
            .arg("https://evil.example.net/")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn_tokio(&cx)
            .expect("proxied spawn");
        assert!(child.egress_proxied(), "fence host → proxy must engage");

        // Reap via the kill-on-drop guard after curl exits; the proxy records
        // the refusal synchronously as it serves the CONNECT.
        let _ = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::time::sleep(Duration::from_secs(1)).await;
        })
        .await;
        assert!(
            child
                .refused_hosts()
                .contains(&"evil.example.net".to_string()),
            "the off-allow-list host must be refused and recorded: {:?}",
            child.refused_hosts()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enforcement_report;
    use crate::{Gate, Tool};

    /// Mint a `ToolContext` the only legitimate way — through the gate.
    fn ctx(granted: Caveats) -> ToolContext {
        struct AnyTool;
        #[async_trait::async_trait]
        impl Tool for AnyTool {
            fn name(&self) -> &str {
                "any"
            }
            fn schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            async fn invoke(
                &self,
                _args: serde_json::Value,
                _cx: &ToolContext,
            ) -> ToolResult<serde_json::Value> {
                Ok(serde_json::Value::Null)
            }
        }
        Gate::new(0)
            .authorize(&AnyTool, &granted)
            .expect("authorize")
    }

    /// Core freezes authority at worker spawn and exposes only a one-shot
    /// payload sender. Payload fields that merely *look* authority-bearing do
    /// not replace the captured caveats, and a second send is structurally
    /// refused.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn trusted_worker_control_freezes_authority_and_is_take_once() {
        let true_program = ["/usr/bin/true", "/bin/true"]
            .into_iter()
            .find(|path| Path::new(path).exists())
            .expect("true executable");
        let process = Command::new(true_program)
            .spawn()
            .expect("spawn PID holder");
        let child_pid = process.id();
        let (client, mut server) =
            std::os::unix::net::UnixStream::pair().expect("private socketpair");
        let (seen_tx, seen_rx) = std::sync::mpsc::channel();

        let peer = std::thread::spawn(move || {
            let mut bootstrap = [0_u8; TRUSTED_WORKER_BOOTSTRAP.len()];
            server.read_exact(&mut bootstrap).expect("read bootstrap");
            assert_eq!(bootstrap, TRUSTED_WORKER_BOOTSTRAP);
            let challenge = [0x5a; 32];
            server
                .write_all(&encode_trusted_worker_hello(child_pid, challenge))
                .and_then(|()| server.flush())
                .expect("write hello");

            let mut header = [0_u8; TRUSTED_WORKER_FRAME_HEADER_LEN];
            server.read_exact(&mut header).expect("read header");
            let (body_len, echoed, digest) =
                decode_trusted_worker_frame_header(&header).expect("decode header");
            assert_eq!(echoed, challenge);
            let mut body = vec![0_u8; body_len];
            server.read_exact(&mut body).expect("read body");
            assert_eq!(digest, trusted_worker_frame_digest(&challenge, &body));
            let request: TrustedWorkerRequest<serde_json::Value> =
                decode_trusted_worker_request(&body).expect("decode request");
            seen_tx.send(request.into_parts()).expect("report request");
            server
                .write_all(&TRUSTED_WORKER_ACK)
                .and_then(|()| server.flush())
                .expect("write ACK");
        });

        let frozen = Caveats {
            exec: Scope::only(["echo".to_string()]),
            ..Caveats::top()
        };
        let control = TrustedWorkerControl {
            stream: client,
            nonce: "core-owned-nonce".to_string(),
            caveats: frozen.clone(),
            strength_floor: EnforcementFloor::DEFAULT,
        };
        let mut worker = SandboxedWorkerChild {
            child: process,
            sandbox_kind: SandboxKind::None,
            control: Some(control),
        };
        let forged_payload = serde_json::json!({
            "cmd": "echo ok",
            "caveats": Caveats::top(),
            "strength_floor": AxisEnforcement::Kernel,
        });
        worker
            .send_payload(&forged_payload, Duration::from_secs(5))
            .expect("first payload");

        let (nonce, authority, floor, payload) = seen_rx.recv().expect("receive decoded request");
        assert_eq!(nonce, "core-owned-nonce");
        assert_eq!(authority, frozen, "payload must not replace frozen caveats");
        assert_eq!(floor, EnforcementFloor::DEFAULT);
        assert_eq!(payload, forged_payload);
        assert!(
            matches!(
                worker.send_payload(&serde_json::json!({}), Duration::from_secs(1)),
                Err(ToolError::Denied { .. })
            ),
            "the private control endpoint must be take-once"
        );

        peer.join().expect("join fake worker");
        let _ = worker.child.wait();
    }

    /// Blocker 1 (trusted-worker leg): the `strength_floor` in a trusted-worker
    /// envelope cannot carry a sub-Kernel filesystem floor. A well-formed request
    /// round-trips with fs pinned Kernel, and a body that injects an `fs_read`/
    /// `fs_write` field into the floor is REJECTED at decode (not normalized) — a
    /// forged/downgraded envelope fails closed before any worker acts on it.
    #[test]
    fn trusted_worker_decode_cannot_forge_a_weak_filesystem_floor() {
        let request = TrustedWorkerRequest {
            version: TRUSTED_WORKER_PROTOCOL_VERSION,
            nonce: "n".to_string(),
            caveats: Caveats::top(),
            strength_floor: EnforcementFloor::CONFINED,
            payload: serde_json::json!({"cmd": "echo ok"}),
        };
        let body = serde_json::to_vec(&request).expect("encode");
        let text = String::from_utf8(body.clone()).unwrap();
        // The strength_floor object itself omits the filesystem axes (the envelope's
        // separate `caveats` field legitimately carries fs_read/fs_write scopes, so
        // we check the floor object specifically, not the whole body).
        let floor_obj = &text[text.find("\"strength_floor\":{").unwrap()..];
        let floor_obj = &floor_obj[..floor_obj.find('}').unwrap()];
        assert!(
            !floor_obj.contains("fs_read") && !floor_obj.contains("fs_write"),
            "the encoded floor must omit the filesystem axes: {floor_obj}",
        );
        // A valid body round-trips and reconstructs the pinned Kernel fs floor.
        let (_, _, floor, _) = decode_trusted_worker_request::<serde_json::Value>(&body)
            .expect("decode valid")
            .into_parts();
        assert_eq!(floor.fs_read(), AxisEnforcement::Kernel);
        assert_eq!(floor.fs_write(), AxisEnforcement::Kernel);
        assert_eq!(floor, EnforcementFloor::CONFINED);

        // Inject a weak fs_write field into the floor object; decode must reject it
        // (deny_unknown_fields), never silently upgrade it to Kernel.
        let forged = text.replace(
            "\"strength_floor\":{",
            "\"strength_floor\":{\"fs_write\":\"advisory\",",
        );
        assert_ne!(forged, text, "the injection must actually change the body");
        assert!(
            decode_trusted_worker_request::<serde_json::Value>(forged.as_bytes()).is_err(),
            "a trusted-worker floor carrying a filesystem field must be rejected",
        );
    }

    #[test]
    fn exec_outside_scope_is_denied_before_any_spawn() {
        let cx = ctx(Caveats {
            exec: Scope::only(["echo".to_string()]),
            ..Caveats::top()
        });
        let res = ConfinedCommand::new("rm").arg("-rf").spawn(&cx);
        assert!(matches!(res, Err(ToolError::Denied { .. })));
    }

    #[test]
    fn unenforceable_predicate_fs_axes_always_strength_floor_for_exec() {
        use AxisEnforcement::{Advisory, Kernel};
        let fs_write = Caveats {
            fs_write: Scope::only(["/tmp/x".to_string()]),
            ..Caveats::top()
        };
        let fs_read = Caveats {
            fs_read: Scope::only(["/tmp/x".to_string()]),
            ..Caveats::top()
        };
        let exec = Caveats {
            exec: Scope::only(["echo".to_string()]),
            ..Caveats::top()
        };

        // (1) FS floor — always, regardless of strength: a restricted fs axis with
        // no OS sandbox is unenforceable, for BOTH fs_write and fs_read (the
        // latter is the ADR 0012 D4 spawn-boundary fail-open this closes).
        assert!(confinement_unenforceable(
            SandboxKind::None,
            &fs_write,
            Advisory
        ));
        assert!(confinement_unenforceable(
            SandboxKind::None,
            &fs_read,
            Advisory
        ));
        // The kernel can enforce the fs axes => fine.
        assert!(!confinement_unenforceable(
            SandboxKind::Landlock,
            &fs_write,
            Advisory
        ));
        assert!(!confinement_unenforceable(
            SandboxKind::Landlock,
            &fs_read,
            Advisory
        ));

        // (2) exec is not kernel-enforceable: the default (Advisory) floor permits
        // it; a strong (Kernel) floor fails closed (the opt-in un-stub posture).
        assert!(!confinement_unenforceable(
            SandboxKind::None,
            &exec,
            Advisory
        ));
        assert!(confinement_unenforceable(SandboxKind::None, &exec, Kernel));

        // Unrestricted grant => nothing to enforce, even under a Kernel floor.
        assert!(!confinement_unenforceable(
            SandboxKind::None,
            &Caveats::top(),
            Kernel
        ));
    }

    /// AppContainer's wired ACL narrowing means fs-only caveats engage the
    /// launcher; `--fs-read`/`--fs-write` grant its SID the requested workspace
    /// paths over the container's default deny of user directories.
    #[test]
    fn fs_restricted_under_appcontainer_engages_the_launcher() {
        let fs = Caveats {
            fs_write: Scope::only(["/tmp/x".to_string()]),
            ..Caveats::top()
        };
        let governing = effective_sandbox_kind(SandboxKind::AppContainer, &fs);
        assert_eq!(
            governing,
            SandboxKind::AppContainer,
            "fs-only must engage AppContainer (ACL narrowing wired, #51)"
        );
        // fs_write is Kernel: DACL grants + AppContainer default deny-user-dirs (#51).
        let report = enforcement_report(&fs, governing);
        assert_eq!(report.fs_write, Some(AxisEnforcement::Kernel));
        // With AppContainer engaged and fs Kernel, confinement is enforceable.
        assert!(
            !confinement_unenforceable(governing, &fs, AxisEnforcement::Advisory),
            "fs-restricted AppContainer is enforceable (launcher wired)"
        );
    }

    /// exec_fully_denied engages the AppContainer backend: governing == AppContainer,
    /// and the enforcement report marks exec → Kernel (#123).
    #[test]
    fn exec_deny_all_under_appcontainer_is_kernel() {
        let exec_denied = Caveats {
            exec: Scope::only([] as [String; 0]),
            ..Caveats::top()
        };
        let governing = effective_sandbox_kind(SandboxKind::AppContainer, &exec_denied);
        assert_eq!(
            governing,
            SandboxKind::AppContainer,
            "exec deny-all must engage AppContainer"
        );
        // With an AppContainer backend and exec fully denied, the axis is kernel-enforced.
        assert!(
            !confinement_unenforceable(governing, &exec_denied, AxisEnforcement::Advisory),
            "exec deny-all under AppContainer is enforceable (kernel-level block)"
        );
        let report = enforcement_report(&exec_denied, governing);
        assert_eq!(
            report.exec,
            Some(AxisEnforcement::Kernel),
            "exec deny-all must be Kernel under AppContainer"
        );
    }

    /// A test projector that admits at L3 by declaring exactly the mechanism's
    /// (folded) exec authority as its runtime closure — isolating the exec-fold /
    /// mechanism-caveats behaviour these tests assert from the scope check (these
    /// mechanisms have no faithful `resolved_authority` on Linux yet, so a real
    /// projection would fail closed and hide what is under test).
    fn worker_admitting_projection(mechanism_caveats: &Caveats) -> BackendProjection {
        BackendProjection {
            resolved: crate::ResolvedAuthority::from_delegated(mechanism_caveats),
            runtime_closure: crate::ResolvedAuthority {
                exec: crate::ResolvedScope::from_scope(&mechanism_caveats.exec),
                ..crate::provenance::empty_closure()
            },
        }
    }

    /// Trusted-worker launch configuration must not erase AppContainer's
    /// deny-all signal. The AppContainer launcher starts the worker itself, then
    /// `--no-child-process` confines what that worker may spawn. This is pure and
    /// host-independent so Linux/macOS CI protects the Windows policy routing.
    #[test]
    fn trusted_worker_preserves_appcontainer_exec_deny_all() {
        let exec_denied = Caveats {
            exec: Scope::only([] as [String; 0]),
            ..Caveats::top()
        };

        let closure = trusted_worker_closure(
            SandboxKind::AppContainer,
            SpawnAuthority::TrustedWorker,
            "this-path-is-not-used-by-appcontainer",
        )
        .expect("AppContainer trusted-worker closure");
        assert!(
            closure.is_empty(),
            "AppContainer declares no exec closure — its launcher starts the worker"
        );
        let mechanism = AdmittedFence::admit(
            &exec_denied,
            closure,
            ConfinementMechanism::new(
                SandboxKind::AppContainer,
                crate::ChildNetworkPolicy::LandlockOnly,
            ),
            EnforcementFloor::from_scalar(AxisEnforcement::Advisory),
            worker_admitting_projection,
        )
        .expect("AppContainer admission")
        .mechanism_caveats()
        .clone();

        assert!(
            crate::sandbox::exec_fully_denied(&mechanism),
            "the launcher must still select --no-child-process"
        );
        assert_eq!(
            effective_sandbox_kind(SandboxKind::AppContainer, &mechanism),
            SandboxKind::AppContainer,
            "deny-all must still engage the AppContainer boundary"
        );
        assert_eq!(
            enforcement_report(&mechanism, SandboxKind::AppContainer).exec,
            Some(AxisEnforcement::Kernel),
            "the preserved mechanism matches the reported kernel guarantee"
        );
    }

    /// Backends whose wrapper/domain must execute the trusted worker still get
    /// its exact path as mechanism-only authority.
    #[test]
    fn trusted_worker_keeps_exec_allowance_for_allowlist_backends() {
        let exec_denied = Caveats {
            exec: Scope::only([] as [String; 0]),
            ..Caveats::top()
        };
        let current = std::env::current_exe()
            .expect("current executable")
            .canonicalize()
            .expect("canonical current executable")
            .to_string_lossy()
            .into_owned();

        for kind in [SandboxKind::Landlock, SandboxKind::Seatbelt] {
            let closure = trusted_worker_closure(kind, SpawnAuthority::TrustedWorker, &current)
                .expect("allowlist trusted-worker closure");
            let mechanism = AdmittedFence::admit(
                &exec_denied,
                closure,
                ConfinementMechanism::new(kind, crate::ChildNetworkPolicy::LandlockOnly),
                EnforcementFloor::from_scalar(AxisEnforcement::Advisory),
                worker_admitting_projection,
            )
            .expect("allowlist admission")
            .mechanism_caveats()
            .clone();
            assert!(
                matches!(&mechanism.exec, Scope::Only(programs) if programs.contains(&current)),
                "{kind:?} must authorize the fixed worker executable"
            );
        }
    }

    /// Builds with **no** available OS sandbox: a restrictive `fs_write` must be
    /// refused rather than spawned unconfined. Gated off where a backend can
    /// actually enforce (Linux+Landlock, macOS+Seatbelt, Windows+AppContainer) —
    /// there the spawn is confined (or fails-closed on missing launcher), not
    /// silently unconfined, so this particular assertion does not apply.
    #[cfg(not(any(
        all(target_os = "linux", feature = "linux-landlock"),
        all(target_os = "macos", feature = "macos-seatbelt"),
        all(target_os = "windows", feature = "windows-appcontainer")
    )))]
    #[test]
    fn restrictive_write_refused_when_no_sandbox_available() {
        let cx = ctx(Caveats {
            exec: Scope::All,
            fs_write: Scope::only(["/tmp/allowed".to_string()]),
            ..Caveats::top()
        });
        let res = ConfinedCommand::new("true").spawn(&cx);
        assert!(
            matches!(res, Err(ToolError::Denied { .. })),
            "must fail closed when confinement is requested but unenforceable"
        );
    }

    /// The no-op backend enforces nothing, so it is EXEMPT from the L3 scope
    /// check — there is no enforced scope to bound. Whether "no OS confinement"
    /// NEGATIVE guard (review #2): an ARBITRARY (model-selected) spawn under the
    /// no-op backend with restricted authority must REFUSE — it is bounded by no
    /// mechanism at all. The brush-ocap identity projection is keyed on the
    /// TRUSTED-WORKER route (`SpawnAuthority::TrustedWorker`), NOT on
    /// `SandboxKind::None`, so a model-selected Noop spawn cannot borrow that
    /// exemption merely because the brush worker path exists. A restricted EXEC
    /// grant refuses (Noop resolves `Unbounded` ⊋ the grant) even under the
    /// Advisory default floor; a restricted FS_WRITE refuses too (also L4/Kernel).
    ///
    /// Only meaningful when NO OS backend is compiled in (else the selected
    /// backend genuinely enforces) — same gate as
    /// `restrictive_write_refused_when_no_sandbox_available`.
    #[cfg(not(any(
        all(target_os = "linux", feature = "linux-landlock"),
        all(target_os = "macos", feature = "macos-seatbelt"),
        all(target_os = "windows", feature = "windows-appcontainer")
    )))]
    #[test]
    fn an_arbitrary_noop_spawn_with_restricted_authority_refuses() {
        let true_bin = ["/usr/bin/true", "/bin/true"]
            .into_iter()
            .find(|p| Path::new(p).exists());
        let Some(true_bin) = true_bin else {
            eprintln!("skipping: no true(1) found");
            return;
        };
        // Restricted exec, Advisory exec floor — L4 would admit, but L3 refuses
        // because Noop bounds nothing (model-selected ⇒ no brush-ocap exemption).
        let cx = ctx(Caveats {
            exec: Scope::only([true_bin.to_string()]),
            ..Caveats::top()
        });
        assert!(
            matches!(
                ConfinedCommand::new(true_bin).spawn(&cx),
                Err(ToolError::Denied { .. })
            ),
            "a model-selected restricted-exec spawn under Noop must refuse (no mechanism)"
        );
        // Restricted fs_write, Kernel fs floor — refuses on both L3 and L4.
        let cx = ctx(Caveats {
            fs_write: Scope::only(["/tmp/x".to_string()]),
            ..Caveats::top()
        });
        assert!(
            matches!(
                ConfinedCommand::new(true_bin).spawn(&cx),
                Err(ToolError::Denied { .. })
            ),
            "a model-selected restricted-fs_write spawn under Noop must refuse"
        );
    }

    /// The environment is scrubbed: only granted vars reach the child, nothing
    /// ambient (e.g. the parent's `HOME`) leaks. Uses a piped stdout to read the
    /// child's view of its own environment.
    #[cfg(unix)]
    #[test]
    fn environment_is_scrubbed_to_the_granted_allow_list() {
        let env_bin = ["/usr/bin/env", "/bin/env"]
            .into_iter()
            .find(|p| Path::new(p).exists());
        let Some(env_bin) = env_bin else {
            eprintln!("skipping env-scrub test: no env(1) found");
            return;
        };
        // Env scrubbing is a spawn-level `env_clear`, independent of any sandbox.
        // Grant fully-unrestricted caveats so the assertion isolates the scrub: a
        // model-selected restricted axis (e.g. `exec: Only`) under the default Noop
        // backend now correctly REFUSES (Noop bounds nothing; the brush-ocap
        // exemption is trusted-worker-only), which is unrelated to what this checks.
        let cx = ctx(Caveats::top());
        let spawned = ConfinedCommand::new(env_bin)
            .env("ALLOWED", "yes")
            .stdout(Stdio::piped())
            .spawn(&cx)
            .expect("spawn env");
        let out = spawned.child.wait_with_output().expect("wait");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("ALLOWED=yes"), "granted var must be present");
        assert!(
            !text.contains("HOME="),
            "ambient parent env must NOT leak into the child: {text:?}"
        );
    }
}

// Kernel-enforcement proof: the *spawned child* (not just the parent thread)
// inherits the Landlock `fs_write` domain. Only meaningful on Linux with the
// feature and a capable kernel.
#[cfg(all(target_os = "linux", feature = "linux-landlock", test))]
mod landlock_child_tests {
    use super::*;
    use crate::{landlock_is_supported, Gate, Tool};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn ctx(granted: Caveats) -> ToolContext {
        struct AnyTool;
        #[async_trait::async_trait]
        impl Tool for AnyTool {
            fn name(&self) -> &str {
                "any"
            }
            fn schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            async fn invoke(
                &self,
                _a: serde_json::Value,
                _c: &ToolContext,
            ) -> ToolResult<serde_json::Value> {
                Ok(serde_json::Value::Null)
            }
        }
        Gate::new(0)
            .authorize(&AnyTool, &granted)
            .expect("authorize")
    }

    fn unique_dir(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let mut d = std::env::temp_dir();
        d.push(format!(
            "agent-bridle-spawn-{}-{}-{}",
            tag,
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn child_inherits_fs_write_domain_out_of_scope_denied_in_scope_allowed() {
        if !landlock_is_supported() {
            eprintln!("skipping: kernel lacks Landlock");
            return;
        }
        let touch = ["/usr/bin/touch", "/bin/touch"]
            .into_iter()
            .find(|p| std::path::Path::new(p).exists());
        let Some(touch) = touch else {
            eprintln!("skipping: no touch(1) found");
            return;
        };

        let allowed = unique_dir("allowed");
        let forbidden = unique_dir("forbidden");
        let cx = ctx(Caveats {
            exec: Scope::only(["touch".to_string()]),
            fs_write: Scope::only([allowed.to_string_lossy().into_owned()]),
            ..Caveats::top()
        });

        // Out of scope: the child's own write is kernel-denied → non-zero exit.
        let mut out = ConfinedCommand::new(touch)
            .arg(forbidden.join("escape.txt"))
            .spawn(&cx)
            .expect("spawn");
        assert_eq!(out.sandbox_kind, SandboxKind::Landlock);
        let status = out.child.wait().expect("wait");
        assert!(
            !status.success(),
            "child write outside fs_write must be kernel-denied"
        );
        assert!(!forbidden.join("escape.txt").exists());

        // In scope: the child write succeeds.
        let mut ok = ConfinedCommand::new(touch)
            .arg(allowed.join("ok.txt"))
            .spawn(&cx)
            .expect("spawn");
        assert!(ok.child.wait().expect("wait").success());
        assert!(allowed.join("ok.txt").exists());

        let _ = fs::remove_dir_all(&allowed);
        let _ = fs::remove_dir_all(&forbidden);
    }

    /// #144 (I5-B): `ConfinedCommand::sandbox_policy` is honored — a child spawned
    /// with a widened `base_read_paths` can read a file outside `fs_read` scope
    /// that the default policy denies. Proves the builder threads the policy into
    /// `best_available_sandbox` (mechanism rides the builder, not `ToolContext`).
    #[test]
    fn confined_command_honors_sandbox_policy_base_read() {
        if !landlock_is_supported() {
            eprintln!("skipping: kernel lacks Landlock");
            return;
        }
        let cat = ["/usr/bin/cat", "/bin/cat"]
            .into_iter()
            .find(|p| std::path::Path::new(p).exists());
        let Some(cat) = cat else {
            eprintln!("skipping: no cat(1) found");
            return;
        };

        let allowed = unique_dir("cfg-allowed");
        let extra = unique_dir("cfg-extra");
        fs::write(extra.join("data.txt"), b"configured").unwrap();
        let cx = ctx(Caveats {
            exec: Scope::only(["cat".to_string()]),
            fs_read: Scope::only([allowed.to_string_lossy().into_owned()]),
            ..Caveats::top()
        });

        // Control: default policy → the child cannot read the out-of-scope file.
        let mut denied = ConfinedCommand::new(cat)
            .arg(extra.join("data.txt"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn(&cx)
            .expect("spawn");
        assert!(
            !denied.child.wait().expect("wait").success(),
            "default base read must deny the child reading the out-of-scope file"
        );

        // Widened policy: add `extra` to base_read_paths → the child reads it.
        let mut base = SandboxPolicy::default().base_read_paths;
        base.extra.push(extra.to_string_lossy().into_owned());
        let policy = Arc::new(SandboxPolicy {
            base_read_paths: base,
            ..SandboxPolicy::default()
        });
        let mut ok = ConfinedCommand::new(cat)
            .arg(extra.join("data.txt"))
            .sandbox_policy(policy)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn(&cx)
            .expect("spawn");
        assert!(
            ok.child.wait().expect("wait").success(),
            "a config-widened base_read_paths must let the child read the extra file"
        );

        let _ = fs::remove_dir_all(&allowed);
        let _ = fs::remove_dir_all(&extra);
    }
}

// Kernel-enforcement proof for macOS: the *spawned child* (not just the parent)
// is confined by the Seatbelt `sandbox-exec` wrapper that `ConfinedCommand`
// applies — the spawn.rs analog of the Landlock child proof above.
#[cfg(all(target_os = "macos", feature = "macos-seatbelt", test))]
mod seatbelt_child_tests {
    use super::*;
    use crate::{seatbelt_is_supported, Gate, Tool};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn ctx(granted: Caveats) -> ToolContext {
        struct AnyTool;
        #[async_trait::async_trait]
        impl Tool for AnyTool {
            fn name(&self) -> &str {
                "any"
            }
            fn schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            async fn invoke(
                &self,
                _a: serde_json::Value,
                _c: &ToolContext,
            ) -> ToolResult<serde_json::Value> {
                Ok(serde_json::Value::Null)
            }
        }
        Gate::new(0)
            .authorize(&AnyTool, &granted)
            .expect("authorize")
    }

    fn unique_dir(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let mut d = std::env::temp_dir();
        d.push(format!(
            "agent-bridle-spawn-sb-{}-{}-{}",
            tag,
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn child_inherits_fs_write_domain_out_of_scope_denied_in_scope_allowed() {
        if !seatbelt_is_supported() {
            eprintln!("skipping: /usr/bin/sandbox-exec unavailable");
            return;
        }
        let allowed = unique_dir("allowed");
        let forbidden = unique_dir("forbidden");
        let cx = ctx(Caveats {
            // Absolute program path: the environment is scrubbed, so sandbox-exec
            // cannot resolve a bare name via PATH (see `wrap_argv`).
            exec: Scope::only(["/usr/bin/touch".to_string()]),
            fs_write: Scope::only([allowed.to_string_lossy().into_owned()]),
            ..Caveats::top()
        });

        // Out of scope: the child's own write is kernel-denied → non-zero exit.
        let mut out = ConfinedCommand::new("/usr/bin/touch")
            .arg(forbidden.join("escape.txt"))
            .spawn(&cx)
            .expect("spawn");
        assert_eq!(out.sandbox_kind, SandboxKind::Seatbelt);
        let status = out.child.wait().expect("wait");
        assert!(
            !status.success(),
            "child write outside fs_write must be kernel-denied"
        );
        assert!(!forbidden.join("escape.txt").exists());

        // In scope: the child write succeeds.
        let mut ok = ConfinedCommand::new("/usr/bin/touch")
            .arg(allowed.join("ok.txt"))
            .spawn(&cx)
            .expect("spawn");
        assert!(ok.child.wait().expect("wait").success());
        assert!(allowed.join("ok.txt").exists());

        let _ = fs::remove_dir_all(&allowed);
        let _ = fs::remove_dir_all(&forbidden);
    }

    /// Honesty (I9): a fully permissive grant confines *nothing*, so the Seatbelt
    /// wrapper applies nothing and the child must be reported `None`, never the raw
    /// backend kind. This is the regression for the original overclaim where
    /// `sandbox_kind` was the backend kind regardless of whether anything was
    /// confined.
    #[test]
    fn top_grant_confines_nothing_reports_none() {
        if !seatbelt_is_supported() {
            eprintln!("skipping: /usr/bin/sandbox-exec unavailable");
            return;
        }
        let cx = ctx(Caveats::top());
        let child = ConfinedCommand::new("/usr/bin/true")
            .spawn(&cx)
            .expect("spawn");
        assert_eq!(
            child.sandbox_kind,
            SandboxKind::None,
            "nothing restricted => nothing confined => None, not the raw backend kind"
        );
    }

    /// A restricted `exec` axis engages Seatbelt **even when both fs axes are
    /// `All`**: `process-exec*` kernel-confines the exec axis (ADR 0014), so
    /// reporting `Seatbelt` is honest, not an overclaim — the inverse of the
    /// `top_grant…` guard above. Before ADR 0014 this same grant reported `None`
    /// (the exec axis was left ambient).
    #[test]
    fn restricted_exec_engages_seatbelt() {
        if !seatbelt_is_supported() {
            eprintln!("skipping: /usr/bin/sandbox-exec unavailable");
            return;
        }
        // exec restricted, both fs axes `All` — a grant a host might give an MCP
        // server: confine *what may run*, leave the filesystem ambient.
        let cx = ctx(Caveats {
            exec: Scope::only(["/usr/bin/true".to_string()]),
            ..Caveats::top()
        });
        let child = ConfinedCommand::new("/usr/bin/true")
            .spawn(&cx)
            .expect("spawn");
        assert_eq!(
            child.sandbox_kind,
            SandboxKind::Seatbelt,
            "a restricted exec axis is kernel-confined by process-exec* (ADR 0014)"
        );
    }

    /// Restricted Seatbelt network authority remains held: admission must refuse
    /// before the program is spawned, regardless of the defense-in-depth profile.
    #[test]
    fn restricted_net_authority_is_denied_before_spawn() {
        if !seatbelt_is_supported() {
            eprintln!("skipping: /usr/bin/sandbox-exec unavailable");
            return;
        }
        let dir = unique_dir("net-held");
        let marker = dir.join("must-not-spawn");
        let cx = ctx(Caveats {
            net: Scope::none(),
            ..Caveats::top()
        });
        match ConfinedCommand::new("/usr/bin/touch")
            .arg(&marker)
            .spawn(&cx)
        {
            Err(ToolError::Denied { .. }) => {}
            Err(other) => panic!("expected a restricted-network authority denial, got {other}"),
            Ok(mut spawned) => {
                let _ = spawned.child.kill();
                panic!("restricted network authority must be denied before spawn");
            }
        }
        assert!(
            !marker.exists(),
            "the denied command must never have executed"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}

// Model: gpt-6-astra | Harness: Codex 0.153.4 | Operator: Shawn Hartsock | Time: 22:29 UTC | Date: 2026-09-12
