//! A persisted, verified workspace profile governs actual Brush descendants.

use std::collections::BTreeSet;
use std::path::Path;

use age::secrecy::ExposeSecret as _;
use agent_mesh_protocol::UserKey;
use newt_core::durable_grants::{self, VerifiedSnapshot, WorkspaceProfile};
use newt_core::secrets::TokenIdentity;
use newt_core::workspace_protection::WorkspaceProtection;
use newt_core::{execute_tool, Caveats, DenialKind, NoMcp, PermissionPreset, ToolPermissions};

async fn command(workspace: &Path, source: &str, caveats: &Caveats) -> String {
    execute_tool(
        "run_command",
        &serde_json::json!({"command": source}),
        &workspace.to_string_lossy(),
        false,
        100,
        caveats,
        &mut NoMcp,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
}

pub async fn run(root: &Path) {
    let workspace = root.join("profile-workspace");
    let operator = root.join("profile-operator");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(&operator).unwrap();
    let signing_path = operator.join("identity.pem");
    let encryption_path = operator.join("identity.txt");
    let executable = operator.join("authority-executable-fixture");
    let signing = UserKey::generate();
    signing.save(&signing_path).unwrap();
    let encryption = TokenIdentity::generate();
    std::fs::write(
        &encryption_path,
        encryption.to_file_string("fixture").expose_secret(),
    )
    .unwrap();
    std::fs::write(&executable, b"authority executable fixture").unwrap();
    let config = operator.join("config.toml");
    std::fs::write(&config, b"# operator fixture\n").unwrap();
    let store = durable_grants::store_path(&config);
    let profile = WorkspaceProfile::new(
        &workspace,
        &workspace,
        PermissionPreset::WorkspaceFullAccess,
        BTreeSet::new(),
        BTreeSet::new(),
    )
    .unwrap();
    let mut edit = VerifiedSnapshot::empty().unwrap().edit();
    edit.set_profile(&workspace, profile.clone()).unwrap();
    edit.set_default_workspace(Some(&workspace)).unwrap();
    let reviewed = edit.review().unwrap();
    durable_grants::commit(&store, &reviewed, &signing, &encryption).unwrap();
    let loaded = durable_grants::load_snapshot(&store, &signing.public(), &encryption).unwrap();
    assert_eq!(loaded.content_id(), reviewed.content_id());
    assert_eq!(loaded.default_workspace(), Some(workspace.as_path()));
    let loaded_profile = loaded.profiles().get(workspace.to_str().unwrap()).unwrap();
    assert_eq!(loaded_profile, &profile);
    // Network policy is independent. macOS filesystem evidence needs the
    // explicitly allowed network axis, not an unsupported restricted-net fence.
    let caveats = loaded_profile
        .caveats(
            &workspace,
            &ToolPermissions {
                net: vec!["*".into()],
                ..Default::default()
            },
        )
        .unwrap();
    let protection = WorkspaceProtection::new(
        &[operator.clone(), std::env::current_exe().unwrap()],
        std::slice::from_ref(&operator),
    )
    .unwrap();
    protection.validate_caveats(&caveats).unwrap();
    protection
        .validate_request(DenialKind::Build, workspace.to_str().unwrap())
        .unwrap();

    let output = command(&workspace,
        "mkdir -p ordinary/nested; printf contents > ordinary/nested/first; mv ordinary/nested/first ordinary/nested/renamed",
        &caveats).await;
    assert_eq!(
        std::fs::read(workspace.join("ordinary/nested/renamed")).unwrap(),
        b"contents",
        "{output}"
    );
    let output = command(&workspace, "rm -rf ordinary", &caveats).await;
    assert!(!workspace.join("ordinary").exists(), "{output}");

    let protected = [
        &store,
        &signing_path,
        &encryption_path,
        &config,
        &executable,
    ];
    let before = protected
        .iter()
        .map(|path| std::fs::read(path).unwrap())
        .collect::<Vec<_>>();
    std::fs::write(workspace.join("payload"), b"model replacement attempt").unwrap();
    std::os::unix::fs::symlink(&operator, workspace.join("operator-alias")).unwrap();
    for (index, path) in protected.iter().enumerate() {
        // Neither request admission nor an already-running descendant can
        // acquire these roots. The latter uses native cp, not a redirection
        // preflight, so unchanged bytes prove the actual filesystem fence.
        assert!(protection
            .validate_request(DenialKind::FsRead, path.to_str().unwrap())
            .is_err());
        assert!(protection
            .validate_request(DenialKind::FsWrite, path.to_str().unwrap())
            .is_err());
        let output = command(&workspace, &format!(
            "printf started > attempt-{index}; cat '{}' > leak-{index}; cp payload '{}'; printf finished > finished-{index}",
            path.display(), path.display()), &caveats).await;
        assert_eq!(
            std::fs::read(workspace.join(format!("attempt-{index}"))).unwrap(),
            b"started",
            "{output}"
        );
        assert_eq!(
            std::fs::read(workspace.join(format!("finished-{index}"))).unwrap(),
            b"finished",
            "{output}"
        );
        assert!(
            std::fs::read(workspace.join(format!("leak-{index}")))
                .unwrap()
                .is_empty(),
            "protected contents escaped the native fence"
        );
        assert_eq!(
            std::fs::read(path).unwrap(),
            before[index],
            "native replacement changed an operator resource"
        );
    }
    let output = command(&workspace,
        "cat operator-alias/identity.pem > alias-leak; cp payload operator-alias/identity.pem; printf finished > alias-finished",
        &caveats).await;
    assert_eq!(
        std::fs::read(workspace.join("alias-finished")).unwrap(),
        b"finished",
        "{output}"
    );
    assert!(std::fs::read(workspace.join("alias-leak"))
        .unwrap()
        .is_empty());
    for (path, bytes) in protected.iter().zip(before) {
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
    let reloaded = durable_grants::load_snapshot(&store, &signing.public(), &encryption).unwrap();
    assert_eq!(reloaded.content_id(), reviewed.content_id());
    assert_eq!(reloaded.profiles(), loaded.profiles());
    eprintln!("test saved_workspace_profile_confines_native_commands ... ok");
}
