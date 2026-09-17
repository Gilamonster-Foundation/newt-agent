use super::*;
// The `permits_*` adaptors live on `CaveatsExt` (post-#95 the
// upstream `agent-mesh-protocol::Caveats` ships algebra only).
use crate::caveats::CaveatsExt;

// Permission presets, caveat lowering, and persistent network grants.

// --- ToolPermissions / to_caveats ---

#[test]
fn terminal_prompt_default_roundtrips_without_granting_authority() {
    let baseline = ToolPermissions::default();
    for action in ["allow_once", "deny"] {
        let configured: ToolPermissions =
            toml::from_str(&format!("prompt_default = {action:?}")).unwrap();
        let encoded = toml::Value::try_from(&configured).unwrap();
        assert_eq!(
            encoded.get("prompt_default").and_then(toml::Value::as_str),
            Some(action)
        );
        assert_eq!(configured.to_caveats("/ws"), baseline.to_caveats("/ws"));
        assert_eq!(
            toml::from_str::<ToolPermissions>(&toml::to_string(&configured).unwrap()).unwrap(),
            configured
        );
    }
}

#[test]
fn terminal_prompt_default_rejects_standing_grants_denials_and_controls() {
    for action in [
        "allow_session",
        "allow_permanent",
        "deny_always",
        "deny_permanent",
        "back",
        "exit",
        "unknown",
    ] {
        assert!(
            toml::from_str::<ToolPermissions>(&format!("prompt_default = {action:?}")).is_err(),
            "invalid terminal default {action}"
        );
    }
}

#[test]
fn mcp_net_prompt_default_is_typed_and_roundtrips() {
    use crate::PermissionAction;
    let default: ToolPermissions = toml::from_str("").unwrap();
    assert_eq!(default.mcp_net_prompt_default, PermissionAction::AllowOnce);
    assert_eq!(default, ToolPermissions::default());
    for action in [
        PermissionAction::AllowOnce,
        PermissionAction::AllowSession,
        PermissionAction::AllowPermanent,
        PermissionAction::Deny,
        PermissionAction::DenyAlways,
        PermissionAction::DenyPermanent,
    ] {
        let configured: ToolPermissions =
            toml::from_str(&format!("mcp_net_prompt_default = {:?}", action.as_str())).unwrap();
        assert_eq!(configured.mcp_net_prompt_default, action);
        assert_eq!(
            toml::from_str::<ToolPermissions>(&toml::to_string(&configured).unwrap()).unwrap(),
            configured
        );
        assert_eq!(
            configured.to_caveats("/ws"),
            default.to_caveats("/ws"),
            "a prompt default grants nothing before an answer"
        );
    }
}

#[test]
fn mcp_net_prompt_default_rejects_controls_and_unknown_actions() {
    for action in ["back", "exit", "allow_everything", "A"] {
        assert!(
            toml::from_str::<ToolPermissions>(&format!("mcp_net_prompt_default = {action:?}"))
                .is_err(),
            "invalid configured default {action}"
        );
    }
}

#[test]
fn workspace_dev_allows_cargo_and_just() {
    let perms = ToolPermissions::default(); // WorkspaceDev
    let cav = perms.to_caveats("/workspace");
    assert!(cav.permits_exec("cargo"), "cargo must be allowed");
    assert!(cav.permits_exec("just"), "just must be allowed");
    assert!(cav.permits_exec("git"), "git must be allowed");
}

#[test]
fn workspace_dev_blocks_rm_and_mv() {
    let perms = ToolPermissions::default();
    let cav = perms.to_caveats("/workspace");
    assert!(!cav.permits_exec("rm"), "rm must be blocked");
    assert!(!cav.permits_exec("mv"), "mv must be blocked");
    assert!(!cav.permits_exec("sudo"), "sudo must be blocked");
}

