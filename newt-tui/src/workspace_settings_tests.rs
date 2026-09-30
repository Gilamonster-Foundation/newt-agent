use super::*;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

#[test]
fn only_exact_operator_commands_request_workspace_settings() {
    for input in [
        "/workspace",
        "  /workspace\t",
        "/settings workspaces",
        " /settings\t workspaces ",
    ] {
        assert!(requested(input), "{input:?}");
    }
    for input in [
        "/status workspace",
        "/workspaces",
        "/workspace other",
        "/settings workspace",
        "/settings workspaces other",
        "workspace",
    ] {
        assert!(!requested(input), "{input:?}");
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    workspace: PathBuf,
    active: Caveats,
    permissions: ToolPermissions,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("current workspace");
        std::fs::create_dir(&workspace).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let permissions = ToolPermissions {
            preset: PermissionPreset::ReadOnly,
            net: vec!["docs.example.test".into()],
            ..Default::default()
        };
        let mut active = permissions.to_caveats(workspace.to_str().unwrap());
        newt_core::caveats::lock_fs_to_workspace(
            &mut active,
            workspace.to_str().unwrap(),
            &[],
            &[],
        );
        Self {
            dir,
            workspace,
            active,
            permissions,
        }
    }

    fn context(&self) -> WorkspaceSettingsContext<'_> {
        WorkspaceSettingsContext {
            workspace: &self.workspace,
            current_dir: &self.workspace,
            active: &self.active,
            permissions: &self.permissions,
            config_path: None,
            key_path: None,
            interactive_operator: true,
            additional_access: &[],
            validate: &|_, _| Ok(()),
        }
    }
}

fn scripted<'a>(
    answers: Vec<&str>,
    seen: &'a RefCell<Vec<SurfaceInteraction>>,
) -> impl Fn(&SurfaceInteraction) -> HumanQuestionOutcome + 'a {
    let answers = RefCell::new(
        answers
            .into_iter()
            .map(str::to_owned)
            .collect::<VecDeque<_>>(),
    );
    move |interaction| {
        seen.borrow_mut().push(interaction.clone());
        answers.borrow_mut().pop_front().map_or(
            HumanQuestionOutcome::Cancelled,
            HumanQuestionOutcome::Answer,
        )
    }
}

#[test]
fn noninteractive_settings_never_ask_or_touch_the_profile_store() {
    let fx = Fixture::new();
    let config = fx.dir.path().join("uncreated/config.toml");
    let asked = Cell::new(false);
    let mut context = fx.context();
    context.interactive_operator = false;
    context.config_path = Some(&config);
    assert!(run(context, &|_| {
        asked.set(true);
        HumanQuestionOutcome::Answer("yes".into())
    })
    .unwrap_err()
    .contains("interactive operator"));
    assert!(!asked.get());
    assert!(!config.parent().unwrap().exists());
}

#[test]
fn review_binds_resolved_profile_and_requires_explicit_save() {
    let fx = Fixture::new();
    let child = fx.workspace.join("source files");
    std::fs::create_dir(&child).unwrap();
    let snapshot = VerifiedSnapshot::empty().unwrap();
    let original_id = *snapshot.content_id();
    let before = fx.active.clone();
    let seen = RefCell::new(Vec::new());
    // Change cwd, add one writable directory, choose confined commands, set
    // the next-launch default, then inspect and explicitly confirm the review.
    let ask = scripted(
        vec![
            "2",
            "source files",
            "4",
            "1",
            "source files",
            "0",
            "5",
            "4",
            "6",
            "1",
            "7",
            "yes",
        ],
        &seen,
    );
    let review = review_edit(&fx.context(), &snapshot, &ask)
        .unwrap()
        .expect("explicit save produces one reviewed edit");
    let profile = &review.profiles()[fx.workspace.to_str().unwrap()];
    assert_eq!(profile.default_cwd, child);
    assert_eq!(profile.write_dirs, BTreeSet::from([child]));
    assert_eq!(profile.preset, PermissionPreset::WorkspaceFullAccess);
    assert_eq!(review.default_workspace(), Some(fx.workspace.as_path()));
    assert_eq!(snapshot.content_id(), &original_id);
    assert_eq!(
        fx.active, before,
        "editing has no mutable session authority"
    );
    let seen = seen.borrow();
    let confirm = seen.last().unwrap();
    assert_eq!(
        confirm.default_option.as_ref().unwrap().as_str(),
        newt_core::interaction_form::NO
    );
    for text in [
        "Active this session",
        "Saved for next launch",
        "Review",
        "real Newt process restart",
        "/restart",
        "Network",
        "docs.example.test",
    ] {
        assert!(
            confirm.definition.markdown.contains(text),
            "missing {text}: {}",
            confirm.definition.markdown
        );
    }
}

