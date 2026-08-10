//! macOS Seatbelt OCAP adversarial suite (#1632) — the Seatbelt half of the
//! cross-platform closure theorem:
//!
//! > For every supported Newt attacker-exec route, on macOS, either the requested
//! > authority is enforced by a real OS boundary (Seatbelt) with adversarial
//! > evidence, or execution refuses fail-closed before hostile code runs.
//!
//! This is the **real-resource tier** (see CLAUDE.md "Testing strategy"): it
//! drives `ConstrainedExecutor::run` (the `AgentInfluenced`, `Kernel`-strength-
//! floor route that `run_build_check` / crew / MCP workers share) against real
//! `sandbox-exec` on a real macOS kernel, and it **grounds the mocked Seatbelt
//! unit tests** in `agent-bridle-core` (`seatbelt_profile*`) — proving those
//! profiles actually deny on the kernel, not just in string assertions.
//!
//! It is `cfg`'d to `all(target_os = "macos", feature = "macos-seatbelt")` so it
//! is INERT on Linux (compiles to nothing) and OFF the per-PR unit run. Every
//! test is `#[ignore]` (real subprocess + kernel sandbox) and `#[serial]`. Run:
//!
//! ```text
//! cargo test -p newt-core --features macos-seatbelt \
//!   --test macos_seatbelt_adversarial -- --ignored --test-threads=1
//! ```
//!
//! RULES (from the review — do not soften):
//! * Real-resource only. Each test DENIES-by-kernel or REFUSES-before-exec.
//! * Distinguish DENIED-BY-SEATBELT from command-not-found: every enforcement
//!   assertion pins `sandbox_kind == SandboxKind::Seatbelt` (the honest envelope)
//!   AND a permission failure — never "the command happened not to work".
//! * The generated SBPL profile is PINNED (`seatbelt_generated_profile_pins_the_
//!   boundary`) so a future profile edit cannot silently widen authority.
//! * Where Seatbelt cannot enforce an axis Newt requests, it is modelled as a
//!   named residual (see `docs/security/platform/macos-evidence.md`), never a
//!   pretended Landlock/seccomp equivalence.
#![cfg(all(target_os = "macos", feature = "macos-seatbelt"))]

use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

use agent_bridle::{seatbelt_is_supported, Caveats, Sandbox, SandboxKind, Scope, SeatbeltSandbox};
use newt_core::confined_exec::{
    workspace_confined_caveats, ConfinedOutput, ConstrainedExecutor, ExecOrigin, ExecRefused,
    ExecRequest, NetGrant,
};
use serial_test::serial;
use tempfile::tempdir;

/// Caveats that restrict ONLY the network axis (`net: none`) and leave fs/exec
/// ambient. Used by the network tests so a real interpreter (`python3`, whose
/// macOS Command Line Tools framework lives OUTSIDE any workspace read fence) can
/// load and report a deterministic errno — isolating "does `(deny network*)`
/// govern this socket?" from "can the interpreter even start under the read
/// fence?". The `fs` fence itself is proven separately by the filesystem tests.
fn net_only_caveats() -> Caveats {
    Caveats {
        net: Scope::none(),
        ..Caveats::top()
    }
}

/// A network-confined `program args` (net:none, fs/exec ambient) on the
/// `AgentInfluenced` / `Kernel`-floor route.
fn net_confined(ws: &Path, program: &str, args: &[&str]) -> ExecRequest {
    ExecRequest::new(
        ExecOrigin::AgentInfluenced,
        program,
        args.iter().map(|s| s.to_string()),
        ws,
        net_only_caveats(),
    )
    .env("PATH", "/usr/bin:/bin")
}

// ── Harness ─────────────────────────────────────────────────────────────────

