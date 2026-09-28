//! Real-spawn reality-check for the carried **brush engine** (agent-bridle#20).
//!
//! Proves the engine's whole thesis: it runs a **dynamic construct the
//! safe-subset engine refuses** (`$(...)`) inside its dedicated worker, AND —
//! unlike the sandbox-host engine, which refuses a restricted `exec` grant — it
//! **confines** a restricted `exec` grant there via the static command and file-open filters,
//! denying an out-of-scope command (structured `denied:true`) while it never
//! runs.
//!
//! This is a harnessless, same-image test executable. Its `main` handles private
//! dispatch before running cases, so production builds do not ship a public
//! full-authority helper merely to make re-exec integration tests possible.
#![cfg(feature = "brush")]
#![cfg_attr(
    not(any(target_os = "linux", target_os = "macos")),
    allow(dead_code, unused_imports)
)]

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use agent_bridle_core::{Caveats, Gate, Scope, Tool, ToolContext};
use agent_bridle_tool_shell::{
    BrushShellTool, ShellInvocationId, ShellOutputObserver, ShellOutputStream,
};

fn tool() -> BrushShellTool {
    BrushShellTool::new()
}

fn selected(name: &str) -> bool {
    let filters: Vec<String> = std::env::args()
        .skip(1)
        .filter(|arg| !arg.starts_with('-'))
        .collect();
    filters.is_empty() || filters.iter().any(|filter| name.contains(filter))
}

/// Decode the framed brush-worker terminal response from raw worker stdout.
///
/// The worker emits an 18-byte header (`ABES` magic, version, frame kind,
/// little-endian sequence, little-endian payload length) followed by the JSON
/// payload. Both authentication refusals exercised here are a single TERMINAL
/// frame (kind 5) at sequence 0 carrying the structured `WorkerResponse`.
fn decode_worker_terminal(stdout: &[u8]) -> serde_json::Value {
    const HEADER_LEN: usize = 18;
    assert!(
        stdout.len() >= HEADER_LEN,
        "worker stdout must hold at least one frame header: {stdout:?}"
    );
    assert_eq!(&stdout[..4], b"ABES", "frame magic: {stdout:?}");
    assert_eq!(
        stdout[5], 5,
        "a refusal is a single TERMINAL frame: {stdout:?}"
    );
    let len = u32::from_le_bytes(stdout[14..18].try_into().expect("length field")) as usize;
    let payload = &stdout[HEADER_LEN..HEADER_LEN + len];
    serde_json::from_slice(payload).expect("structured worker terminal response")
}

fn run_case(name: &str, case: impl FnOnce()) {
    if selected(name) {
        eprintln!("test {name} ...");
        case();
        eprintln!("test {name} ... ok");
    }
}

fn run_async_case<F>(runtime: &tokio::runtime::Runtime, name: &str, case: impl FnOnce() -> F)
where
    F: Future<Output = ()>,
{
    if selected(name) {
        eprintln!("test {name} ...");
        runtime.block_on(case());
        eprintln!("test {name} ... ok");
    }
}

