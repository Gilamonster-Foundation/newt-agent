//! Real-resource proof that the **`ShellTool` production path** installs the
//! `ChildNetworkPolicy::DenyDirect` seccomp egress floor on the child it spawns
//! — closing the UDP/DNS/raw/packet leg Landlock's TCP-only net rule misses —
//! and that the floor is inherited by a forked/exec'd descendant.
//!
//! This grounds the direct-thread `apply` proofs in `agent-bridle-core`
//! (`sandbox::landlock_kernel_tests::deny_direct_*`) against the real tool: it
//! proves the `SandboxPolicy.child_network` field is actually threaded from
//! `ShellTool::with_sandbox_policy` through `run_confined` → `Sandbox::apply` on
//! the same thread that spawns the child.
//!
//! Linux + `linux-landlock`, gated on kernel Landlock + a `python3` probe (the
//! child that attempts an AF_INET socket). Where either is absent the test skips.
#![cfg(all(target_os = "linux", feature = "linux-landlock"))]

use agent_bridle_core::{
    landlock_is_supported, Caveats, ChildNetworkPolicy, Gate, SandboxPolicy, Scope, Tool,
    ToolContext,
};
use agent_bridle_tool_shell::ShellTool;

/// A `ShellTool` carrying the requested child-network policy.
fn tool(child_network: ChildNetworkPolicy) -> ShellTool {
    ShellTool::new().with_sandbox_policy(SandboxPolicy {
        child_network,
        ..SandboxPolicy::default()
    })
}

/// Mint a context for `tool` granting exec of `execs` and denying ALL network.
/// `net: none` arms `DenyDirect`; under `LandlockOnly` it is intentionally
/// inadmissible because Landlock cannot conservatively bound direct sockets.
fn ctx(tool: &ShellTool, execs: &[&str]) -> ToolContext {
    let granted = Caveats {
        exec: Scope::only(execs.iter().map(|s| (*s).to_string())),
        net: Scope::none(),
        ..Caveats::top()
    };
    Gate::new(0).authorize(tool, &granted).expect("authorize")
}

/// Mint a context granting exec + AMBIENT network (`net: All`). No `net:none`, so
/// `DenyDirect` does not fire and the seccomp egress/io_uring floor is NOT
/// installed — the capability baseline for the io_uring positive control.
fn ctx_net_all(tool: &ShellTool, execs: &[&str]) -> ToolContext {
    let granted = Caveats {
        exec: Scope::only(execs.iter().map(|s| (*s).to_string())),
        net: Scope::All,
        ..Caveats::top()
    };
    Gate::new(0).authorize(tool, &granted).expect("authorize")
}

fn skip() -> bool {
    if !landlock_is_supported() {
        eprintln!("skipping: kernel lacks Landlock");
        return true;
    }
    if !std::path::Path::new("/usr/bin/python3").exists() {
        eprintln!("skipping: /usr/bin/python3 (the AF_INET probe) not present");
        return true;
    }
    false
}

/// The child CREATES an AF_INET datagram socket. Under DenyDirect the seccomp
/// filter EACCES-denies `socket()` and Python exits non-zero. Under LandlockOnly,
/// production admission refuses `net: none` before this probe can execute.
const PROBE: &str =
    r#"python3 -c "import socket; socket.socket(socket.AF_INET, socket.SOCK_DGRAM)""#;

/// A forked/exec'd descendant probe: the child python re-execs python to attempt
/// the socket in a grandchild, and propagates its exit code — so a non-zero
/// result proves the seccomp floor was inherited across the fork/exec.
const DESCENDANT_PROBE: &str = r#"python3 -c "import subprocess,sys; sys.exit(subprocess.run([sys.executable,'-c','import socket; socket.socket(socket.AF_INET, socket.SOCK_DGRAM)']).returncode)""#;