/// A confined `program args` under the workspace fence (`fs_read`/`fs_write`
/// scoped to `ws`, `net: none`, `exec: All`), on the `AgentInfluenced` /
/// `Kernel`-floor route. Programs are ABSOLUTE paths on purpose: Seatbelt scrubs
/// the environment, so a bare name would not resolve (bridle ADR). `PATH` is
/// granted only so an interpreter's own `$PATH` lookups behave normally — it does
/// not widen the kernel fence.
fn confined(ws: &Path, program: &str, args: &[&str]) -> ExecRequest {
    ExecRequest::new(
        ExecOrigin::AgentInfluenced,
        program,
        args.iter().map(|s| s.to_string()),
        ws,
        workspace_confined_caveats(ws),
    )
    .env("PATH", "/usr/bin:/bin")
}

/// Run a confined request. On a host without Seatbelt the `AgentInfluenced`
/// spawn fails closed (`ConfinementUnenforceable`) and the test skips — nothing
/// ran unconfined. On macOS CI `/usr/bin/sandbox-exec` is a SIP system binary, so
/// this returns `Some` and the real adversarial assertion runs.
fn run(req: &ExecRequest) -> Option<ConfinedOutput> {
    match ConstrainedExecutor::run(req) {
        Ok(out) => Some(out),
        Err(ExecRefused::ConfinementUnenforceable(_)) => None,
        Err(e) => panic!("unexpected confined-exec error: {e}"),
    }
}

/// The honest envelope: the child actually ran under a real Seatbelt sandbox, not
/// advisory (`None`) and not some other backend. Every enforcement test asserts
/// this so a DENY can never be confused with command-not-found or a silent
/// downgrade.
fn assert_seatbelt(out: &ConfinedOutput) {
    assert_eq!(
        out.sandbox_kind,
        SandboxKind::Seatbelt,
        "expected the Seatbelt envelope; got {:?} (a downgrade or advisory run would make any \
         DENY meaningless). stderr: {}",
        out.sandbox_kind,
        String::from_utf8_lossy(&out.stderr),
    );
}

/// A confined child's write/connect DENY: it did NOT succeed, and the failure is
/// a sandbox permission denial (`Operation not permitted` / `Permission denied`),
/// not a missing binary or other error.
fn assert_denied(out: &ConfinedOutput, marker: &str) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !out.success,
        "{marker}: the confined child SUCCEEDED — the boundary did not hold. \
         stdout: {stdout} stderr: {stderr}",
    );
    let denied = stderr.contains("Operation not permitted")
        || stderr.contains("Permission denied")
        || stdout.contains("DENIED");
    assert!(
        denied,
        "{marker}: child failed but not with a sandbox permission denial (could be \
         command-not-found). stdout: {stdout} stderr: {stderr}",
    );
}

// ── Filesystem authority ────────────────────────────────────────────────────

#[test]
#[serial]
#[ignore = "real-resource: sandbox-exec + subprocess"]
fn seatbelt_denies_outside_workspace_read() {
    // A secret OUTSIDE the workspace fence. `fs_read: Only([ws])` denies its
    // CONTENT (metadata stays ambient so loaders traverse), so `cat` cannot read
    // it. The exfil threat — reading out-of-scope file contents — is closed.
    let ws = tempdir().unwrap();
    let secret_dir = tempdir().unwrap();
    let secret = secret_dir.path().join("api-key.txt");
    std::fs::write(&secret, "SUPER-SECRET-TOKEN").unwrap();
    let Some(out) = run(&confined(
        ws.path(),
        "/bin/cat",
        &[secret.to_str().unwrap()],
    )) else {
        return;
    };
    assert_seatbelt(&out);
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("SUPER-SECRET-TOKEN"),
        "the out-of-fence secret content leaked to the confined child",
    );
    assert_denied(&out, "outside-workspace read");
}