fn main() {
    // This must be the first branch: worker mode must never initialize the
    // test runner or any ambient-authority fixture.
    if let Some(code) = agent_bridle_tool_shell::maybe_dispatch() {
        std::process::exit(code);
    }
    run_platform();
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_platform() {
    let runtime = tokio::runtime::Runtime::new().expect("test runtime");
    #[cfg(not(feature = "carried-coreutils"))]
    run_async_case(
        &runtime,
        "brush_worker_dispatch_does_not_require_carried_coreutils",
        brush_worker_dispatch_does_not_require_carried_coreutils,
    );
    run_case(
        "direct_worker_dispatch_cannot_mint_its_own_authority",
        direct_worker_dispatch_cannot_mint_its_own_authority,
    );
    #[cfg(unix)]
    run_case(
        "different_image_socket_parent_cannot_answer_worker_challenge",
        different_image_socket_parent_cannot_answer_worker_challenge,
    );
    run_async_case(
        &runtime,
        "output_observer_matches_the_brush_envelope",
        output_observer_matches_the_brush_envelope,
    );
    run_async_case(
        &runtime,
        "real_worker_delivers_line_one_before_the_command_can_exit",
        real_worker_delivers_line_one_before_the_command_can_exit,
    );
    #[cfg(unix)]
    run_async_case(
        &runtime,
        "stderr_observer_and_brush_envelope_apply_the_output_cap",
        stderr_observer_and_brush_envelope_apply_the_output_cap,
    );
    run_async_case(
        &runtime,
        "full_access_runs_dynamic_construct_and_captures",
        full_access_runs_dynamic_construct_and_captures,
    );
    #[cfg(unix)]
    run_async_case(
        &runtime,
        "xtrace_ps4_cannot_hide_an_uninspected_command",
        xtrace_ps4_cannot_hide_an_uninspected_command,
    );
    run_async_case(
        &runtime,
        "restricted_exec_denies_out_of_scope_command_in_worker",
        restricted_exec_denies_out_of_scope_command_in_worker,
    );
    // The Brush production path installs the DenyDirect seccomp egress floor via
    // core's ConfinedCommand `Sandbox::apply` (same seam the safe-subset engine
    // uses). Only meaningful with the Landlock backend + a python3 probe.
    #[cfg(all(target_os = "linux", feature = "linux-landlock"))]
    run_async_case(
        &runtime,
        "brush_deny_direct_denies_a_childs_socket",
        brush_deny_direct_denies_a_childs_socket,
    );
    run_async_case(
        &runtime,
        "command_substitution_keeps_inner_exec_independently_gated",
        command_substitution_keeps_inner_exec_independently_gated,
    );
    run_async_case(
        &runtime,
        "restricted_exec_allows_in_scope_command",
        restricted_exec_allows_in_scope_command,
    );
    #[cfg(not(any(
        feature = "linux-landlock",
        feature = "macos-seatbelt",
        feature = "windows-appcontainer"
    )))]
    run_async_case(
        &runtime,
        "restricted_filesystem_without_l3_refuses_before_execution",
        restricted_filesystem_without_l3_refuses_before_execution,
    );
    run_async_case(
        &runtime,
        "env_seam_delivers_caller_vars_to_the_shell",
        env_seam_delivers_caller_vars_to_the_shell,
    );
    #[cfg(unix)]
    run_async_case(
        &runtime,
        "confined_stdin_reader_gets_eof_not_the_operator_terminal",
        confined_stdin_reader_gets_eof_not_the_operator_terminal,
    );
    #[cfg(unix)]
    run_async_case(
        &runtime,
        "confined_run_is_bounded_by_the_wall_clock_ceiling",
        confined_run_is_bounded_by_the_wall_clock_ceiling,
    );
    #[cfg(unix)]
    run_async_case(
        &runtime,
        "timeout_kills_worker_descendants",
        timeout_kills_worker_descendants,
    );
    #[cfg(unix)]
    run_async_case(
        &runtime,
        "a_background_descendant_cannot_hang_or_falsify_normal_completion",
        a_background_descendant_cannot_hang_or_falsify_normal_completion,
    );
    run_async_case(
        &runtime,
        "env_seam_delivers_home_for_tilde_class_tooling",
        env_seam_delivers_home_for_tilde_class_tooling,
    );
    run_async_case(
        &runtime,
        "env_seam_vars_reach_a_real_spawned_childs_environment",
        env_seam_vars_reach_a_real_spawned_childs_environment,
    );
    run_async_case(
        &runtime,
        "a_child_gets_the_seeded_path_and_the_seam_but_no_ambient_variable",
        a_child_gets_the_seeded_path_and_the_seam_but_no_ambient_variable,
    );
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn run_platform() {
    let runtime = tokio::runtime::Runtime::new().expect("test runtime");
    let tool = tool();
    let result = runtime.block_on(tool.invoke(
        serde_json::json!({ "cmd": "echo MUST-NOT-RUN" }),
        &ctx(Caveats::top()),
    ));
    let error = result.expect_err("unsupported private control must fail closed");
    assert!(
        error.to_string().contains("private-control transport"),
        "unsupported target must explain the fail-closed boundary: {error}"
    );
}

#[cfg(unix)]
const OUTPUT_CAP: usize = 64 * 1024;

#[derive(Default)]
struct OutputRecorder {
    chunks: Mutex<Vec<(ShellOutputStream, Vec<u8>)>>,
    finished: Mutex<bool>,
    finished_cv: Condvar,
}

impl ShellOutputObserver for OutputRecorder {
    fn on_output(&self, _invocation: ShellInvocationId, stream: ShellOutputStream, chunk: &[u8]) {
        self.chunks
            .lock()
            .expect("output recorder lock")
            .push((stream, chunk.to_vec()));
        self.finished_cv.notify_all();
    }

    fn on_finish(&self, _invocation: ShellInvocationId) {
        *self.finished.lock().expect("finished lock") = true;
        self.finished_cv.notify_all();
    }
}

impl OutputRecorder {
    fn bytes(&self, stream: ShellOutputStream) -> Vec<u8> {
        self.chunks
            .lock()
            .expect("output recorder lock")
            .iter()
            .filter(|(seen, _)| *seen == stream)
            .flat_map(|(_, chunk)| chunk.iter().copied())
            .collect()
    }

