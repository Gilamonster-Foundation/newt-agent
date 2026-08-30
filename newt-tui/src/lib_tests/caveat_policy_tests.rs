use super::{policy_for, SessionCapability};
// The `permits_*` adaptors live on `CaveatsExt` (post-#95).
use newt_core::CaveatsExt;

/// RAII guard neutralizing the ambient `--full-access` / `NEWT_FULL_ACCESS`
/// override for the duration of a test, restoring it on drop. These tests
/// assert the DEFAULT (unconfigured / read-only) policy; if the test binary
/// is launched under `--full-access` (which exports `NEWT_FULL_ACCESS=1`),
/// `policy_for` would otherwise short-circuit to `Caveats::top()` and every
/// read-only assertion here would fail through no fault of the code. This
/// makes the preset assumption explicit and hermetic.
struct ForceDefaultPreset {
    saved: Option<String>,
}

impl ForceDefaultPreset {
    fn new() -> Self {
        let saved = std::env::var("NEWT_FULL_ACCESS").ok();
        std::env::remove_var("NEWT_FULL_ACCESS");
        Self { saved }
    }
}

impl Drop for ForceDefaultPreset {
    fn drop(&mut self) {
        match self.saved.take() {
            Some(v) => std::env::set_var("NEWT_FULL_ACCESS", v),
            None => std::env::remove_var("NEWT_FULL_ACCESS"),
        }
    }
}

fn tui_with(preset: newt_core::PermissionPreset) -> newt_core::TuiConfig {
    newt_core::TuiConfig {
        permissions: newt_core::ToolPermissions {
            preset,
            extra_exec: Vec::new(),
            net: Vec::new(),
            prompt: false,
        },
        ..Default::default()
    }
}

#[test]
fn absent_config_is_read_only() {
    // Serialize against env-mutating tests: policy_for reads NEWT_EXEC_PATHS
    // / NEWT_VENV via scan_cli_exec_grants. We also neutralize an ambient
    // NEWT_FULL_ACCESS, which needs the exclusive (write) guard.
    let _env = crate::test_env_guard::env_write_guard();
    let _preset = ForceDefaultPreset::new();
    // #86 regression: with no [tui] config the policy must be READ-ONLY,
    // never `Caveats::top()` (the old fallback granted full access).
    let policy = policy_for(None, "/ws");
    assert_ne!(policy, newt_core::caveats::Caveats::top());
    assert!(!policy.permits_exec("cargo"), "no exec when unconfigured");
    assert!(
        !policy.permits_fs_write("/ws/x"),
        "no write when unconfigured"
    );
    // Reads are now LOCKED to the workspace (the operator wants the agent
    // confined to the CWD). The workspace root is readable; paths outside it
    // are not. (Files *under* the root, e.g. /ws/x, are reached at runtime
    // via the TUI's prefix match in `tui_permits_path`; the core method here
    // is exact-set, matching how fs_write has always stored the root.)
    assert!(policy.permits_fs_read("/ws"), "the workspace is readable");
    assert!(
        !policy.permits_fs_read("/etc/passwd"),
        "reads are locked to the workspace by default"
    );
}

#[serial_test::serial(real_fs)]
#[test]
fn establish_unconfigured_is_signed_read_only() {
    // Serialize against env-mutating tests: policy_for reads NEWT_EXEC_PATHS
    // / NEWT_VENV via scan_cli_exec_grants.
    let _env = crate::test_env_guard::env_write_guard();
    let _preset = ForceDefaultPreset::new();
    // #86 end-to-end: no config + a real (temp) key → read-only caveats via
    // the signed-capability path; the per-user key was generated.
    let dir = tempfile::TempDir::new().unwrap();
    let key = dir.path().join("identity.pem");
    let cap = SessionCapability::establish(None, Some(&key), "/ws");
    assert_ne!(*cap.caveats(), newt_core::caveats::Caveats::top());
    assert!(!cap.caveats().permits_exec("cargo"));
    // Reads locked to the workspace (see absent_config_is_read_only).
    assert!(cap.caveats().permits_fs_read("/ws"));
    assert!(!cap.caveats().permits_fs_read("/etc/passwd"));
    assert!(key.exists(), "the per-user identity key was generated");
}

#[test]
fn establish_without_key_is_read_only_policy() {
    // Serialize against env-mutating tests: policy_for reads NEWT_EXEC_PATHS
    // / NEWT_VENV via scan_cli_exec_grants.
    let _env = crate::test_env_guard::env_write_guard();
    let _preset = ForceDefaultPreset::new();
    let cap = SessionCapability::establish(None, None, "/ws");
    assert_ne!(*cap.caveats(), newt_core::caveats::Caveats::top());
    assert!(!cap.caveats().permits_exec("cargo"));
}