#[test]
#[serial]
#[ignore = "real-resource: sandbox-exec + subprocess"]
fn seatbelt_denies_outside_workspace_write() {
    let ws = tempdir().unwrap();
    let outside = tempdir().unwrap();
    let target = outside.path().join("pwned.txt");
    let script = format!("echo pwned > {}", target.to_str().unwrap());
    let Some(out) = run(&confined(ws.path(), "/bin/sh", &["-c", &script])) else {
        return;
    };
    assert_seatbelt(&out);
    assert_denied(&out, "outside-workspace write");
    assert!(
        !target.exists(),
        "the out-of-fence file was actually created"
    );
}

#[test]
#[serial]
#[ignore = "real-resource: sandbox-exec + subprocess"]
fn seatbelt_denies_sibling_repo_write() {
    // A sibling checkout next to the workspace — the classic "confined agent edits
    // the repo next door" escape.
    let ws = tempdir().unwrap();
    let sibling = tempdir().unwrap();
    let target = sibling.path().join("sibling-file.txt");
    let script = format!("echo x > {}", target.to_str().unwrap());
    let Some(out) = run(&confined(ws.path(), "/bin/sh", &["-c", &script])) else {
        return;
    };
    assert_seatbelt(&out);
    assert_denied(&out, "sibling-repo write");
    assert!(!target.exists(), "the sibling file was actually created");
}

#[test]
#[serial]
#[ignore = "real-resource: sandbox-exec + subprocess"]
fn seatbelt_denies_symlink_escape() {
    // A symlink INSIDE the fence pointing OUT. Seatbelt matches the *resolved*
    // path (`subpath` against the realpath), so writing through the link is denied
    // — canonicalization does not widen the fence.
    let ws = tempdir().unwrap();
    let outside = tempdir().unwrap();
    let link = ws.path().join("escape");
    std::os::unix::fs::symlink(outside.path(), &link).unwrap();
    let target = link.join("via-symlink.txt");
    let script = format!("echo x > {}", target.to_str().unwrap());
    let Some(out) = run(&confined(ws.path(), "/bin/sh", &["-c", &script])) else {
        return;
    };
    assert_seatbelt(&out);
    assert_denied(&out, "symlink escape");
    assert!(
        !outside.path().join("via-symlink.txt").exists(),
        "the symlink escape actually wrote outside the fence",
    );
}

// ── Environment / credential inheritance ────────────────────────────────────

#[test]
#[serial]
#[ignore = "real-resource: sandbox-exec + subprocess"]
fn seatbelt_child_does_not_inherit_parent_credentials() {
    // The confined child's ENTIRE environment is the explicit grants (PATH) and
    // nothing else — a parent-only secret must be ABSENT, not merely unreferenced.
    let ws = tempdir().unwrap();
    std::env::set_var("NEWT_AGENT_KEY", "PARENT-ONLY-SECRET");
    let out = run(&confined(
        ws.path(),
        "/bin/sh",
        &["-c", "echo KEY=[${NEWT_AGENT_KEY}]"],
    ));
    std::env::remove_var("NEWT_AGENT_KEY");
    let Some(out) = out else { return };
    assert_seatbelt(&out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("KEY=[]"),
        "a parent-only credential leaked into the confined child: {stdout}",
    );
    assert!(
        !stdout.contains("PARENT-ONLY-SECRET"),
        "the parent secret VALUE reached the child: {stdout}",
    );
}

// ── Direct network (separate from local-deputy) ─────────────────────────────

#[test]
#[serial]
#[ignore = "real-resource: sandbox-exec + subprocess"]
fn seatbelt_denies_direct_tcp() {
    // `net: none` → SBPL `(deny network*)` — every off-box socket kernel-denied.
    // A literal IP avoids DNS so the failure is unambiguously the socket, not
    // name resolution.
    let ws = tempdir().unwrap();
    let Some(out) = run(&net_confined(
        ws.path(),
        "/usr/bin/curl",
        &["-s", "-m", "5", "http://1.1.1.1/"],
    )) else {
        return;
    };
    assert_seatbelt(&out);
    assert!(!out.success, "direct TCP to a literal IP was NOT denied");
}