    /// Bound the wait for the observer's asynchronous finish notification.
    ///
    /// This is a LIVENESS bound, not a latency assertion — the test asserts
    /// that the observer finishes, never that it finishes quickly. It is
    /// deliberately generous: output now reaches the observer as a stream of
    /// per-chunk frames rather than one terminal blob, so a run makes many more
    /// hops than it used to, and under `cargo llvm-cov` instrumentation on a
    /// loaded machine the old two-second deadline expired while the run was
    /// still healthy (agent-bridle#360: wall-clock failures under build
    /// saturation are artifacts, not defects). A real hang still fails here,
    /// just after a bound that load cannot cross.
    fn wait_finished(&self) {
        const OBSERVER_FINISH_BOUND: Duration = Duration::from_secs(60);
        let finished = self.finished.lock().expect("finished lock");
        let (finished, _) = self
            .finished_cv
            .wait_timeout_while(finished, OBSERVER_FINISH_BOUND, |finished| !*finished)
            .expect("finished condition variable");
        assert!(
            *finished,
            "timed out waiting for observer finish after {OBSERVER_FINISH_BOUND:?}"
        );
    }

    fn wait_for_bytes(&self, stream: ShellOutputStream, expected: &[u8], timeout: Duration) {
        let chunks = self.chunks.lock().expect("output recorder lock");
        let (chunks, _) = self
            .finished_cv
            .wait_timeout_while(chunks, timeout, |chunks| {
                !chunks
                    .iter()
                    .filter(|(seen, _)| *seen == stream)
                    .flat_map(|(_, chunk)| chunk.iter().copied())
                    .collect::<Vec<_>>()
                    .windows(expected.len())
                    .any(|window| window == expected)
            })
            .expect("output condition variable");
        let seen = chunks
            .iter()
            .filter(|(seen, _)| *seen == stream)
            .flat_map(|(_, chunk)| chunk.iter().copied())
            .collect::<Vec<_>>();
        assert!(
            seen.windows(expected.len())
                .any(|window| window == expected),
            "timed out waiting for live bytes {expected:?}; saw {seen:?}"
        );
    }
}

/// Mint a [`ToolContext`] carrying `granted` — the public-API path an embedder
/// uses (mirrors `host_shell_real.rs`).
fn ctx(granted: Caveats) -> ToolContext {
    Gate::new(0)
        .authorize(&tool(), &granted)
        .expect("authorize")
}

fn unique_temp(tag: &str) -> std::path::PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "ab-brush-{}-{}-{}",
        tag,
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ))
}

/// The private worker protocol belongs to `brush`, not `carried-coreutils`.
/// This test is compiled and run specifically in the lean Brush-only feature
/// lane, proving the dispatch-capable helper can serve the worker without any
/// bundled uutils provider.
#[cfg(not(feature = "carried-coreutils"))]
async fn brush_worker_dispatch_does_not_require_carried_coreutils() {
    let out = tool()
        .invoke(
            serde_json::json!({ "cmd": "echo brush-only-worker-ok" }),
            &ctx(Caveats::top()),
        )
        .await
        .expect("invoke Brush-only worker");

    assert_ne!(out["denied"], true, "Brush-only worker must run: {out}");
    assert_eq!(out["exit_code"], 0, "Brush-only worker must succeed: {out}");
    assert_eq!(
        out["stdout"].as_str().unwrap_or("").trim(),
        "brush-only-worker-ok"
    );
}

/// A caller that selects the hidden worker argv, its environment nonce, and the
/// complete JSON request still lacks the live inherited control capability. The
/// worker must reject before parsing or executing the supplied command.
fn direct_worker_dispatch_cannot_mint_its_own_authority() {
    use std::io::{BufRead as _, Read as _, Write as _};
    use std::process::{Command, Stdio};

    let nonce = "caller-selected-nonce";
    let forged_payload = serde_json::json!({
        "cmd": "echo FORGED-WORKER-RAN",
        "cwd": null,
        "path": "",
        "env": {},
        "max_output": 4096,
    });
    let mut child = Command::new(std::env::current_exe().expect("current test executable"))
        .args(["--agent-bridle-worker", "brush"])
        .env_clear()
        .env("AGENT_BRIDLE_WORKER_NONCE", nonce)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn direct worker");

    // Be a genuinely interactive attacker: wait for and inspect the worker's
    // advertised challenge, then answer it and send a complete model-chosen
    // payload. Challenge knowledge is deliberately insufficient without the
    // parent-created kernel capability.
    let mut stderr = std::io::BufReader::new(child.stderr.take().expect("worker stderr"));
    let mut challenge_line = String::new();
    stderr
        .read_line(&mut challenge_line)
        .expect("read advertised worker challenge");
    let challenge = challenge_line
        .strip_prefix("agent-bridle-private-control-v1 challenge ")
        .and_then(|line| line.strip_suffix('\n'))
        .expect("well-formed advertised challenge");
    let mut stdin = child.stdin.take().expect("worker stdin");
    let _ = writeln!(
        stdin,
        "agent-bridle-private-control-v1 response {challenge}"
    );
    let _ = serde_json::to_writer(&mut stdin, &forged_payload);
    let _ = stdin.flush();
    drop(stdin);

    let status = child.wait().expect("wait direct worker");
    let mut stdout_bytes = Vec::new();
    child
        .stdout
        .take()
        .expect("worker stdout")
        .read_to_end(&mut stdout_bytes)
        .expect("read worker stdout");
    let stdout = String::from_utf8_lossy(&stdout_bytes);
    assert_eq!(
        status.code(),
        Some(126),
        "authentication refusal must be a process-level cannot-execute"
    );
    assert!(
        !stdout.contains("FORGED-WORKER-RAN"),
        "the forged request must not execute: {stdout:?}"
    );
    let response = decode_worker_terminal(&stdout_bytes);
    assert!(
        response["error"]
            .as_str()
            .is_some_and(|error| error.contains("trusted worker authentication failed")),
        "worker must refuse the missing inherited capability: {response}"
    );
}

