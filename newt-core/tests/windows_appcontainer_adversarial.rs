//! Windows AppContainer adversarial evidence for Newt's attacker-exec routes.
//!
//! These tests intentionally use real Windows resources: the installed
//! `agent-bridle-aclaunch.exe`, ACL-scoped temp directories, loopback sockets,
//! named pipes, and the production `ConstrainedExecutor`. They are ignored in
//! the ordinary test lane and run explicitly from the Windows evidence gate.
#![cfg(all(target_os = "windows", feature = "windows-appcontainer"))]

use std::ffi::OsStr;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use newt_core::confined_exec::{
    workspace_confined_caveats, ConfinedOutput, ConstrainedExecutor, ExecOrigin, ExecRefused,
    ExecRequest,
};

const SENTINEL: &str = "ORIG";
const WRITTEN: &str = "WRITTEN_BY_APPCONTAINER";
const SECRET_MARKER: &str = "NEWT_WINDOWS_APPCONTAINER_SECRET";
const UDP_MARKER: &str = "NEWT-UDP-DATAGRAM";
const PIPE_MARKER: &str = "NEWT-PIPE-DEPUTY";

static N: AtomicU64 = AtomicU64::new(0);

fn tag(kind: &str) -> String {
    format!(
        "newt-{kind}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

fn env_truthy(name: &str) -> bool {
    std::env::var(name)
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

fn find_on_path(exe: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(exe))
            .find(|p| p.is_file())
    })
}

fn launcher_path() -> Option<PathBuf> {
    std::env::var_os("NEWT_APPCONTAINER_LAUNCHER")
        .map(PathBuf::from)
        .filter(|p| p.is_file())
        .or_else(|| find_on_path("agent-bridle-aclaunch.exe"))
}

fn require_launcher() -> Option<PathBuf> {
    if let Some(path) = launcher_path() {
        return Some(path);
    }
    if env_truthy("BRIDLE_REQUIRE_APPCONTAINER") {
        panic!("agent-bridle-aclaunch.exe is required but was not found");
    }
    eprintln!("skipping: agent-bridle-aclaunch.exe is not installed or on PATH");
    None
}

fn launch(launcher: &Path, args: impl IntoIterator<Item = String>) -> Output {
    Command::new(launcher)
        .args(args)
        .current_dir("C:\\Windows")
        .output()
        .expect("spawn agent-bridle-aclaunch")
}

fn launch_strs(launcher: &Path, args: &[&str]) -> Output {
    launch(launcher, args.iter().map(|s| (*s).to_string()))
}

fn appcontainer_available(launcher: &Path) -> Result<(), String> {
    let out = launch_strs(
        launcher,
        &["--name", &tag("probe"), "cmd.exe", "/c", "exit 0"],
    );
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "status={:?} stderr={}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

fn require_appcontainer() -> Option<PathBuf> {
    let launcher = require_launcher()?;
    match appcontainer_available(&launcher) {
        Ok(()) => Some(launcher),
        Err(e) if env_truthy("BRIDLE_REQUIRE_APPCONTAINER") => {
            panic!("AppContainer launch is required but probe failed: {e}")
        }
        Err(e) => {
            eprintln!("skipping: AppContainer launch probe failed: {e}");
            None
        }
    }
}

fn fresh_dir(kind: &str) -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix(&format!("newt-ac-{kind}-"))
        .tempdir()
        .expect("create temp dir");
    lower_integrity(dir.path());
    dir
}

fn lower_integrity(path: &Path) {
    let _ = Command::new("icacls")
        .arg(path)
        .args(["/setintegritylevel", "(OI)(CI)Low"])
        .output();
}

