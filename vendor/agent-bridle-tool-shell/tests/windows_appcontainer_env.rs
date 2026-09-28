//! #323 §1A/B — ambient-env isolation through the **REAL** confined path on Windows:
//! `ShellTool → OsSpawner → agent-bridle-aclaunch.exe → AppContainer child`.
//!
//! Unlike `windows_env_isolation.rs` (which exercises the OsSpawner env-clear on the
//! unwrapped path), this test forces the AppContainer wrap to engage (a restricted
//! `fs_write` axis) so the child runs INSIDE a real AppContainer created by the built
//! launcher, and proves the #323 contract end-to-end:
//!   - **Negative (§1A):** a provider-shaped secret that exists ONLY in the parent
//!     environment (`OPENAI_API_KEY`) is NOT observable by the confined child.
//!   - **Positive (§1B):** an explicitly delegated var (`AB323_GRANTED=allowed`) IS
//!     observed by the child with exactly that value. (`SystemRoot` is the platform
//!     baseline, NOT caller-delegated authority, so it is not used as the proof.)
//!   - **Non-vacuity (§1C):** the delegated value appearing proves the confined child
//!     actually executed — a launcher failure / skip cannot satisfy it.
//!
//! Strict gate (§8): if the built launcher (hence a real AppContainer) is unavailable,
//! this FAILS when `BRIDLE_REQUIRE_APPCONTAINER` is set (as CI does), and only skips
//! on a casual local run — a security-critical proof never silently passes.
#![cfg(all(windows, feature = "shell", feature = "windows-appcontainer"))]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use agent_bridle_core::{Caveats, Gate, SandboxPolicy, Scope, Tool, ToolContext};
use agent_bridle_tool_shell::ShellTool;

fn ctx(granted: Caveats) -> ToolContext {
    Gate::new(0)
        .authorize(&ShellTool::new(), &granted)
        .expect("authorize")
}

fn unique_temp(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "ab-acenv-{}-{}-{}",
        tag,
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ))
}

/// The built launcher, next to the test binary's target profile dir
/// (`target/<profile>/agent-bridle-aclaunch.exe`), present when the workspace is
/// built. Returns `None` if not found.
fn locate_launcher() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?; // .../target/<profile>/deps/<test>.exe
    let profile = exe.parent()?.parent()?; // .../target/<profile>
    let cand = profile.join("agent-bridle-aclaunch.exe");
    cand.exists().then_some(cand)
}

/// Whether `BRIDLE_REQUIRE_APPCONTAINER` demands a real AppContainer (CI sets it).
fn appcontainer_required() -> bool {
    std::env::var("BRIDLE_REQUIRE_APPCONTAINER")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

/// Probe that this host can actually create an AppContainer via the located launcher.
fn appcontainer_works(launcher: &std::path::Path) -> bool {
    std::process::Command::new(launcher)
        .args(["--name", "ab-acenv-probe", "cmd.exe", "/c", "exit 0"])
        .current_dir("C:\\Windows")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[tokio::test]
async fn shelltool_appcontainer_env_isolation_real_path() {
    // Strict gate: locate the built launcher + confirm a real AppContainer works.
    let launcher = match locate_launcher() {
        Some(p) if appcontainer_works(&p) => p,
        _ if appcontainer_required() => panic!(
            "BRIDLE_REQUIRE_APPCONTAINER is set but no working AppContainer launcher was found next \
             to the test binary — build the workspace first; the real ShellTool→aclaunch→AppContainer \
             env proof cannot be verified"
        ),
        _ => {
            eprintln!(
                "skipping: agent-bridle-aclaunch.exe / a working AppContainer not available here \
                 (build the workspace; set BRIDLE_REQUIRE_APPCONTAINER=1 to require it, as CI does)"
            );
            return;
        }
    };

    let tool = ShellTool::new().with_sandbox_policy(SandboxPolicy {
        appcontainer_launcher_path: Some(launcher.to_string_lossy().into_owned()),
        ..SandboxPolicy::default()
    });

    // A provider-shaped secret that lives ONLY in the parent environment and is NOT
    // delegated — it must not cross the confinement boundary.
    std::env::set_var("OPENAI_API_KEY", "parent-only-secret-must-not-leak");

    // A restricted `fs_write` axis ENGAGES the AppContainer wrap (so the child runs
    // inside a real AppContainer via aclaunch); `cmd /c set` prints the child's env.
    // The delegated env var rides the `env` seam. cwd is `C:\Windows` — readable by
    // every AppContainer (ALL_APPLICATION_PACKAGES), so the child can start.
    let workspace = unique_temp("ws");
    std::fs::create_dir_all(&workspace).expect("create workspace dir");
    let caveats = Caveats {
        fs_write: Scope::only([workspace.to_string_lossy().into_owned()]),
        ..Caveats::top()
    };
    // A generation-2 descendant (`cmd` → `cmd`) prints the env: this also proves
    // (§6) that env isolation holds for a real second-generation process running
    // inside the AppContainer boundary, not just the direct child.
    let out = tool
        .invoke(
            serde_json::json!({
                "program": "cmd",
                "args": ["/c", "cmd", "/c", "set"],
                "cwd": "C:\\Windows",
                "env": { "AB323_GRANTED": "allowed" },
            }),
            &ctx(caveats),
        )
        .await
        .expect("invoke");
    std::env::remove_var("OPENAI_API_KEY");
    let _ = std::fs::remove_dir_all(&workspace);

    let stdout = out["stdout"].as_str().unwrap_or_default();
    // §1B/§1C — the explicitly delegated var reaches the child with its exact value
    // (this ALSO proves the confined child actually executed: non-vacuity).
    assert!(
        stdout.contains("AB323_GRANTED=allowed"),
        "the delegated env var must reach the confined AppContainer child (and prove it ran):\n{stdout}"
    );
    // §1A — the parent-only provider secret must NOT cross into the child.
    assert!(
        !stdout.contains("parent-only-secret-must-not-leak") && !stdout.contains("OPENAI_API_KEY"),
        "a parent-only provider secret must not reach the confined AppContainer child:\n{stdout}"
    );
}