#[test]
fn workspace_dev_allows_common_dev_tools() {
    // Regression: these were denied under the default preset even though
    // they're the same risk tier as cargo/git (issue #149). `gh` in
    // particular is authenticated outside but was blocked in-agent.
    let cav = ToolPermissions::default().to_caveats("/workspace");
    for tool in [
        "gh", "python", "python3", "pip", "npm", "node", "make", "jq", "curl", "awk", "sed", "cut",
        "xargs", "which", "env",
    ] {
        assert!(cav.permits_exec(tool), "`{tool}` must be allowed");
    }
    // Adding tools must NOT escalate to full access — destructive commands
    // outside the allowlist stay blocked.
    assert!(!cav.permits_exec("rm"), "rm must still be blocked");
    assert!(!cav.permits_exec("sudo"), "sudo must still be blocked");
}

#[test]
fn workspace_dev_allows_extra_exec() {
    let perms = ToolPermissions {
        preset: PermissionPreset::WorkspaceDev,
        extra_exec: vec!["bacon".into(), "make".into()],
        net: vec![],
        prompt: false,
        ..Default::default()
    };
    let cav = perms.to_caveats("/workspace");
    assert!(cav.permits_exec("bacon"));
    assert!(cav.permits_exec("make"));
    assert!(!cav.permits_exec("rm")); // extra_exec does not weaken the block
}

#[test]
fn read_only_blocks_writes_and_exec() {
    let perms = ToolPermissions {
        preset: PermissionPreset::ReadOnly,
        extra_exec: vec![],
        net: vec![],
        prompt: false,
        ..Default::default()
    };
    let cav = perms.to_caveats("/workspace");
    assert!(!cav.permits_fs_write("/workspace/src/main.rs"));
    assert!(!cav.permits_exec("cargo"));
    assert!(cav.permits_fs_read("/workspace/src/main.rs"));
}

#[test]
fn workspace_edit_allows_write_blocks_exec() {
    let perms = ToolPermissions {
        preset: PermissionPreset::WorkspaceEdit,
        extra_exec: vec![],
        net: vec![],
        prompt: false,
        ..Default::default()
    };
    let cav = perms.to_caveats("/workspace");
    assert!(!cav.permits_exec("cargo"));
    // The caveat stores workspace root; prefix matching is in the TUI layer.
    // Here we just verify the lattice is set up correctly (not All, not none).
    use crate::caveats::Scope;
    assert!(matches!(cav.fs_write, Scope::Only(_)));
}

// --- #1292: the shared MCP probe leash (doctor + `newt mcp probe`) ---

#[test]
fn mcp_probe_caveats_default_is_read_only_never_top() {
    let cav = Config::default().mcp_probe_caveats(std::path::Path::new("/workspace"));
    assert!(cav.permits_fs_read("/workspace/src/main.rs"));
    assert!(
        !cav.permits_fs_write("/workspace/src/main.rs"),
        "unconfigured probe leash must not write"
    );
    assert!(
        !cav.permits_exec("cargo"),
        "unconfigured probe leash grants no exec (the spawn path widens \
             exactly the probed command, nothing else)"
    );
}

#[test]
fn mcp_probe_caveats_honors_the_configured_preset() {
    let cfg = Config {
        tui: Some(TuiConfig {
            permissions: ToolPermissions::default(), // WorkspaceDev
            ..Default::default()
        }),
        ..Default::default()
    };
    let cav = cfg.mcp_probe_caveats(std::path::Path::new("/ws"));
    assert!(cav.permits_exec("cargo"), "configured preset respected");
    use crate::caveats::Scope;
    assert!(matches!(cav.fs_write, Scope::Only(_)));
}

#[test]
fn full_access_is_top() {
    let perms = ToolPermissions {
        preset: PermissionPreset::FullAccess,
        extra_exec: vec![],
        net: vec![],
        prompt: false,
        ..Default::default()
    };
    let cav = perms.to_caveats("/workspace");
    assert_eq!(cav, crate::caveats::Caveats::top());
}