fn path_s(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn cmd_quote(path: &Path) -> String {
    path_s(path)
}

fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn token_tree_probe_command() -> String {
    format!(
        "echo CHILD-BEGIN & whoami /groups & echo GRANDCHILD-BEGIN & powershell.exe -NoProfile -NonInteractive -EncodedCommand {}",
        powershell_encoded("whoami /groups")
    )
}

fn powershell_encoded(command: &str) -> String {
    let bytes: Vec<u8> = command.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn assert_two_generation_low_token(output: &str) {
    let combined = output.to_ascii_lowercase();
    let low_count = combined.matches("low mandatory level").count();
    assert!(
        combined.contains("child-begin")
            && combined.contains("grandchild-begin")
            && low_count >= 2,
        "cmd child and PowerShell grandchild must both report AppContainer low-integrity token evidence; output={combined}"
    );
}

fn admin_share_unc_path(path: &Path) -> Option<String> {
    let path = path_s(path);
    let mut chars = path.chars();
    let drive = chars.next()?;
    if chars.next()? != ':' || chars.next()? != '\\' {
        return None;
    }
    let rest: String = chars.collect();
    Some(format!(r"\\localhost\{drive}$\{rest}"))
}

fn write_cmd(path: &Path, marker: &str) -> String {
    format!("echo {marker}>{}", cmd_quote(path))
}

fn contains_file(path: &Path, needle: &str) -> bool {
    std::fs::read_to_string(path)
        .map(|s| s.contains(needle))
        .unwrap_or(false)
}

fn host_cmd(command: &str) -> Output {
    Command::new("cmd.exe")
        .args(["/c", command])
        .output()
        .expect("spawn host cmd")
}

fn host_powershell(script: &str) -> Output {
    Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()
        .expect("spawn host powershell")
}

fn require_netprobe() -> Option<PathBuf> {
    if let Some(path) = find_on_path("ab-netprobe.exe") {
        return Some(path);
    }
    if env_truthy("BRIDLE_REQUIRE_APPCONTAINER") {
        panic!("ab-netprobe.exe is required but was not found; install agent-bridle-aclaunch");
    }
    eprintln!("skipping: ab-netprobe.exe is not installed or on PATH");
    None
}

fn stage_netprobe() -> Option<(tempfile::TempDir, PathBuf)> {
    let source = require_netprobe()?;
    let dir = fresh_dir("netprobe");
    let dest = dir.path().join("ab-netprobe.exe");
    std::fs::copy(&source, &dest).expect("stage ab-netprobe.exe");
    Some((dir, dest))
}

/// How long a MUST-ARRIVE signal (a positive control's connection, payload, or
/// relay) may take end to end on a loaded CI runner. Success returns early, so
/// a generous budget costs wall clock only when the test is already failing;
/// a tight one turns runner load into flakes. Keep negative "must NOT arrive"
/// windows short and separate — widening those only slows every denial test.
const ARRIVAL_WAIT: Duration = Duration::from_secs(30);

/// The child-execution budget for MUST-SUCCEED control runs (`constrained_run`
/// / `constrained_cmd` calls whose output is then asserted on). The deadline
/// starts after spawn and absorbs launcher init + AppContainer profile
/// bring-up + a cold child start; at expiry the child tree is killed and the
/// run reports empty stdout, so a tight budget turns runner load straight into
/// a red assert — the same flake class the loopback listeners had before
/// [`LoopbackListener`] tied their accept loop to test lifetime. Success
/// returns early; only already-failing runs pay the wider ceiling.
/// Deliberately NOT used for the intentional-timeout run (500ms), which
/// exists to expire.
const CONTROL_BUDGET: Duration = Duration::from_secs(30);

/// Head-room over (measured startup + the child's own timeout budget) allowed
/// to the timeout-cleanup promptness assertion: kill + reap + pipe drain, plus
/// scheduler noise. Bounded well below the timed-out child's own runtime
/// (minutes), so the assertion still fails if the host ever blocks behind the
/// full child wait.
const PROMPTNESS_SLACK: Duration = Duration::from_secs(10);

/// A loopback listener whose accept loop lives exactly as long as the test
/// that owns it — the semantically correct accept window.
///
/// The old design armed a wall-clock deadline at listener CREATION, before
/// the AppContainer child was even launched. A cold `powershell.exe` start
/// inside a fresh AppContainer on a loaded hosted runner could outlast that
/// deadline while `launch()` was still blocked, so the accept thread exited
/// (dropping its sender and the bound port) and the deputy's later relay hit
/// a dead port — the `appcontainer_named_pipe_deputy` flake: same SHA red on
/// one run, green on its twin. No fixed pre-launch deadline can be correct,
/// because the launch delay it must cover is unbounded scheduler noise.
///
/// Instead the accept thread now exits when the owning test drops this
/// handle (observed as the `_live` channel disconnecting). Positive "did the
/// connection arrive" deadlines live only at the `rx.recv_timeout(...)` call
/// sites, whose clocks start at the semantically meaningful points; negative
/// must-NOT-arrive windows stay short and separate. The listener itself can
/// no longer lose a race with a slow child launch, and the accept thread
/// still cannot outlive the test (panic unwind included).
struct LoopbackListener {
    port: u16,
    rx: mpsc::Receiver<Vec<u8>>,
    /// Dropped when the owning test scope ends; the accept thread observes
    /// the disconnect and exits. This — not a wall clock — bounds the loop.
    _live: mpsc::Sender<()>,
}

fn tcp_listener() -> LoopbackListener {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    listener
        .set_nonblocking(true)
        .expect("set nonblocking listener");
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    let (live_tx, live_rx) = mpsc::channel::<()>();
    std::thread::spawn(move || loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = stream.write_all(b"ok");
                let mut buf = Vec::new();
                let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
                let _ = stream.read_to_end(&mut buf);
                let _ = tx.send(buf);
                return;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if matches!(live_rx.try_recv(), Err(mpsc::TryRecvError::Disconnected)) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => {
                return;
            }
        }
    });
    LoopbackListener {
        port,
        rx,
        _live: live_tx,
    }
}

/// Printed to stdout by a hostile child IMMEDIATELY BEFORE it attempts the
/// forbidden operation. Asserting on it separates "the process started and
/// reached the attempt" from "the operation was denied", so a failed
/// AppContainer launch, a shell startup failure, or a crashed child cannot
/// vacuously satisfy a denial assert that only checks for absent effects.
const ATTEMPT_MARKER: &str = "DENIAL-ATTEMPT-BEGIN";

/// A hostile write command that first proves the shell is alive and about to
/// attempt the write. `&` (not `&&`) so the write attempt runs regardless,
/// and the redirection binds only to the second `echo`.
fn attempted_write_cmd(path: &Path, marker: &str) -> String {
    format!("echo {ATTEMPT_MARKER}& {}", write_cmd(path, marker))
}

/// The credential-read probe, identical for the positive-grant and the denial
/// run so the only difference between them is the granted environment. The
/// leading [`ATTEMPT_MARKER`] proves the child started and reached the read —
/// without it, an empty stdout (killed at the budget, never launched, crashed
/// shell) satisfies "no credential appeared" while proving nothing.
fn env_probe_command() -> String {
    format!("echo {ATTEMPT_MARKER}& echo %OPENAI_API_KEY% & echo %OPENAI_BASE_URL%")
}

