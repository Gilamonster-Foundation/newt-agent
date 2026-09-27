use super::*;
use crate::config::{PermissionPreset, ToolPermissions};
use crate::{Scope, ScopeExt as _};

fn profile(workspace: &Path) -> WorkspaceProfile {
    WorkspaceProfile::new(
        workspace,
        workspace,
        PermissionPreset::WorkspaceDev,
        BTreeSet::new(),
        BTreeSet::new(),
    )
    .unwrap()
}

#[test]
fn workspace_profile_preserves_configured_network_and_dev_commands_without_global_access() {
    let workspace = tempfile::tempdir().unwrap();
    let extra = tempfile::tempdir().unwrap();
    let workspace = workspace.path().canonicalize().unwrap();
    let extra = extra.path().canonicalize().unwrap();
    let mut profile = profile(&workspace);
    profile.read_dirs.insert(extra.clone());
    let configured = ToolPermissions {
        preset: PermissionPreset::FullAccess,
        net: vec!["api.example.test".into()],
        extra_exec: vec!["custom-builder".into()],
        ..Default::default()
    };
    let caveats = profile.caveats(&workspace, &configured).unwrap();
    assert_eq!(caveats.net, Scope::only(["api.example.test".into()]));
    assert!(caveats.exec.permits(&"custom-builder".into()));
    assert_eq!(
        caveats.fs_read,
        Scope::only([
            workspace.to_str().unwrap().to_owned(),
            extra.to_str().unwrap().to_owned(),
        ])
    );
    assert_eq!(
        caveats.fs_write,
        Scope::only([workspace.to_str().unwrap().to_owned()])
    );
    profile.preset = PermissionPreset::WorkspaceFullAccess;
    assert_eq!(
        profile.caveats(&workspace, &configured).unwrap().exec,
        Scope::All
    );
    profile.preset = PermissionPreset::ReadOnly;
    let readonly = profile.caveats(&workspace, &configured).unwrap();
    assert_eq!(readonly.fs_write, Scope::none());
    assert_eq!(readonly.exec, Scope::none());
    profile.write_dirs.insert(extra);
    assert!(profile.validate(&workspace).is_err());
    for preset in [PermissionPreset::FullAccess, PermissionPreset::Custom] {
        profile.preset = preset;
        assert!(profile.validate(&workspace).is_err());
    }
}

#[test]
fn workspace_profile_cwd_must_stay_within_canonical_existing_workspace() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = workspace.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("nested")).unwrap();
    let profile = WorkspaceProfile::new(
        workspace.path(),
        Path::new("nested"),
        PermissionPreset::WorkspaceEdit,
        BTreeSet::new(),
        BTreeSet::new(),
    )
    .unwrap();
    assert_eq!(profile.default_cwd, root.join("nested"));
    assert!(WorkspaceProfile::new(
        &root,
        outside.path(),
        PermissionPreset::WorkspaceEdit,
        BTreeSet::new(),
        BTreeSet::new(),
    )
    .is_err());
    let mut malformed = profile;
    malformed.read_dirs.insert(root.join("missing"));
    assert!(malformed.validate(&root).is_err());
    malformed.read_dirs.clear();
    malformed.default_cwd = root.join("nested/..");
    assert!(malformed.validate(&root).is_err());
    malformed.default_cwd = root.join("nested/.");
    assert!(malformed.validate(&root).is_err());
}

#[cfg(unix)]
#[test]
fn workspace_profile_refuses_symlink_retarget_after_review() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = workspace.path().canonicalize().unwrap();
    let directory = root.join("readable");
    std::fs::create_dir(&directory).unwrap();
    let profile = WorkspaceProfile::new(
        &root,
        &root,
        PermissionPreset::WorkspaceEdit,
        BTreeSet::from([directory.clone()]),
        BTreeSet::new(),
    )
    .unwrap();
    std::fs::remove_dir(&directory).unwrap();
    std::os::unix::fs::symlink(outside.path(), &directory).unwrap();
    assert!(profile.validate(&root).is_err());
}

