//! Adversarial closure-proof: the seccomp egress floor deliberately ALLOWS
//! `AF_UNIX`. This asks whether that leaves an **ambient local-deputy path around
//! the direct-socket prohibition** — a confined child reaching a host
//! unix-domain socket that could relay network for it.
//!
//! It probes the two AF_UNIX address forms against a confined child on the
//! `ConstrainedExecutor` `NetGrant::DenyAll` route (build_check / crew share this
//! exact Landlock-fs + seccomp mechanism):
//!
//! * **pathname** unix socket OUTSIDE the workspace fence — governed by the
//!   Landlock fs fence (the socket file must be reachable to `connect`);
//! * **abstract-namespace** unix socket — has no filesystem path, so Landlock
//!   cannot govern it (the residual the netguard docs name).
//!
//! The result is load-bearing for the public claim: we may only say "hostile
//! code cannot exfiltrate over the network" as broadly as this test proves.
//!
//! Linux, `#[serial]`, gated on Landlock + python3. Where either is absent the
//! `AgentInfluenced` spawn fails closed and the test skips.
#![cfg(target_os = "linux")]

use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixListener};

use newt_core::confined_exec::{
    build_tool_caveats, ConfinedOutput, ConstrainedExecutor, ExecOrigin, ExecRefused, ExecRequest,
    NetGrant,
};
use serial_test::serial;
use tempfile::tempdir;

const GUARD_BIN: &str = env!("CARGO_BIN_EXE_newt-net-guard");

fn run(req: &ExecRequest) -> Option<ConfinedOutput> {
    match ConstrainedExecutor::run(req) {
        Ok(out) => Some(out),
        Err(ExecRefused::ConfinementUnenforceable(_)) => None, // no Landlock → skip
        Err(e) => panic!("unexpected confined-exec error: {e}"),
    }
}

fn have_python3() -> bool {
    std::path::Path::new("/usr/bin/python3").exists()
}

/// A confined `python3` child that tries to `connect()` an AF_UNIX socket to a
/// host deputy at BOTH a pathname (outside the fence) and an abstract name, and
/// prints `pathname=<result>` / `abstract=<result>`.
#[test]
#[serial]
fn af_unix_deputy_reachability_on_the_deny_all_route() {
    if !have_python3() {
        eprintln!("skipping: /usr/bin/python3 unavailable");
        return;
    }

    // Host deputies (UNCONFINED — they stand in for an ambient local relay):
    // (1) a pathname socket in a dir OUTSIDE the child's workspace fence;
    let outside = tempdir().unwrap();
    let sock_path = outside.path().join("deputy.sock");
    let _path_listener = UnixListener::bind(&sock_path).unwrap();
    // (2) an abstract-namespace socket (no filesystem path at all).
    let abstract_name = format!("newt-afunix-deputy-{}", std::process::id());
    let abstract_addr = SocketAddr::from_abstract_name(abstract_name.as_bytes()).unwrap();
    let _abs_listener = UnixListener::bind_addr(&abstract_addr).unwrap();

    // A CONTROL secret file in the same out-of-fence dir: reading it must be
    // DENIED, proving the Landlock fs fence genuinely covers this dir. If the
    // read is denied but the socket CONNECTS, Landlock does not govern
    // unix-socket connect() (the real finding, not a `/tmp`-in-fence artifact).
    let secret_path = outside.path().join("secret.txt");
    std::fs::write(&secret_path, b"TOP-SECRET").unwrap();

    // The confined child's own workspace (the fence root) — a DIFFERENT dir, so
    // the pathname deputy + secret are genuinely out of scope.
    let ws = tempdir().unwrap();

    // python: connect AF_UNIX to each address + read the control secret; report
    // CONNECTED / denied:<errno> / the read result.
    let script = format!(
        r#"python3 -c "
import socket
def probe(addr):
    try:
        s=socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.connect(addr); s.close(); return 'CONNECTED'
    except OSError as e:
        return 'denied:'+str(e.errno)
def readfile(p):
    try:
        open(p).read(); return 'READ'
    except OSError as e:
        return 'denied:'+str(e.errno)
print('control_read='+readfile('{secret}'))
print('pathname='+probe('{path}'))
print('abstract='+probe('\0{abs}'))
""#,
        secret = secret_path.to_string_lossy(),
        path = sock_path.to_string_lossy(),
        abs = abstract_name,
    );

    let req = ExecRequest::new(
        ExecOrigin::AgentInfluenced,
        "sh",
        ["-c", &script],
        ws.path(),
        build_tool_caveats(ws.path()),
    )
    .env("PATH", "/usr/bin:/bin")
    .net_grant(NetGrant::DenyAll)
    .net_guard_bin(GUARD_BIN);

    let Some(out) = run(&req) else {
        eprintln!("skipping: Landlock unavailable (confined spawn refused — fail-closed)");
        return;
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    eprintln!("--- af_unix child stdout ---\n{stdout}\n--- stderr ---\n{stderr}");

    // CONTROL: the fence must genuinely cover this out-of-scope dir. If the
    // secret read is NOT denied, the dir is in-fence and the socket results below
    // would be meaningless — so require the denial first (this is the same
    // fs-fence guarantee net_guard_executor.rs / confined_exec_landlock.rs prove).
    assert!(
        stdout.contains("control_read=denied:"),
        "CONTROL FAILED: the Landlock fs fence did not deny an out-of-scope file read, so this \
         dir is in-fence and the socket probe is inconclusive — fix the fixture: {stdout}"
    );

    // GROUND TRUTH: with the fence PROVEN active (secret read denied), what does
    // the kernel do for AF_UNIX connect? Record both; these fix exactly how broad
    // the public claim may be.
    let pathname_reachable = stdout.contains("pathname=CONNECTED");
    let abstract_reachable = stdout.contains("abstract=CONNECTED");
    eprintln!(
        "GROUND TRUTH — fence active (secret read denied). \
         pathname AF_UNIX deputy reachable: {pathname_reachable}; \
         abstract-namespace AF_UNIX deputy reachable: {abstract_reachable}. \
         Any `true` here => an AF_UNIX local-deputy egress path the seccomp floor does NOT close; \
         the claim narrows to 'direct AF_INET/AF_INET6/AF_PACKET socket creation denied' and the \
         residual is registered (closed only by the deferred netns / mediated-egress floor #1599)."
    );

    // Pin the empirical finding so a future kernel/fence change that silently
    // flips it trips CI and forces the register + public claim to be revisited.
    // (Current ground truth on the supported Linux backend: Landlock's AccessFs
    // rights do not include a unix-socket-connect right, so BOTH forms connect.)
    assert!(
        pathname_reachable && abstract_reachable,
        "ground-truth pin: expected BOTH pathname and abstract AF_UNIX deputies reachable under \
         the fence (Landlock does not govern unix-socket connect). If this changed, unix-socket \
         connect became fenced — WIDEN the guarantee + update the register + verify_network_\
         confinement evidence: {stdout}"
    );
}
