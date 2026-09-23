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
    // #2524 round 2 item 6: a broad root from the environment (`$HOME`)
    // panics on a Windows runner with no `HOME` set. The fixture's own
    // tempdir is an equally broad root for this test's purpose, and reading
    // it needs no environment at all.
    let (dir, config, key_path, _root) = fixture();
    let broad_root = dir.path().to_path_buf();
    let mut state = PermissionPromptState::default();
    let request = req(DenialKind::FsWrite, &broad_root.to_string_lossy());
    match gate_with_danger(
        &mut state,
        &config,
        Some(key_path),
        None,
        PromptChoice::AllowPermanent,
        danger::DangerTable::builtin().with_fs_root(broad_root),
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

/// #2524 round 2 item 1, BLOCKER — RED FIRST: a pre-existing UNSIGNED
/// `approve.toml` entry (`curl`, never vouched for by the operator's root
/// key) must stay inert after a completely unrelated `AllowPermanent` of
/// `cargo` through the same arm. Before the fix, the arm folded the
/// `PolicyFile` `persist_approve` had just written — which still carried the
/// unsigned `curl` entry verbatim, never checked by anything on that path —
/// straight into the live `ocap_policy`, laundering it into authority on
/// the strength of one unrelated interactive answer.
#[test]
fn unrelated_permanent_allow_does_not_launder_a_preexisting_unsigned_approve_entry() {
    let (_dir, config, key_path, _root) = fixture();
    let ocap_dir = config.with_file_name("ocap");
    std::fs::create_dir_all(&ocap_dir).unwrap();
    std::fs::write(
        ocap_dir.join("approve.toml"),
        "[[exec]]\ntarget = \"curl\"\n",
    )
    .unwrap();

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
        newt_core::PermissionDecision::Deny => panic!("cargo must be granted"),
    }
    // The freshly-signed entry this turn actually answered must be live —
    // folded straight into THIS session's durable grants.
    assert!(
        state
            .durable_grants
            .contains(&(DenialKind::Exec, "cargo".to_string())),
        "the entry this turn actually signed must be folded in"
    );
    // The pre-existing UNSIGNED entry must never become live authority —
    // neither folded into this session nor still verifiable on disk.
    assert!(
        !state
            .durable_grants
            .contains(&(DenialKind::Exec, "curl".to_string())),
        "an unsigned pre-existing entry must never launder into authority"
    );
    let (set, _warnings) =
        newt_core::ocap_store::load_store(&config, Some(_root.public().as_bytes()));
    assert_eq!(
        newt_core::ocap_store::evaluate_request(&set, DenialKind::Exec, "curl"),
        None,
        "an unsigned entry must not verify even after the file is re-read"
    );
}

/// Same regression, for a `curl` entry carrying a SYNTACTICALLY present but
/// BOGUS `sig` (tampered, not merely missing) — the other shape `verify_at_load`
/// must also drop.
#[test]
fn unrelated_permanent_allow_does_not_launder_a_bogus_signature_approve_entry() {
    let (_dir, config, key_path, _root) = fixture();
    let ocap_dir = config.with_file_name("ocap");
    std::fs::create_dir_all(&ocap_dir).unwrap();
    std::fs::write(
        ocap_dir.join("approve.toml"),
        "[[exec]]\ntarget = \"curl\"\nsig = \"00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899aabbccdd\"\n",
    )
    .unwrap();

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
        newt_core::PermissionDecision::Deny => panic!("cargo must be granted"),
    }
    assert!(
        state
            .durable_grants
            .contains(&(DenialKind::Exec, "cargo".to_string())),
        "the entry this turn actually signed must be folded in"
    );
    assert!(
        !state
            .durable_grants
            .contains(&(DenialKind::Exec, "curl".to_string())),
        "a bogus-signature pre-existing entry must never launder into authority"
    );
}

/// #2524 round 2 item 3 — three distinct fallback causes must land on three
/// distinct records (and hence three distinct operator-facing notices), not
/// one shared message. This branch: no trusted config path.
#[test]
fn permanent_allow_with_no_config_path_gets_its_own_distinct_fallback() {
    let (_dir, config, key_path, _root) = fixture();
    let mut state = PermissionPromptState::default();
    let request = req(DenialKind::Exec, "cargo");
    let mut script = vec![PromptChoice::AllowPermanent].into_iter();
    match (PromptPermissionGate {
        state: &mut state,
        base: open_caveats(),
        key_path: Some(key_path),
        conversation_id: "conv-2524".to_string(),
        log_path: None,
        denials_path: None,
        config_path: None, // no trusted config path this session
        preset_clamp: None,
        delegation: None,
        danger: danger::DangerTable::builtin(),
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
    })
    .ask(std::slice::from_ref(&request))
    {
        newt_core::PermissionDecision::Allow(c) => assert!(c.permits_exec("cargo")),
        newt_core::PermissionDecision::Deny => panic!("must still be granted this session"),
    }
    assert_eq!(state.decisions.last().unwrap().scope, "session-no-config");
    let _ = config; // fixture's config file is unused by this branch on purpose
}

/// Same as above; this branch: no root key resolves at `key_path`.
#[test]
fn permanent_allow_with_no_root_key_gets_its_own_distinct_fallback() {
    let (_dir, config, _key_path, _root) = fixture();
    let mut state = PermissionPromptState::default();
    let request = req(DenialKind::Exec, "cargo");
    match gate(
        &mut state,
        &config,
        None, // no key_path configured — no root key resolves
        None,
        PromptChoice::AllowPermanent,
    )
    .ask(std::slice::from_ref(&request))
    {
        newt_core::PermissionDecision::Allow(c) => assert!(c.permits_exec("cargo")),
        newt_core::PermissionDecision::Deny => panic!("must still be granted this session"),
    }
    assert_eq!(state.decisions.last().unwrap().scope, "session-no-key");
    assert!(!approve_path(&config).exists(), "nothing should be written");
}

