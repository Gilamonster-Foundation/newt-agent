//! Real-spawn reality-check for the **sandboxed-host engine** (ADR 0019, #194).
//!
//! These exercise the *real* [`HostShellTool`] with an actual `/bin/sh -c`
//! subprocess. The keystone test proves the ADR's whole thesis end to end: a
//! **dynamic construct the safe-subset engine structurally refuses**
//! (`$(...)`) *runs* under this engine, yet an out-of-scope filesystem write
//! from inside that same full shell is **kernel-denied** — the guarantee is
//! entirely on L3, exactly as ADR 0019 D1/D2 claim. The refusal tests prove the
//! honesty posture: a restricted `exec`/`net` grant is refused (D5.2), never run
//! advisory.
//!
//! Kept out of the unit tests (which mock the spawner) per the workspace norm:
//! no real subprocesses/fs in unit tests.
#![cfg(all(feature = "host-shell", unix))]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use agent_bridle_core::{Caveats, Gate, Scope, Tool, ToolContext};
use agent_bridle_tool_shell::{
    HostShellTool, ShellInvocationId, ShellOutputObserver, ShellOutputStream,
};

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

    fn wait_finished(&self) {
        let finished = self.finished.lock().expect("finished lock");
        let (finished, _) = self
            .finished_cv
            .wait_timeout_while(finished, Duration::from_secs(2), |finished| !*finished)
            .expect("finished condition variable");
        assert!(*finished, "timed out waiting for observer finish");
    }
}

/// Mint a [`ToolContext`] carrying `granted` — the public-API path an embedder
/// uses (mirrors `real_spawn.rs`).
fn ctx(granted: Caveats) -> ToolContext {
    Gate::new(0)
        .authorize(&HostShellTool::new(), &granted)
        .expect("authorize")
}

fn unique_temp(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "ab-hostshell-{}-{}-{}",
        tag,
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ))
}

#[tokio::test]
async fn output_observer_matches_the_host_shell_envelope() {
    let observer = Arc::new(OutputRecorder::default());
    let out = HostShellTool::new()
        .with_output_observer(observer.clone())
        .invoke(
            serde_json::json!({ "cmd": "printf host-out; printf host-err >&2" }),
            &ctx(Caveats::top()),
        )
        .await
        .expect("invoke");

    observer.wait_finished();
    assert_eq!(observer.bytes(ShellOutputStream::Stdout), b"host-out");
    assert_eq!(observer.bytes(ShellOutputStream::Stderr), b"host-err");
    assert_eq!(out["stdout"], "host-out");
    assert_eq!(out["stderr"], "host-err");
}

#[cfg(unix)]
#[tokio::test]
async fn stderr_observer_and_host_envelope_apply_the_output_cap() {
    let observer = Arc::new(OutputRecorder::default());
    let out = HostShellTool::new()
        .with_output_observer(observer.clone())
        .invoke(
            serde_json::json!({
                "cmd": format!("yes h | head -c {} >&2", OUTPUT_CAP + 4),
            }),
            &ctx(Caveats::top()),
        )
        .await
        .expect("invoke chatty host shell");

    observer.wait_finished();
    let observed = observer.bytes(ShellOutputStream::Stderr);
    assert_eq!(observed.len(), OUTPUT_CAP);
    let envelope = out["stderr"].as_str().expect("stderr string");
    let captured = envelope
        .strip_suffix("…[truncated]")
        .expect("host envelope marks truncation");
    assert_eq!(captured.as_bytes(), observed);
}

