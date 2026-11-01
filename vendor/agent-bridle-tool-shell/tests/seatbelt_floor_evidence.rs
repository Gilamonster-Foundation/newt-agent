//! macOS Seatbelt **CONFINED-floor** evidence (agent-bridle#317 /
//! newt-agent#1632). Restricted Seatbelt net scopes are currently Advisory:
//! direct socket rules do not bound every ambient Mach/XPC deputy. A Newt-shaped
//! grant must therefore fail closed before spawn rather than claim that its
//! `net: Kernel` floor was witnessed.
//!
//! The Newt `CONFINED` contract remains `{fs_read Kernel, fs_write Kernel, net
//! Kernel, exec Interceptor}`. Filesystem and exec behavior are tested separately;
//! this suite pins the consequential net refusal and proves the requested child
//! side effect never occurs.
#![cfg(all(target_os = "macos", feature = "macos-seatbelt", feature = "shell"))]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use agent_bridle_core::{seatbelt_is_supported, Caveats, Gate, Scope, Tool, ToolContext};
use agent_bridle_tool_shell::ShellTool;

fn ctx(granted: Caveats) -> ToolContext {
    Gate::new(0)
        .authorize(&ShellTool::new(), &granted)
        .expect("authorize")
}

fn unique_temp(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "ab-floor-{}-{}-{}",
        tag,
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ))
}

/// agent-bridle#416 round 3 (fix-first review): round 2's rename to
/// `..._admits_restricted_net` relied on the SAME L3 gap this round's fix
/// closes — `SeatbeltSandbox::resolved_authority` ignored stdio, so ANY
/// net:none spawn (audited or not) resolved a named `Bounded(∅)` and admitted
/// regardless of floor. `ShellTool` never declares the audited stdio shape
/// (raw `std::process::Stdio`, never `ConfinedCommand`), so admission
/// correctly reverts to refusing on the net axis — NOTE: despite its
/// name/doc, `ctx()` here mints under `Gate::new(0)` (`EnforcementFloor::
/// DEFAULT`), not the `CONFINED` preset named above; that pre-existing
/// mismatch is unrelated to #405/#416 and left as-is, and is moot here since
/// L3 (not the floor) is what refuses.
#[tokio::test]
async fn real_seatbelt_confined_floor_refuses_restricted_net_before_spawn() {
    assert!(
        seatbelt_is_supported(),
        "macOS Seatbelt evidence requires /usr/bin/sandbox-exec"
    );
    let ws = unique_temp("ws");
    std::fs::create_dir_all(&ws).unwrap();

    // A Newt-shaped grant. `touch` is a NON-launcher program, so its Seatbelt
    // process-exec* closure is exact (Kernel), unlike a `sh` grant (Interceptor,
    // model B). fs_read/fs_write are scoped to the workspace; net is deny-all.
    let ws_s = ws.to_string_lossy().into_owned();
    let confined = Caveats {
        fs_read: Scope::only([ws_s.clone()]),
        fs_write: Scope::only([ws_s.clone()]),
        net: Scope::none(),
        exec: Scope::only(["touch".to_string()]),
        ..Caveats::top()
    };

    let marker = ws.join("now-exists");
    let out = ShellTool::new()
        .invoke(
            serde_json::json!({ "cmd": format!("touch {}", marker.display()) }),
            &ctx(confined),
        )
        .await
        .expect("invoke");
    assert_eq!(
        out["denied"], true,
        "a net:none spawn through ShellTool's unaudited stdio must fail closed: {out}"
    );
    assert_eq!(out["enforcement"]["fs_write"], "kernel", "{out}");
    assert_eq!(out["enforcement"]["net"], "advisory", "{out}");
    assert_eq!(out["denials"][0]["kind"], "net", "{out}");
    assert!(
        !marker.exists(),
        "the marker proves ShellTool must not have spawned: {out}"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

/// Scope-fidelity at the floor (the #317 review, witnessed): a grant whose exec
/// names `sh` measures `exec = interceptor`, NOT `kernel` — Apple's `/bin/sh`
/// pulls `/bin/bash` into the process-exec* closure, so the fence permits a
/// program the Caveat did not name. The provenance `EnforcementEvidence` for such
/// a grant must record `interceptor` (a truthful degradation), never an
/// over-claimed Kernel. Net is unrestricted here so this remains an exec/fs test
/// rather than being preempted by the independent restricted-net refusal.
#[tokio::test]
async fn real_seatbelt_exec_floor_downgrades_to_interceptor_for_sh() {
    assert!(
        seatbelt_is_supported(),
        "macOS Seatbelt evidence requires /usr/bin/sandbox-exec"
    );
    let ws = unique_temp("ws-sh");
    std::fs::create_dir_all(&ws).unwrap();
    let ws_s = ws.to_string_lossy().into_owned();
    let grant = Caveats {
        fs_write: Scope::only([ws_s.clone()]),
        exec: Scope::only(["sh".to_string()]),
        ..Caveats::top()
    };
    let out = ShellTool::new()
        .invoke(
            serde_json::json!({ "cmd": format!("touch {ws_s}/ok") }),
            &ctx(grant),
        )
        .await
        .expect("invoke");
    assert_eq!(out["sandbox_kind"], "seatbelt", "{out}");
    assert_eq!(
        out["enforcement"]["exec"], "interceptor",
        "a granted sh widens the closure (/bin/bash) → exec witnessed interceptor, not kernel: {out}"
    );
    assert_eq!(out["enforcement"]["fs_write"], "kernel", "{out}");
    assert!(
        out["enforcement"]["net"].is_null(),
        "an unrestricted net axis has no witness entry: {out}"
    );
    let _ = std::fs::remove_dir_all(&ws);
}