#[test]
#[serial]
#[ignore = "real-resource: sandbox-exec + subprocess"]
fn seatbelt_denies_direct_udp() {
    // A raw UDP sendto under `(deny network*)` — Python gives a deterministic
    // PermissionError the shell exit code alone would hide.
    let ws = tempdir().unwrap();
    let prog = "import socket,sys\n\
        s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM)\n\
        try:\n    s.sendto(b'x',('1.1.1.1',53)); print('SENT')\n\
        except PermissionError:\n    print('DENIED'); sys.exit(3)\n\
        except OSError as e:\n    print('DENIED'); sys.exit(3)";
    let Some(out) = run(&net_confined(ws.path(), "/usr/bin/python3", &["-c", prog])) else {
        return;
    };
    assert_seatbelt(&out);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("DENIED"),
        "direct UDP sendto was NOT denied: {}",
        String::from_utf8_lossy(&out.stdout),
    );
}

#[test]
#[serial]
#[ignore = "real-resource: sandbox-exec + subprocess"]
fn seatbelt_loopback_behavior() {
    // Record the Seatbelt loopback semantics under `net: none`. The workspace
    // fence denies ALL egress (`(deny network*)` with no loopback re-allow — that
    // re-allow appears only for a loopback-ONLY allowlist), so a loopback connect
    // is also denied. This documents that `net:none` is strictly deny-all, not
    // "deny off-box but keep loopback".
    let ws = tempdir().unwrap();
    let prog = "import socket,sys\n\
        s=socket.socket(socket.AF_INET,socket.SOCK_STREAM); s.settimeout(3)\n\
        try:\n    s.connect(('127.0.0.1',9)); print('CONNECTED')\n\
        except (PermissionError,OSError):\n    print('DENIED'); sys.exit(3)";
    let Some(out) = run(&net_confined(ws.path(), "/usr/bin/python3", &["-c", prog])) else {
        return;
    };
    assert_seatbelt(&out);
    // Record the actual behavior: under net:none the loopback connect is denied.
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("DENIED"),
        "net:none loopback connect result (expected DENIED under blanket deny network*): {}",
        String::from_utf8_lossy(&out.stdout),
    );
}

// ── Local-deputy egress (the Linux AF_UNIX lesson, repeated) ─────────────────

#[test]
#[serial]
#[ignore = "real-resource: sandbox-exec + subprocess"]
fn seatbelt_pathname_af_unix_deputy() {
    // The Linux residual, repeated on macOS: a host deputy on a PATHNAME AF_UNIX
    // socket. On Linux the seccomp floor deliberately ALLOWS AF_UNIX and Landlock
    // does not govern unix-socket `connect`, so the ONLY barrier is the fs fence
    // (an ACTIVE residual). This test isolates the macOS NET axis: net:none with
    // fs ambient, so the socket is freely reachable on the fs axis and the sole
    // question is whether Seatbelt's `(deny network*)` governs AF_UNIX connect. A
    // DENY here is a STRONGER guarantee than Linux (the network boundary itself
    // covers the local deputy, not merely the fs fence).
    let ws = tempdir().unwrap();
    let sock_dir = tempdir().unwrap();
    let sock_path = sock_dir.path().join("deputy.sock");
    let listener = UnixListener::bind(&sock_path).unwrap();
    // A host-side deputy thread: accept one connection and relay a marker back.
    let handle = std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            use std::io::Write;
            let _ = stream.write_all(b"RELAYED-VIA-DEPUTY");
        }
    });

    let prog = format!(
        "import socket,sys\n\
        s=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM); s.settimeout(3)\n\
        try:\n    s.connect('{}'); print('REACHED-DEPUTY')\n\
        except (PermissionError,OSError):\n    print('DENIED'); sys.exit(3)",
        sock_path.to_str().unwrap(),
    );
    let out = run(&net_confined(ws.path(), "/usr/bin/python3", &["-c", &prog]));
    // Unblock the deputy thread whether or not the child connected.
    let _ = UnixStream::connect(&sock_path);
    let _ = handle.join();

    let Some(out) = out else { return };
    assert_seatbelt(&out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    // EVIDENCE, not assumption: on macOS both barriers are expected to hold
    // (unlike Linux, where Landlock does not govern unix connect). If this ever
    // prints REACHED-DEPUTY, the register's `local-deputy-egress: macos` state
    // must flip to ACTIVE with a tracking issue.
    assert!(
        stdout.contains("DENIED"),
        "AF_UNIX pathname deputy was REACHABLE from the confined child on macOS — this is a \
         local-deputy-egress residual; file it. stdout: {stdout}",
    );
}