/// Even a launcher that supplies a real socketpair, reads the child-selected
/// challenge, and sends a correctly-shaped response is not the trusted parent.
/// The kernel peer PID may match PPID, but its executable image is Python rather
/// than this same-image host, so authentication must fail before payload decode.
#[cfg(unix)]
fn different_image_socket_parent_cannot_answer_worker_challenge() {
    use std::process::Command;

    let python = r#"
import os, socket, struct, subprocess, sys
parent, child = socket.socketpair()
env = {"AGENT_BRIDLE_WORKER_NONCE": "different-image-attacker"}
p = subprocess.Popen(
    [sys.argv[1], "--agent-bridle-worker", "brush"],
    stdin=child.fileno(),
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    pass_fds=(child.fileno(),),
    env=env,
)
child.close()
challenge_line = p.stderr.readline()
parent.sendall(b"ABTW-B1\x00")
hello = b""
while len(hello) < 44:
    chunk = parent.recv(44 - len(hello))
    if not chunk:
        break
    hello += chunk
if len(hello) == 44:
    parent.sendall(b"ABTW-R1\x00" + struct.pack("<I", 0) + hello[12:44] + bytes(32))
parent.shutdown(socket.SHUT_WR)
stdout, stderr = p.communicate(timeout=10)
sys.stdout.buffer.write(stdout)
sys.stderr.buffer.write(challenge_line + stderr)
sys.exit(p.returncode)
"#;
    let out = match Command::new("python3")
        .arg("-c")
        .arg(python)
        .arg(std::env::current_exe().expect("current test executable"))
        .output()
    {
        Ok(out) => out,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("skipping different-image adversary: python3 is unavailable");
            return;
        }
        Err(error) => panic!("launch different-image adversary: {error}"),
    };

    assert_eq!(
        out.status.code(),
        Some(126),
        "different-image private parent must be refused: stderr={:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    let response = decode_worker_terminal(&out.stdout);
    assert!(
        response["error"]
            .as_str()
            .is_some_and(|error| error.contains("parent image is not this executable")),
        "refusal must identify the image mismatch: {response}"
    );
}

async fn output_observer_matches_the_brush_envelope() {
    let observer = Arc::new(OutputRecorder::default());
    let out = tool()
        .with_output_observer(observer.clone())
        .invoke(
            serde_json::json!({ "cmd": "printf brush-out; printf brush-err >&2" }),
            &ctx(Caveats::top()),
        )
        .await
        .expect("invoke");

    observer.wait_finished();
    assert_eq!(observer.bytes(ShellOutputStream::Stdout), b"brush-out");
    assert_eq!(observer.bytes(ShellOutputStream::Stderr), b"brush-err");
    assert_eq!(out["stdout"], "brush-out");
    assert_eq!(out["stderr"], "brush-err");
}

/// Acceptance proof for the private worker transport: line 1 must cross the
/// worker boundary while the command is still blocked on a gate that only this
/// test can open. Observing both chunks after completion is insufficient.
async fn real_worker_delivers_line_one_before_the_command_can_exit() {
    let gate = unique_temp("live-stream-gate");
    let _ = std::fs::remove_file(&gate);
    let observer = Arc::new(OutputRecorder::default());
    let command = format!(
        "printf 'line 1\\n'; while [ ! -f '{}' ]; do sleep 0.05; done; printf 'line 2\\n'",
        gate.display()
    );
    let tool = tool()
        .with_timeout(Duration::from_secs(2))
        .with_output_observer(observer.clone());
    let cx = ctx(Caveats::top());
    let invocation = tokio::spawn(async move {
        tool.invoke(serde_json::json!({ "cmd": command }), &cx)
            .await
    });

    observer.wait_for_bytes(
        ShellOutputStream::Stdout,
        b"line 1\n",
        Duration::from_millis(500),
    );
    assert!(
        !invocation.is_finished(),
        "the invocation cannot finish while the child is waiting on the unopened gate"
    );
    assert!(!gate.exists(), "the command's wait gate is still closed");

    std::fs::write(&gate, b"continue").expect("open command wait gate");
    let out = invocation
        .await
        .expect("join Brush invocation")
        .expect("complete Brush invocation");
    let _ = std::fs::remove_file(&gate);
    observer.wait_finished();
    assert_eq!(out["exit_code"], 0, "the gated command completes: {out}");
    assert_eq!(out["stdout"], "line 1\nline 2\n");
}