#[tokio::test]
async fn shell_tool_deny_direct_denies_a_childs_udp_socket() {
    if skip() {
        return;
    }
    let t = tool(ChildNetworkPolicy::DenyDirect);
    let out = t
        .invoke(serde_json::json!({ "cmd": PROBE }), &ctx(&t, &["python3"]))
        .await
        .expect("invoke");
    // The confined path must actually have been taken (else the floor never ran).
    assert_eq!(
        out["sandbox_kind"], "landlock",
        "the child must be kernel-confined: {out}"
    );
    assert_ne!(
        out["exit_code"], 0,
        "DenyDirect must deny the child's AF_INET socket creation: {out}"
    );
}

#[tokio::test]
async fn shell_tool_deny_direct_denies_a_forked_descendants_socket() {
    if skip() {
        return;
    }
    let t = tool(ChildNetworkPolicy::DenyDirect);
    let out = t
        .invoke(
            serde_json::json!({ "cmd": DESCENDANT_PROBE }),
            &ctx(&t, &["python3"]),
        )
        .await
        .expect("invoke");
    assert_ne!(
        out["exit_code"], 0,
        "DenyDirect must be inherited across fork/exec into a descendant: {out}"
    );
}

#[tokio::test]
async fn shell_tool_landlock_only_refuses_unenforceable_network_restriction() {
    if skip() {
        return;
    }
    // Default policy == LandlockOnly cannot conservatively bound the child's
    // direct network authority. The restricted request must fail before spawn,
    // rather than silently running with UDP access.
    let t = tool(ChildNetworkPolicy::LandlockOnly);
    let out = t
        .invoke(serde_json::json!({ "cmd": PROBE }), &ctx(&t, &["python3"]))
        .await
        .expect("invoke");
    assert_eq!(out["denied"], true, "restricted net must be refused: {out}");
    assert_eq!(
        out["denials"][0]["kind"], "net",
        "refusal must identify the unresolved network axis: {out}"
    );
    assert!(
        out.get("exit_code").is_none(),
        "the socket probe must not have spawned: {out}"
    );
}