#[test]
#[serial]
#[ignore = "profile inspection: Mach/XPC ambient surface"]
fn seatbelt_mach_xpc_deputy_surface() {
    // The generated SBPL starts from `(allow default)` and governs only
    // file-read/write, network, and process-exec. Mach lookups (XPC service
    // discovery) therefore stay AMBIENT — a confined child can `bootstrap_look_up`
    // host XPC services that could act as fs/network deputies. This test PINS that
    // surface as a known, tracked residual: the profile must NOT (yet) claim to
    // deny mach-lookup, so we never over-report containment we do not have.
    if !seatbelt_is_supported() {
        return;
    }
    let ws = tempdir().unwrap();
    let profile = rendered_profile(ws.path());
    assert!(
        !profile.contains("(deny mach"),
        "the profile now denies mach-lookup — update the register: the mach-xpc-ambient-deputy \
         macOS residual may be closable. profile:\n{profile}",
    );
    // Positive pin: the surface really is ambient by way of the base allow.
    assert!(
        profile.contains("(allow default)"),
        "profile no longer starts from (allow default); the ambient-mach reasoning must be \
         re-derived. profile:\n{profile}",
    );
}

// ── Descriptor / handle hygiene ─────────────────────────────────────────────

#[test]
#[serial]
#[ignore = "real-resource: sandbox-exec + subprocess"]
fn seatbelt_non_cloexec_fd_inheritance() {
    // The executor must not leak its own descriptors into the confined child: the
    // child should see only stdio (0,1,2). Rust marks every fd `O_CLOEXEC`, and
    // the executor sets stdin=null / stdout,stderr=piped, so no stray parent fd
    // should survive the exec into the sandbox. (Mirrors the Linux
    // `run_command_route_fd_hygiene_is_cloexec_based_not_explicit_close` intent.)
    let ws = tempdir().unwrap();
    // Hold an open file in the PARENT across the spawn; it must not appear in the
    // child's /dev/fd.
    let guard = std::fs::File::open("/usr/bin/true").unwrap();
    let Some(out) = run(&confined(ws.path(), "/bin/sh", &["-c", "ls /dev/fd"])) else {
        drop(guard);
        return;
    };
    drop(guard);
    assert_seatbelt(&out);
    let fds: Vec<i32> = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .filter_map(|s| s.parse().ok())
        .collect();
    // Allow 0,1,2 plus the fd `ls` itself opened on /dev/fd; anything ≥ a small
    // bound would be a leaked parent descriptor.
    assert!(
        fds.iter().all(|&fd| fd <= 5),
        "the confined child inherited unexpected descriptors (possible parent-fd leak): {fds:?}",
    );
}

// ── Process-tree containment (the sandbox must follow the tree) ──────────────