/// Both child streams as one string, for diagnostics and for the probes whose
/// evidence is a whole-output signature rather than a specific stream.
fn combined(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// Every denial-by-absent-effect assert must be preceded by this: the child
/// printed [`ATTEMPT_MARKER`], so the forbidden operation was genuinely
/// attempted and its absence is kernel policy, not a broken harness.
fn assert_attempt_reached(what: &str, out: &Output) {
    // Checked on stdout ONLY, not the combined streams: `agent-bridle-aclaunch`
    // hands the child its own inherited stdio handles, so the child's `echo`
    // lands in stdout. Accepting the marker from stderr as well would let a
    // launcher diagnostic that happens to quote the command line stand in for
    // the child having run — reintroducing the vacuity this guard closes.
    assert!(
        String::from_utf8_lossy(&out.stdout).contains(ATTEMPT_MARKER),
        "{what}: hostile child must start and reach its forbidden-operation attempt \
         (marker {ATTEMPT_MARKER} missing — a failed launch would otherwise pass \
         the denial assert vacuously); status={:?} output={:?}",
        out.status.code(),
        combined(out),
    );
}

/// Proof that `ab-netprobe` launched inside the container and completed a
/// DENIED connect attempt: on failure it prints `ab-netprobe: ... failed: ...`
/// (0.7.10 says `connect to`, newer versions say `tcp to` — assert on the
/// stable pieces). Without this, a probe that never launched is
/// indistinguishable from a denied one.
fn assert_netprobe_attempted(what: &str, out: &Output) {
    // `ab-netprobe` writes its own diagnostic to stderr; the launcher passes
    // the child's stderr handle straight through. Same reasoning as
    // [`assert_attempt_reached`] — assert on the stream that carries the
    // child's evidence, and print both streams when it is missing.
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(
        text.contains("ab-netprobe:") && text.contains("failed"),
        "{what}: ab-netprobe must launch and report a failed connect attempt — \
         otherwise 'denied' is indistinguishable from 'probe never ran'; \
         status={:?} output={:?}",
        out.status.code(),
        combined(out),
    );
}

fn host_ab_netprobe(port: u16) -> bool {
    let Some(probe) = require_netprobe() else {
        return false;
    };
    Command::new(probe)
        .args(["127.0.0.1", &port.to_string()])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn receive_udp_with_timeout(socket: &UdpSocket) -> Option<String> {
    let mut buf = [0u8; 256];
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set UDP timeout");
    socket
        .recv_from(&mut buf)
        .ok()
        .map(|(n, _)| String::from_utf8_lossy(&buf[..n]).into_owned())
}

fn powershell_udp_send(port: u16, marker: &str, marker_file: Option<&Path>) -> String {
    let write_marker = marker_file
        .map(|p| {
            format!(
                "Set-Content -LiteralPath {} -Value {};",
                ps_quote(&path_s(p)),
                ps_quote("POWERSHELL-RAN")
            )
        })
        .unwrap_or_default();
    format!(
        "{write_marker}$u=[Net.Sockets.UdpClient]::new();\
         $b=[Text.Encoding]::UTF8.GetBytes({});\
         [void]$u.Send($b,$b.Length,'127.0.0.1',{port});$u.Close()",
        ps_quote(marker)
    )
}

fn constrained_run(
    workspace: &Path,
    program: &str,
    args: Vec<String>,
    timeout: Duration,
    env: Vec<(&str, String)>,
) -> Result<ConfinedOutput, ExecRefused> {
    let mut req = ExecRequest::new(
        ExecOrigin::AgentInfluenced,
        program,
        args,
        workspace,
        workspace_confined_caveats(workspace),
    )
    .timeout(timeout);
    for (key, value) in std::env::vars() {
        if child_env_denylist(&key) {
            continue;
        }
        req = req.env(key, value);
    }
    req = req
        .env("HOME", path_s(workspace))
        .env("TMP", path_s(workspace))
        .env("TEMP", path_s(workspace));
    for (key, value) in env {
        req = req.env(key, value);
    }
    ConstrainedExecutor::run(&req)
}

fn child_env_denylist(key: &str) -> bool {
    const DENY: &[&str] = &[
        "OPENAI_API_KEY",
        "OPENAI_BASE_URL",
        "NEWT_AGENT_KEY",
        "NEWT_OPERATOR_KEY",
        "NEWT_TOKEN_PASSPHRASE",
        "TOKEN_PASSPHRASE",
        "NEWT_DISABLE_OCAP",
        "NEWT_FULL_ACCESS",
    ];
    DENY.iter().any(|denied| key.eq_ignore_ascii_case(denied))
}

fn constrained_cmd(
    workspace: &Path,
    command: &str,
    timeout: Duration,
) -> Result<ConfinedOutput, ExecRefused> {
    constrained_run(
        workspace,
        "cmd.exe",
        vec!["/c".to_string(), command.to_string()],
        timeout,
        Vec::new(),
    )
}

fn assert_appcontainer(out: &ConfinedOutput) {
    assert_eq!(
        out.sandbox_kind,
        agent_bridle::SandboxKind::AppContainer,
        "expected production route to use AppContainer, got {:?}; stdout={} stderr={}",
        out.sandbox_kind,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[derive(Debug)]
struct EnvGuard {
    key: &'static str,
    old: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let old = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, old }
    }

    fn unset(key: &'static str) -> Self {
        let old = std::env::var_os(key);
        std::env::remove_var(key);
        Self { key, old }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        if let Some(old) = self.old.take() {
            std::env::set_var(self.key, old);
        } else {
            std::env::remove_var(self.key);
        }
    }
}

// -- Filesystem authority ----------------------------------------------------

#[test]
#[ignore = "real Windows AppContainer proof"]
fn appcontainer_denies_profile_secret_read() {
    let Some(launcher) = require_appcontainer() else {
        return;
    };
    let profile = std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .expect("USERPROFILE set");
    let secret = profile.join(format!("{}.txt", tag("profile-secret")));
    std::fs::write(&secret, SECRET_MARKER).expect("write profile secret");

    assert!(
        std::fs::read_to_string(&secret)
            .unwrap_or_default()
            .contains(SECRET_MARKER),
        "host control must be able to read the secret"
    );

    let denied = launch(
        &launcher,
        [
            "--name".to_string(),
            tag("profile-secret"),
            "cmd.exe".to_string(),
            "/c".to_string(),
            // No embedded `"`: aclaunch quotes with MSVC/CommandLineToArgvW
            // rules (`"` -> `\"`), which cmd.exe's own parser does not honour.
            // Every command string in this file stays quote-free, like
            // `write_cmd`.
            format!("echo {ATTEMPT_MARKER}& type {}", cmd_quote(&secret)),
        ],
    );
    assert_attempt_reached("profile secret read", &denied);
    let stdout = String::from_utf8_lossy(&denied.stdout);
    let stderr = String::from_utf8_lossy(&denied.stderr);
    assert!(
        !stdout.contains(SECRET_MARKER) && (!denied.status.success() || stderr.contains("Access")),
        "profile secret must be denied by AppContainer policy; status={:?} stdout={stdout:?} stderr={stderr:?}",
        denied.status.code()
    );

    let _ = std::fs::remove_file(secret);
}

#[test]
#[ignore = "real Windows AppContainer proof"]
fn appcontainer_denies_outside_workspace_write() {
    let Some(launcher) = require_appcontainer() else {
        return;
    };
    let workspace = fresh_dir("workspace");
    let outside = fresh_dir("outside");
    let control = outside.path().join("control.txt");
    let target = outside.path().join("target.txt");
    std::fs::write(&control, SENTINEL).unwrap();
    std::fs::write(&target, SENTINEL).unwrap();

    let control_out = launch(
        &launcher,
        [
            "--name".to_string(),
            tag("outside-control"),
            "--fs-write".to_string(),
            path_s(outside.path()),
            "cmd.exe".to_string(),
            "/c".to_string(),
            write_cmd(&control, WRITTEN),
        ],
    );
    assert!(
        contains_file(&control, WRITTEN),
        "control must prove the write command works when the directory is granted; stderr={}",
        String::from_utf8_lossy(&control_out.stderr)
    );

    let denied = launch(
        &launcher,
        [
            "--name".to_string(),
            tag("outside-deny"),
            "--fs-write".to_string(),
            path_s(workspace.path()),
            "cmd.exe".to_string(),
            "/c".to_string(),
            attempted_write_cmd(&target, WRITTEN),
        ],
    );
    assert_attempt_reached("outside-workspace write", &denied);
    assert!(
        contains_file(&target, SENTINEL) && !contains_file(&target, WRITTEN),
        "outside write must be denied; status={:?} stdout={} stderr={}",
        denied.status.code(),
        String::from_utf8_lossy(&denied.stdout),
        String::from_utf8_lossy(&denied.stderr)
    );
}

#[test]
#[ignore = "real Windows AppContainer proof"]
fn appcontainer_denies_sibling_dir_write() {
    let Some(launcher) = require_appcontainer() else {
        return;
    };
    let parent = fresh_dir("siblings");
    let workspace = parent.path().join("workspace");
    let sibling = parent.path().join("sibling");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&sibling).unwrap();
    lower_integrity(&workspace);
    lower_integrity(&sibling);
    let target = sibling.join("target.txt");
    std::fs::write(&target, SENTINEL).unwrap();

    let denied = launch(
        &launcher,
        [
            "--name".to_string(),
            tag("sibling-deny"),
            "--fs-write".to_string(),
            path_s(&workspace),
            "cmd.exe".to_string(),
            "/c".to_string(),
            attempted_write_cmd(&target, WRITTEN),
        ],
    );
    assert_attempt_reached("sibling-dir write", &denied);
    assert!(
        contains_file(&target, SENTINEL) && !contains_file(&target, WRITTEN),
        "sibling write must be denied; status={:?} stdout={} stderr={}",
        denied.status.code(),
        String::from_utf8_lossy(&denied.stdout),
        String::from_utf8_lossy(&denied.stderr)
    );
}

#[test]
#[ignore = "real Windows AppContainer proof"]
fn appcontainer_denies_reparse_and_unc_escape() {
    let Some(launcher) = require_appcontainer() else {
        return;
    };
    let parent = fresh_dir("reparse");
    let workspace = parent.path().join("workspace");
    let sibling = parent.path().join("sibling");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&sibling).unwrap();
    lower_integrity(&workspace);
    lower_integrity(&sibling);

    let dotdot_target = sibling.join("dotdot.txt");
    let extended_target = sibling.join("extended.txt");
    let junction_target = sibling.join("junction.txt");
    let unc_target = sibling.join("unc.txt");
    std::fs::write(&dotdot_target, SENTINEL).unwrap();
    std::fs::write(&extended_target, SENTINEL).unwrap();
    std::fs::write(&junction_target, SENTINEL).unwrap();
    std::fs::write(&unc_target, SENTINEL).unwrap();

    let dotdot_path = workspace.join("..").join("sibling").join("dotdot.txt");
    let denied_dotdot = launch(
        &launcher,
        [
            "--name".to_string(),
            tag("dotdot"),
            "--fs-write".to_string(),
            path_s(&workspace),
            "cmd.exe".to_string(),
            "/c".to_string(),
            attempted_write_cmd(&dotdot_path, WRITTEN),
        ],
    );
    assert_attempt_reached("`..` escape write", &denied_dotdot);
    assert!(
        contains_file(&dotdot_target, SENTINEL) && !contains_file(&dotdot_target, WRITTEN),
        "`..` escape must be denied; status={:?} stderr={}",
        denied_dotdot.status.code(),
        String::from_utf8_lossy(&denied_dotdot.stderr)
    );

    let extended = format!(r"\\?\{}", path_s(&extended_target));
    let denied_extended = launch(
        &launcher,
        [
            "--name".to_string(),
            tag("extended-path"),
            "--fs-write".to_string(),
            path_s(&workspace),
            "cmd.exe".to_string(),
            "/c".to_string(),
            format!("echo {ATTEMPT_MARKER}& echo {WRITTEN}>{extended}"),
        ],
    );
    assert_attempt_reached("`\\\\?\\` alternate-spelling write", &denied_extended);
    assert!(
        contains_file(&extended_target, SENTINEL) && !contains_file(&extended_target, WRITTEN),
        "`\\\\?\\` alternate spelling must be denied; status={:?} stderr={}",
        denied_extended.status.code(),
        String::from_utf8_lossy(&denied_extended.stderr)
    );

    let junction = workspace.join("jump");
    let mklink = host_cmd(&format!(
        "mklink /J {} {}",
        cmd_quote(&junction),
        cmd_quote(&sibling)
    ));
    assert!(
        mklink.status.success(),
        "junction control failed; stdout={} stderr={}",
        String::from_utf8_lossy(&mklink.stdout),
        String::from_utf8_lossy(&mklink.stderr)
    );
    let through_junction = junction.join("junction.txt");
    let denied_junction = launch(
        &launcher,
        [
            "--name".to_string(),
            tag("junction"),
            "--fs-write".to_string(),
            path_s(&workspace),
            "cmd.exe".to_string(),
            "/c".to_string(),
            attempted_write_cmd(&through_junction, WRITTEN),
        ],
    );
    assert_attempt_reached("junction escape write", &denied_junction);
    assert!(
        contains_file(&junction_target, SENTINEL) && !contains_file(&junction_target, WRITTEN),
        "junction escape must be denied; status={:?} stderr={}",
        denied_junction.status.code(),
        String::from_utf8_lossy(&denied_junction.stderr)
    );

    let Some(unc) = admin_share_unc_path(&unc_target) else {
        if env_truthy("BRIDLE_REQUIRE_UNC_CONTROL") {
            panic!("UNC control requires a drive-qualified test path");
        }
        eprintln!("UNC positive control unavailable: test path is not drive-qualified");
        return;
    };
    let host_unc = host_powershell(&format!(
        "Set-Content -LiteralPath {} -Value {}",
        ps_quote(&unc),
        ps_quote("HOST_UNC_CONTROL")
    ));
    if !host_unc.status.success() {
        if env_truthy("BRIDLE_REQUIRE_UNC_CONTROL") {
            panic!(
                "UNC host positive control failed; stderr={}",
                String::from_utf8_lossy(&host_unc.stderr)
            );
        }
        eprintln!(
            "UNC positive control unavailable: stderr={}",
            String::from_utf8_lossy(&host_unc.stderr)
        );
        return;
    }
    assert!(
        contains_file(&unc_target, "HOST_UNC_CONTROL"),
        "host positive control must write through UNC path {unc}"
    );
    std::fs::write(&unc_target, SENTINEL).unwrap();
    let denied_unc = launch(
        &launcher,
        [
            "--name".to_string(),
            tag("unc"),
            "--fs-write".to_string(),
            path_s(&workspace),
            "powershell.exe".to_string(),
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-Command".to_string(),
            format!(
                "Write-Output {}; Set-Content -LiteralPath {} -Value {}",
                ps_quote(ATTEMPT_MARKER),
                ps_quote(&unc),
                ps_quote(WRITTEN)
            ),
        ],
    );
    assert_attempt_reached("UNC admin-share write", &denied_unc);
    assert!(
        contains_file(&unc_target, SENTINEL) && !contains_file(&unc_target, WRITTEN),
        "UNC admin-share escape must be denied by AppContainer policy, not by host unavailability; status={:?} stderr={}",
        denied_unc.status.code(),
        String::from_utf8_lossy(&denied_unc.stderr)
    );
}

