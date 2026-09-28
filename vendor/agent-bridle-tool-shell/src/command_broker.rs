//! Host-owned command preparation and scoped helper callbacks.
//!
//! These callbacks do not execute a command on the host or grant authority. The
//! original native command runs once inside the worker's existing fence. Helper
//! messages remain untrusted input, even after transport identity is verified.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use agent_bridle_core::{ToolContext, ToolError, ToolResult};

/// The final native command before any host-provided preparation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BrokerCommand {
    /// Resolved executable path; source spelling is kept separately.
    pub program: PathBuf,
    /// Original command spelling before executable resolution.
    pub original_command: OsString,
    /// Argument zero presented to the native program.
    pub argv0: OsString,
    /// Exact argument boundaries, excluding argv[0].
    pub args: Vec<OsString>,
    /// Explicit exported environment; ambient environment is never inherited.
    pub env: Vec<(OsString, OsString)>,
    /// Final absolute working directory.
    pub cwd: PathBuf,
}

/// Cancellation and lifetime bound shared with the owning worker invocation.
#[derive(Clone)]
pub struct BrokerControl {
    pub(crate) cancelled: Arc<AtomicBool>,
    pub(crate) deadline: Instant,
}

impl BrokerControl {
    /// Whether the owner has cancelled or the invocation deadline has elapsed.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire) || Instant::now() >= self.deadline
    }

    /// Refuse work after cancellation or timeout.
    pub fn check(&self) -> ToolResult<()> {
        if self.is_cancelled() {
            Err(ToolError::denied(
                "command broker invocation cancelled or expired",
            ))
        } else {
            Ok(())
        }
    }

    /// The invocation's fixed deadline; a callback cannot extend it.
    pub fn deadline(&self) -> Instant {
        self.deadline
    }
}

/// Kernel-observed identity of a helper making a scoped callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerPeer {
    /// Actual peer process ID, not a value asserted by the request body.
    pub process_id: u32,
    /// OS process birth identity of the helper, guarding PID reuse.
    pub process_birth: u64,
    /// Actual direct parent process ID.
    pub parent_process_id: u32,
    /// Canonical executable image of the helper.
    pub executable: PathBuf,
    /// Canonical executable image of its direct parent.
    pub parent_executable: PathBuf,
    /// OS process birth identity of the direct parent, guarding PID reuse.
    pub parent_birth: u64,
}

/// Kernel-observed native process identity captured at spawn registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerProcess {
    /// Actual native child process ID.
    pub process_id: u32,
    /// Canonical executable path.
    pub executable: PathBuf,
    /// OS birth identity captured before helper requests are admitted.
    pub birth: u64,
}

/// Invocation-local opaque helper policy and any resources it must retain.
///
/// The owner retains this object until the native worker has been stopped and
/// reaped and all callbacks have joined. Implementations can own hook-directory
/// leases here. A helper's message kind or success claim is never authority:
/// validate canonical payloads and actual repository state independently.
pub trait BrokerSession: Send + Sync {
    /// Bind the native child's actual PID before any helper request is admitted.
    /// An error aborts the launch owner; no helper receives a successful reply.
    fn on_spawn(&self, process: &BrokerProcess, control: &BrokerControl) -> ToolResult<()>;

    /// Handle bounded opaque bytes from an authenticated helper.
    fn request(
        &self,
        peer: &BrokerPeer,
        payload: &[u8],
        control: &BrokerControl,
    ) -> ToolResult<Vec<u8>>;
}

/// Host preparation for a selected native command.
///
/// Program, cwd, argv[0], and stdio remain unchanged. The final execution policy
/// rechecks the resulting command and explicit descriptor delegation before
/// spawn. The callback session is kept alive through owner cleanup.
pub struct PreparedBrokerCommand {
    /// Explicit trusted image reached by a platform launcher after exec.
    /// `None` requires the exact admitted executable image. This is host policy
    /// data, never learned from a helper's reported path or basename.
    pub expected_process_image: Option<PathBuf>,
    /// Replacement native argument vector, or `None` to retain exact arguments.
    pub args: Option<Vec<OsString>>,
    /// Environment updates for protected helpers; other entries are retained.
    pub env: Vec<(OsString, OsString)>,
    /// Explicit Unix descriptor number for the scoped helper endpoint (>=3).
    /// A collision with an existing shell redirection is a refusal.
    pub target_fd: i32,
    /// Scoped host callback policy and resource lease.
    pub session: Arc<dyn BrokerSession>,
}

/// Host-owned optional preparation of already-resolved native commands.
///
/// Returning `None` leaves normal execution unchanged. The context is the exact
/// original invocation context; a callback cannot replace or widen it. This is
/// not a host command-execution API.
pub trait CommandBroker: Send + Sync {
    /// Read-only fixed-worker alias directories prepared before admission.
    ///
    /// Only flat symlink directories targeting the fixed worker image qualify;
    /// no additional executable authority is introduced. The fence refuses any
    /// effective write scope that reaches these paths or
    /// their relocation ancestors. They do not widen the invocation context.
    /// In particular, an all-files write grant cannot protect these resources.
    fn read_only_resources(&self, _context: &ToolContext) -> ToolResult<Vec<PathBuf>> {
        Ok(Vec::new())
    }

    /// Prepare a command or leave it unchanged, within the supplied context.
    fn prepare(
        &self,
        command: &BrokerCommand,
        context: &ToolContext,
        control: &BrokerControl,
    ) -> ToolResult<Option<PreparedBrokerCommand>>;
}