#[test]
fn net_allowlist_controls_the_net_axis() {
    use crate::caveats::Scope;

    // Default (empty `net`) => no network: web_fetch is denied.
    let none = ToolPermissions::default().to_caveats("/ws");
    assert!(
        matches!(none.net, Scope::Only(ref s) if s.is_empty()),
        "empty net config must yield an empty (deny-all) net scope"
    );

    // Explicit host allowlist — works under ANY preset (here ReadOnly), so
    // web access does not require granting writes/exec.
    let hosts = ToolPermissions {
        preset: PermissionPreset::ReadOnly,
        extra_exec: vec![],
        net: vec!["docs.rs".into(), "github.com".into()],
        prompt: false,
        ..Default::default()
    }
    .to_caveats("/ws");
    assert!(
        matches!(hosts.net, Scope::Only(ref s) if s.contains("docs.rs") && s.contains("github.com")),
        "explicit hosts must populate the net allowlist"
    );

    // A single "*" grants all hosts (still SSRF-screened by the web tool).
    let all = ToolPermissions {
        preset: PermissionPreset::WorkspaceDev,
        extra_exec: vec![],
        net: vec!["*".into()],
        prompt: false,
        ..Default::default()
    }
    .to_caveats("/ws");
    assert!(
        matches!(all.net, Scope::All),
        "a `*` entry must grant the whole net axis"
    );
}

#[test]
fn custom_is_workspace_dev_not_top() {
    // Regression: editing the exec allowlist auto-flips the preset to
    // `Custom`, which used to map to `Caveats::top()` — a silent escalation
    // from "add one command" to "full access". `Custom` must now carry
    // WorkspaceDev authority plus the extra commands, never `top()`.
    let custom = ToolPermissions {
        preset: PermissionPreset::Custom,
        extra_exec: vec!["bacon".into()],
        net: vec![],
        prompt: false,
        ..Default::default()
    }
    .to_caveats("/workspace");
    assert_ne!(
        custom,
        crate::caveats::Caveats::top(),
        "Custom must not be full access"
    );
    assert!(custom.permits_exec("cargo"), "workspace-dev tools allowed");
    assert!(custom.permits_exec("bacon"), "extra_exec command allowed");
    assert!(!custom.permits_exec("rm"), "non-allowlisted command denied");
    // Identical to WorkspaceDev with the same extras.
    let workspace_dev = ToolPermissions {
        preset: PermissionPreset::WorkspaceDev,
        extra_exec: vec!["bacon".into()],
        net: vec![],
        prompt: false,
        ..Default::default()
    }
    .to_caveats("/workspace");
    assert_eq!(
        custom, workspace_dev,
        "Custom carries WorkspaceDev authority + extras"
    );
}

#[test]
fn preset_toggle_cycles() {
    assert_eq!(
        PermissionPreset::ReadOnly.toggle(),
        PermissionPreset::WorkspaceEdit
    );
    assert_eq!(
        PermissionPreset::WorkspaceEdit.toggle(),
        PermissionPreset::WorkspaceDev
    );
    assert_eq!(
        PermissionPreset::WorkspaceDev.toggle(),
        PermissionPreset::FullAccess
    );
    assert_eq!(
        PermissionPreset::FullAccess.toggle(),
        PermissionPreset::ReadOnly
    );
}

#[test]
fn tool_permissions_toml_roundtrip() {
    let perms = ToolPermissions {
        preset: PermissionPreset::WorkspaceDev,
        extra_exec: vec!["bacon".into()],
        net: vec![],
        prompt: false,
        ..Default::default()
    };
    let toml = toml::to_string(&perms).unwrap();
    assert!(toml.contains("workspace_dev"));
    assert!(toml.contains("bacon"));
    let back: ToolPermissions = toml::from_str(&toml).unwrap();
    assert_eq!(back, perms);
}

// ---- #904: comment-preserving "allow permanently" net writer ----

#[test]
fn with_net_host_creates_table_from_empty_and_scope_includes_host() {
    let out = Config::with_net_host("", "github.com").unwrap();
    // The written TOML parses back and its net scope now permits the host.
    let cfg: Config = toml::from_str(&out).unwrap();
    let perms = cfg.tui.unwrap().permissions;
    assert!(perms.net.contains(&"github.com".to_string()));
    assert!(
        matches!(perms.net_scope(), crate::caveats::Scope::Only(ref s) if s.contains("github.com")),
        "net_scope must permit the granted host"
    );
}