/// #2524 follow-up item 1 (red first): the no-root-key fallback notice must
/// name the ACTUAL configured path, or that none is configured, never the
/// literal word `key_path`.
#[test]
fn no_key_notice_names_the_actual_path_not_the_literal_word() {
    let configured = std::path::Path::new("/opt/newt-fixture/identity.pem");
    let with_path = no_key_notice(Some(configured));
    assert!(
        with_path.contains("/opt/newt-fixture/identity.pem"),
        "must name the real path: {with_path}"
    );
    assert!(
        !with_path.contains("`key_path`"),
        "must not print the literal placeholder word: {with_path}"
    );

    let without_path = no_key_notice(None);
    assert!(
        without_path.contains("no key_path configured"),
        "an unconfigured key_path must say so plainly: {without_path}"
    );
    assert!(
        !without_path.contains("`key_path`"),
        "must not print the literal placeholder word either: {without_path}"
    );
}

/// #2524 follow-up item 2 (red first): the danger check (and its refusal
/// notice) must classify and name the NORMALIZED fs target persist_approve
/// would actually write — `/ws/..` must be judged and reported as `/`, not
/// as the raw string that hides how dangerous the request is.
#[test]
fn danger_target_is_normalized_before_classification() {
    assert_eq!(
        normalized_danger_target(DenialKind::FsWrite, "/ws/.."),
        "/",
        "a climbing fs target must be classified against its normalized form"
    );
    assert_eq!(
        normalized_danger_target(DenialKind::FsRead, "/ws/x/../y"),
        "/ws/y",
        "an internally-contained relative segment normalizes too"
    );
    // A relative/climbing target that fails to normalize falls back to the
    // raw string — persist_approve's own check refuses it downstream.
    assert_eq!(
        normalized_danger_target(DenialKind::FsWrite, "../../"),
        "../../"
    );
    // Exec/net targets are not paths and pass through unchanged.
    assert_eq!(normalized_danger_target(DenialKind::Exec, "bash"), "bash");
    assert_eq!(
        normalized_danger_target(DenialKind::Net, "evil.example"),
        "evil.example"
    );
}

/// The end-to-end wiring: a broad root reached only via a climbing relative
/// segment (`/ws/..` normalizes cleanly to `/`, under a danger table rooted
/// at `/`) must still be refused as high-danger, exactly as a direct `/`
/// request would be — before this fix the raw `/ws/..` string did not
/// classify as High at all, silently reaching the writer instead of being
/// refused at the gate.
#[test]
fn allow_permanent_refuses_a_climbing_path_that_normalizes_to_a_broad_root() {
    let (_dir, config, key_path, _root) = fixture();
    let mut state = PermissionPromptState::default();
    let climbing = "/ws/..";
    let request = req(DenialKind::FsWrite, climbing);
    match gate_with_danger(
        &mut state,
        &config,
        Some(key_path),
        None,
        PromptChoice::AllowPermanent,
        danger::DangerTable::builtin().with_fs_root(std::path::PathBuf::from("/")),
    )
    .ask(std::slice::from_ref(&request))
    {
        newt_core::PermissionDecision::Deny => {}
        newt_core::PermissionDecision::Allow(_) => {
            panic!("a climbing path to a broad root must be refused")
        }
    }
    assert_eq!(
        state.decisions.last().unwrap().scope,
        "permanent-allow-refused-high-danger"
    );
    assert!(!approve_path(&config).exists(), "nothing should be written");
}

/// #2524 follow-up item 3 (red first): `persist_approve` reports whether the
/// pre-existing on-disk text had a comment, so the caller can tell the
/// operator their comment was NOT preserved across the re-serialise. This is
/// the actual signal the interactive notice gates on.
#[test]
fn persist_approve_reports_when_preexisting_comments_were_dropped() {
    let dir = tempfile::TempDir::new().unwrap();
    let config = dir.path().join("config.toml");
    let ocap_dir = config.with_file_name("ocap");
    std::fs::create_dir_all(&ocap_dir).unwrap();
    std::fs::write(
        ocap_dir.join("approve.toml"),
        "# hand-written note\n[[exec]]\ntarget = \"curl\"\nsig = \"aa\"\n",
    )
    .unwrap();
    let k = agent_mesh_protocol::UserKey::generate();
    let had_comments = newt_core::ocap_store::persist_approve(
        &config,
        newt_core::ocap_store::ApproveEntry::Exec {
            target: "cargo".to_string(),
        },
        |_, _| false,
        |payload| k.sign(payload).to_bytes(),
    )
    .unwrap();
    assert!(had_comments, "the pre-existing file had a comment");

    // A second write, now against a file with NO comment (the one just
    // written by `to_toml`), must report false.
    let had_comments_again = newt_core::ocap_store::persist_approve(
        &config,
        newt_core::ocap_store::ApproveEntry::Exec {
            target: "just".to_string(),
        },
        |_, _| false,
        |payload| k.sign(payload).to_bytes(),
    )
    .unwrap();
    assert!(
        !had_comments_again,
        "a re-serialised file (no hand-written comment left) must report false"
    );
}