#[test]
fn blank_save_and_every_nonanswer_cancel_without_a_reviewed_edit() {
    for outcome in [
        HumanQuestionOutcome::Answer(String::new()),
        HumanQuestionOutcome::Cancelled,
        HumanQuestionOutcome::Unavailable,
        HumanQuestionOutcome::InputClosed,
        HumanQuestionOutcome::InputFailed,
        HumanQuestionOutcome::ExitRequested,
    ] {
        let fx = Fixture::new();
        let snapshot = VerifiedSnapshot::empty().unwrap();
        let count = Cell::new(0);
        let ask = |_: &SurfaceInteraction| {
            let n = count.get();
            count.set(n + 1);
            if n == 0 {
                HumanQuestionOutcome::Answer("7".into())
            } else {
                outcome.clone()
            }
        };
        assert!(review_edit(&fx.context(), &snapshot, &ask)
            .unwrap()
            .is_none());
        assert_eq!(count.get(), 2, "the review was actually displayed");
    }
}

#[test]
fn selecting_another_workspace_does_not_copy_current_profile_roots() {
    let fx = Fixture::new();
    let other = fx.dir.path().join("other workspace");
    std::fs::create_dir(&other).unwrap();
    let other = other.canonicalize().unwrap();
    let snapshot = VerifiedSnapshot::empty().unwrap();
    let seen = RefCell::new(Vec::new());
    // A draft write root is discarded when selecting a different workspace.
    let other_text = other.to_str().unwrap();
    let ask = scripted(
        vec!["4", "1", ".", "0", "1", "0", other_text, "7", "yes"],
        &seen,
    );
    let review = review_edit(&fx.context(), &snapshot, &ask)
        .unwrap()
        .unwrap();
    assert_eq!(review.profiles().len(), 1);
    let profile = &review.profiles()[other_text];
    assert!(profile.write_dirs.is_empty());
    assert!(profile.read_dirs.is_empty());
    assert_eq!(profile.default_cwd, other);
    assert_eq!(profile.preset, PermissionPreset::ReadOnly);
    assert!(
        review.default_workspace().is_none(),
        "selection is not a default launch change"
    );
    assert!(seen
        .borrow()
        .last()
        .unwrap()
        .definition
        .markdown
        .contains("current session stays"));
}

#[test]
fn protected_candidate_is_rejected_before_save_confirmation() {
    let fx = Fixture::new();
    let snapshot = VerifiedSnapshot::empty().unwrap();
    let seen = RefCell::new(Vec::new());
    let validate = |_: &Path, _: &WorkspaceProfile| Err("protected control directory".into());
    let mut context = fx.context();
    context.validate = &validate;
    let ask = scripted(vec!["7", "0"], &seen);
    assert!(review_edit(&context, &snapshot, &ask).unwrap().is_none());
    assert!(seen.borrow().iter().any(|q| q
        .definition
        .markdown
        .contains("protected control directory")));
    assert!(
        seen.borrow().iter().all(|q| q.default_option.is_none()),
        "invalid authority never reaches Save confirmation"
    );
}