/// Issue #93: a subprocess plugin spawned from the TUI must inherit
/// an `AgentKey` whose cert chain walks back to the operator's
/// `UserKey` from `~/.newt/identity.pem`. This pins the chain-
/// rooting property end to end through `SessionCapability`'s
/// envelope-mint chokepoint.
#[serial_test::serial(real_fs)]
#[test]
fn plugin_envelope_chain_roots_at_operator_userkey() {
    // Serialize against env-mutating tests: policy_for reads NEWT_EXEC_PATHS
    // / NEWT_VENV via scan_cli_exec_grants.
    let _env = crate::test_env_guard::env_read_guard();
    use base64::Engine;
    let dir = tempfile::TempDir::new().unwrap();
    let key_path = dir.path().join("identity.pem");
    let cap = SessionCapability::establish(
        Some(tui_with(newt_core::PermissionPreset::WorkspaceDev)),
        Some(&key_path),
        "/ws",
    );

    // Re-load the user key to get its fingerprint for the chain walk.
    let user = newt_identity::load_or_generate(&key_path).unwrap();
    let user_fp = user.fingerprint();

    // Plugin runs read-only — strictly narrower than WorkspaceDev.
    let plugin_caveats = newt_core::Caveats {
        fs_write: newt_core::Scope::none(),
        exec: newt_core::Scope::none(),
        ..cap.caveats().clone()
    };
    let envelope = cap
        .plugin_envelope_for("tui-spawned-plugin", plugin_caveats)
        .expect("operating key present → envelope path is available")
        .expect("attenuating delegation must succeed");

    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&envelope)
        .unwrap();
    let leaf: agent_mesh_protocol::CertChain = serde_json::from_slice(&bytes).unwrap();
    leaf.verify().expect("plugin cert chain must verify");
    assert_eq!(
        leaf.user_fingerprint(),
        user_fp,
        "TUI-side plugin envelope must root at the operator's UserKey, \
             not a synthetic key"
    );
}

#[test]
fn plugin_envelope_unavailable_without_operating_key() {
    // Serialize against env-mutating tests: policy_for reads NEWT_EXEC_PATHS
    // / NEWT_VENV via scan_cli_exec_grants.
    let _env = crate::test_env_guard::env_read_guard();
    // When the per-user key isn't on disk (None path), the TUI
    // degrades to a caveats-only floor. The plugin-spawn chokepoint
    // returns None — the caller must NOT manufacture an AgentKey
    // (issue #93). No synthetic-key fallback exists.
    let cap = SessionCapability::establish(None, None, "/ws");
    assert!(
        cap.plugin_envelope_for("tui-plugin", newt_core::Caveats::top())
            .is_none(),
        "no operating key → no envelope minted (issue #93: no synthetic fallback)"
    );
}

#[serial_test::serial(real_fs)]
#[test]
fn establish_configured_is_workspace_dev() {
    // Serialize against env-mutating tests: policy_for reads NEWT_EXEC_PATHS
    // / NEWT_VENV via scan_cli_exec_grants.
    let _env = crate::test_env_guard::env_write_guard();
    let _preset = ForceDefaultPreset::new();
    let dir = tempfile::TempDir::new().unwrap();
    let cap = SessionCapability::establish(
        Some(newt_core::TuiConfig::default()),
        Some(&dir.path().join("identity.pem")),
        "/ws",
    );
    assert!(cap.caveats().permits_exec("cargo"), "workspace-dev tools");
    assert!(!cap.caveats().permits_exec("rm"), "dangerous cmds denied");
}

#[serial_test::serial(real_fs)]
#[test]
fn reapply_narrows_but_cannot_widen() {
    // Serialize against env-mutating tests: policy_for reads NEWT_EXEC_PATHS
    // / NEWT_VENV via scan_cli_exec_grants.
    let _env = crate::test_env_guard::env_write_guard();
    let _preset = ForceDefaultPreset::new();
    // The headline runtime property: within a session, a config reload can tighten
    // authority but never loosen it (keyed off a temp identity).
    let dir = tempfile::TempDir::new().unwrap();
    let mut cap = SessionCapability::establish(
        Some(tui_with(newt_core::PermissionPreset::WorkspaceDev)),
        Some(&dir.path().join("identity.pem")),
        "/ws",
    );
    assert!(
        cap.caveats().permits_exec("cargo"),
        "starts at workspace-dev"
    );

    // Narrow to read-only: accepted, not clamped.
    let clamped = cap.reapply(Some(tui_with(newt_core::PermissionPreset::ReadOnly)), "/ws");
    assert!(!clamped, "narrowing is not a clamp");
    assert!(!cap.caveats().permits_exec("cargo"), "now read-only");

    // Try to widen back to workspace-dev: clamped, stays read-only.
    let clamped = cap.reapply(
        Some(tui_with(newt_core::PermissionPreset::WorkspaceDev)),
        "/ws",
    );
    assert!(clamped, "a widening request must be reported as clamped");
    assert!(
        !cap.caveats().permits_exec("cargo"),
        "authority must not widen within a session"
    );
}

#[serial_test::serial(real_fs)]
#[test]
fn reapply_without_key_still_narrows() {
    // Serialize against env-mutating tests: policy_for reads NEWT_EXEC_PATHS
    // / NEWT_VENV via scan_cli_exec_grants.
    let _env = crate::test_env_guard::env_write_guard();
    let _preset = ForceDefaultPreset::new();
    let mut cap = SessionCapability::establish(
        Some(tui_with(newt_core::PermissionPreset::WorkspaceDev)),
        None,
        "/ws",
    );
    assert!(cap.caveats().permits_exec("cargo"));
    let clamped = cap.reapply(Some(tui_with(newt_core::PermissionPreset::ReadOnly)), "/ws");
    assert!(!clamped);
    assert!(!cap.caveats().permits_exec("cargo"));
}