// -- Environment / credential inheritance -----------------------------------

#[test]
#[serial_test::serial(windows_appcontainer_env)]
#[ignore = "real Windows AppContainer proof"]
fn appcontainer_child_does_not_inherit_provider_credentials() {
    if require_appcontainer().is_none() {
        return;
    }
    let _key = EnvGuard::set("OPENAI_API_KEY", "sk-newt-windows-secret");
    let _base = EnvGuard::set("OPENAI_BASE_URL", "https://secret.invalid");
    let workspace = fresh_dir("env");

    let positive = constrained_run(
        workspace.path(),
        "cmd.exe",
        vec!["/c".to_string(), env_probe_command()],
        CONTROL_BUDGET,
        vec![
            ("OPENAI_API_KEY", "explicit-grant".to_string()),
            ("OPENAI_BASE_URL", "https://explicit.invalid".to_string()),
        ],
    )
    .expect("positive env grant run");
    assert_appcontainer(&positive);
    assert!(
        String::from_utf8_lossy(&positive.stdout).contains("explicit-grant"),
        "positive control must prove the child can print explicitly granted env vars; stdout={} stderr={}",
        String::from_utf8_lossy(&positive.stdout),
        String::from_utf8_lossy(&positive.stderr)
    );

    let denied = constrained_run(
        workspace.path(),
        "cmd.exe",
        vec!["/c".to_string(), env_probe_command()],
        CONTROL_BUDGET,
        Vec::new(),
    )
    .expect("env denial run");
    assert_appcontainer(&denied);
    // The denial is only proven by a run that actually reached the read: a
    // child killed at the budget, or one that never started, would report
    // empty stdout and vacuously satisfy the no-leak assert below.
    assert!(
        denied.success,
        "env denial control must complete before its output can prove anything: {denied:?}"
    );
    assert!(
        String::from_utf8_lossy(&denied.stdout).contains(ATTEMPT_MARKER),
        "env denial control must prove the child reached the credential read: {denied:?}"
    );
    let stdout = String::from_utf8_lossy(&denied.stdout);
    assert!(
        !stdout.contains("sk-newt-windows-secret") && !stdout.contains("secret.invalid"),
        "parent/provider credentials must not leak into child env; stdout={stdout:?}"
    );
}