#[test]
fn removing_a_profile_reviews_fallback_and_preserves_separate_approvals() {
    let fx = Fixture::new();
    let path = fx.dir.path().join("profiles.enc");
    let root = newt_identity::UserKey::generate();
    let identity = newt_core::secrets::TokenIdentity::generate();
    let grants = durable_grants::GrantSet::from([(
        newt_core::DenialKind::FsWrite,
        fx.workspace.to_string_lossy().into_owned(),
    )]);
    durable_grants::merge(&path, &fx.workspace, &grants, &root, &identity).unwrap();
    let loaded = durable_grants::load_snapshot(&path, &root.public(), &identity).unwrap();
    let mut edit = loaded.edit();
    edit.set_profile(
        &fx.workspace,
        WorkspaceProfile::new(
            &fx.workspace,
            &fx.workspace,
            PermissionPreset::WorkspaceFullAccess,
            BTreeSet::new(),
            BTreeSet::new(),
        )
        .unwrap(),
    )
    .unwrap();
    edit.set_default_workspace(Some(&fx.workspace)).unwrap();
    let snapshot =
        durable_grants::commit(&path, &edit.review().unwrap(), &root, &identity).unwrap();
    let before = std::fs::read(&path).unwrap();
    let seen = RefCell::new(Vec::new());
    let ask = scripted(vec!["8", "7", "yes"], &seen);
    let review = review_edit(&fx.context(), &snapshot, &ask)
        .unwrap()
        .unwrap();
    assert!(review.profiles().is_empty());
    assert!(review.default_workspace().is_none());
    assert_eq!(snapshot.grants_for(&fx.workspace).unwrap(), grants);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "review does not persist anything"
    );
    let seen = seen.borrow();
    let body = &seen.last().unwrap().definition.markdown;
    for text in [
        "Remove saved profile",
        "fallback",
        "Separate saved approvals",
        "fs_write",
        "default workspace",
    ] {
        assert!(body.contains(text), "missing {text}: {body}");
    }
}

#[test]
fn displayed_paths_are_literal_not_terminal_or_markdown_instructions() {
    let raw = "a`**[approve](https://example.test)**\u{1b}[2J\nSave now\u{202e}b";
    let rendered = literal(raw);
    assert!(!rendered.contains('\u{1b}'));
    assert!(!rendered.contains('\n'));
    assert!(!rendered.contains('\u{202e}'));
    let projected = newt_core::markup::spans::project(&rendered);
    let text = projected
        .iter()
        .map(newt_core::markup::spans::SpanLine::text)
        .collect::<String>();
    assert_eq!(text, format!("{raw:?}"));
    assert!(projected
        .iter()
        .flat_map(|line| &line.spans)
        .all(|span| span.emphasis == newt_core::markup::spans::Emphasis::Code));
}
#[test]
fn cancelling_live_workflow_does_not_create_a_store_or_load_a_missing_key() {
    let fx = Fixture::new();
    let config = fx.dir.path().join("uncreated/config.toml");
    let key = fx.dir.path().join("uncreated/signing.key");
    let mut context = fx.context();
    context.config_path = Some(&config);
    context.key_path = Some(&key);
    let before = fx.active.clone();
    let result = run(context, &|_| HumanQuestionOutcome::Cancelled).unwrap();
    assert!(result.iter().any(|line| line.contains("cancelled")));
    assert!(!config.parent().unwrap().exists());
    assert_eq!(fx.active, before);
}