#[test]
fn with_net_host_preserves_comments_and_other_keys() {
    let original = "\
# my hand-authored config — keep this comment
[tui.permissions]
preset = \"workspace_dev\"  # inline comment
net = [\"already.example.com\"]
";
    let out = Config::with_net_host(original, "github.com").unwrap();
    // Comments survive (the whole point vs Config::save).
    assert!(
        out.contains("# my hand-authored config"),
        "top comment lost: {out}"
    );
    assert!(
        out.contains("# inline comment"),
        "inline comment lost: {out}"
    );
    // The pre-existing host is kept and the new one appended.
    assert!(out.contains("already.example.com"));
    assert!(out.contains("github.com"));
    // preset key untouched.
    assert!(out.contains("workspace_dev"));
}

#[test]
fn with_net_host_is_idempotent_no_duplicate() {
    let once = Config::with_net_host("", "github.com").unwrap();
    let twice = Config::with_net_host(&once, "github.com").unwrap();
    assert_eq!(
        twice.matches("github.com").count(),
        1,
        "duplicated host: {twice}"
    );
}

#[test]
fn with_net_host_rejects_invalid_toml() {
    assert!(Config::with_net_host("this = = not toml", "github.com").is_err());
}

/// Grounds the comment-preserving transformer in the same real-file writer
/// used for permanent network approvals; changing the default grants nothing.
#[test]
fn set_permission_prompt_default_preserves_config_and_comments() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let original = "# operator config\n[tui.permissions]\npreset = \"read_only\"\nnet = [\"docs.example\"]\nprompt_default = \"deny\" # deliberate choice\n[unmodeled]\nretain = true\n";
    std::fs::write(&path, original).unwrap();
    let before: Config = toml::from_str(original).unwrap();
    Config::set_permission_prompt_default(&path, crate::PermissionAction::AllowOnce).unwrap();
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(saved.contains("# operator config"));
    assert!(saved.contains("# deliberate choice"));
    assert!(saved.contains("[unmodeled]"));
    let after: Config = toml::from_str(&saved).unwrap();
    let before = before.tui.unwrap().permissions;
    let after = after.tui.unwrap().permissions;
    assert_eq!(
        after.prompt_default,
        Some(crate::PermissionAction::AllowOnce)
    );
    assert_eq!(after.to_caveats("/ws"), before.to_caveats("/ws"));
    Config::set_permission_prompt_default(&path, crate::PermissionAction::Deny).unwrap();
    let saved: Config = toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(
        saved.tui.unwrap().permissions.prompt_default,
        Some(crate::PermissionAction::Deny)
    );
}

#[test]
fn set_permission_prompt_default_rejects_invalid_actions_without_writing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing/config.toml");
    for action in [
        crate::PermissionAction::AllowSession,
        crate::PermissionAction::AllowPermanent,
        crate::PermissionAction::DenyAlways,
        crate::PermissionAction::DenyPermanent,
        crate::PermissionAction::Back,
        crate::PermissionAction::Exit,
    ] {
        assert!(Config::set_permission_prompt_default(&path, action).is_err());
        assert!(!path.parent().unwrap().exists());
    }
    std::fs::create_dir(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "broken = = toml").unwrap();
    assert!(
        Config::set_permission_prompt_default(&path, crate::PermissionAction::AllowOnce).is_err()
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), "broken = = toml");
}

#[test]
fn set_permission_prompt_default_and_net_updates_share_the_locked_writer() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::thread::scope(|scope| {
        let path = &path;
        scope.spawn(move || {
            Config::set_permission_prompt_default(path, crate::PermissionAction::Deny).unwrap();
        });
        scope.spawn(move || Config::append_permission_net_host(path, "docs.example").unwrap());
    });
    let saved: Config = toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let permissions = saved.tui.unwrap().permissions;
    assert_eq!(
        permissions.prompt_default,
        Some(crate::PermissionAction::Deny)
    );
    assert_eq!(permissions.net, vec!["docs.example"]);
}
