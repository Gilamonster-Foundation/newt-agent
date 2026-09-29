//! Real-spawn integration tests for the engine's `std::process` path.
//!
//! These exercise the *real* `OsSpawner` with actual processes (and, for
//! redirections, real files), and are kept out of the unit tests (which mock the
//! spawner) per the workspace norm: no real subprocesses/fs in unit tests. They
//! use only tools that are universally present *on Unix* (`echo`, `cat`, `sort`,
//! `true`, `false`, `env`, and `touch` for the Linux-only Landlock test).
//!
//! Unix-only (issue #193): every case here spawns a standalone POSIX binary, none
//! of which ships on stock Windows — `echo`/`true`/`false` are `cmd` builtins, and
//! `cat`/`sort`/`env` do not exist at all — so `cargo test --workspace` on Windows
//! fails to spawn them. The Linux/macOS cases below already narrow to their kernel
//! (`target_os = "linux"`/`"macos"`), and the real Windows kernel boundary is
//! proven separately by the AppContainer suite in `agent-bridle-aclaunch`
//! (`kernel_proofs.rs`/`net_proofs.rs`), so gating the whole file to `unix` loses
//! no Windows coverage. Without this gate the nightly Windows `cargo test
//! --workspace` runner reports 17 of these as failed.
#![cfg(all(unix, feature = "shell"))]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use agent_bridle_core::{Caveats, Gate, Scope, Tool, ToolContext};
use agent_bridle_tool_shell::ShellTool;

/// Mint a context the only legitimate way — through the gate.
fn ctx(granted: Caveats) -> ToolContext {
    Gate::new(0)
        .authorize(&ShellTool::new(), &granted)
        .expect("authorize")
}

fn exec_only(names: &[&str]) -> Caveats {
    Caveats {
        exec: Scope::only(names.iter().map(|s| (*s).to_string())),
        ..Caveats::top()
    }
}

/// A unique temp path (pid + atomic counter, never a clock) so parallel tests do
/// not collide. The file is created/used by the test and removed at the end.
fn unique_temp(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "ab-redir-{}-{}-{}",
        tag,
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ))
}

fn shell_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[tokio::test]
async fn real_echo_runs_and_captures_stdout() {
    let out = ShellTool::new()
        .invoke(
            serde_json::json!({"program": "echo", "args": ["hello"]}),
            &ctx(Caveats::top()),
        )
        .await
        .expect("invoke");
    assert_eq!(out["exit_code"], 0);
    assert_eq!(out["stdout"], "hello\n");
    assert!(out.get("denied").is_none());
}

/// A production `OsSpawner` must refuse restricted authority when this build has
/// no native sandbox backend. The marker proves refusal happens before spawn.
#[cfg(not(any(
    all(target_os = "linux", feature = "linux-landlock"),
    all(target_os = "macos", feature = "macos-seatbelt")
)))]
#[tokio::test]
async fn real_no_backend_restricted_authority_refuses_before_spawn() {
    let marker = unique_temp("no-backend-refusal");
    let _ = std::fs::remove_file(&marker);
    let restricted = Caveats {
        net: Scope::none(),
        ..Caveats::top()
    };

    let out = ShellTool::new()
        .invoke(
            serde_json::json!({"program": "touch", "args": [shell_path(&marker)]}),
            &ctx(restricted),
        )
        .await
        .expect("backend admission refusal is a structured envelope");

    assert_eq!(
        out["denied"], true,
        "restricted authority must fail closed: {out}"
    );
    assert_eq!(
        out["denials"][0]["kind"], "net",
        "unexpected refusal: {out}"
    );
    assert!(
        !marker.exists(),
        "the marker proves the real OsSpawner must not have spawned: {out}"
    );
}

/// A permitted direct command still must not spawn when its restricted exec
/// authority has no native backend. This isolates the exec axis: every other
/// caveat is unrestricted, and the marker proves refusal precedes `OsSpawner`.
#[cfg(not(any(
    all(target_os = "linux", feature = "linux-landlock"),
    all(target_os = "macos", feature = "macos-seatbelt")
)))]
#[tokio::test]
async fn real_no_backend_restricted_exec_refuses_before_spawn() {
    let marker = unique_temp("no-backend-exec-refusal");
    let _ = std::fs::remove_file(&marker);
    let restricted = Caveats {
        exec: Scope::only(["touch".to_string()]),
        ..Caveats::top()
    };

    let out = ShellTool::new()
        .invoke(
            serde_json::json!({"program": "touch", "args": [shell_path(&marker)]}),
            &ctx(restricted),
        )
        .await
        .expect("backend admission refusal is a structured envelope");

    assert_eq!(
        out["denied"], true,
        "restricted exec must fail closed: {out}"
    );
    assert_eq!(
        out["denials"][0]["kind"], "exec",
        "unexpected refusal: {out}"
    );
    assert!(
        !marker.exists(),
        "the marker proves the real OsSpawner must not have spawned: {out}"
    );
}

/// #269 / AB-006: a timed-out child is KILLED and reaped — it must not run to
/// completion. The script writes a START marker immediately, then `sleep 3`, then
/// a DONE marker; with a 1s timeout the envelope reports a confirmed timeout,
/// START exists (a positive control: the child ACTUALLY STARTED — so "DONE
/// absent" reflects a kill, not a child that never ran), and after the would-be
/// sleep elapses DONE never appears (the sleep was SIGKILLed, not merely
/// un-awaited).
///
/// This process-lifecycle test intentionally uses unrestricted caveats. Native
/// Seatbelt exec-closure behavior is covered by the dedicated test below.
#[tokio::test]
async fn real_timed_out_child_is_killed_and_never_writes_marker() {
    let start = unique_temp("ab006-child-start");
    let done = unique_temp("ab006-child-done");
    let script = format!(
        "touch {}; sleep 3; touch {}",
        shell_path(&start),
        shell_path(&done)
    );
    let out = ShellTool::new()
        .invoke(
            serde_json::json!({"program": "sh", "args": ["-c", script], "timeout_secs": 1}),
            &ctx(Caveats::top()),
        )
        .await
        .expect("invoke");
    assert_eq!(out["timed_out"], true, "must report a confirmed timeout");
    // Positive control: the child really started and ran its first line, so the
    // DONE-absent assertion below cannot be satisfied by a child that never ran
    // (the exact #318 false-green this guards against).
    assert!(
        start.exists(),
        "the child never started (START marker absent) — a fast-return \
         masquerading as a timeout, not a real kill"
    );
    // Wait well past the child's 3s sleep; a killed child never reaches DONE.
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    assert!(
        !done.exists(),
        "the timed-out child must be killed, not run to completion (its DONE marker appeared)"
    );
    let _ = std::fs::remove_file(&start);
    let _ = std::fs::remove_file(&done);
}