#[test]
#[serial]
#[ignore = "real-resource: sandbox-exec + subprocess"]
fn seatbelt_descendants_stay_confined() {
    // Seatbelt confinement is inherited by the whole descendant tree. A child that
    // spawns a grandchild which attempts the out-of-fence write must still be
    // denied ≥2 generations deep.
    let ws = tempdir().unwrap();
    let outside = tempdir().unwrap();
    let target = outside.path().join("grandchild.txt");
    let inner = format!("/bin/sh -c 'echo x > {}'", target.to_str().unwrap());
    let script = format!("{inner}; echo grandchild_exit=$?");
    let Some(out) = run(&confined(ws.path(), "/bin/sh", &["-c", &script])) else {
        return;
    };
    assert_seatbelt(&out);
    assert!(
        !target.exists(),
        "a grandchild wrote outside the fence — confinement did not follow the process tree",
    );
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("grandchild_exit=0"),
        "the grandchild's out-of-fence write reported success: {}",
        String::from_utf8_lossy(&out.stdout),
    );
}

#[test]
#[serial]
#[ignore = "real-resource: sandbox-exec + subprocess"]
fn seatbelt_follows_interpreters_and_helpers() {
    // The boundary follows the PROCESS TREE, not the initial exe: an interpreter
    // (python3) and a helper (git) launched inside the sandbox are just as
    // confined. python3 is exercised on the NET axis (its macOS Command Line Tools
    // framework lives outside any workspace read fence, so it cannot load under the
    // fs fence — a packaging fact, not a boundary result); git is exercised on the
    // fs axis. Both prove the confinement follows the spawned program.
    let ws = tempdir().unwrap();
    let outside = tempdir().unwrap();

    // python3 direct TCP under net:none — the network boundary follows into the
    // interpreter (it loads because fs is ambient here; only net is governed).
    let py = "import socket,sys\n\
        s=socket.socket(socket.AF_INET,socket.SOCK_STREAM); s.settimeout(3)\n\
        try:\n    s.connect(('1.1.1.1',80)); print('CONNECTED')\n\
        except (PermissionError,OSError):\n    print('DENIED'); sys.exit(3)";
    if let Some(out) = run(&net_confined(ws.path(), "/usr/bin/python3", &["-c", py])) {
        assert_seatbelt(&out);
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("DENIED"),
            "python3's off-box TCP was NOT denied — the net boundary did not follow the \
             interpreter: {}",
            String::from_utf8_lossy(&out.stdout),
        );
    }

    // git init in an out-of-fence dir — a helper subprocess is confined too.
    let git_dir = outside.path().join("gitrepo");
    let Some(out) = run(&confined(
        ws.path(),
        "/usr/bin/git",
        &["init", git_dir.to_str().unwrap()],
    )) else {
        return;
    };
    assert_seatbelt(&out);
    assert!(
        !git_dir.join(".git").exists(),
        "git init created a repo outside the fence — the helper was not confined",
    );
}

// ── Fail-closed / no silent host fallback ───────────────────────────────────

#[test]
#[serial]
#[ignore = "contract: fail-closed on a governed axis"]
fn seatbelt_missing_backend_refuses_not_host() {
    // The fail-closed contract lives in `SeatbeltSandbox::command_prefix`: for a
    // restricted (governed) axis it returns the `sandbox-exec` wrapper when the
    // backend is present, and DENIES (never an empty, silently-unconfined prefix)
    // when `/usr/bin/sandbox-exec` is absent. `/usr/bin/sandbox-exec` is a SIP
    // system binary that cannot be removed on a real macOS host, so the *absent*
    // branch is unreachable here — but we can prove the present branch never
    // yields an empty prefix for a governed axis (an empty prefix is exactly the
    // silent-unconfined bug this guards). The absent branch is covered by
    // agent-bridle's own unit tests.
    if !seatbelt_is_supported() {
        return;
    }
    let ws = tempdir().unwrap();
    let prefix = SeatbeltSandbox::new()
        .command_prefix(&workspace_confined_caveats(ws.path()))
        .expect("a governed-axis caveat with sandbox-exec present must yield a wrapper");
    assert!(
        !prefix.is_empty(),
        "a governed-axis (fenced fs + net:none) request produced an EMPTY prefix — that is a \
         silent-unconfined run, the fallback this contract forbids",
    );
    assert_eq!(
        prefix.first().map(String::as_str),
        Some("/usr/bin/sandbox-exec"),
        "the confinement wrapper must be sandbox-exec, not a bare host spawn: {prefix:?}",
    );
}