#[test]
fn workspace_profile_save_is_exact_encrypted_cas_and_preserves_legacy_grants() {
    let root = UserKey::generate();
    let encryption = TokenIdentity::generate();
    let directory = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let workspace = workspace.path().canonicalize().unwrap();
    let path = directory.path().join("grants.age");
    let empty = VerifiedSnapshot::empty().unwrap();
    let mut edit = empty.edit();
    edit.set_profile(&workspace, profile(&workspace)).unwrap();
    edit.set_default_workspace(Some(&workspace)).unwrap();
    let reviewed = edit.review().unwrap();
    let saved = commit(&path, &reviewed, &root, &encryption).unwrap();
    assert_eq!(saved.content_id(), reviewed.content_id());
    assert_eq!(saved.default_workspace(), Some(workspace.as_path()));
    assert_eq!(saved.profiles(), reviewed.profiles());
    assert!(std::fs::read(&path)
        .unwrap()
        .starts_with(crate::secrets::AGE_ARMOR_MAGIC));
    let mut stale = saved.edit();
    stale.remove_profile(workspace.to_str().unwrap()).unwrap();
    let stale = stale.review().unwrap();
    let grants = GrantSet::from([(DenialKind::Exec, "helper".into())]);
    merge(&path, &workspace, &grants, &root, &encryption).unwrap();
    let before = std::fs::read(&path).unwrap();
    assert!(commit(&path, &stale, &root, &encryption)
        .unwrap_err()
        .to_string()
        .contains("changed"));
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let latest = load_snapshot(&path, &root.public(), &encryption).unwrap();
    let mut remove = latest.edit();
    remove.remove_profile(workspace.to_str().unwrap()).unwrap();
    let removed = commit(&path, &remove.review().unwrap(), &root, &encryption).unwrap();
    assert!(removed.profiles().is_empty());
    assert_eq!(removed.default_workspace(), None);
    assert_eq!(removed.grants_for(&workspace).unwrap(), grants);
    assert_eq!(removed.profile_history().len(), 2);
}

#[test]
fn workspace_profile_inverse_preserves_interleaved_changes_and_restores_state_bytes() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let original = VerifiedSnapshot::empty().unwrap();
    let mut edit = original.edit();
    edit.set_profile(first.path(), profile(first.path()))
        .unwrap();
    let one = edit.review().unwrap();
    let one = VerifiedSnapshot::from_payload(one.payload.clone()).unwrap();
    let first_change = one.profile_history()[0].node.id().unwrap();
    let mut edit = one.edit();
    edit.set_profile(second.path(), profile(second.path()))
        .unwrap();
    let two = edit.review().unwrap();
    let two = VerifiedSnapshot::from_payload(two.payload.clone()).unwrap();
    let second_change = two.profile_history()[1].node.id().unwrap();
    let mut inverse = two.edit();
    inverse.revert_profile_change(&first_change).unwrap();
    let three = inverse.review().unwrap();
    assert_eq!(three.profiles().len(), 1);
    assert!(three
        .profiles()
        .contains_key(second.path().canonicalize().unwrap().to_str().unwrap()));
    let three = VerifiedSnapshot::from_payload(three.payload.clone()).unwrap();
    let mut inverse = three.edit();
    inverse.revert_profile_change(&second_change).unwrap();
    let final_state = inverse.review().unwrap();
    assert_eq!(
        canonical::to_canonical_dagcbor(&final_state.payload.profile_state()).unwrap(),
        canonical::to_canonical_dagcbor(&original.payload.profile_state()).unwrap(),
    );
    assert_eq!(
        final_state.payload.profile_history.len(),
        4,
        "inverses append evidence"
    );
}

#[test]
fn workspace_profile_history_is_verified_even_when_the_outer_signature_is_valid() {
    let workspace = tempfile::tempdir().unwrap();
    let mut edit = VerifiedSnapshot::empty().unwrap().edit();
    edit.set_profile(workspace.path(), profile(workspace.path()))
        .unwrap();
    let reviewed = edit.review().unwrap();
    let root = UserKey::generate();
    let encryption = TokenIdentity::generate();
    let mut substituted = reviewed.payload.clone();
    substituted.profiles.clear();
    // Deliberately sign invalid state without the safe encoder to test the
    // production decoder's history check independently of outer authenticity.
    let signed = SignedPayload {
        signature: SerdeSig(root.sign(&substituted.canonical_form().unwrap())),
        payload: substituted,
    };
    let bytes = crate::secrets::encrypt_to_identity(&encryption, &signed.canonical_form().unwrap())
        .unwrap();
    assert!(decode(bytes.as_bytes(), &root.public(), &encryption).is_err());
    let mut truncated = reviewed.payload.clone();
    truncated.profile_history.clear();
    assert!(truncated.validate().is_err());
}