/// #269 / AB-006: the kill reaches a GRANDCHILD, not just the direct child —
/// process-group SIGKILL takes the whole tree. The outer shell writes START, then
/// an inner `sh -c 'sleep 3; touch DONE'`; the grandchild's sleep is what the
/// timeout must kill. START proves the tree actually started (#318 positive
/// control); DONE must never appear.
#[tokio::test]
async fn real_timed_out_grandchild_is_killed() {
    let start = unique_temp("ab006-grandchild-start");
    let done = unique_temp("ab006-grandchild-done");
    let script = format!(
        "touch {}; sh -c 'sleep 3; touch {}'",
        shell_path(&start),
        shell_path(&done)
    );
    let out = ShellTool::new()
        .invoke(
            serde_json::json!({"program": "sh", "args": ["-c", script], "timeout_secs": 1}),
            &ctx(Caveats::top()),
        )
        .await
        .expect("invoke");
    assert_eq!(out["timed_out"], true);
    assert!(
        start.exists(),
        "the process tree never started (START marker absent) — not a real kill"
    );
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    assert!(
        !done.exists(),
        "the timed-out grandchild must be killed via the process group (DONE appeared)"
    );
    let _ = std::fs::remove_file(&start);
    let _ = std::fs::remove_file(&done);
}