// -- Direct network ----------------------------------------------------------

#[test]
#[ignore = "real Windows AppContainer proof"]
fn appcontainer_denies_direct_tcp() {
    let Some(launcher) = require_appcontainer() else {
        return;
    };
    let Some((probe_dir, probe)) = stage_netprobe() else {
        return;
    };

    let host_listener = tcp_listener();
    assert!(
        host_ab_netprobe(host_listener.port),
        "host netprobe control must connect"
    );
    assert!(
        host_listener.rx.recv_timeout(ARRIVAL_WAIT).is_ok(),
        "host control listener must observe the connection"
    );

    let deny_listener = tcp_listener();
    let denied = launch(
        &launcher,
        [
            "--name".to_string(),
            tag("tcp-deny"),
            "--fs-read".to_string(),
            path_s(probe_dir.path()),
            path_s(&probe),
            "127.0.0.1".to_string(),
            deny_listener.port.to_string(),
        ],
    );
    assert_netprobe_attempted("direct TCP denial", &denied);
    assert!(
        !denied.status.success(),
        "AppContainer net:none must deny direct TCP; stdout={} stderr={}",
        String::from_utf8_lossy(&denied.stdout),
        String::from_utf8_lossy(&denied.stderr)
    );
    assert!(
        deny_listener
            .rx
            .recv_timeout(Duration::from_millis(500))
            .is_err(),
        "parent listener must not observe a denied AppContainer TCP connection"
    );
}

#[test]
#[ignore = "real Windows AppContainer proof"]
fn appcontainer_denies_direct_udp() {
    let Some(launcher) = require_appcontainer() else {
        return;
    };
    let socket = UdpSocket::bind("127.0.0.1:0").expect("bind host UDP");
    let port = socket.local_addr().unwrap().port();
    let host_script = powershell_udp_send(port, UDP_MARKER, None);
    let host = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &host_script])
        .output()
        .expect("host powershell UDP control");
    assert!(
        host.status.success(),
        "host UDP control must run; stderr={}",
        String::from_utf8_lossy(&host.stderr)
    );
    assert_eq!(
        receive_udp_with_timeout(&socket).as_deref(),
        Some(UDP_MARKER),
        "host UDP control must deliver the datagram"
    );

    let socket = UdpSocket::bind("127.0.0.1:0").expect("bind host UDP");
    let port = socket.local_addr().unwrap().port();
    let workspace = fresh_dir("udp-workspace");
    let ran_marker = workspace.path().join("powershell-ran.txt");
    let script = powershell_udp_send(port, UDP_MARKER, Some(&ran_marker));
    let denied = launch(
        &launcher,
        [
            "--name".to_string(),
            tag("udp-deny"),
            "--fs-write".to_string(),
            path_s(workspace.path()),
            "powershell.exe".to_string(),
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-Command".to_string(),
            script,
        ],
    );
    assert!(
        contains_file(&ran_marker, "POWERSHELL-RAN"),
        "PowerShell must have run so a missing UDP datagram is kernel policy, not a broken command; status={:?} stderr={}",
        denied.status.code(),
        String::from_utf8_lossy(&denied.stderr)
    );
    assert!(
        receive_udp_with_timeout(&socket).is_none(),
        "AppContainer net:none must not deliver a UDP datagram to loopback"
    );
}

