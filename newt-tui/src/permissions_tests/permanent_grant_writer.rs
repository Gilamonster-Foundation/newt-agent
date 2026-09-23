//! #2535/#2524 PR1 — the smallest first slice of "permanently allow" writing
//! a signed durable grant for EVERY kind, not just net. `AllowPermanent` now
//! routes exec/fs/net through [`newt_core::ocap_store::persist_approve`]
//! instead of `continue`-ing past the durable store (exec/fs) or writing
//! `[tui.permissions] net` (net).
//!
//! RED-FIRST evidence (confirmed against origin/main before this PR's fix):
//! every test below either failed to compile (`ApproveEntry`/`persist_approve`
//! did not exist) or failed its assertion — `exec`/`fs` fell straight to
//! `continue` (`store_unchanged` held, but `session grants only` never
//! reached the store on purpose), the relative-path and delegated-session
//! guards did not exist, and the high-danger refusal ran ONLY for net.

use super::*;
use newt_core::caveats::{Caveats, CountBound, Scope};
use newt_core::ocap_store::Verdict;
use newt_core::{CaveatsExt as _, DenialKind, PermissionGate as _, PermissionRequest};

fn open_caveats() -> Caveats {
    Caveats {
        fs_read: Scope::All,
        fs_write: Scope::All,
        exec: Scope::All,
        net: Scope::All,
        max_calls: CountBound::Unlimited,
        valid_for_generation: Scope::All,
    }
}

/// A fresh `(config dir, config.toml path, root key path, root key)` fixture
/// — never `~/.newt`.
fn fixture() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    std::path::PathBuf,
    agent_mesh_protocol::UserKey,
) {
    let dir = tempfile::TempDir::new().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        "# my config\n[tui.permissions]\npreset = \"full_access\"\n",
    )
    .unwrap();
    let key_path = dir.path().join("identity.pem");
    let root = agent_mesh_protocol::UserKey::generate();
    root.save(&key_path).unwrap();
    (dir, config, key_path, root)
}

fn gate<'a>(
    state: &'a mut PermissionPromptState,
    config: &std::path::Path,
    key_path: Option<std::path::PathBuf>,
    delegation: Option<&'a newt_identity::VerifiedDelegation>,
    choice: PromptChoice,
) -> PromptPermissionGate<'a, impl FnMut(&PromptWindow, &SurfaceInteraction) -> PromptChoice> {
    gate_with_danger(
        state,
        config,
        key_path,
        delegation,
        choice,
        danger::DangerTable::builtin(),
    )
}

fn gate_with_danger<'a>(
    state: &'a mut PermissionPromptState,
    config: &std::path::Path,
    key_path: Option<std::path::PathBuf>,
    delegation: Option<&'a newt_identity::VerifiedDelegation>,
    choice: PromptChoice,
    danger: danger::DangerTable,
) -> PromptPermissionGate<'a, impl FnMut(&PromptWindow, &SurfaceInteraction) -> PromptChoice> {
    let mut script = vec![choice].into_iter();
    PromptPermissionGate {
        state,
        base: open_caveats(),
        key_path,
        conversation_id: "conv-2535".to_string(),
        log_path: None,
        denials_path: None,
        config_path: Some(config.to_path_buf()),
        preset_clamp: None,
        delegation,
        danger,
        color: false,
        verbose: false,
        authorization_prompts_enabled: true,
        web_decision_timeout: Duration::from_secs(2),
        cancel: None,
        exit: None,
        ask_surface: None,
        #[cfg(feature = "rich-tui")]
        open_panel: None,
        ask_human: move |_w: &PromptWindow, _d: &SurfaceInteraction| {
            script.next().expect("script exhausted")
        },
    }
}

fn req(kind: DenialKind, target: &str) -> PermissionRequest {
    PermissionRequest {
        tool: "run_command".to_string(),
        kind,
        target: target.to_string(),
        reason: "not within the granted authority".to_string(),
    }
}

fn approve_path(config: &std::path::Path) -> std::path::PathBuf {
    config.with_file_name("ocap").join("approve.toml")
}

/// `(Exec, "cargo")` — signed into `approve.toml`, the session
/// immediately permits it, and one journal-shaped `scope = "permanent"`
/// record is kept.
#[test]
fn exec_allow_permanent_writes_a_signed_approve_entry_and_folds_the_session() {
    let (_dir, config, key_path, root) = fixture();
    let mut state = PermissionPromptState::default();
    let request = req(DenialKind::Exec, "cargo");
    match gate(
        &mut state,
        &config,
        Some(key_path),
        None,
        PromptChoice::AllowPermanent,
    )
    .ask(std::slice::from_ref(&request))
    {
        newt_core::PermissionDecision::Allow(c) => assert!(c.permits_exec("cargo")),
        newt_core::PermissionDecision::Deny => panic!("must be granted"),
    }
    assert_eq!(state.decisions.last().unwrap().scope, "permanent");
    let text = std::fs::read_to_string(approve_path(&config)).unwrap();
    assert!(text.contains("target = \"cargo\""), "{text}");
    let file = newt_core::ocap_store::PolicyFile::parse(&text).unwrap();
    let entry = file.exec.iter().find(|e| e.target == "cargo").unwrap();
    assert!(entry.sig.is_some(), "must be signed");
    let (set, warnings) =
        newt_core::ocap_store::load_store(&config, Some(root.public().as_bytes()));
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(
        newt_core::ocap_store::evaluate_request(&set, DenialKind::Exec, "cargo"),
        Some(Verdict::Approve)
    );
}