/// **The reality check (ADR 0019 D1/D2).** Full shell semantics run — a `$(...)`
/// command substitution the safe-subset engine refuses by design — while an
/// out-of-scope write from inside that same shell is stopped by the kernel, not
/// by any parser. Requires a Landlock-capable Linux build (`linux-landlock`);
/// otherwise the engine would fail-closed (restricted fs, no backend) and the
/// dynamic construct would never get to run, which is a different (also correct)
/// posture covered by the refusal tests.
#[cfg(all(target_os = "linux", feature = "linux-landlock"))]
#[tokio::test]
async fn dynamic_construct_runs_but_out_of_scope_write_is_kernel_denied() {
    use agent_bridle_core::landlock_is_supported;

    if !landlock_is_supported() {
        eprintln!("skipping: kernel lacks Landlock");
        return;
    }

    let allowed = unique_temp("allowed");
    std::fs::create_dir_all(&allowed).unwrap();
    let forbidden = unique_temp("forbidden");
    std::fs::create_dir_all(&forbidden).unwrap();

    // fs_write fenced to `allowed`; exec/net stay ambient (the engine only
    // serves fs-restricted). fs_read stays open so the loader can map libc.
    let caveats = Caveats {
        fs_write: Scope::only([allowed.to_string_lossy().into_owned()]),
        ..Caveats::top()
    };

    // A single full-shell command line that (1) uses `$(...)` — the exact
    // dynamic construct the safe-subset engine refuses — to produce content,
    // writing it IN scope; (2) then tries to write OUT of scope; (3) always
    // exits 0 so the assertions key off filesystem effects, not exit status.
    let ok_path = format!("{}/ok.txt", allowed.to_string_lossy());
    let evil_path = format!("{}/evil.txt", forbidden.to_string_lossy());
    let cmd = format!(
        "echo \"$(echo dynamic-ran)\" > {ok_path}; echo escaped > {evil_path} 2>/dev/null; echo done"
    );

    let out = HostShellTool::new()
        .invoke(serde_json::json!({ "cmd": cmd }), &ctx(caveats))
        .await
        .expect("invoke");

    // The jail engaged and is honestly disclosed.
    assert_eq!(
        out["sandbox_kind"], "landlock",
        "engine must report real kernel enforcement: {out}"
    );
    assert_eq!(
        out["disclosure"]["engine"], "sandbox-host",
        "engine identity must be disclosed (ADR 0019 D4): {out}"
    );
    // `denied` is omitted from JSON when false (skip_serializing_if), so it
    // reads as absent, not literal `false` — assert it is not the denied path.
    assert_ne!(
        out["denied"], true,
        "an fs-restricted grant is served: {out}"
    );

    // (1) The `$(...)` dynamic construct RAN and wrote in scope — the middle of
    // the ADR: full shell semantics, allowed because the kernel bounds reach.
    assert!(
        allowed.join("ok.txt").exists(),
        "the in-scope write from a dynamic construct must succeed: {out}"
    );
    let body = std::fs::read_to_string(allowed.join("ok.txt")).unwrap();
    assert_eq!(
        body.trim(),
        "dynamic-ran",
        "the $(...) substitution must have executed"
    );

    // (2) The out-of-scope write was stopped by Landlock — no parser involved.
    assert!(
        !forbidden.join("evil.txt").exists(),
        "the out-of-scope write must be kernel-denied: {out}"
    );

    let _ = std::fs::remove_dir_all(&allowed);
    let _ = std::fs::remove_dir_all(&forbidden);
}

/// The macOS mirror of the fence test (ADR 0019 D4 / D5.1): Seatbelt confines a
/// process *and its descendants*, so one SBPL profile on `/bin/sh -c` bounds the
/// whole tree. Same thesis as the Linux test — a full-shell command runs, an
/// out-of-scope write is kernel-denied — with `sandbox_kind == "seatbelt"`.
/// Compiled only on a `macos-seatbelt` build; this is the E2E check to run
/// during Mac usability testing (Linux is exercised by the Landlock test above).
#[cfg(all(target_os = "macos", feature = "macos-seatbelt"))]
#[tokio::test]
async fn macos_dynamic_construct_runs_but_out_of_scope_write_is_seatbelt_denied() {
    let allowed = unique_temp("allowed");
    std::fs::create_dir_all(&allowed).unwrap();
    let forbidden = unique_temp("forbidden");
    std::fs::create_dir_all(&forbidden).unwrap();

    let caveats = Caveats {
        fs_write: Scope::only([allowed.to_string_lossy().into_owned()]),
        ..Caveats::top()
    };

    let ok_path = format!("{}/ok.txt", allowed.to_string_lossy());
    let evil_path = format!("{}/evil.txt", forbidden.to_string_lossy());
    let cmd = format!(
        "echo \"$(echo dynamic-ran)\" > {ok_path}; echo escaped > {evil_path} 2>/dev/null; echo done"
    );

    let out = HostShellTool::new()
        .invoke(serde_json::json!({ "cmd": cmd }), &ctx(caveats))
        .await
        .expect("invoke");

    assert_eq!(
        out["sandbox_kind"], "seatbelt",
        "engine must report real kernel enforcement: {out}"
    );
    assert_ne!(
        out["denied"], true,
        "an fs-restricted grant is served: {out}"
    );
    assert!(
        allowed.join("ok.txt").exists(),
        "the in-scope write from a dynamic construct must succeed: {out}"
    );
    assert!(
        !forbidden.join("evil.txt").exists(),
        "the out-of-scope write must be kernel-denied: {out}"
    );

    let _ = std::fs::remove_dir_all(&allowed);
    let _ = std::fs::remove_dir_all(&forbidden);
}