#[cfg(unix)]
async fn stderr_observer_and_brush_envelope_apply_the_output_cap() {
    let observer = Arc::new(OutputRecorder::default());
    let out = tool()
        .with_output_observer(observer.clone())
        .invoke(
            serde_json::json!({
                "cmd": format!("yes b | head -c {} >&2", OUTPUT_CAP + 4),
            }),
            &ctx(Caveats::top()),
        )
        .await
        .expect("invoke chatty brush shell");

    observer.wait_finished();
    let observed = observer.bytes(ShellOutputStream::Stderr);
    assert_eq!(observed.len(), OUTPUT_CAP);
    assert_eq!(
        out["stderr"].as_str().expect("stderr string").as_bytes(),
        observed
    );
}

/// Full-access: a `$(...)` command substitution — refused by the safe-subset
/// engine — runs inside the dedicated worker, and the engine identity is
/// disclosed.
async fn full_access_runs_dynamic_construct_and_captures() {
    let out = tool()
        .invoke(
            serde_json::json!({ "cmd": "echo \"$(echo composed)\"" }),
            &ctx(Caveats::top()),
        )
        .await
        .expect("invoke");

    assert_ne!(out["denied"], true, "ambient grant must run: {out}");
    assert_eq!(out["exit_code"], 0, "the command must succeed: {out}");
    assert_eq!(
        out["stdout"].as_str().unwrap_or("").trim(),
        "composed",
        "the $(...) substitution must have executed in the Brush worker: {out}"
    );
    assert_eq!(
        out["disclosure"]["engine"], "brush",
        "engine identity must be disclosed: {out}"
    );
}

/// Xtrace reparses PS4 as a prompt. A model-controlled PS4 could therefore hide
/// a second command from the up-front shell inspection inventory. The confined
/// runtime pins PS4 to a readonly literal after env import, so even full-access
/// execution cannot smuggle an uninspected side effect through `set -x`.
#[cfg(unix)]
async fn xtrace_ps4_cannot_hide_an_uninspected_command() {
    let sentinel = unique_temp("ps4-hidden-command");
    let _ = std::fs::remove_file(&sentinel);
    let cmd = format!(
        "PS4='$(/usr/bin/touch \"{}\")'; set -x; echo ok",
        sentinel.display()
    );

    let out = tool()
        .invoke(serde_json::json!({ "cmd": cmd }), &ctx(Caveats::top()))
        .await
        .expect("invoke xtrace regression");

    assert!(
        !sentinel.exists(),
        "readonly literal PS4 must prevent the hidden command: {out}"
    );
}

/// Restricted `exec` (only `echo`): an out-of-scope external is DENIED by the
/// interceptor — structured `denied:true`, `kind:"exec"` — and never runs. This
/// is the engine's differentiator: it confines a restricted exec grant that the
/// sandbox-host engine refuses to serve.
async fn restricted_exec_denies_out_of_scope_command_in_worker() {
    let caveats = Caveats {
        exec: Scope::only(["echo".to_string()]),
        ..Caveats::top()
    };
    let sentinel = unique_temp("exec-sentinel");
    let _ = std::fs::remove_file(&sentinel);
    // Path-separator form goes straight to the external-spawn funnel → before_exec.
    let cmd = format!("/bin/touch {}", sentinel.to_string_lossy());

    let out = tool()
        .invoke(serde_json::json!({ "cmd": cmd }), &ctx(caveats))
        .await
        .expect("invoke");

    assert_eq!(
        out["denied"], true,
        "an out-of-scope exec must be denied by the interceptor: {out}"
    );
    assert_eq!(
        out["denials"][0]["kind"], "exec",
        "denial names exec: {out}"
    );
    assert!(
        !sentinel.exists(),
        "the denied command must not have run: {out}"
    );
}

