//! Grounds durable-grant verification and merge semantics in encrypted files,
//! private atomic replacement, and concurrent writers using disposable keys.

use agent_mesh_protocol::UserKey;
use newt_core::durable_grants::{load, merge, store_path, GrantSet};
use newt_core::secrets::{TokenIdentity, AGE_ARMOR_MAGIC};
use newt_core::DenialKind;

fn grants() -> GrantSet {
    [
        (DenialKind::Exec, "helper"),
        (DenialKind::FsRead, "/example/read-only"),
        (DenialKind::FsWrite, "/example/output"),
        (DenialKind::Net, "api.example.test"),
        (DenialKind::RemoteTool, "example__read_messages"),
        (DenialKind::GitWrite, "add"),
    ]
    .into_iter()
    .map(|(kind, target)| (kind, target.to_string()))
    .collect()
}

#[test]
fn durable_grants_round_trip_all_six_exact_kinds_and_isolate_workspaces() {
    let root = UserKey::generate();
    let encryption = TokenIdentity::generate();
    let directory = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let other_workspace = tempfile::tempdir().unwrap();
    let path = store_path(&directory.path().join("config.toml"));
    let approved = grants();
    let saved = merge(&path, workspace.path(), &approved, &root, &encryption).unwrap();
    let loaded = load(&path, workspace.path(), &root.public(), &encryption).unwrap();
    assert_eq!(loaded.grants(), &approved);
    assert_eq!(saved.content_id(), loaded.content_id());
    for (kind, target) in &approved {
        assert!(!loaded
            .grants()
            .contains(&(*kind, format!("{target}/child"))));
    }
    assert!(!loaded
        .grants()
        .contains(&(DenialKind::FsWrite, "/example/read-only".into())));
    assert!(!loaded
        .grants()
        .contains(&(DenialKind::Exec, "example__read_messages".into())));
    assert!(!loaded.grants().contains(&(DenialKind::Exec, "add".into())));
    assert!(
        load(&path, other_workspace.path(), &root.public(), &encryption)
            .unwrap()
            .grants()
            .is_empty()
    );

    let ciphertext = std::fs::read(&path).unwrap();
    assert!(ciphertext.starts_with(AGE_ARMOR_MAGIC));
    for (_, target) in approved.iter().filter(|(_, target)| target.len() >= 12) {
        assert!(!String::from_utf8_lossy(&ciphertext).contains(target));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let files = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    assert_eq!(
        files,
        [path.file_name().unwrap()],
        "no plaintext or staged residue"
    );
}

#[test]
fn durable_grants_merge_preserves_existing_workspaces_and_is_idempotent() {
    let root = UserKey::generate();
    let encryption = TokenIdentity::generate();
    let directory = tempfile::tempdir().unwrap();
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let path = directory.path().join("grants.age");
    let all = grants();
    let one: GrantSet = all.iter().take(1).cloned().collect();
    merge(&path, first.path(), &one, &root, &encryption).unwrap();
    merge(&path, second.path(), &all, &root, &encryption).unwrap();
    let before = merge(&path, first.path(), &all, &root, &encryption).unwrap();
    let repeated = merge(&path, first.path(), &all, &root, &encryption).unwrap();
    assert_eq!(before.content_id(), repeated.content_id());
    for workspace in [first.path(), second.path()] {
        assert_eq!(
            load(&path, workspace, &root.public(), &encryption)
                .unwrap()
                .grants(),
            &all
        );
    }
}

#[test]
fn durable_grants_wrong_keys_and_invalid_additions_never_overwrite() {
    let root = UserKey::generate();
    let other_root = UserKey::generate();
    let encryption = TokenIdentity::generate();
    let other_encryption = TokenIdentity::generate();
    let directory = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let path = directory.path().join("grants.age");
    merge(&path, workspace.path(), &grants(), &root, &encryption).unwrap();
    let before = std::fs::read(&path).unwrap();
    assert!(load(&path, workspace.path(), &other_root.public(), &encryption).is_err());
    assert!(load(&path, workspace.path(), &root.public(), &other_encryption).is_err());
    assert!(merge(&path, workspace.path(), &grants(), &other_root, &encryption).is_err());
    assert!(merge(&path, workspace.path(), &grants(), &root, &other_encryption).is_err());
    for target in ["", "   ", "bad\0target"] {
        let mut additions = grants();
        additions.insert((DenialKind::Net, target.into()));
        assert!(merge(&path, workspace.path(), &additions, &root, &encryption).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn durable_grants_corruption_and_plaintext_fail_closed_without_replacement() {
    let root = UserKey::generate();
    let encryption = TokenIdentity::generate();
    let directory = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let path = directory.path().join("grants.age");
    merge(&path, workspace.path(), &grants(), &root, &encryption).unwrap();
    let mut truncated = std::fs::read(&path).unwrap();
    truncated.truncate(truncated.len() / 2);
    for bad in [truncated, br#"{"grants":[["Exec","helper"]]}"#.to_vec()] {
        std::fs::write(&path, &bad).unwrap();
        assert!(load(&path, workspace.path(), &root.public(), &encryption).is_err());
        assert!(merge(&path, workspace.path(), &grants(), &root, &encryption).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bad);
    }
}

#[test]
fn durable_grants_missing_store_is_empty_without_creating_files() {
    let root = UserKey::generate();
    let encryption = TokenIdentity::generate();
    let directory = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let path = directory.path().join("missing").join("grants.age");
    assert!(load(&path, workspace.path(), &root.public(), &encryption)
        .unwrap()
        .grants()
        .is_empty());
    assert!(!path.parent().unwrap().exists());
}

#[test]
fn durable_grants_concurrent_merges_keep_every_acknowledged_grant() {
    use std::sync::{Arc, Barrier};
    let root = Arc::new(UserKey::generate());
    let encryption = Arc::new(TokenIdentity::generate());
    let directory = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let path = Arc::new(directory.path().join("grants.age"));
    let start = Arc::new(Barrier::new(3));
    let workers: Vec<_> = (0..3)
        .map(|index| {
            let root = Arc::clone(&root);
            let encryption = Arc::clone(&encryption);
            let path = Arc::clone(&path);
            let start = Arc::clone(&start);
            let workspace = workspace.path().to_path_buf();
            std::thread::spawn(move || {
                let additions: GrantSet =
                    [(DenialKind::Net, format!("service-{index}.example.test"))]
                        .into_iter()
                        .collect();
                start.wait();
                merge(&path, &workspace, &additions, &root, &encryption).unwrap();
                additions
            })
        })
        .collect();
    let expected: GrantSet = workers
        .into_iter()
        .flat_map(|worker| worker.join().unwrap())
        .collect();
    assert_eq!(
        load(&path, workspace.path(), &root.public(), &encryption)
            .unwrap()
            .grants(),
        &expected
    );
}

#[cfg(unix)]
#[test]
fn durable_grants_workspace_aliases_share_binding_and_dangling_store_is_an_error() {
    let root = UserKey::generate();
    let encryption = TokenIdentity::generate();
    let directory = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let alias = directory.path().join("workspace-alias");
    std::os::unix::fs::symlink(workspace.path(), &alias).unwrap();
    let path = directory.path().join("grants.age");
    merge(&path, &alias, &grants(), &root, &encryption).unwrap();
    assert_eq!(
        load(&path, workspace.path(), &root.public(), &encryption)
            .unwrap()
            .grants(),
        &grants()
    );
    let dangling = directory.path().join("dangling.age");
    let missing = directory.path().join("missing.age");
    std::os::unix::fs::symlink(&missing, &dangling).unwrap();
    assert!(load(&dangling, workspace.path(), &root.public(), &encryption).is_err());
    assert!(merge(&dangling, workspace.path(), &grants(), &root, &encryption).is_err());
    assert!(!missing.exists());
}