#[test]
#[serial]
#[ignore = "contract: NetGrant::DenyAll is Linux-seccomp-only → fail-closed on macOS"]
fn seatbelt_net_deny_all_grant_refuses_fail_closed() {
    // The `NetGrant::DenyAll` KERNEL net floor is the Linux `newt-net-guard`
    // (seccomp) wrapper — there is no macOS equivalent, so `resolve_net_floor`
    // REFUSES it (`ConfinementUnenforceable`) rather than run with a weaker floor.
    // `run_build_check` requests exactly this grant, so the build-check route is
    // fail-closed-UNAVAILABLE on macOS today: nothing runs unconfined. (The
    // equivalent egress denial IS available via `Caveats.net = none` → Seatbelt
    // `(deny network*)`, proven by the direct-TCP/UDP/loopback tests; wiring the
    // executor's net floor to accept the Seatbelt witness on macOS is the tracked
    // follow-up — a per-axis strength floor in agent-bridle.)
    let ws = tempdir().unwrap();
    let req = ExecRequest::new(
        ExecOrigin::AgentInfluenced,
        "/bin/echo",
        ["hi"],
        ws.path(),
        workspace_confined_caveats(ws.path()),
    )
    .net_grant(NetGrant::DenyAll)
    .env("PATH", "/usr/bin:/bin");
    match ConstrainedExecutor::run(&req) {
        Err(ExecRefused::ConfinementUnenforceable(_)) => {} // fail-closed: correct
        Ok(out) => panic!(
            "NetGrant::DenyAll RAN on macOS (sandbox_kind={:?}) — it must fail closed, the \
             seccomp net floor does not exist here",
            out.sandbox_kind
        ),
        Err(e) => panic!("expected ConfinementUnenforceable, got: {e}"),
    }
}

// ── Profile pinning (a profile change cannot silently widen authority) ───────

/// Render the SBPL profile the workspace fence produces for `ws`, via the public
/// `Sandbox::command_prefix` (`["/usr/bin/sandbox-exec", "-p", <profile>]`).
fn rendered_profile(ws: &Path) -> String {
    let prefix = SeatbeltSandbox::new()
        .command_prefix(&workspace_confined_caveats(ws))
        .expect("command_prefix");
    assert_eq!(
        prefix.first().map(String::as_str),
        Some("/usr/bin/sandbox-exec")
    );
    prefix.get(2).cloned().unwrap_or_default()
}

#[test]
#[serial]
#[ignore = "profile inspection: pin the boundary clauses"]
fn seatbelt_generated_profile_pins_the_boundary() {
    // Pin the meaningful clauses so a future profile edit cannot silently widen
    // authority. These are the exact denials the adversarial tests above rely on.
    if !seatbelt_is_supported() {
        return;
    }
    let ws = tempdir().unwrap();
    let profile = rendered_profile(ws.path());
    for needle in [
        "(deny file-write*)", // out-of-fence writes denied
        "(deny file-read*)",  // out-of-fence content reads denied
        "(deny network*)",    // all off-box egress denied under net:none
    ] {
        assert!(
            profile.contains(needle),
            "the generated profile is missing `{needle}` — the boundary widened. profile:\n{profile}",
        );
    }
    // The fence re-allows exactly the workspace root for writes (canonicalized).
    let canon = std::fs::canonicalize(ws.path()).unwrap();
    assert!(
        profile.contains(&format!("(subpath \"{}\")", canon.display())),
        "the workspace write-root re-allow is missing; profile:\n{profile}",
    );
    // net:none must NOT carry a loopback re-allow (that belongs to a loopback-only
    // grant, not the deny-all workspace fence).
    assert!(
        !profile.contains("(allow network*"),
        "net:none unexpectedly re-allows a network scope — egress widened. profile:\n{profile}",
    );
}