#[test]
fn unrelated_edit_retains_a_vanished_profile_and_unchanged_default() {
    let fx = Fixture::new();
    let other = fx.dir.path().join("old workspace");
    std::fs::create_dir(&other).unwrap();
    let other = other.canonicalize().unwrap();
    let path = fx.dir.path().join("profiles.enc");
    let root = newt_identity::UserKey::generate();
    let identity = newt_core::secrets::TokenIdentity::generate();
    let empty = VerifiedSnapshot::empty().unwrap();
    let mut edit = empty.edit();
    edit.set_profile(
        &other,
        WorkspaceProfile::new(
            &other,
            &other,
            PermissionPreset::ReadOnly,
            BTreeSet::new(),
            BTreeSet::new(),
        )
        .unwrap(),
    )
    .unwrap();
    edit.set_default_workspace(Some(&other)).unwrap();
    let snapshot =
        durable_grants::commit(&path, &edit.review().unwrap(), &root, &identity).unwrap();
    std::fs::remove_dir(&other).unwrap();
    let before = std::fs::read(&path).unwrap();
    let validate = |workspace: &Path, _: &WorkspaceProfile| {
        assert_ne!(
            workspace, other,
            "unchanged vanished profiles are not new authority"
        );
        Ok(())
    };
    let mut context = fx.context();
    context.validate = &validate;
    let seen = RefCell::new(Vec::new());
    let ask = scripted(vec!["7", "yes"], &seen);
    let review = review_edit(&context, &snapshot, &ask).unwrap().unwrap();
    assert_eq!(review.profiles().len(), 2);
    assert_eq!(review.default_workspace(), Some(other.as_path()));
    assert_eq!(
        review.profiles()[other.to_str().unwrap()],
        snapshot.profiles()[other.to_str().unwrap()]
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn confirmed_live_workflow_persists_exact_profile_without_changing_active_authority() {
    let _lock = crate::test_env_guard::env_write_guard();
    let fx = Fixture::new();
    let operator = fx.dir.path().join("operator");
    std::fs::create_dir(&operator).unwrap();
    let _env = crate::disable_ocap_session_tests::EnvVar::set(
        "NEWT_CONFIG_DIR",
        operator.to_str().unwrap(),
    );
    let config = operator.join("config.toml");
    std::fs::write(&config, b"# existing operator configuration\n").unwrap();
    let config_before = std::fs::read(&config).unwrap();
    let key = operator.join("identity.pem");
    let signer = newt_identity::load_or_generate(&key).unwrap();
    let store = durable_grants::store_path(&config);
    let encryption_path = operator.join("secrets/identity.txt");
    let active_before = fx.active.clone();
    let mut context = fx.context();
    context.config_path = Some(&config);
    context.key_path = Some(&key);
    let seen = RefCell::new(Vec::new());
    let script = scripted(vec!["5", "4", "6", "1", "7", "yes"], &seen);
    let ask = |interaction: &SurfaceInteraction| {
        assert!(!store.exists(), "nothing persists before the Save answer");
        assert!(
            !encryption_path.exists(),
            "no identity is generated before Save"
        );
        script(interaction)
    };
    let receipt = run(context, &ask).unwrap();
    assert!(receipt
        .iter()
        .any(|line| line.contains("saved for next launch")));
    assert!(receipt
        .iter()
        .any(|line| line.contains("authority is unchanged")));
    let encryption = newt_core::secrets::load_identity().unwrap().unwrap();
    let saved = durable_grants::load_snapshot(&store, &signer.public(), &encryption).unwrap();
    let profile = &saved.profiles()[fx.workspace.to_str().unwrap()];
    assert_eq!(profile.preset, PermissionPreset::WorkspaceFullAccess);
    assert_eq!(profile.default_cwd, fx.workspace);
    assert_eq!(saved.default_workspace(), Some(fx.workspace.as_path()));
    assert_eq!(
        profile
            .caveats(&fx.workspace, &fx.permissions)
            .unwrap()
            .exec,
        Scope::All
    );
    assert_eq!(fx.active, active_before);
    assert_eq!(std::fs::read(config).unwrap(), config_before);
    assert!(!std::fs::read_to_string(store)
        .unwrap()
        .contains(fx.workspace.to_str().unwrap()));
    assert_eq!(
        seen.borrow()
            .last()
            .unwrap()
            .default_option
            .as_ref()
            .unwrap()
            .as_str(),
        newt_core::interaction_form::NO
    );
}

#[cfg(feature = "rich-tui")]
pub(crate) fn review_for_renderer() -> (tempfile::TempDir, SurfaceInteraction) {
    let fx = Fixture::new();
    let long_name = format!(
        "{}workspace-tail-marker",
        "long-directory-component-".repeat(5)
    );
    std::fs::create_dir(fx.workspace.join(&long_name)).unwrap();
    let snapshot = VerifiedSnapshot::empty().unwrap();
    let seen = RefCell::new(Vec::new());
    let ask = scripted(vec!["3", "1", &long_name, "0", "7", "yes"], &seen);
    assert!(review_edit(&fx.context(), &snapshot, &ask)
        .unwrap()
        .is_some());
    let review = seen.borrow().last().unwrap().clone();
    (fx.dir, review)
}

#[test]
fn save_failure_receipts_escape_untrusted_path_control_bytes() {
    let fx = Fixture::new();
    let config = fx.dir.path().join("uncreated/config.toml");
    let key = fx
        .dir
        .path()
        .join("missing\u{1b}[2J\nSpoofed receipt\u{202e}.pem");
    let mut context = fx.context();
    context.config_path = Some(&config);
    context.key_path = Some(&key);
    let seen = RefCell::new(Vec::new());
    let ask = scripted(vec!["7", "yes"], &seen);
    let error = run(context, &ask).unwrap_err();
    assert!(error.contains("signing key not found"), "{error}");
    assert!(!error.contains(['\u{1b}', '\n', '\u{202e}']));
    assert!(
        error.contains("\\u{1b}[2J\\nSpoofed receipt\\u{202e}"),
        "{error}"
    );
    assert!(!config.parent().unwrap().exists());
}