/// The Brush production path (worker → core `ConfinedCommand` → `Sandbox::apply`)
/// installs the `ChildNetworkPolicy::DenyDirect` seccomp egress floor: a python3
/// child in the brush worker cannot create an AF_INET socket under `net: none`.
/// This proves the BrushShellTool threads `child_network` through the SAME apply
/// seam the safe-subset engine uses.
#[cfg(all(target_os = "linux", feature = "linux-landlock"))]
async fn brush_deny_direct_denies_a_childs_socket() {
    use agent_bridle_core::{ChildNetworkPolicy, SandboxPolicy, landlock_is_supported};
    if !landlock_is_supported() || !std::path::Path::new("/usr/bin/python3").exists() {
        eprintln!("skipping brush_deny_direct: needs Landlock + python3");
        return;
    }
    let t = BrushShellTool::new().with_sandbox_policy(Arc::new(SandboxPolicy {
        child_network: ChildNetworkPolicy::DenyDirect,
        ..SandboxPolicy::default()
    }));
    let caveats = Caveats {
        exec: Scope::only(["python3".to_string()]),
        net: Scope::none(),
        ..Caveats::top()
    };
    let cx = Gate::new(0).authorize(&t, &caveats).expect("authorize");
    let out = t
        .invoke(
            serde_json::json!({ "cmd": r#"python3 -c "import socket; socket.socket(socket.AF_INET, socket.SOCK_DGRAM)""# }),
            &cx,
        )
        .await
        .expect("invoke");
    assert_ne!(
        out["exit_code"], 0,
        "DenyDirect must deny the brush child's AF_INET socket creation: {out}"
    );
}

/// Syntax consent is not exec consent: even when Brush interprets `$()`, the
/// inner command crosses the ordinary interceptor independently and is denied.
async fn command_substitution_keeps_inner_exec_independently_gated() {
    let caveats = Caveats {
        exec: Scope::only(["echo".to_string()]),
        ..Caveats::top()
    };
    let sentinel = unique_temp("substitution-exec-sentinel");
    let _ = std::fs::remove_file(&sentinel);
    let cmd = format!("echo \"$(/bin/touch '{}')\"", sentinel.display());

    let out = tool()
        .invoke(serde_json::json!({ "cmd": cmd }), &ctx(caveats))
        .await
        .expect("invoke");

    assert_eq!(out["denied"], true, "inner touch must be denied: {out}");
    assert!(
        out["denials"]
            .as_array()
            .is_some_and(
                |denials| denials.iter().any(|denial| denial["kind"] == "exec"
                    && denial["target"]
                        .as_str()
                        .is_some_and(|target| target.ends_with("touch")))
            ),
        "the structured denial must name the inner executable: {out}"
    );
    assert!(
        !sentinel.exists(),
        "syntax approval must not let an ungranted inner command run: {out}"
    );
}

/// Restricted `exec` (only `echo`): an in-scope command still runs.
async fn restricted_exec_allows_in_scope_command() {
    let caveats = Caveats {
        exec: Scope::only(["echo".to_string()]),
        ..Caveats::top()
    };

    let out = tool()
        .invoke(serde_json::json!({ "cmd": "echo ok" }), &ctx(caveats))
        .await
        .expect("invoke");

    assert_ne!(out["denied"], true, "in-scope command must run: {out}");
    assert_eq!(out["stdout"].as_str().unwrap_or("").trim(), "ok", "{out}");
}

/// A restricted filesystem grant is never silently downgraded to the L2
/// interceptor when this build has no OS backend for the worker boundary.
#[cfg(not(any(
    feature = "linux-landlock",
    feature = "macos-seatbelt",
    feature = "windows-appcontainer"
)))]
async fn restricted_filesystem_without_l3_refuses_before_execution() {
    let marker = unique_temp("l3-refusal");
    let caveats = Caveats {
        fs_write: Scope::only([marker.to_string_lossy().into_owned()]),
        ..Caveats::top()
    };
    let result = tool()
        .invoke(
            serde_json::json!({ "cmd": format!("echo escaped > '{}'", marker.display()) }),
            &ctx(caveats),
        )
        .await;

    assert!(result.is_err(), "restricted fs must fail closed without L3");
    assert!(!marker.exists(), "refused command must not execute");
}

/// The schema's `env` seam now reaches the shell (EPIC #1243 Leg 2). Before
/// this, brush silently DROPPED `args["env"]` — a caller var expanded to empty.
/// This is the regression guard: a passed var expands inside the confined shell.
async fn env_seam_delivers_caller_vars_to_the_shell() {
    let out = tool()
        .invoke(
            serde_json::json!({
                "cmd": "echo \"$NEWT_SEAM_PROBE\"",
                "env": { "NEWT_SEAM_PROBE": "delivered" },
            }),
            &ctx(Caveats::top()),
        )
        .await
        .expect("invoke");

    assert_ne!(out["denied"], true, "{out}");
    assert_eq!(
        out["stdout"].as_str().unwrap_or("").trim(),
        "delivered",
        "the caller-provided env var must expand in the shell (was dropped before Leg 2): {out}"
    );
}