#[test]
#[ignore = "real Windows AppContainer proof"]
fn appcontainer_loopback_behavior() {
    let Some(launcher) = require_appcontainer() else {
        return;
    };
    let Some((probe_dir, probe)) = stage_netprobe() else {
        return;
    };

    let deny_listener = tcp_listener();
    let denied = launch(
        &launcher,
        [
            "--name".to_string(),
            tag("loopback-deny"),
            "--fs-read".to_string(),
            path_s(probe_dir.path()),
            path_s(&probe),
            "127.0.0.1".to_string(),
            deny_listener.port.to_string(),
        ],
    );
    assert_netprobe_attempted("default-loopback denial", &denied);
    assert!(
        !denied.status.success(),
        "default AppContainer loopback must be denied without loopback exemption"
    );

    if !elevated() {
        if env_truthy("BRIDLE_REQUIRE_ELEVATED") {
            panic!("loopback exemption proof requires an elevated token");
        }
        eprintln!(
            "loopback exemption positive control classified UNSUPPORTED_FAIL_CLOSED: token is not elevated"
        );
        return;
    }
    let allow_listener = tcp_listener();
    let allowed = launch(
        &launcher,
        [
            "--name".to_string(),
            tag("loopback-allow"),
            "--loopback-exemption".to_string(),
            "--fs-read".to_string(),
            path_s(probe_dir.path()),
            path_s(&probe),
            "127.0.0.1".to_string(),
            allow_listener.port.to_string(),
        ],
    );
    assert!(
        allowed.status.success(),
        "loopback exemption must permit loopback; stderr={}",
        String::from_utf8_lossy(&allowed.stderr)
    );
    assert!(
        allow_listener.rx.recv_timeout(ARRIVAL_WAIT).is_ok(),
        "parent listener must observe loopback-exemption connection"
    );
}

fn elevated() -> bool {
    Command::new("net")
        .args(["session"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// -- Local-deputy egress -----------------------------------------------------

#[test]
#[ignore = "real Windows AppContainer proof"]
fn appcontainer_named_pipe_deputy() {
    let Some(launcher) = require_appcontainer() else {
        return;
    };

    let Some((probe_dir, probe)) = stage_netprobe() else {
        return;
    };
    let direct_listener = tcp_listener();
    let direct = launch(
        &launcher,
        [
            "--name".to_string(),
            tag("pipe-direct-deny"),
            "--fs-read".to_string(),
            path_s(probe_dir.path()),
            path_s(&probe),
            "127.0.0.1".to_string(),
            direct_listener.port.to_string(),
        ],
    );
    assert_netprobe_attempted("pipe-deputy direct-loopback control", &direct);
    assert!(
        !direct.status.success(),
        "direct loopback control must be denied before testing the pipe deputy"
    );
    assert!(
        direct_listener
            .rx
            .recv_timeout(Duration::from_millis(500))
            .is_err(),
        "parent listener must not observe the denied direct-loopback control connection"
    );

    let relay_listener = tcp_listener();
    let pipe_name = tag("pipe-deputy");
    let pipe_rx = spawn_named_pipe_deputy(&pipe_name, relay_listener.port);
    let script = format!(
        "$p=New-Object System.IO.Pipes.NamedPipeClientStream('.',{},[System.IO.Pipes.PipeDirection]::Out);\
         $p.Connect(2000);\
         $b=[Text.Encoding]::UTF8.GetBytes({});\
         $p.Write($b,0,$b.Length);$p.Flush();$p.Dispose()",
        ps_quote(&pipe_name),
        ps_quote(PIPE_MARKER)
    );
    let out = launch(
        &launcher,
        [
            "--name".to_string(),
            tag("pipe-client"),
            "powershell.exe".to_string(),
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-Command".to_string(),
            script,
        ],
    );
    assert!(
        out.status.success(),
        "pipe client should be able to reach an ALL APPLICATION PACKAGES pipe; status={:?} stderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let pipe_payload = pipe_rx
        .recv_timeout(ARRIVAL_WAIT)
        .expect("named pipe deputy should receive payload");
    assert!(
        pipe_payload.contains(PIPE_MARKER),
        "named pipe deputy payload mismatch: {pipe_payload:?}"
    );
    assert!(
        relay_listener.rx.recv_timeout(ARRIVAL_WAIT).is_ok(),
        "AppContainer child caused a host named-pipe deputy to relay over loopback"
    );
}

fn spawn_named_pipe_deputy(name: &str, relay_port: u16) -> mpsc::Receiver<String> {
    let pipe_path = format!(r"\\.\pipe\{name}");
    let handle = unsafe { create_permissive_pipe(&pipe_path) };
    let raw_handle = handle as isize;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || unsafe {
        use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_PIPE_CONNECTED};
        use windows_sys::Win32::Storage::FileSystem::ReadFile;
        use windows_sys::Win32::System::Pipes::ConnectNamedPipe;

        let handle = raw_handle as windows_sys::Win32::Foundation::HANDLE;
        let connected = ConnectNamedPipe(handle, std::ptr::null_mut()) != 0
            || GetLastError() == ERROR_PIPE_CONNECTED;
        if !connected {
            let _ = tx.send("CONNECT_FAILED".to_string());
            CloseHandle(handle);
            return;
        }
        let mut buf = [0u8; 256];
        let mut read = 0u32;
        let ok = ReadFile(
            handle,
            buf.as_mut_ptr(),
            buf.len() as u32,
            &mut read,
            std::ptr::null_mut(),
        ) != 0;
        let payload = if ok {
            String::from_utf8_lossy(&buf[..read as usize]).into_owned()
        } else {
            "READ_FAILED".to_string()
        };
        if payload.contains(PIPE_MARKER) {
            if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", relay_port)) {
                let _ = stream.write_all(b"relayed");
            }
        }
        let _ = tx.send(payload);
        CloseHandle(handle);
    });
    rx
}

unsafe fn create_permissive_pipe(path: &str) -> windows_sys::Win32::Foundation::HANDLE {
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
    use windows_sys::Win32::System::Pipes::{
        CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
    };

    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    let sddl = wide("D:(A;;GA;;;AC)(A;;GA;;;WD)");
    let ok = ConvertStringSecurityDescriptorToSecurityDescriptorW(
        sddl.as_ptr(),
        SDDL_REVISION_1,
        &mut sd,
        std::ptr::null_mut(),
    ) != 0;
    assert!(ok, "convert permissive pipe SDDL");

    let mut sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.cast(),
        bInheritHandle: 0,
    };
    let path_w = wide(path);
    let handle = CreateNamedPipeW(
        path_w.as_ptr(),
        PIPE_ACCESS_DUPLEX,
        PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
        1,
        512,
        512,
        2_000,
        &mut sa,
    );
    LocalFree(sd.cast());
    if handle == INVALID_HANDLE_VALUE {
        panic!(
            "CreateNamedPipeW failed: {}",
            std::io::Error::last_os_error()
        );
    }
    // Keep clippy happy that `CloseHandle` is in scope for the ownership model:
    // the handle is intentionally returned and later closed by the server thread.
    let _ = CloseHandle as unsafe extern "system" fn(_) -> _;
    handle
}