/// `(FsWrite, "$HOME")` — a broad root is refused; nothing is written.
#[test]
fn fs_write_of_a_broad_root_is_refused_and_store_is_untouched() {
    let (_dir, config, key_path, _root) = fixture();
    let home = std::env::var("HOME").unwrap();
    let mut state = PermissionPromptState::default();
    let request = req(DenialKind::FsWrite, &home);
    match gate_with_danger(
        &mut state,
        &config,
        Some(key_path),
        None,
        PromptChoice::AllowPermanent,
        danger::DangerTable::builtin().with_fs_root(std::path::PathBuf::from(&home)),
    )
    .ask(std::slice::from_ref(&request))
    {
        newt_core::PermissionDecision::Deny => {}
        newt_core::PermissionDecision::Allow(_) => panic!("a broad root must be refused"),
    }
    assert!(!approve_path(&config).exists(), "nothing should be written");
}

/// `(Exec, "./x")` — a relative path is refused; nothing is written.
#[test]
fn exec_relative_path_is_refused_and_store_is_untouched() {
    let (_dir, config, key_path, _root) = fixture();
    let mut state = PermissionPromptState::default();
    let request = req(DenialKind::Exec, "./x");
    match gate(
        &mut state,
        &config,
        Some(key_path),
        None,
        PromptChoice::AllowPermanent,
    )
    .ask(std::slice::from_ref(&request))
    {
        newt_core::PermissionDecision::Deny => {}
        newt_core::PermissionDecision::Allow(_) => panic!("a relative path must be refused"),
    }
    assert!(!approve_path(&config).exists(), "nothing should be written");
}

/// A delegated session never establishes its own root: the grant stays
/// session-only, the store is untouched, and the operator is told why.
#[test]
fn delegated_session_never_persists_stays_session_only() {
    let (_dir, config, key_path, _root) = fixture();
    let ceiling = open_caveats();
    let delegation = crate::caveat_policy_tests::verified_delegation(ceiling);
    let mut state = PermissionPromptState::default();
    let request = req(DenialKind::Net, "docs.rs");
    match gate(
        &mut state,
        &config,
        Some(key_path),
        Some(&delegation),
        PromptChoice::AllowPermanent,
    )
    .ask(std::slice::from_ref(&request))
    {
        newt_core::PermissionDecision::Allow(c) => assert!(c.permits_net("docs.rs")),
        newt_core::PermissionDecision::Deny => panic!("must still be granted this session"),
    }
    assert_eq!(state.decisions.last().unwrap().scope, "session");
    assert!(
        state
            .session_grants
            .contains(&(DenialKind::Net, "docs.rs".to_string())),
        "session-scoped grant must still land"
    );
    assert!(
        !approve_path(&config).exists(),
        "a delegated session must never persist"
    );
}

/// `(Net, "docs.rs")` — written to `approve.toml`, never to
/// `[tui.permissions] net` (P-1: that config write path is retired).
#[test]
fn net_allow_permanent_writes_approve_toml_not_config_net() {
    let (_dir, config, key_path, root) = fixture();
    let mut state = PermissionPromptState::default();
    let request = req(DenialKind::Net, "docs.rs");
    match gate(
        &mut state,
        &config,
        Some(key_path),
        None,
        PromptChoice::AllowPermanent,
    )
    .ask(std::slice::from_ref(&request))
    {
        newt_core::PermissionDecision::Allow(c) => assert!(c.permits_net("docs.rs")),
        newt_core::PermissionDecision::Deny => panic!("must be granted"),
    }
    let config_text = std::fs::read_to_string(&config).unwrap();
    assert!(
        !config_text.contains("docs.rs"),
        "must not land in config.toml: {config_text}"
    );
    let (set, warnings) =
        newt_core::ocap_store::load_store(&config, Some(root.public().as_bytes()));
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(
        newt_core::ocap_store::evaluate_request(&set, DenialKind::Net, "docs.rs"),
        Some(Verdict::Approve)
    );
}

/// A high-danger exec target is refused for ALL kinds now, not just net —
/// the real bug fix: today the danger check ran only inside the net arm.
#[test]
fn high_danger_exec_target_is_refused_before_any_write_attempt() {
    let (_dir, config, key_path, _root) = fixture();
    let mut state = PermissionPromptState::default();
    let request = req(DenialKind::Exec, "bash");
    match gate(
        &mut state,
        &config,
        Some(key_path),
        None,
        PromptChoice::AllowPermanent,
    )
    .ask(std::slice::from_ref(&request))
    {
        newt_core::PermissionDecision::Deny => {}
        newt_core::PermissionDecision::Allow(_) => panic!("an interpreter must be refused"),
    }
    assert_eq!(
        state.decisions.last().unwrap().scope,
        "permanent-allow-refused-high-danger"
    );
    assert!(!approve_path(&config).exists(), "nothing should be written");
}