/// FIX 1 (critical #4): a confined stdin-reader must get **immediate EOF**, not
/// the operator's terminal. Before the fix `run_in_brush` seeded `STDIN_FD` from
/// the real `std::io::stdin()`, so a bare `cat`/`wc`/`grep` with no pipe read the
/// operator's fd 0 — hanging the turn and stealing keystrokes. With `STDIN_FD`
/// backed by `/dev/null`, a bare `cat` returns promptly with empty output + EOF.
/// The `tokio::time::timeout` here is the regression teeth: on the old behavior a
/// terminal fd 0 would block `cat` forever and this test would time out.
#[cfg(unix)]
async fn confined_stdin_reader_gets_eof_not_the_operator_terminal() {
    let cx = ctx(Caveats::top());
    let tool = tool();
    // Path-separator form runs the real external `/bin/cat` (the carried-coreutils
    // `cat` shim would otherwise re-exec this non-dispatch test binary); an
    // external child inherits the shell's STDIN_FD, so this proves the null fd
    // reaches spawned children.
    let invoke = tool.invoke(serde_json::json!({ "cmd": "/bin/cat" }), &cx);
    let out = tokio::time::timeout(Duration::from_secs(10), invoke)
        .await
        .expect("a confined stdin-reader must not block on the operator terminal")
        .expect("invoke");

    assert_eq!(out["exit_code"], 0, "cat on /dev/null exits 0: {out}");
    assert_eq!(
        out["stdout"], "",
        "stdin is /dev/null → cat reads immediate EOF, empty output: {out}"
    );
}

/// FIX 3 (critical #2/#8): the brush path had NO wall-clock timeout and hardcoded
/// `timed_out:false`, so a grinding/blocking confined command ran unbounded. With
/// a ceiling, a command that outlasts it is cut AT the ceiling — `timed_out:true`,
/// exit 124 — not after the command's full duration.
///
/// A short `sleep` (not the 30s of the field repro) keeps this real-spawn test
/// fast. The supervisor terminates the worker process group at the ceiling; the
/// next test separately proves a background descendant cannot survive it.
#[cfg(unix)]
async fn confined_run_is_bounded_by_the_wall_clock_ceiling() {
    let cx = ctx(Caveats::top());
    let tool = tool().with_timeout(Duration::from_secs(1));
    let start = std::time::Instant::now();
    let out = tokio::time::timeout(
        Duration::from_secs(20),
        tool.invoke(serde_json::json!({ "cmd": "sleep 3" }), &cx),
    )
    .await
    .expect("invoke must return at the ~1s ceiling, not after the sleep completes")
    .expect("invoke");
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_secs(3),
        "the run must be cut at the ~1s ceiling, took {elapsed:?}: {out}"
    );
    assert_eq!(out["timed_out"], true, "timed_out must be raised: {out}");
    assert_eq!(out["exit_code"], 124, "the timeout exit code is 124: {out}");
}

#[cfg(unix)]
async fn timeout_kills_worker_descendants() {
    let marker = unique_temp("timeout-descendant");
    let _ = std::fs::remove_file(&marker);
    let tool = tool().with_timeout(Duration::from_millis(150));
    let out = tool
        .invoke(
            serde_json::json!({
                "cmd": format!("(sleep 1; touch '{}') & wait", marker.display())
            }),
            &ctx(Caveats::top()),
        )
        .await
        .expect("invoke");
    assert_eq!(out["timed_out"], true, "{out}");
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(
        !marker.exists(),
        "a descendant must not survive the worker timeout"
    );
}

/// #373-C: a background descendant that outlives the worker and keeps the
/// inherited stdout/stderr writers open must not be able to hang the supervisor
/// or leave a terminal that claims completion while a writer is still alive.
///
/// The command exits immediately while a descendant sleeps far longer than this
/// test would tolerate. The supervisor owns the worker's process group, so it
/// reaps the tree on the NORMAL completion path — not only on timeout — and the
/// framed stream reaches end-of-stream instead of blocking on the surviving dup.
///
/// Honesty about strength: this is a GUARD, not a proven-failing regression.
/// With the tree reap disabled it still passes on Linux, because Brush does not
/// leave this descendant holding the worker's own stdout — so the hazard the
/// reap closes could not be reproduced through the public tool. The test pins
/// the properties that must never regress (prompt completion, complete
/// transcript, no surviving descendant); the reap makes them true by
/// construction rather than by luck.
///
/// The positive control matters regardless: without it, a lane where the marker
/// simply cannot be written would pass this test for the wrong reason.
#[cfg(unix)]
async fn a_background_descendant_cannot_hang_or_falsify_normal_completion() {
    let tool = tool().with_timeout(Duration::from_secs(30));
    let cx = ctx(Caveats::top());

    // Positive control: this lane can create a marker from the shell at all.
    let control = unique_temp("descendant-control");
    let _ = std::fs::remove_file(&control);
    tool.invoke(
        serde_json::json!({"cmd": format!("touch '{}'", control.display())}),
        &cx,
    )
    .await
    .expect("control invoke");
    assert!(
        control.exists(),
        "positive control failed: this lane cannot write a marker, so the \
         descendant assertion below would pass vacuously"
    );

    let marker = unique_temp("normal-descendant");
    let _ = std::fs::remove_file(&marker);
    // No `wait`: the command completes at once and leaves the descendant running.
    let invocation = tool.invoke(
        serde_json::json!({
            "cmd": format!("(sleep 5; touch '{}') & echo done", marker.display())
        }),
        &cx,
    );
    let out = tokio::time::timeout(Duration::from_secs(10), invocation)
        .await
        .expect("a surviving descendant must not hang the supervisor")
        .expect("invoke");

    assert_eq!(out["timed_out"], false, "{out}");
    assert_eq!(out["exit_code"], 0, "{out}");
    assert!(
        out["stdout"].as_str().unwrap_or_default().contains("done"),
        "the transcript must still be complete: {out}"
    );

    // The terminal was honest: by the time it was reported, the tree was gone.
    tokio::time::sleep(Duration::from_secs(7)).await;
    assert!(
        !marker.exists(),
        "a descendant survived a terminal that claimed the execution was complete"
    );
}