// -- Handle hygiene ----------------------------------------------------------

#[test]
#[ignore = "real Windows AppContainer proof"]
fn appcontainer_inheritable_handle_inheritance() {
    if require_appcontainer().is_none() {
        return;
    }
    let workspace = fresh_dir("handle");
    let marker = workspace.path().join("handle.txt");
    let handle = unsafe { InheritableFile::create(&marker) };
    unsafe { handle.write(b"PARENT-HANDLE-VALID\n") };

    let control = constrained_run(
        workspace.path(),
        "cmd.exe",
        vec!["/c".to_string(), "echo HANDLE-PROBE-RAN".to_string()],
        CONTROL_BUDGET,
        Vec::new(),
    )
    .expect("handle control run");
    assert_appcontainer(&control);
    assert!(
        control.success && String::from_utf8_lossy(&control.stdout).contains("HANDLE-PROBE-RAN"),
        "handle control must prove this fixture can execute an AppContainer child; output={control:?}"
    );

    let script = format!(
        "$ErrorActionPreference='SilentlyContinue';\
         Write-Output '{ATTEMPT_MARKER}';\
         $h=[IntPtr]::new({});\
         $sfh=New-Object Microsoft.Win32.SafeHandles.SafeFileHandle($h,$false);\
         $fs=New-Object System.IO.FileStream($sfh,[System.IO.FileAccess]::Write);\
         $b=[Text.Encoding]::UTF8.GetBytes('HANDLE-LEAK');\
         $fs.Write($b,0,$b.Length);$fs.Flush();$fs.Dispose()",
        handle.raw_value()
    );
    let out = constrained_run(
        workspace.path(),
        "powershell.exe",
        vec![
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-Command".to_string(),
            script,
        ],
        CONTROL_BUDGET,
        Vec::new(),
    )
    .expect("handle inheritance probe");
    assert_appcontainer(&out);
    let text = std::fs::read_to_string(&marker).unwrap_or_default();
    assert!(
        text.contains("PARENT-HANDLE-VALID"),
        "parent handle positive control must write the marker; file={text:?}"
    );

    // This fixture CLASSIFIES a residual; it asserts no denial. The
    // classification is only evidence if the probe actually ran — a PowerShell
    // that never started leaves the same absent HANDLE-LEAK as a kernel that
    // closed the handle, and reporting that as CLOSED_ON_THIS_RUNNER is a
    // fabricated result. [`ATTEMPT_MARKER`] separates the two.
    //
    // On GitHub-hosted `windows-latest` the probe currently does NOT run: the
    // PowerShell child of the `ConstrainedExecutor` route exits 1 with empty
    // stdout AND stderr (the `cmd.exe` control in this same fixture runs
    // fine), so every CI run to date has recorded CLOSED_ON_THIS_RUNNER
    // without evidence. Until that is diagnosed, CI reports the honest
    // PROBE_DID_NOT_RUN — the platform residual stays ACTIVE either way, so
    // nothing is claimed on the strength of a probe that never ran. A host
    // that expects the probe to work can make the gap fatal with
    // `BRIDLE_REQUIRE_HANDLE_PROBE=1`.
    let probe_ran = String::from_utf8_lossy(&out.stdout).contains(ATTEMPT_MARKER);
    if !probe_ran {
        if env_truthy("BRIDLE_REQUIRE_HANDLE_PROBE") {
            panic!(
                "inheritable HANDLE probe was required but never reached the handle write: {out:?}"
            );
        }
        eprintln!(
            "inheritable HANDLE result: PROBE_DID_NOT_RUN - the probe never reached the handle write, so NO conclusion is drawn about handle inheritance on this host; {out:?}"
        );
    } else if text.contains("HANDLE-LEAK") {
        eprintln!(
            "inheritable HANDLE result: ACTIVE - raw inheritable parent handle was usable by the AppContainer child"
        );
    } else {
        eprintln!(
            "inheritable HANDLE result: CLOSED_ON_THIS_RUNNER - the probe ran and could NOT use the raw inheritable parent handle; stdout={} stderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

struct InheritableFile {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

impl InheritableFile {
    unsafe fn create(path: &Path) -> Self {
        use windows_sys::Win32::Foundation::GENERIC_WRITE;
        use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };

        let mut sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: 1,
        };
        let path_w = wide_os(path.as_os_str());
        let handle = CreateFileW(
            path_w.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &mut sa,
            CREATE_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        );
        assert!(
            !handle.is_null() && handle != windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE,
            "CreateFileW inheritable handle failed: {}",
            std::io::Error::last_os_error()
        );
        Self { handle }
    }

    fn raw_value(&self) -> isize {
        self.handle as isize
    }

    unsafe fn write(&self, bytes: &[u8]) {
        use windows_sys::Win32::Storage::FileSystem::WriteFile;
        let mut written = 0u32;
        let ok = WriteFile(
            self.handle,
            bytes.as_ptr(),
            bytes.len() as u32,
            &mut written,
            std::ptr::null_mut(),
        ) != 0;
        assert!(
            ok && written as usize == bytes.len(),
            "parent WriteFile failed"
        );
    }
}

impl Drop for InheritableFile {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

// -- Process-tree containment ------------------------------------------------

#[test]
#[ignore = "real Windows AppContainer proof"]
fn appcontainer_descendants_stay_in_the_same_token() {
    let Some(launcher) = require_appcontainer() else {
        return;
    };
    let out = launch(
        &launcher,
        [
            "--name".to_string(),
            tag("token-tree"),
            "cmd.exe".to_string(),
            "/c".to_string(),
            token_tree_probe_command(),
        ],
    );
    assert_two_generation_low_token(&combined(&out));
}

#[test]
#[ignore = "real Windows AppContainer proof"]
fn appcontainer_follows_shells_and_helpers() {
    let Some(launcher) = require_appcontainer() else {
        return;
    };
    let Some((probe_dir, probe)) = stage_netprobe() else {
        return;
    };
    let out = launch(
        &launcher,
        [
            "--name".to_string(),
            tag("helpers-shells"),
            "cmd.exe".to_string(),
            "/c".to_string(),
            token_tree_probe_command(),
        ],
    );
    assert_two_generation_low_token(&combined(&out));

    let helper = launch(
        &launcher,
        [
            "--name".to_string(),
            tag("helpers-workspace-exe"),
            "--fs-read".to_string(),
            path_s(probe_dir.path()),
            path_s(&probe),
        ],
    );
    let helper_output = combined(&helper).to_ascii_lowercase();
    assert!(
        helper_output.contains("usage: ab-netprobe"),
        "staged workspace .exe helper must launch inside the AppContainer; output={helper_output}"
    );

    let Some(git) = find_on_path("git.exe") else {
        eprintln!("skipping git helper probe: git.exe not found on PATH");
        return;
    };
    let mut args = vec!["--name".to_string(), tag("helpers-git")];
    if let Some(parent) = git.parent() {
        args.extend(["--fs-read".to_string(), path_s(parent)]);
    }
    args.extend([path_s(&git), "--version".to_string()]);
    let git_out = launch(&launcher, args);
    let git_output = combined(&git_out).to_ascii_lowercase();
    if !git_output.contains("git version") {
        eprintln!("git helper probe did not run under this AppContainer DACL shape: {git_output}");
    }
}

// -- Timeout/cancellation ----------------------------------------------------

#[test]
#[ignore = "real Windows AppContainer proof"]
fn appcontainer_timeout_cleanup_is_distinct_from_authority() {
    if require_appcontainer().is_none() {
        return;
    }
    let workspace = fresh_dir("timeout");

    // The quick control doubles as the startup calibration for the promptness
    // assertion below: its wall time is (AppContainer bring-up + a trivial
    // child + teardown) measured on THIS machine at THIS moment, which is
    // exactly the term that must not be charged against the timeout budget.
    let quick_started = Instant::now();
    let quick = constrained_run(
        workspace.path(),
        "cmd.exe",
        vec![
            "/d".to_string(),
            "/c".to_string(),
            "echo QUICK-RAN".to_string(),
        ],
        CONTROL_BUDGET,
        Vec::new(),
    )
    .expect("quick timeout control");
    let startup_reference = quick_started.elapsed();
    assert_appcontainer(&quick);
    assert!(quick.success, "quick control should complete: {quick:?}");
    assert!(
        String::from_utf8_lossy(&quick.stdout).contains("QUICK-RAN"),
        "quick control must prove stdout capture from an AppContainer child: {quick:?}"
    );

    let started = Instant::now();
    let slow = constrained_run(
        workspace.path(),
        "cmd.exe",
        vec![
            "/d".to_string(),
            "/c".to_string(),
            "for /L %I in (1,1,100000000) do @rem.".to_string(),
        ],
        Duration::from_millis(500),
        Vec::new(),
    )
    .expect("slow timeout run");
    assert_appcontainer(&slow);
    assert!(
        slow.timed_out,
        "slow child must be reported timed out: {slow:?}"
    );
    // Promptness proof, anchored at the right semantic point. The claim is
    // "the host tore the child down at its 500ms budget instead of blocking
    // behind the full child wait" — and the child's spin loop runs on the
    // order of minutes. A fixed wall-clock cap charges AppContainer bring-up
    // (unbounded scheduler noise on a hosted runner) against that budget,
    // which is what turns runner load into a red assert. Measuring against
    // the startup cost observed moments earlier removes exactly that term
    // while keeping the cap orders of magnitude below "blocked on the child".
    let elapsed = started.elapsed();
    let bound = startup_reference + Duration::from_millis(500) + PROMPTNESS_SLACK;
    assert!(
        elapsed < bound,
        "timeout cleanup must not block behind the Windows child wait; \
         elapsed={elapsed:?} bound={bound:?} (startup reference {startup_reference:?} \
         + 500ms budget + {PROMPTNESS_SLACK:?} slack)"
    );
    assert!(
        slow.stdout.is_empty(),
        "timed-out CPU-only child must not produce output: {slow:?}"
    );
}

// -- Fail-closed / no silent host fallback ----------------------------------

#[test]
#[serial_test::serial(windows_appcontainer_env)]
#[ignore = "real Windows AppContainer proof"]
fn appcontainer_missing_backend_refuses_not_host() {
    let current_exe = std::env::current_exe().expect("current exe");
    let sibling_launcher = current_exe
        .parent()
        .map(|p| p.join("agent-bridle-aclaunch.exe"))
        .filter(|p| p.exists());
    if sibling_launcher.is_some() {
        eprintln!(
            "skipping missing-backend probe: launcher exists next to current test exe and cannot be hidden safely"
        );
        return;
    }

    let workspace = fresh_dir("missing-backend");
    let marker = workspace.path().join("should-not-run.txt");
    let empty_path = fresh_dir("empty-path");
    let _path = EnvGuard::set("PATH", &path_s(empty_path.path()));
    let _launcher_override = EnvGuard::unset("NEWT_APPCONTAINER_LAUNCHER");

    let result = constrained_cmd(
        workspace.path(),
        &write_cmd(&marker, "HOST-FALLBACK-RAN"),
        Duration::from_secs(3),
    );
    assert!(
        matches!(
            result,
            Err(ExecRefused::ConfinementUnenforceable(_)) | Err(ExecRefused::Authorize(_))
        ),
        "missing AppContainer backend must refuse before hostile code runs; got {result:?}"
    );
    assert!(
        !contains_file(&marker, "HOST-FALLBACK-RAN"),
        "missing backend must not fall back to host cmd.exe"
    );
}

fn wide(s: &str) -> Vec<u16> {
    wide_os(OsStr::new(s))
}

fn wide_os(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}