/// Exec-closure scope-fidelity, on real Seatbelt (model B — the #317 review): a
/// grant of `sh` necessarily pulls `/bin/bash` into the kernel `process-exec*`
/// allow-list (Apple's `/bin/sh` re-execs `/bin/bash`). This RECORDS the real
/// authority and asserts the report agrees: from a confined `sh` (exec=Only{sh}),
/// `/bin/bash` DOES run (it is in the closure — the widening beyond the literal
/// Caveat), which is exactly why `enforcement.exec` is reported `interceptor`, not
/// an exact Kernel witness. An un-granted, non-variant program (`/bin/cat`) stays
/// kernel-denied, so the closure is bounded to {sh, bash}, not opened.
#[cfg(all(target_os = "macos", feature = "macos-seatbelt"))]
#[tokio::test]
async fn real_seatbelt_exec_closure_includes_bash_but_not_unrelated() {
    use agent_bridle_core::seatbelt_is_supported;
    if !seatbelt_is_supported() {
        return;
    }
    let bash = ShellTool::new()
        .invoke(
            serde_json::json!({"program": "sh", "args": ["-c", "/bin/bash -c 'echo IN_CLOSURE'"]}),
            &ctx(exec_only(&["sh"])),
        )
        .await
        .expect("invoke");
    assert_eq!(bash["sandbox_kind"], "seatbelt", "{bash}");
    assert_eq!(
        bash["enforcement"]["exec"], "interceptor",
        "a granted sh pulls /bin/bash → exec is NOT an exact Kernel witness: {bash}"
    );
    assert!(
        bash["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("IN_CLOSURE"),
        "/bin/bash is in the exec closure of a granted sh (the widening): {bash}"
    );
    // An unrelated program stays kernel-denied: the closure is bounded to {sh,bash}.
    let cat = ShellTool::new()
        .invoke(
            serde_json::json!({"program": "sh", "args": ["-c", "/bin/cat /etc/hosts && echo CATRAN || echo CATDENIED"]}),
            &ctx(exec_only(&["sh"])),
        )
        .await
        .expect("invoke");
    assert!(
        !cat["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("CATRAN"),
        "an un-granted, non-variant program must stay kernel-denied in the sh closure: {cat}"
    );
}

/// #268 / AB-004: loader / interpreter / hook env vars are DROPPED before the
/// child, regardless of the exec leash — they hijack what an allowed program
/// actually executes. A non-denied var still passes through.
#[tokio::test]
async fn real_loader_hook_env_vars_are_fenced_out() {
    let out = ShellTool::new()
        .invoke(
            serde_json::json!({
                "program": "env",
                "env": {
                    "LD_PRELOAD": "/evil.so",
                    "PYTHONPATH": "/evil",
                    "GIT_SSH_COMMAND": "evil",
                    "BASH_ENV": "/evil",
                    "NODE_OPTIONS": "--require /evil",
                    "SAFE_VAR": "keep",
                },
            }),
            &ctx(Caveats::top()),
        )
        .await
        .expect("invoke");
    let stdout = out["stdout"].as_str().unwrap_or_default();
    for denied in [
        "LD_PRELOAD",
        "PYTHONPATH",
        "GIT_SSH_COMMAND",
        "BASH_ENV",
        "NODE_OPTIONS",
    ] {
        assert!(
            !stdout.contains(denied),
            "denied loader/hook var {denied} must never reach the child:\n{stdout}"
        );
    }
    assert!(
        stdout.contains("SAFE_VAR=keep"),
        "a non-denied caller var must still pass through:\n{stdout}"
    );
}

/// #268 / AB-016: the child does NOT inherit the parent's ambient environment —
/// it starts from a fixed minimal baseline plus only the (fenced) caller env.
#[tokio::test]
async fn real_ambient_env_is_not_inherited() {
    // A parent-process (ambient) var not passed as a caller env entry.
    std::env::set_var("AB016_AMBIENT_SECRET", "leak");
    let out = ShellTool::new()
        .invoke(serde_json::json!({"program": "env"}), &ctx(Caveats::top()))
        .await
        .expect("invoke");
    std::env::remove_var("AB016_AMBIENT_SECRET");
    let stdout = out["stdout"].as_str().unwrap_or_default();
    assert!(
        !stdout.contains("AB016_AMBIENT_SECRET"),
        "ambient parent env must not leak into the confined child:\n{stdout}"
    );
}

/// agent-bridle#319 regression: an *ambient* descriptor the parent left open
/// (CLOEXEC cleared) is an object capability, and it MUST NOT be inherited by a
/// confined child. The env analog is `real_ambient_env_is_not_inherited` above;
/// this is the descriptor analog. A `std::fs::File` is `O_CLOEXEC` by default, so
/// we explicitly clear CLOEXEC to model a descriptor the parent opened
/// inheritable, then confirm it does not appear in the child's `/proc/self/fd`.
///
/// RED until #319 lands: today the spawn boundary relies on the CLOEXEC
/// convention only and does not close ambient fds, so the file leaks into the
/// child. GREEN once the spawn path closes inherited fds >= 3 (preserving stdio).
#[cfg(target_os = "linux")]
#[tokio::test]
async fn real_ambient_fd_is_not_inherited() {
    use std::os::fd::AsRawFd;

    // A regular file the parent holds open with CLOEXEC CLEARED — i.e. an
    // inheritable ambient descriptor (an object capability the child was never
    // delegated).
    let path = unique_temp("ab319-ambient");
    std::fs::write(&path, b"ambient-capability").expect("write temp");
    let file = std::fs::File::open(&path).expect("open temp");
    rustix::io::fcntl_setfd(&file, rustix::io::FdFlags::empty()).expect("clear cloexec");
    let ambient_fd = file.as_raw_fd();
    assert!(
        ambient_fd >= 3,
        "ambient fd should be >= 3, got {ambient_fd}"
    );

    // Run a confined child that reports its descriptor table with symlink
    // targets. `top()` imposes no fs/exec fence, isolating fd hygiene as the only
    // variable. An inherited *file* fd shows its path in `/proc/self/fd`; the
    // child's own stdio pipes and dir handle do not carry this path.
    let out = ShellTool::new()
        .invoke(
            serde_json::json!({"program": "ls", "args": ["-l", "/proc/self/fd"]}),
            &ctx(Caveats::top()),
        )
        .await
        .expect("invoke");

    // Keep the ambient fd open across the spawn, then release it.
    let stdout = out["stdout"].as_str().unwrap_or_default().to_string();
    drop(file);
    let _ = std::fs::remove_file(&path);

    // Prove the OBSERVATION happened before concluding anything from an
    // absence. Without this, an empty `stdout` — child never ran, `ls` failed,
    // invocation returned nothing — satisfies the "path is absent" assertion
    // below and certifies the claim from a dead child. The macOS twin has had
    // an unguarded positive control since it was written; the Linux original
    // never did, so this closes a gap rather than opening one.
    let seen = listed_fds_long(&stdout);
    assert_eq!(out["exit_code"], 0, "the confined `ls` must run: {out}");
    assert!(
        seen.contains(&1) && seen.contains(&2) && seen.len() >= 3,
        "the child must report its OWN descriptor table (stdout, stderr and the \
         handle `ls` opened on the directory); an empty or truncated listing \
         would make the absence assertion below vacuous. fd 0 is not required — \
         a confined stage\u{27}s stdin disposition is the caller\u{27}s choice.\n{stdout}"
    );

    let path_str = path.to_string_lossy();
    assert!(
        !stdout.contains(path_str.as_ref()),
        "ambient descriptor {ambient_fd} ({path_str}) leaked into the confined child (agent-bridle#319):\n{stdout}"
    );
}

/// Descriptor numbers in an `ls -l /proc/self/fd` listing — the `N -> target`
/// column. Used to prove the child produced a real descriptor table, so that an
/// absence in that table is an observation rather than a void.
#[cfg(target_os = "linux")]
fn listed_fds_long(listing: &str) -> std::collections::BTreeSet<std::os::fd::RawFd> {
    listing
        .lines()
        .filter_map(|line| line.split_once(" -> "))
        .filter_map(|(head, _)| head.rsplit(' ').next()?.parse().ok())
        .collect()
}

/// macOS twin of `real_ambient_fd_is_not_inherited` (agent-bridle#319, macOS
/// leg). macOS has no `/proc/self/fd` with symlink targets, so the confined
/// child reports its OWN descriptor table via `ls /dev/fd` (devfs lists the
/// calling process's open descriptors) and the ambient descriptor number must
/// be absent from it. An unguarded positive control proves the descriptor
/// really was inheritable, so the test measures the guard rather than a
/// descriptor that was never at risk.
///
/// **Never probe this with a shell redirection** (`echo probe >&N`). That was
/// the original oracle here and it is unsound on Darwin: with `N = 10` and the
/// descriptor *provably closed* (verified in a standalone C replica — the
/// child's inherited table was exactly `{0, 1, 2}` and the parent's file stayed
/// empty), macOS `bash` 3.2.57 still resolved `>&10` to a descriptor of its own
/// connected to stdout, printed `probe` and exited 0 — a false alarm on a
/// working guard, and by the same mechanism a potential false pass. macOS
/// `/bin/sh` (a *different* binary of the same family) does the same, silently,
/// 3 times out of 3. Shells reserve descriptor numbers from 10 upward for their
/// own save/restore slots; the oracle must be the child's descriptor table.
///
/// The `/bin/sh` -> `/bin/bash` switch in 38ca0d2 was treating the false-FAIL
/// face of this same defect (dash parse-errors on multi-digit descriptor
/// redirections), which is why changing shells did not help: the oracle, not
/// the shell, was wrong.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn real_ambient_fd_is_not_inherited_macos() {
    use std::os::fd::AsRawFd;

    // A file the parent holds open with CLOEXEC CLEARED — an inheritable
    // ambient descriptor (an object capability the child was never delegated).
    let path = unique_temp("ab319-ambient-macos");
    std::fs::write(&path, b"ambient-capability").expect("write temp");
    let file = std::fs::File::open(&path).expect("open temp");

    // ESTABLISH the descriptor number; do not assert about it. `open` in a
    // freshly-spawned test process deterministically returns 3, and 3 is where
    // `ls` puts its own handle on /dev/fd in the confined child —
    // indistinguishable from an inherited one. Asserting `>= 4` would make the
    // test's ability to RUN depend on how many descriptors the machine happened
    // to have open: green on a CI runner, red on a quiet Mac, both correct
    // reports of different worlds. Duplicating to a slot above everything
    // currently open removes that environment dependence, exactly as
    // `place_ambient` does in `agent-bridle-fdguard/tests/hostile_fds.rs`.
    // Never depend on an fd number you did not choose.
    let ambient = rustix::io::fcntl_dupfd_cloexec(&file, highest_open_fd() + 16)
        .expect("place the ambient descriptor above the open table");
    rustix::io::fcntl_setfd(&ambient, rustix::io::FdFlags::empty()).expect("clear cloexec");
    let ambient_fd = ambient.as_raw_fd();

    // Positive control: an UNGUARDED spawn inherits the descriptor, so the
    // number really is visible in a child's table when nothing closes it.
    let control = std::process::Command::new("/bin/ls")
        .arg("/dev/fd")
        .output()
        .expect("spawn control ls");
    let control_out = String::from_utf8_lossy(&control.stdout).into_owned();
    assert!(
        listed_fds(&control_out).contains(&ambient_fd),
        "positive control: an unguarded child must inherit fd {ambient_fd}:\n{control_out}"
    );

    // The confined child must not see it. `top()` imposes no fs/exec fence,
    // isolating fd hygiene as the only variable.
    let out = ShellTool::new()
        .invoke(
            serde_json::json!({"program": "/bin/ls", "args": ["/dev/fd"]}),
            &ctx(Caveats::top()),
        )
        .await
        .expect("invoke");

    // Keep the ambient fd open across the spawn, then release it.
    let stdout = out["stdout"].as_str().unwrap_or_default().to_string();
    drop(ambient);
    drop(file);
    let _ = std::fs::remove_file(&path);

    let seen = listed_fds(&stdout);
    assert_eq!(out["exit_code"], 0, "the confined `ls` must run: {out}");
    assert!(
        seen.contains(&1) && seen.contains(&2) && seen.len() >= 3,
        "the child must report its OWN descriptor table (stdout, stderr and the \
         handle `ls` opened on the directory); an empty or truncated listing \
         would make the absence assertion below vacuous. fd 0 is not required — \
         a confined stage\u{27}s stdin disposition is the caller\u{27}s choice.\n{stdout}"
    );
    assert!(
        !seen.contains(&ambient_fd),
        "ambient descriptor {ambient_fd} leaked into the confined child (agent-bridle#319 macOS leg):\n{stdout}"
    );
}

/// One past the highest descriptor currently open in this process — the floor
/// for a descriptor slot the test can call its own.
#[cfg(target_os = "macos")]
fn highest_open_fd() -> std::os::fd::RawFd {
    std::fs::read_dir("/dev/fd")
        .expect("/dev/fd must be enumerable")
        .filter_map(|entry| {
            entry
                .ok()?
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<std::os::fd::RawFd>().ok())
        })
        .max()
        .unwrap_or(2)
}