/// HOME crosses the seam — the concrete #783-class motivation: without it,
/// `~` expansion and HOME-relative tooling misbehave under the brush engine.
/// Nothing ambient leaks in (do_not_inherit_env); only the passed value shows.
async fn env_seam_delivers_home_for_tilde_class_tooling() {
    let out = tool()
        .invoke(
            serde_json::json!({
                "cmd": "echo \"$HOME\"",
                "env": { "HOME": "/seam/home" },
            }),
            &ctx(Caveats::top()),
        )
        .await
        .expect("invoke");

    assert_eq!(
        out["stdout"].as_str().unwrap_or("").trim(),
        "/seam/home",
        "HOME must cross the import surface: {out}"
    );
}

/// The vacuous-green gap the two tests above shared: `echo "$VAR"` is a brush
/// BUILTIN, so it reads brush's own internal variable table directly — it
/// would pass identically whether or not the seeded variable was ever
/// `.export()`ed, since export only controls what a *spawned child process*
/// inherits, never brush's own expansion. `ShellVariable::new` defaults
/// `exported: false`; `set_global` alone never flips it. Found live: a real
/// confined `run_command` dispatch showed only `PWD`/`SHLVL`/`_` in `env |
/// sort`'s output — none of PATH/HOME/USER, despite this file's own tests all
/// passing. `/usr/bin/env` (never a shell builtin) is this test's ground
/// truth: it can only print what its OWN process actually inherited.
async fn env_seam_vars_reach_a_real_spawned_childs_environment() {
    let out = tool()
        .invoke(
            serde_json::json!({
                "cmd": "env",
                "env": { "NEWT_SEAM_EXPORT_PROBE": "reached-the-child" },
            }),
            &ctx(Caveats::top()),
        )
        .await
        .expect("invoke");

    assert_ne!(out["denied"], true, "{out}");
    let stdout = out["stdout"].as_str().unwrap_or("");
    assert!(
        stdout
            .lines()
            .any(|line| line == "NEWT_SEAM_EXPORT_PROBE=reached-the-child"),
        "the caller-provided env var must reach a REAL spawned child's \
         environment, not just brush's own variable table (a `set_global` \
         without `.export()` passes `echo \"$VAR\"` yet still fails this): \
         {out}"
    );
}

/// The two halves of the child-environment contract, checked against a REAL
/// spawned child (`env` is never a shell builtin), so a `set_global` without
/// `.export()` cannot pass:
///
/// * the seeded PATH and the caller's `env` seam ARE in the child's
///   environment — without PATH a toolchain cannot find its own helpers
///   (`rustc` reports "linker `cc` not found" from an otherwise fine host);
/// * a variable that exists only in the AMBIENT process environment and was
///   NOT passed through the seam is ABSENT: `do_not_inherit_env` still holds,
///   so exporting the seeded set widened nothing.
async fn a_child_gets_the_seeded_path_and_the_seam_but_no_ambient_variable() {
    const AMBIENT: &str = "BRIDLE_AMBIENT_MUST_NOT_LEAK";
    // SAFETY: this runner executes its cases sequentially on one thread.
    unsafe { std::env::set_var(AMBIENT, "ambient-secret") };
    let out = tool()
        .invoke(
            serde_json::json!({
                "cmd": "env",
                "env": { "BRIDLE_SEAM_PROBE": "passed" },
            }),
            &ctx(Caveats::top()),
        )
        .await
        .expect("invoke");
    unsafe { std::env::remove_var(AMBIENT) };

    assert_ne!(out["denied"], true, "{out}");
    let stdout = out["stdout"].as_str().unwrap_or("");
    let has = |name: &str| stdout.lines().any(|l| l.starts_with(&format!("{name}=")));
    assert!(has("PATH"), "the seeded PATH must reach the child: {out}");
    assert!(
        stdout.lines().any(|l| l == "BRIDLE_SEAM_PROBE=passed"),
        "a seam variable must reach the child: {out}"
    );
    assert!(
        !stdout.contains("ambient-secret") && !has(AMBIENT),
        "an ambient variable that was not passed must NOT reach the child: {out}"
    );
}