#[test]
fn legacy_payload_bytes_and_identity_remain_unchanged_with_no_profiles() {
    let legacy = serde_json::json!({"schema": SCHEMA, "workspaces": {}});
    assert_eq!(
        Payload::empty().canonical_form().unwrap(),
        canonical::to_canonical_dagcbor(&legacy).unwrap()
    );
}

#[test]
fn stale_unselected_profile_does_not_block_other_profile_repairs() {
    let root = UserKey::generate();
    let encryption = TokenIdentity::generate();
    let directory = tempfile::tempdir().unwrap();
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let third = tempfile::tempdir().unwrap();
    let first_path = first.path().canonicalize().unwrap();
    let second_path = second.path().canonicalize().unwrap();
    let path = directory.path().join("grants.age");
    let mut edit = VerifiedSnapshot::empty().unwrap().edit();
    edit.set_profile(&first_path, profile(&first_path)).unwrap();
    edit.set_profile(&second_path, profile(&second_path))
        .unwrap();
    let saved = commit(&path, &edit.review().unwrap(), &root, &encryption).unwrap();
    first.close().unwrap();
    second.close().unwrap();

    let mut repair = saved.edit();
    repair.remove_profile(first_path.to_str().unwrap()).unwrap();
    let reviewed = repair
        .review()
        .expect("an unrelated stale entry must not prevent removing a missing profile");
    let saved = commit(&path, &reviewed, &root, &encryption).unwrap();
    let mut edit = saved.edit();
    edit.set_profile(third.path(), profile(third.path()))
        .unwrap();
    let saved = commit(&path, &edit.review().unwrap(), &root, &encryption).unwrap();
    assert_eq!(saved.profiles().len(), 2);
    assert!(saved.profiles()[second_path.to_str().unwrap()]
        .validate(&second_path)
        .is_err());
    assert!(saved
        .edit()
        .set_default_workspace(Some(&second_path))
        .is_err());
    assert_eq!(
        load_snapshot(&path, &root.public(), &encryption)
            .unwrap()
            .content_id(),
        saved.content_id()
    );
}

#[test]
fn redundant_profile_directories_preserve_effective_access_and_pass_protection() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let workspace = root.join("workspace");
    let nested = workspace.join("nested");
    let operator = root.join("operator");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::create_dir(&operator).unwrap();
    let protection = crate::workspace_protection::WorkspaceProtection::new(
        &[operator.join("policy.age")],
        &[operator.join("identity.pem")],
    )
    .unwrap();
    for writes in [false, true] {
        let mut profile = WorkspaceProfile::new(
            &workspace,
            &workspace,
            PermissionPreset::WorkspaceFullAccess,
            BTreeSet::from([nested.clone()]),
            BTreeSet::new(),
        )
        .unwrap();
        if writes {
            profile.write_dirs.insert(nested.clone());
        }
        let id = profile.content_id().unwrap();
        let caveats = profile
            .caveats(&workspace, &ToolPermissions::default())
            .unwrap();
        protection
            .validate_caveats(&caveats)
            .expect("redundant in-workspace roots must not invalidate the admitted fence");
        assert!(crate::permits_path(
            &caveats.fs_write,
            nested.join("output").to_str().unwrap()
        ));
        assert!(!crate::permits_path(
            &caveats.fs_write,
            operator.join("output").to_str().unwrap()
        ));
        assert!(!crate::permits_path(
            &caveats.fs_read,
            operator.join("identity.pem").to_str().unwrap()
        ));
        assert_eq!(
            caveats.fs_read,
            Scope::only([workspace.to_str().unwrap().to_owned()])
        );
        assert_eq!(
            caveats.fs_write,
            Scope::only([workspace.to_str().unwrap().to_owned()])
        );
        assert_eq!(
            profile.content_id().unwrap(),
            id,
            "projection must preserve the reviewed profile"
        );
    }
}