/// Honesty (ADR 0019 D2 / D5.2): a restricted `exec` grant is **refused** with a
/// structured denial — the engine cannot bound a full shell's forked children,
/// so it does not pretend to. No subprocess runs.
#[tokio::test]
async fn restricted_exec_is_refused_not_run() {
    let caveats = Caveats {
        exec: Scope::only(["echo".to_string()]),
        ..Caveats::top()
    };
    let sentinel = unique_temp("exec-refused-sentinel");
    let cmd = format!("echo pwned > {}", sentinel.to_string_lossy());

    let out = HostShellTool::new()
        .invoke(serde_json::json!({ "cmd": cmd }), &ctx(caveats))
        .await
        .expect("invoke");

    assert_eq!(
        out["denied"], true,
        "a restricted exec grant must be refused: {out}"
    );
    assert_eq!(
        out["denials"][0]["kind"], "exec",
        "the denial must name the exec axis: {out}"
    );
    assert_eq!(
        out["disclosure"]["engine"], "sandbox-host",
        "engine identity disclosed even on refusal: {out}"
    );
    assert!(
        !sentinel.exists(),
        "nothing may run when the grant is refused: {out}"
    );
}

/// Honesty (ADR 0019 D2): a restricted `net` grant is refused the same way,
/// until the netns/seccomp sibling lands.
#[tokio::test]
async fn restricted_net_is_refused() {
    let caveats = Caveats {
        net: Scope::only(["example.com:443".to_string()]),
        ..Caveats::top()
    };

    let out = HostShellTool::new()
        .invoke(serde_json::json!({ "cmd": "echo hi" }), &ctx(caveats))
        .await
        .expect("invoke");

    assert_eq!(
        out["denied"], true,
        "a restricted net grant must be refused: {out}"
    );
    assert_eq!(
        out["denials"][0]["kind"], "net",
        "the denial must name the net axis: {out}"
    );
}

/// The unrestricted, ambient case still runs (exec=net=fs=All): the engine is
/// additive, not a new refusal surface. This is the "full shell, no fence"
/// baseline; enforcement only appears once an fs axis is restricted.
#[tokio::test]
async fn fully_ambient_grant_runs_the_command() {
    let out = HostShellTool::new()
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
        "the dynamic construct must have executed: {out}"
    );
}

/// Regression (Track 1a — full-access parity): under a fully-authorized grant the
/// engine seeds a usable `PATH` into the child so bare program names
/// (`grep`/`ls`/`find`) resolve like the host shell, instead of relying on the
/// shell's fragile compiled `_CS_PATH` fallback (empty when `env_clear` scrubs
/// `PATH`). Proven by having the child echo its own `$PATH`: **pre-fix it was
/// unset (empty); post-fix it equals `default_exec_path()`**. `printf` is a shell
/// builtin, so this isolates the *seeding* — it needs no PATH itself.
#[tokio::test]
async fn full_access_seeds_default_path_into_the_child() {
    use agent_bridle_core::default_exec_path;

    let out = HostShellTool::new()
        .invoke(
            serde_json::json!({ "cmd": "printf '%s' \"$PATH\"" }),
            &ctx(Caveats::top()),
        )
        .await
        .expect("invoke");

    assert_ne!(out["denied"], true, "ambient grant must run: {out}");
    assert_eq!(
        out["stdout"].as_str().unwrap_or_default(),
        default_exec_path(),
        "the child must see the seeded default PATH (was unset pre-fix): {out}"
    );
}

/// A caller-provided `PATH` wins over the seeded default, and a **bare program
/// name** then resolves and runs inside the engine — the functional end of the
/// parity fix (grep/ls/find resolving). Deterministic: a temp dir with a marker
/// tool, no dependence on which host binaries live where.
#[cfg(unix)]
#[tokio::test]
async fn bare_name_resolves_when_path_includes_its_dir() {
    use std::os::unix::fs::PermissionsExt;

    let dir = unique_temp("bin");
    std::fs::create_dir_all(&dir).unwrap();
    let tool = dir.join("marker-tool");
    std::fs::write(&tool, "#!/bin/sh\necho marker-ran\n").unwrap();
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();

    let out = HostShellTool::new()
        .invoke(
            serde_json::json!({
                "cmd": "marker-tool",
                "env": { "PATH": dir.to_string_lossy() },
            }),
            &ctx(Caveats::top()),
        )
        .await
        .expect("invoke");

    assert_ne!(out["denied"], true, "ambient grant must run: {out}");
    assert_eq!(out["exit_code"], 0, "the bare-name tool must run: {out}");
    assert_eq!(
        out["stdout"].as_str().unwrap_or_default().trim(),
        "marker-ran",
        "the bare program name must resolve via the provided PATH: {out}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