/// Descriptor numbers in an `ls /dev/fd` listing.
#[cfg(target_os = "macos")]
fn listed_fds(listing: &str) -> std::collections::BTreeSet<std::os::fd::RawFd> {
    listing
        .split_whitespace()
        .filter_map(|token| token.parse().ok())
        .collect()
}

/// #143 regression: the captured-output cap is config-driven (not a hard-coded
/// const). A `ShellTool` built with a tiny `max_output_bytes` truncates a chatty
/// command's stdout at the configured bound and flags it truncated.
#[tokio::test]
async fn real_output_cap_is_config_driven() {
    let limits = agent_bridle_core::LimitsPolicy {
        max_output_bytes: 8,
        ..agent_bridle_core::LimitsPolicy::default()
    };
    let out = ShellTool::with_config(limits)
        .invoke(
            serde_json::json!({"program": "echo", "args": ["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]}),
            &ctx(Caveats::top()),
        )
        .await
        .expect("invoke");
    let stdout = out["stdout"].as_str().expect("stdout string");
    assert!(
        stdout.len() <= 8,
        "output must be capped at the configured 8 bytes, got {}",
        stdout.len()
    );
    assert_eq!(
        out["stdout_truncated"], true,
        "a source past the configured cap is flagged truncated"
    );
}

#[tokio::test]
async fn real_pipeline_passes_data_between_stages() {
    // echo's stdout becomes cat's stdin; cat echoes it back.
    let out = ShellTool::new()
        .invoke(
            serde_json::json!({"cmd": "echo hello | cat"}),
            &ctx(Caveats::top()),
        )
        .await
        .expect("invoke");
    assert_eq!(out["exit_code"], 0);
    assert_eq!(out["stdout"], "hello\n");
}

#[tokio::test]
async fn real_pipeline_exit_code_is_the_last_stage() {
    // `true | false` → last stage (false) exits 1.
    let out = ShellTool::new()
        .invoke(
            serde_json::json!({"cmd": "true | false"}),
            &ctx(Caveats::top()),
        )
        .await
        .expect("invoke");
    assert_eq!(
        out["exit_code"], 1,
        "pipeline exit is the last stage's: {out}"
    );

    // `false | true` → last stage (true) exits 0, even though stage 1 failed.
    let out = ShellTool::new()
        .invoke(
            serde_json::json!({"cmd": "false | true"}),
            &ctx(Caveats::top()),
        )
        .await
        .expect("invoke");
    assert_eq!(out["exit_code"], 0, "no pipefail: {out}");
}

#[tokio::test]
async fn real_stderr_and_nonzero_exit_are_captured() {
    // `cat` of a nonexistent path writes to stderr and exits non-zero.
    let out = ShellTool::new()
        .invoke(
            serde_json::json!({"program": "cat", "args": ["/nonexistent/agent-bridle/path"]}),
            &ctx(Caveats::top()),
        )
        .await
        .expect("invoke");
    assert_ne!(
        out["exit_code"], 0,
        "cat of a missing file must fail: {out}"
    );
    assert!(
        !out["stderr"].as_str().unwrap_or("").is_empty(),
        "stderr must be captured: {out}"
    );
}

#[tokio::test]
async fn real_out_of_scope_program_is_denied_and_never_spawns() {
    // `rm` is not granted: the leash denies it before any real process starts.
    let out = ShellTool::new()
        .invoke(
            serde_json::json!({"program": "rm", "args": ["-rf", "/tmp/agent-bridle-should-not-exist"]}),
            &ctx(exec_only(&["echo"])),
        )
        .await
        .expect("invoke");
    assert_eq!(out["denied"], true);
    assert_eq!(out["denials"][0]["target"], "rm");
    assert!(out.get("exit_code").is_none(), "nothing ran: {out}");
}

#[tokio::test]
async fn real_stdout_redirect_truncates_then_appends_a_file() {
    let path = unique_temp("out");
    let p = shell_path(&path);

    // `> file` writes (truncates).
    let out = ShellTool::new()
        .invoke(
            serde_json::json!({"cmd": format!("echo first > {p}")}),
            &ctx(Caveats::top()),
        )
        .await
        .expect("invoke");
    assert_eq!(out["exit_code"], 0, "truncate redirect should run: {out}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "first\n");

    // `>> file` appends.
    let out = ShellTool::new()
        .invoke(
            serde_json::json!({"cmd": format!("echo second >> {p}")}),
            &ctx(Caveats::top()),
        )
        .await
        .expect("invoke");
    assert_eq!(out["exit_code"], 0, "append redirect should run: {out}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "first\nsecond\n");

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn real_stdin_redirect_feeds_a_file() {
    let path = unique_temp("in");
    std::fs::write(&path, "b\na\nc\n").unwrap();
    let p = shell_path(&path);

    let out = ShellTool::new()
        .invoke(
            serde_json::json!({"cmd": format!("cat < {p}")}),
            &ctx(Caveats::top()),
        )
        .await
        .expect("invoke");
    assert_eq!(out["exit_code"], 0);
    assert_eq!(out["stdout"], "b\na\nc\n");

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn real_pipeline_with_stdout_redirect_on_last_stage() {
    // `echo … | cat > file` — the pipe feeds cat, whose stdout goes to the file;
    // captured stdout is empty because the last stage redirected to a file.
    let path = unique_temp("pipe");
    let p = shell_path(&path);

    let out = ShellTool::new()
        .invoke(
            serde_json::json!({"cmd": format!("echo piped | cat > {p}")}),
            &ctx(Caveats::top()),
        )
        .await
        .expect("invoke");
    assert_eq!(out["exit_code"], 0);
    assert_eq!(
        out["stdout"], "",
        "last-stage redirect means empty captured stdout: {out}"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "piped\n");

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn real_and_chain_runs_then_short_circuits() {
    // `true && echo ran` → echo runs.
    let out = ShellTool::new()
        .invoke(
            serde_json::json!({"cmd": "true && echo ran"}),
            &ctx(Caveats::top()),
        )
        .await
        .expect("invoke");
    assert_eq!(out["stdout"], "ran\n");
    assert_eq!(out["exit_code"], 0);

    // `false && echo nope` → echo is skipped; exit is `false`'s.
    let out = ShellTool::new()
        .invoke(
            serde_json::json!({"cmd": "false && echo nope"}),
            &ctx(Caveats::top()),
        )
        .await
        .expect("invoke");
    assert_eq!(out["stdout"], "", "echo must be skipped: {out}");
    assert_eq!(out["exit_code"], 1);
}

#[tokio::test]
async fn real_or_fallback_and_semicolon_sequence() {
    // `false || echo fallback` → fallback runs.
    let out = ShellTool::new()
        .invoke(
            serde_json::json!({"cmd": "false || echo fallback"}),
            &ctx(Caveats::top()),
        )
        .await
        .expect("invoke");
    assert_eq!(out["stdout"], "fallback\n");

    // `echo a ; echo b` → both run, output concatenated in order.
    let out = ShellTool::new()
        .invoke(
            serde_json::json!({"cmd": "echo a ; echo b"}),
            &ctx(Caveats::top()),
        )
        .await
        .expect("invoke");
    assert_eq!(out["stdout"], "a\nb\n");
}

#[tokio::test]
async fn real_glob_expands_against_the_filesystem() {
    // A unique temp dir with a.rs="A", b.rs="B", c.txt="C".
    let dir = unique_temp("glob");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.rs"), "A").unwrap();
    std::fs::write(dir.join("b.rs"), "B").unwrap();
    std::fs::write(dir.join("c.txt"), "C").unwrap();
    let d = dir.to_string_lossy().into_owned();

    // `cat *.rs` (cwd = the temp dir) → expands to `cat a.rs b.rs` (sorted) → "AB".
    let out = ShellTool::new()
        .invoke(
            serde_json::json!({"cmd": "cat *.rs", "cwd": d}),
            &ctx(Caveats::top()),
        )
        .await
        .expect("invoke");
    assert_eq!(out["exit_code"], 0);
    assert_eq!(out["stdout"], "AB", "glob expanded + sorted: {out}");

    // No match → the literal pattern; `cat zzz*` fails (no such file).
    let out = ShellTool::new()
        .invoke(
            serde_json::json!({"cmd": "cat zzz*", "cwd": d}),
            &ctx(Caveats::top()),
        )
        .await
        .expect("invoke");
    assert_ne!(
        out["exit_code"], 0,
        "unmatched glob → literal, cat fails: {out}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn real_allowlisted_var_expands_from_the_environment() {
    // `echo $HOME` expands HOME from this process's env (the test reads the same
    // env, so the assertion is deterministic regardless of the host value).
    let expected = format!("{}\n", std::env::var("HOME").unwrap_or_default());
    let out = ShellTool::new()
        .invoke(
            serde_json::json!({"cmd": "echo $HOME"}),
            &ctx(Caveats::top()),
        )
        .await
        .expect("invoke");
    assert_eq!(out["exit_code"], 0);
    assert_eq!(
        out["stdout"], expected,
        "$HOME must expand to the env value: {out}"
    );
}

#[tokio::test]
async fn real_env_map_reaches_the_child() {
    // The env seam (newt #783): a `"env"` map on the dispatch is set on the real
    // child. `env` (coreutils) with no args prints its environment; the var we
    // injected must appear — proof it crossed into the spawned process, not just
    // the recording mock. A unique value so a stray host `FOO` can't false-pass.
    let marker = format!("ab-env-seam-{}", std::process::id());
    let out = ShellTool::new()
        .invoke(
            serde_json::json!({
                "program": "env",
                "env": { "AB_ENV_SEAM_PROOF": marker },
            }),
            &ctx(Caveats::top()),
        )
        .await
        .expect("invoke");
    assert_eq!(out["exit_code"], 0, "env must run: {out}");
    let stdout = out["stdout"].as_str().unwrap_or_default();
    assert!(
        stdout.contains(&format!("AB_ENV_SEAM_PROOF={marker}")),
        "the injected env var must reach the child: {out}"
    );
}

#[tokio::test]
async fn real_mixed_and_quoted_variable_words_expand() {
    let home = std::env::var("HOME").unwrap_or_default();

    // Mixed word: `$HOME/sub` → "<home>/sub".
    let out = ShellTool::new()
        .invoke(
            serde_json::json!({"cmd": "echo $HOME/sub"}),
            &ctx(Caveats::top()),
        )
        .await
        .expect("invoke");
    assert_eq!(out["stdout"], format!("{home}/sub\n"), "mixed word: {out}");

    // Inside double quotes the variable still expands.
    let out = ShellTool::new()
        .invoke(
            serde_json::json!({"cmd": "echo \"prefix-$HOME\""}),
            &ctx(Caveats::top()),
        )
        .await
        .expect("invoke");
    assert_eq!(
        out["stdout"],
        format!("prefix-{home}\n"),
        "quoted var: {out}"
    );
}

#[tokio::test]
async fn real_stderr_redirect_to_file() {
    // `cat <missing> 2> err` — cat's error goes to the file; captured stderr is
    // empty (it was redirected), stdout empty, exit non-zero.
    let path = unique_temp("err");
    let p = shell_path(&path);
    let out = ShellTool::new()
        .invoke(
            serde_json::json!({"cmd": format!("cat /nonexistent/agent-bridle 2> {p}")}),
            &ctx(Caveats::top()),
        )
        .await
        .expect("invoke");
    assert_ne!(out["exit_code"], 0);
    assert_eq!(out["stderr"], "", "stderr went to the file: {out}");
    assert!(
        !std::fs::read_to_string(&path).unwrap().is_empty(),
        "the error must be in the file"
    );
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn real_2to1_merges_stderr_into_captured_stdout() {
    // `cat <missing> 2>&1` — the error is merged into stdout (captured), so
    // captured stdout is non-empty and captured stderr is empty.
    let out = ShellTool::new()
        .invoke(
            serde_json::json!({"cmd": "cat /nonexistent/agent-bridle 2>&1"}),
            &ctx(Caveats::top()),
        )
        .await
        .expect("invoke");
    assert_ne!(out["exit_code"], 0);
    assert!(
        !out["stdout"].as_str().unwrap_or("").is_empty(),
        "the error must appear on merged stdout: {out}"
    );
    assert_eq!(out["stderr"], "", "stderr was merged into stdout: {out}");
}

#[tokio::test]
async fn real_2to1_before_a_pipe_feeds_stderr_downstream() {
    // `cat <missing> 2>&1 | cat` — stage 0's stderr is merged into its stdout
    // pipe, so stage 1 (cat) receives and re-emits the error to captured stdout.
    let out = ShellTool::new()
        .invoke(
            serde_json::json!({"cmd": "cat /nonexistent/agent-bridle 2>&1 | cat"}),
            &ctx(Caveats::top()),
        )
        .await
        .expect("invoke");
    assert!(
        !out["stdout"].as_str().unwrap_or("").is_empty(),
        "stderr merged into the pipe must reach the downstream stage: {out}"
    );
}

// ── L3 boundary: Landlock actually confines the spawned child (#35) ──────────
//
// This is the regression proof for ADR 0005's claim that L3 is the *boundary*: a
// permitted program's OWN write (not a bridle-performed redirect) is blocked by
// the kernel when it targets a path outside `fs_write` — something L2 cannot see
// once the child has spawned. Linux + `linux-landlock` only; self-skips if the
// kernel lacks Landlock.
#[cfg(all(target_os = "linux", feature = "linux-landlock"))]
#[tokio::test]
async fn real_landlock_confines_a_spawned_childs_own_write() {
    use agent_bridle_core::landlock_is_supported;

    if !landlock_is_supported() {
        eprintln!("skipping: kernel lacks Landlock");
        return;
    }

    let allowed = unique_temp("ll-allowed");
    std::fs::create_dir_all(&allowed).unwrap();
    let forbidden = unique_temp("ll-forbidden");
    std::fs::create_dir_all(&forbidden).unwrap();

    // exec `touch`, but only allow writes under `allowed` (fs_read stays open so
    // the dynamic loader can map libc — the fs_write axis is what we confine).
    let caveats = Caveats {
        exec: Scope::only(["touch".to_string()]),
        fs_write: Scope::only([allowed.to_string_lossy().into_owned()]),
        ..Caveats::top()
    };

    // `touch` itself opens the file for writing — bridle does NOT open it (it is
    // an argument, not a redirect), so only L3 can stop it.
    let inside = ShellTool::new()
        .invoke(
            serde_json::json!({"cmd": format!("touch {}/ok", allowed.to_string_lossy())}),
            &ctx(caveats.clone()),
        )
        .await
        .expect("invoke");
    assert_eq!(
        inside["exit_code"], 0,
        "write within fs_write must succeed: {inside}"
    );
    assert_eq!(
        inside["sandbox_kind"], "landlock",
        "must report kernel enforcement: {inside}"
    );
    assert!(allowed.join("ok").exists(), "the in-scope file must exist");

    let outside = ShellTool::new()
        .invoke(
            serde_json::json!({"cmd": format!("touch {}/escape", forbidden.to_string_lossy())}),
            &ctx(caveats),
        )
        .await
        .expect("invoke");
    assert_ne!(
        outside["exit_code"], 0,
        "the kernel must deny a write outside fs_write scope: {outside}"
    );
    assert!(
        !forbidden.join("escape").exists(),
        "the out-of-scope file must NOT have been created"
    );

    let _ = std::fs::remove_dir_all(&allowed);
    let _ = std::fs::remove_dir_all(&forbidden);
}

// agent-bridle#35/#57 — the "swamp tools die" spike. The proof above confines a
// child bridle spawned directly (`touch`). This one closes the harder hole ADR
// 0001 names: a GRANDCHILD a *permitted* tool forks on its own (`find -exec …`),
// which never re-enters bridle's spawn funnel — exactly what L2 is blind to once
// `find` runs. Both `find` and `touch` are exec-granted (so the grandchild RUNS —
// the #57 Execute axis would otherwise kernel-deny `touch`'s execve); the point
// here is that the kernel `fs_write` ruleset, inherited across find's fork/exec,
// still stops the grandchild's WRITE outside scope. Linux + `linux-landlock`;
// self-skips without Landlock.
#[cfg(all(target_os = "linux", feature = "linux-landlock"))]
#[tokio::test]
async fn real_landlock_confines_a_find_exec_grandchild_write() {
    use agent_bridle_core::landlock_is_supported;
    if !landlock_is_supported() {
        eprintln!("skipping: kernel lacks Landlock");
        return;
    }

    let allowed = unique_temp("ll-fe-allowed");
    std::fs::create_dir_all(&allowed).unwrap();
    // A file for `find` to walk and fire `-exec` on (its content is irrelevant).
    std::fs::write(allowed.join("seed"), b"x").unwrap();
    let forbidden = unique_temp("ll-fe-forbidden");
    std::fs::create_dir_all(&forbidden).unwrap();

    // `find` and `touch` are exec-permitted (so the grandchild runs); writes are
    // confined to `allowed`. The `;` terminating `-exec` is single-quoted so the
    // safe-subset parser treats it as a literal argument to find, not a separator.
    let caveats = Caveats {
        exec: Scope::only(["find".to_string(), "touch".to_string()]),
        fs_write: Scope::only([allowed.to_string_lossy().into_owned()]),
        ..Caveats::top()
    };

    // In scope: the grandchild writes under `allowed` → allowed.
    let inside = ShellTool::new()
        .invoke(
            serde_json::json!({"cmd": format!(
                "find {a} -type f -exec touch {a}/ok ';'",
                a = allowed.to_string_lossy()
            )}),
            &ctx(caveats.clone()),
        )
        .await
        .expect("invoke");
    assert_eq!(inside["sandbox_kind"], "landlock", "{inside}");
    assert!(
        allowed.join("ok").exists(),
        "an in-scope grandchild write must succeed: {inside}"
    );

    // Out of scope: the grandchild's write to `forbidden` is denied by the
    // inherited ruleset — even though `find` was permitted and bridle never saw
    // the `touch` spawn.
    let _ = ShellTool::new()
        .invoke(
            serde_json::json!({"cmd": format!(
                "find {a} -type f -exec touch {f}/escape ';'",
                a = allowed.to_string_lossy(),
                f = forbidden.to_string_lossy()
            )}),
            &ctx(caveats),
        )
        .await
        .expect("invoke");
    assert!(
        !forbidden.join("escape").exists(),
        "the out-of-scope grandchild write must be denied by the kernel"
    );

    let _ = std::fs::remove_dir_all(&allowed);
    let _ = std::fs::remove_dir_all(&forbidden);
}

// agent-bridle#35 — the read-injection spike (`grep -f FILE`, the `grep -f
// /etc/shadow` shape). bridle admits the command because `grep` is permitted and
// it cannot know `-f` means "read this file as control input" (the gap #58's
// command packs would close at L1). Only the kernel `fs_read` ruleset stops grep
// from reading the out-of-scope file. Linux + `linux-landlock`; self-skips.
#[cfg(all(target_os = "linux", feature = "linux-landlock"))]
#[tokio::test]
async fn real_landlock_confines_a_grep_dash_f_read_injection() {
    use agent_bridle_core::landlock_is_supported;
    if !landlock_is_supported() {
        eprintln!("skipping: kernel lacks Landlock");
        return;
    }

    let allowed = unique_temp("ll-ri-allowed");
    std::fs::create_dir_all(&allowed).unwrap();
    std::fs::write(allowed.join("data"), b"hello\n").unwrap();
    let forbidden = unique_temp("ll-ri-forbidden");
    std::fs::create_dir_all(&forbidden).unwrap();
    // The "secret" grep must not be able to read as its -f pattern file.
    std::fs::write(forbidden.join("secret"), b"hello\n").unwrap();

    // `grep` permitted; reads confined to `allowed` (fs_write stays open).
    let caveats = Caveats {
        exec: Scope::only(["grep".to_string()]),
        fs_read: Scope::only([allowed.to_string_lossy().into_owned()]),
        ..Caveats::top()
    };

    let out = ShellTool::new()
        .invoke(
            serde_json::json!({"cmd": format!(
                "grep -f {f}/secret {a}/data",
                f = forbidden.to_string_lossy(),
                a = allowed.to_string_lossy()
            )}),
            &ctx(caveats),
        )
        .await
        .expect("invoke");
    // grep cannot open the out-of-scope pattern file → it errors (non-zero), and
    // no out-of-scope content reaches stdout.
    assert_ne!(
        out["exit_code"], 0,
        "the kernel must deny grep reading the out-of-scope -f file: {out}"
    );
    assert_eq!(
        out["sandbox_kind"], "landlock",
        "must report kernel enforcement: {out}"
    );
    assert_eq!(out["stdout"], "", "no out-of-scope content may leak: {out}");

    let _ = std::fs::remove_dir_all(&allowed);
    let _ = std::fs::remove_dir_all(&forbidden);
}

// The macOS Seatbelt analog of the Landlock proof above: end to end through the
// shell tool, a *permitted* program's OWN write/read of a path outside the
// fs_write/fs_read scope is blocked by the kernel (`sandbox-exec`), and the
// envelope honestly reports `sandbox_kind: seatbelt`. macOS + `macos-seatbelt`
// only; self-skips if `sandbox-exec` is unavailable.
#[cfg(all(target_os = "macos", feature = "macos-seatbelt"))]
#[tokio::test]
async fn real_seatbelt_confines_a_spawned_childs_own_write() {
    use agent_bridle_core::seatbelt_is_supported;

    if !seatbelt_is_supported() {
        eprintln!("skipping: /usr/bin/sandbox-exec unavailable");
        return;
    }

    let allowed = unique_temp("sb-allowed");
    std::fs::create_dir_all(&allowed).unwrap();
    let forbidden = unique_temp("sb-forbidden");
    std::fs::create_dir_all(&forbidden).unwrap();

    // exec `touch`, but only allow writes under `allowed`. `touch` opens the
    // target itself (an argument, not a bridle redirect), so only L3 can stop it.
    let caveats = Caveats {
        exec: Scope::only(["touch".to_string()]),
        fs_write: Scope::only([allowed.to_string_lossy().into_owned()]),
        ..Caveats::top()
    };

    let inside = ShellTool::new()
        .invoke(
            serde_json::json!({"cmd": format!("touch {}/ok", allowed.to_string_lossy())}),
            &ctx(caveats.clone()),
        )
        .await
        .expect("invoke");
    assert_eq!(
        inside["sandbox_kind"], "seatbelt",
        "must report kernel enforcement: {inside}"
    );
    assert_eq!(
        inside["exit_code"], 0,
        "write within fs_write must succeed: {inside}"
    );
    assert!(allowed.join("ok").exists(), "the in-scope file must exist");

    let outside = ShellTool::new()
        .invoke(
            serde_json::json!({"cmd": format!("touch {}/escape", forbidden.to_string_lossy())}),
            &ctx(caveats),
        )
        .await
        .expect("invoke");
    assert_ne!(
        outside["exit_code"], 0,
        "the kernel must deny a write outside fs_write scope: {outside}"
    );
    assert!(
        !forbidden.join("escape").exists(),
        "the out-of-scope file must NOT have been created"
    );

    let _ = std::fs::remove_dir_all(&allowed);
    let _ = std::fs::remove_dir_all(&forbidden);
}

// Every stage of a pipeline is wrapped in its own `sandbox-exec`; the wrapper is
// transparent to the OS pipes, so data still flows stage→stage. Restricting
// `fs_write` engages the Seatbelt wrapper even though neither stage writes.
#[cfg(all(target_os = "macos", feature = "macos-seatbelt"))]
#[tokio::test]
async fn real_seatbelt_wrapped_pipeline_pipes_data_between_stages() {
    use agent_bridle_core::seatbelt_is_supported;

    if !seatbelt_is_supported() {
        eprintln!("skipping: /usr/bin/sandbox-exec unavailable");
        return;
    }
    let scope = unique_temp("sb-pipe");
    std::fs::create_dir_all(&scope).unwrap();
    let caveats = Caveats {
        exec: Scope::only(["echo".to_string(), "cat".to_string()]),
        fs_write: Scope::only([scope.to_string_lossy().into_owned()]),
        ..Caveats::top()
    };
    let out = ShellTool::new()
        .invoke(
            serde_json::json!({"cmd": "echo wrapped | cat"}),
            &ctx(caveats),
        )
        .await
        .expect("invoke");
    assert_eq!(out["sandbox_kind"], "seatbelt", "{out}");
    assert_eq!(out["exit_code"], 0, "{out}");
    assert_eq!(
        out["stdout"], "wrapped\n",
        "data must flow through both wrapped stages: {out}"
    );
    let _ = std::fs::remove_dir_all(&scope);
}

// Read confinement end to end: a permitted program (`cat`) can read in-scope but
// the kernel denies its read of a file *outside* `fs_read` — the `grep -f
// /etc/shadow`-style exfil, blocked at L3 where L2 cannot see the child's open.
#[cfg(all(target_os = "macos", feature = "macos-seatbelt"))]
#[tokio::test]
async fn real_seatbelt_confines_a_spawned_childs_own_read() {
    use agent_bridle_core::seatbelt_is_supported;

    if !seatbelt_is_supported() {
        eprintln!("skipping: /usr/bin/sandbox-exec unavailable");
        return;
    }

    let allowed = unique_temp("sb-r-allowed");
    std::fs::create_dir_all(&allowed).unwrap();
    let forbidden = unique_temp("sb-r-forbidden");
    std::fs::create_dir_all(&forbidden).unwrap();
    std::fs::write(allowed.join("ok.txt"), b"in-scope\n").unwrap();
    std::fs::write(forbidden.join("secret.txt"), b"top-secret\n").unwrap();

    // Confine reads to `allowed`; `cat`'s own open of an out-of-scope file is what
    // L3 must stop (the path is an argument, not a bridle-checked redirect).
    let caveats = Caveats {
        exec: Scope::only(["cat".to_string()]),
        fs_read: Scope::only([allowed.to_string_lossy().into_owned()]),
        ..Caveats::top()
    };

    let inside = ShellTool::new()
        .invoke(
            serde_json::json!({"cmd": format!("cat {}/ok.txt", allowed.to_string_lossy())}),
            &ctx(caveats.clone()),
        )
        .await
        .expect("invoke");
    assert_eq!(inside["sandbox_kind"], "seatbelt", "{inside}");
    assert_eq!(
        inside["exit_code"], 0,
        "in-scope read must succeed (binary loads + reads): {inside}"
    );
    assert!(
        inside["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("in-scope"),
        "must read the in-scope file's contents: {inside}"
    );

    let outside = ShellTool::new()
        .invoke(
            serde_json::json!({"cmd": format!("cat {}/secret.txt", forbidden.to_string_lossy())}),
            &ctx(caveats),
        )
        .await
        .expect("invoke");
    assert_ne!(
        outside["exit_code"], 0,
        "the kernel must deny reading a file outside fs_read scope: {outside}"
    );
    assert!(
        !outside["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("top-secret"),
        "out-of-scope file contents must NOT leak: {outside}"
    );

    let _ = std::fs::remove_dir_all(&allowed);
    let _ = std::fs::remove_dir_all(&forbidden);
}

// Restricted Seatbelt net is unsupported until ambient Mach/XPC deputies are
// faithfully bounded. End to end, CONFINED must refuse before any child starts;
// direct socket-rule mechanism evidence lives below admission in core.
#[cfg(all(target_os = "macos", feature = "macos-seatbelt"))]
#[tokio::test]
async fn real_seatbelt_restricted_net_refuses_before_spawn() {
    use agent_bridle_core::seatbelt_is_supported;

    assert!(
        seatbelt_is_supported(),
        "macOS Seatbelt evidence requires /usr/bin/sandbox-exec"
    );

    let marker = unique_temp("sb-net-refused");
    let _ = std::fs::remove_file(&marker);
    // `touch` would create the marker if admission accidentally spawned it.
    let caveats = Caveats {
        exec: Scope::only(["touch".to_string()]),
        net: Scope::none(),
        ..Caveats::top()
    };
    let out = ShellTool::new()
        .invoke(
            serde_json::json!({ "cmd": format!("touch {}", marker.display()) }),
            &ctx(caveats),
        )
        .await
        .expect("invoke");

    assert_eq!(
        out["denied"], true,
        "restricted Seatbelt net must fail closed before spawn: {out}"
    );
    assert_eq!(
        out["denials"][0]["kind"], "net",
        "the refusal must identify the net axis: {out}"
    );
    assert_eq!(out["enforcement"]["net"], "advisory", "{out}");
    assert!(
        !marker.exists(),
        "the child must not spawn after the net-floor refusal: {out}"
    );
}

// #1220 regression: a write-confined child must still be able to OPEN the
// device sinks — git opens `/dev/null` O_RDWR as plumbing and died with
// "could not open '/dev/null' for reading and writing: Permission denied"
// before the fix (the sinks were absent from the Landlock write ruleset;
// #969 had only fixed the L2 interceptor). `dd of=/dev/null` performs the
// same fresh write-open inside the jail. RED before the fix, GREEN after.
// Linux + `linux-landlock` only; self-skips if the kernel lacks Landlock.
#[cfg(all(target_os = "linux", feature = "linux-landlock"))]
#[tokio::test]
async fn real_landlock_write_confined_child_can_open_dev_null() {
    use agent_bridle_core::landlock_is_supported;

    if !landlock_is_supported() {
        eprintln!("skipping: kernel lacks Landlock");
        return;
    }

    let allowed = unique_temp("ll-devnull-allowed");
    std::fs::create_dir_all(&allowed).unwrap();

    // fs_write confined to `allowed` — /dev/null is NOT in the granted scope;
    // only the #1220 device-sink fold makes its open succeed.
    let caveats = Caveats {
        exec: Scope::only(["dd".to_string()]),
        fs_write: Scope::only([allowed.to_string_lossy().into_owned()]),
        ..Caveats::top()
    };

    let inside = ShellTool::new()
        .invoke(
            serde_json::json!({"cmd": "dd if=/dev/zero of=/dev/null count=1"}),
            &ctx(caveats),
        )
        .await
        .expect("invoke");
    assert_eq!(
        inside["exit_code"], 0,
        "a fresh write-open of /dev/null must succeed inside the jail: {inside}"
    );
    assert_eq!(
        inside["sandbox_kind"], "landlock",
        "must report kernel enforcement: {inside}"
    );

    let _ = std::fs::remove_dir_all(&allowed);
}

/// #385/#386: forward-port of `66960bb`'s real acceptance test, onto main's
/// seatbelt-fixture style (`agent_bridle_core::seatbelt_is_supported`, asserted
/// rather than env-gated, matching the other macOS fixtures in this file).
/// Grounds the mocked private-host forwarding seam in a real confined child: an
/// explicit private approval must not add ordinary host authority. The reserved
/// `.invalid` destination is refused before DNS or any upstream dial.
#[cfg(all(target_os = "macos", feature = "macos-seatbelt"))]
#[tokio::test]
async fn real_private_hosts_do_not_widen_the_fenced_host_scope() {
    use agent_bridle_core::seatbelt_is_supported;

    assert!(
        seatbelt_is_supported(),
        "macOS Seatbelt evidence requires /usr/bin/sandbox-exec"
    );
    assert!(
        std::path::Path::new("/usr/bin/curl").exists(),
        "fixture requires /usr/bin/curl"
    );

    let caveats = Caveats {
        exec: Scope::only(["curl".to_string()]),
        net: Scope::only(["allowed.invalid".to_string()]),
        ..Caveats::top()
    };
    let out = ShellTool::new()
        .with_private_hosts(["refused.invalid".to_string()])
        .expect("exact private approval")
        .invoke(
            serde_json::json!({
                "program": "/usr/bin/curl",
                "args": ["--silent", "--show-error", "--noproxy", "", "--max-time", "5",
                    "--output", "/dev/null", "--write-out", "%{http_code}", "http://refused.invalid/"],
                "timeout_secs": 10
            }),
            &ctx(caveats),
        )
        .await
        .expect("confined proxy invocation");
    // 0.8 posture: every restricted Seatbelt `net` scope resolves Advisory and
    // admission refuses it before spawn (L3 BOUND; see the core README), so the
    // 0.7 test's proxied 403 never happens here. The property still holds, more
    // strongly: an exact private approval adds no authority, and nothing runs.
    assert_eq!(out["denied"], true, "{out}");
    assert_eq!(out["denials"][0]["kind"], "net", "{out}");
    assert!(
        out["denials"][0]["reason"]
            .as_str()
            .is_some_and(|r| r.contains("L3 BOUND")),
        "private approval cannot widen a refused Seatbelt net scope: {out}"
    );
    assert!(
        out["stdout"].as_str().unwrap_or("").is_empty(),
        "curl never ran: {out}"
    );
}