#[tokio::test]
async fn shell_tool_deny_direct_leaves_ordinary_non_network_work_unchanged() {
    if skip() {
        return;
    }
    // A non-network command runs normally under DenyDirect — the floor only
    // touches off-box socket creation; stdout capture and the shell path are
    // otherwise unchanged.
    let t = tool(ChildNetworkPolicy::DenyDirect);
    let out = t
        .invoke(
            serde_json::json!({ "cmd": "echo hello-from-confined" }),
            &ctx(&t, &["echo"]),
        )
        .await
        .expect("invoke");
    assert_eq!(out["exit_code"], 0, "ordinary command must run: {out}");
    assert!(
        out["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("hello-from-confined"),
        "stdout must be captured unchanged: {out}"
    );
}

// ── E3: the io_uring egress floor (ASM-SECCOMP-IOURING) ──────────────────────
//
// `net:none` on Landlock is bypassable off-box because `IORING_OP_SOCKET` +
// `IORING_OP_CONNECT`/`SEND` create and use a socket WITHOUT the `socket()`
// syscall, so the socket()-only seccomp deny misses it. The honest close is to
// deny the io_uring setup/enter primitive (agent-bridle-core installs
// `SYS_io_uring_setup`/`enter`/`register` = EACCES on the DenyDirect leg). These
// tests prove that natively: under DenyDirect a child cannot even create a ring,
// with a capability positive control so a host that lacks io_uring SKIPS rather
// than passing vacuously. `io_uring_setup` is syscall 425 on x86_64 + aarch64.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
mod io_uring_e3 {
    use super::*;

    /// Attempt `io_uring_setup(1, &params)` via ctypes and map the outcome to an
    /// exit code: 0 = a ring was created (NOT denied), 13 = EACCES (the Bridle
    /// seccomp deny, errno-exact), 20 = denied with some OTHER errno. A 256-byte
    /// zeroed params buffer is ≥ the kernel struct, so the kernel's `copy_from_user`
    /// never faults on the positive-control path.
    const IOURING_PROBE: &str = r#"python3 -c "import ctypes,os; libc=ctypes.CDLL(None,use_errno=True); p=(ctypes.c_ubyte*256)(); fd=libc.syscall(425,1,ctypes.byref(p)); os.close(fd) if fd>=0 else None; os._exit(0 if fd>=0 else (13 if ctypes.get_errno()==13 else 20))""#;

    /// The same probe re-exec'd in a grandchild, propagating its code — proves the
    /// io_uring deny is inherited across fork/exec like the socket() deny.
    const IOURING_DESCENDANT_PROBE: &str = r#"python3 -c "import subprocess,sys; sys.exit(subprocess.run([sys.executable,'-c','import ctypes,os; libc=ctypes.CDLL(None,use_errno=True); p=(ctypes.c_ubyte*256)(); fd=libc.syscall(425,1,ctypes.byref(p)); os.close(fd) if fd>=0 else None; os._exit(0 if fd>=0 else (13 if ctypes.get_errno()==13 else 20))']).returncode)""#;

    /// Whether THIS host can create an io_uring ring at all — run the probe with
    /// AMBIENT net (no DenyDirect, so no io_uring deny). If it cannot (kernel too
    /// old, `io_uring_disabled` sysctl, an outer container seccomp), the denial
    /// test would pass vacuously, so it must SKIP instead. SKIP is not PASS.
    async fn io_uring_capable() -> bool {
        let t = tool(ChildNetworkPolicy::LandlockOnly);
        let out = t
            .invoke(
                serde_json::json!({ "cmd": IOURING_PROBE }),
                &ctx_net_all(&t, &["python3"]),
            )
            .await
            .expect("invoke");
        out["exit_code"] == 0
    }

    #[tokio::test]
    async fn io_uring_positive_control_the_host_can_create_a_ring() {
        if skip() {
            return;
        }
        if !io_uring_capable().await {
            eprintln!(
                "skipping E3: this host cannot create an io_uring ring even unconfined \
                       (kernel/sysctl/outer-seccomp) — the denial proof would be vacuous"
            );
        }
    }

    #[tokio::test]
    async fn deny_direct_denies_a_childs_io_uring_setup_with_eacces() {
        if skip() || !io_uring_capable().await {
            eprintln!("skipping E3 denial: io_uring not creatable on this host (SKIP is not PASS)");
            return;
        }
        let t = tool(ChildNetworkPolicy::DenyDirect);
        let out = t
            .invoke(
                serde_json::json!({ "cmd": IOURING_PROBE }),
                &ctx(&t, &["python3"]),
            )
            .await
            .expect("invoke");
        // The confined path must actually have been taken (else the floor never ran).
        assert_eq!(
            out["sandbox_kind"], "landlock",
            "the child must be kernel-confined: {out}"
        );
        // Errno-EXACT: EACCES (13) is Bridle's rule; any other errno (20) would be
        // an outer layer, not our floor.
        assert_eq!(
            out["exit_code"], 13,
            "DenyDirect must deny io_uring_setup with EACCES (not another errno / not allowed): {out}"
        );
    }

    #[tokio::test]
    async fn deny_direct_denies_a_forked_descendants_io_uring_setup() {
        if skip() || !io_uring_capable().await {
            eprintln!(
                "skipping E3 descendant: io_uring not creatable on this host (SKIP is not PASS)"
            );
            return;
        }
        let t = tool(ChildNetworkPolicy::DenyDirect);
        let out = t
            .invoke(
                serde_json::json!({ "cmd": IOURING_DESCENDANT_PROBE }),
                &ctx(&t, &["python3"]),
            )
            .await
            .expect("invoke");
        assert_eq!(
            out["exit_code"], 13,
            "the io_uring deny must be inherited across fork/exec (EACCES in the grandchild): {out}"
        );
    }
}
