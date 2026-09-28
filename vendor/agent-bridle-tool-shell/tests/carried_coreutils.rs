//! Integration keystone for carried coreutils (Track 2 Gate 2 / issue #206).
//!
//! Runs this harnessless, same-image test executable with the **environment
//! scrubbed** (`env_clear` plus a guaranteed-dead outer `PATH`, and an empty
//! Brush `PATH`), asking its embedded Brush engine to run coreutils. These
//! succeed only if the carried uutils dispatch via authenticated re-exec of
//! this same image. No installable full-authority test helper is shipped in a
//! production package.
#![cfg(feature = "carried-coreutils")]
#![cfg_attr(
    not(any(target_os = "linux", target_os = "macos")),
    allow(dead_code, unused_imports)
)]

use agent_bridle_core::{Caveats, Gate, Tool};
use agent_bridle_tool_shell::{maybe_dispatch, BrushShellTool};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

const FIXTURE_FLAG: &str = "--agent-bridle-test-fixture";

fn selected(name: &str) -> bool {
    let filters: Vec<String> = std::env::args()
        .skip(1)
        .filter(|arg| !arg.starts_with('-'))
        .collect();
    filters.is_empty() || filters.iter().any(|filter| name.contains(filter))
}

fn run_case(name: &str, case: impl FnOnce()) {
    if selected(name) {
        eprintln!("test {name} ...");
        case();
        eprintln!("test {name} ... ok");
    }
}

fn run_fixture(mut args: impl Iterator<Item = String>) -> i32 {
    let cmd = args
        .next()
        .expect("fixture requires a Brush command argument");
    let cwd = args.next();
    let tool = BrushShellTool::new();
    let cx = Gate::new(0)
        .authorize(&tool, &Caveats::top())
        .expect("authorize fixture");
    let mut invocation = serde_json::json!({
        "cmd": cmd,
        "env": { "PATH": "" },
    });
    if let Some(cwd) = cwd {
        invocation["cwd"] = serde_json::Value::String(cwd);
    }
    let runtime = tokio::runtime::Runtime::new().expect("fixture runtime");
    let out = runtime
        .block_on(tool.invoke(invocation, &cx))
        .expect("invoke fixture");
    if let Some(stdout) = out.get("stdout").and_then(serde_json::Value::as_str) {
        print!("{stdout}");
    }
    if let Some(stderr) = out.get("stderr").and_then(serde_json::Value::as_str) {
        eprint!("{stderr}");
    }
    out.get("exit_code")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(1) as i32
}

fn main() {
    // Private worker/carried dispatch must precede every fixture/test branch.
    if let Some(code) = maybe_dispatch() {
        std::process::exit(code);
    }
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() == Some(FIXTURE_FLAG) {
        std::process::exit(run_fixture(args));
    }
    run_platform();
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_platform() {
    run_case(
        "carried_ls_runs_via_dispatch_reexec_with_env_scrubbed",
        carried_ls_runs_via_dispatch_reexec_with_env_scrubbed,
    );
    run_case(
        "carried_cat_runs_via_dispatch_reexec_with_env_scrubbed",
        carried_cat_runs_via_dispatch_reexec_with_env_scrubbed,
    );
    run_case(
        "carried_wc_counts_each_file_with_env_scrubbed",
        carried_wc_counts_each_file_with_env_scrubbed,
    );
    run_case(
        "carried_wc_sort_head_pipeline_runs_with_env_scrubbed",
        carried_wc_sort_head_pipeline_runs_with_env_scrubbed,
    );
    run_case(
        "direct_bundled_dispatch_without_live_parent_is_refused",
        direct_bundled_dispatch_without_live_parent_is_refused,
    );
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn run_platform() {
    let tool = BrushShellTool::new();
    let cx = Gate::new(0)
        .authorize(&tool, &Caveats::top())
        .expect("authorize fail-closed probe");
    let runtime = tokio::runtime::Runtime::new().expect("test runtime");
    let error = runtime
        .block_on(tool.invoke(serde_json::json!({ "cmd": "echo MUST-NOT-RUN" }), &cx))
        .expect_err("unsupported private control must fail closed");
    assert!(
        error.to_string().contains("private-control transport"),
        "unsupported target must explain the fail-closed boundary: {error}"
    );
}

/// Shell-quote a path for splicing into a brush command string. brush's
/// parser is POSIX-style: an unquoted `\` is an escape character. Windows
/// paths are backslash-separated, so without this an unquoted path like
/// `C:\Users\...\hello.txt` gets silently mangled to `C:Users...hello.txt`
/// (issue #209 W4 finding) — `\U`, `\A`, etc. get collapsed to the escaped
/// letter. Single-quoting is a no-op on Unix (paths there never contain `'`
/// in these tests) and makes the Windows path safe. Mirrors
/// `agent-bridle-jaild::vm::shell_quote`.
fn shell_quote(p: &Path) -> String {
    format!("'{}'", p.to_string_lossy().replace('\'', "'\\''"))
}

fn unique_temp(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "ab-carried-{}-{}-{}",
        tag,
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ))
}

/// A same-image fixture command with **host tools removed from PATH**. The
/// outer process gets a unique nonexistent `PATH`, and the fixture explicitly
/// clears Brush's shell `PATH`, so only carried shims can satisfy a bare
/// coreutil. On Windows a fully empty environment breaks process startup
/// (`SystemRoot`, …), so we keep only those non-secret, required variables.
fn scrubbed() -> Command {
    let mut c = Command::new(std::env::current_exe().expect("current test executable"));
    c.arg(FIXTURE_FLAG);
    c.env_clear();
    c.env("PATH", unique_temp("empty-path"));
    #[cfg(windows)]
    for key in [
        "SystemRoot",
        "SystemDrive",
        "windir",
        "TEMP",
        "TMP",
        "USERPROFILE",
        "NUMBER_OF_PROCESSORS",
    ] {
        if let Ok(v) = std::env::var(key) {
            c.env(key, v);
        }
    }
    c
}

/// Carried `ls` lists a directory with the environment fully scrubbed — no host
/// `/bin/ls`, and no usable `PATH`. It resolves to the bundled uutils `ls` via
/// authenticated same-image re-exec.
fn carried_ls_runs_via_dispatch_reexec_with_env_scrubbed() {
    let dir = unique_temp("ls");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("MARKER.txt"), b"x").unwrap();

    let out = scrubbed()
        .arg(format!("ls {}", shell_quote(&dir)))
        .output()
        .expect("run same-image fixture");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "carried ls exited nonzero: stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        stdout.contains("MARKER.txt"),
        "carried ls must list the dir with NO host tools: stdout={stdout:?} stderr={stderr:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Carried `cat` reads a file with the environment fully scrubbed.
fn carried_cat_runs_via_dispatch_reexec_with_env_scrubbed() {
    let dir = unique_temp("cat");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("hello.txt");
    std::fs::write(&file, b"carried-cat-ok\n").unwrap();

    let out = scrubbed()
        .arg(format!("cat {}", shell_quote(&file)))
        .output()
        .expect("run same-image fixture");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stdout.contains("carried-cat-ok"),
        "carried cat must read the file with NO host tools: stdout={stdout:?} stderr={stderr:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Carried `wc` reports each file and the aggregate with no usable host
/// `PATH`. This preserves the focused multi-file counting regression while the
/// pipeline test below proves composition with the other carried utilities.
fn carried_wc_counts_each_file_with_env_scrubbed() {
    let dir = unique_temp("wc");
    std::fs::create_dir_all(&dir).unwrap();
    let first = dir.join("first.txt");
    let second = dir.join("second.txt");
    std::fs::write(&first, b"one\ntwo\n").unwrap();
    std::fs::write(&second, b"three\nfour\nfive\n").unwrap();

    let out = scrubbed()
        .arg(format!(
            "wc -l {} {}",
            shell_quote(&first),
            shell_quote(&second)
        ))
        .output()
        .expect("run same-image fixture");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "carried wc exited nonzero: stdout={stdout:?} stderr={stderr:?}"
    );
    let counts: Vec<&str> = stdout
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    assert_eq!(
        counts,
        ["2", "3", "5"],
        "carried wc must count both files and their total with NO host tools: \
         stdout={stdout:?} stderr={stderr:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Carried `wc`, `sort`, and `head` compose as a real Brush pipeline with the
/// environment fully scrubbed. Input comes from fixture files, not a host or
/// carried `printf`, so every executable in the command is one of the three
/// coreutils this test is proving.
fn carried_wc_sort_head_pipeline_runs_with_env_scrubbed() {
    let dir = unique_temp("wc-sort-head");
    std::fs::create_dir_all(&dir).unwrap();
    let short = dir.join("short.txt");
    let long = dir.join("long.txt");
    std::fs::write(&short, b"one\ntwo\n").unwrap();
    std::fs::write(&long, b"one\ntwo\nthree\nfour\nfive\n").unwrap();

    let out = scrubbed()
        .arg(format!(
            "wc -l {} {} | sort -nr | head -1",
            shell_quote(&short),
            shell_quote(&long)
        ))
        .output()
        .expect("run same-image fixture");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "carried wc/sort/head pipeline exited nonzero: stdout={stdout:?} stderr={stderr:?}"
    );
    assert_eq!(
        stdout.trim(),
        "7 total",
        "wc must count both fixtures, sort must put the total first, and head must keep one row"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The embedding binary's private dispatch flag is not itself authority. A
/// direct caller has no live worker on the other ends of the authentication
/// pipes, so the utility must not run even when the caller names a real bundled
/// command and supplies its input path.
fn direct_bundled_dispatch_without_live_parent_is_refused() {
    let dir = unique_temp("direct-dispatch");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("secret.txt");
    std::fs::write(&file, b"MUST-NOT-BE-READ\n").unwrap();

    let mut direct = Command::new(std::env::current_exe().expect("current test executable"));
    direct.env_clear();
    let out = direct
        .arg("--invoke-bundled")
        .arg("cat")
        .arg("--agent-bridle-live-parent-v1")
        .arg(&file)
        .output()
        .expect("run direct private dispatch");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "direct dispatch must fail: {stderr}");
    assert!(
        !stdout.contains("MUST-NOT-BE-READ"),
        "direct dispatch must not call the bundled utility: {stdout:?}"
    );
    assert!(
        stderr.contains("private parent authentication failed"),
        "refusal must identify the missing trusted parent: {stderr:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
