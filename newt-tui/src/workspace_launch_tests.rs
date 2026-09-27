use super::*;
use newt_core::{PermissionPreset, Scope};
use std::collections::BTreeSet;

struct Environment(Vec<(&'static str, Option<std::ffi::OsString>)>);

impl Environment {
    fn isolated(config: &Path) -> Self {
        let names = [
            "NEWT_CONFIG_DIR",
            "NEWT_CONFIG",
            "NEWT_FULL_ACCESS",
            "NEWT_DISABLE_OCAP",
            "NEWT_UNSAFE_HOST_EXEC",
            "NEWT_READ_PATHS",
            "NEWT_WRITE_PATHS",
            "NEWT_EXEC_PATHS",
            "NEWT_VENV",
        ];
        let saved = names
            .into_iter()
            .map(|name| (name, std::env::var_os(name)))
            .collect();
        for name in names {
            std::env::remove_var(name);
        }
        std::env::set_var("NEWT_CONFIG_DIR", config);
        Self(saved)
    }
}

impl Drop for Environment {
    fn drop(&mut self) {
        for (name, value) in &self.0 {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn retargeted_saved_workspace_is_refused_before_policy_fallback() {
    let _lock = crate::test_env_guard::env_write_guard();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let operator = root.join("operator");
    let workspace = root.join("workspace");
    let outside = root.join("outside");
    for path in [&operator, &workspace, &outside] {
        std::fs::create_dir(path).unwrap();
    }
    let _env = Environment::isolated(&operator);
    let key = newt_identity::load_or_generate(&newt_identity::default_key_path().unwrap()).unwrap();
    let encryption = newt_core::secrets::load_or_generate_identity().unwrap();
    let mut edit = VerifiedSnapshot::empty().unwrap().edit();
    edit.set_profile(
        &workspace,
        WorkspaceProfile::new(
            &workspace,
            &workspace,
            PermissionPreset::WorkspaceFullAccess,
            BTreeSet::new(),
            BTreeSet::new(),
        )
        .unwrap(),
    )
    .unwrap();
    edit.set_default_workspace(Some(&workspace)).unwrap();
    durable_grants::commit(
        &durable_grants::store_path(&operator.join("config.toml")),
        &edit.review().unwrap(),
        &key,
        &encryption,
    )
    .unwrap();
    let alias = root.join("workspace-alias");
    std::os::unix::fs::symlink(&workspace, &alias).unwrap();
    assert!(
        resolve_code_workspace(Some(&alias)).unwrap().has_profile,
        "a stable alternate spelling still selects the saved profile"
    );
    std::fs::rename(&workspace, root.join("original-workspace")).unwrap();
    std::os::unix::fs::symlink(&outside, &workspace).unwrap();
    assert!(
        resolve_code_workspace(None).is_err(),
        "retargeted default cannot disable saved policy"
    );
    assert!(
        resolve_code_workspace(Some(&workspace)).is_err(),
        "explicit saved name cannot bypass its binding"
    );
    assert!(
        resolve_code_workspace(Some(&alias)).is_err(),
        "an alias cannot hide a retargeted saved binding"
    );
    assert!(
        WorkspaceSession::load(&workspace).is_err(),
        "embedded TUI uses the same binding check"
    );
}

#[cfg(unix)]
#[test]
fn symlinked_workspace_signing_and_encryption_keys_protect_their_actual_targets() {
    let _lock = crate::test_env_guard::env_write_guard();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let operator = root.join("operator");
    let workspace = root.join("workspace");
    let private = root.join("private");
    for path in [&operator.join("secrets"), &workspace, &private] {
        std::fs::create_dir_all(path).unwrap();
    }
    let _env = Environment::isolated(&operator);
    let permissions = ToolPermissions::default();
    let mut caveats = permissions.to_caveats(workspace.to_str().unwrap());
    caveats.fs_read = Scope::only([
        workspace.to_string_lossy().into_owned(),
        private.to_string_lossy().into_owned(),
    ]);
    for name in ["identity.pem", "secrets/identity.txt"] {
        let target = private.join("test-key");
        std::fs::write(&target, "test-only private key bytes").unwrap();
        let link = operator.join(name);
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let session = WorkspaceSession::load(&workspace).unwrap();
        assert!(
            session
                .make_protection()
                .unwrap()
                .validate_caveats(&caveats)
                .is_err(),
            "read grant must not expose the resolved {name} key"
        );
        std::fs::remove_file(link).unwrap();
    }
}

#[test]
fn signed_workspace_settings_resolve_default_and_apply_only_to_a_new_session() {
    let _lock = crate::test_env_guard::env_write_guard();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let operator = root.join("operator");
    let workspace = root.join("workspace");
    let alternate = root.join("alternate");
    let reference = root.join("reference");
    let cwd = workspace.join("source");
    for path in [&operator, &cwd, &alternate, &reference] {
        std::fs::create_dir_all(path).unwrap();
    }
    let _env = Environment::isolated(&operator);
    let key_path = newt_identity::default_key_path().unwrap();
    let key = newt_identity::load_or_generate(&key_path).unwrap();
    let encryption = newt_core::secrets::load_or_generate_identity().unwrap();
    let store = durable_grants::store_path(&operator.join("config.toml"));
    let profile = WorkspaceProfile::new(
        &workspace,
        &cwd,
        PermissionPreset::WorkspaceFullAccess,
        BTreeSet::from([reference.clone()]),
        BTreeSet::new(),
    )
    .unwrap();
    let mut edit = VerifiedSnapshot::empty().unwrap().edit();
    edit.set_profile(&workspace, profile.clone()).unwrap();
    edit.set_default_workspace(Some(&workspace)).unwrap();
    let saved = durable_grants::commit(&store, &edit.review().unwrap(), &key, &encryption).unwrap();

    let selected = resolve_code_workspace(None).unwrap();
    assert_eq!(selected.path.as_deref(), Some(workspace.as_path()));
    assert!(selected.has_profile);
    let explicit = resolve_code_workspace(Some(&alternate)).unwrap();
    assert_eq!(explicit.path.as_deref(), Some(alternate.as_path()));
    assert!(!explicit.has_profile);

    let configured = ToolPermissions {
        net: vec!["*".into()],
        ..Default::default()
    };
    let mut live = WorkspaceSession::load(&workspace).unwrap();
    let authority = live.policy(&workspace, &configured).unwrap().unwrap();
    live.protect(&authority).unwrap();
    assert!(live.protection.is_some());
    assert_eq!(live.profile.as_ref().unwrap().default_cwd, cwd);
    assert_eq!(authority.exec, Scope::All);
    assert!(newt_core::caveats::permits_path(
        &authority.fs_read,
        reference.to_str().unwrap()
    ));
    assert!(!newt_core::caveats::permits_path(
        &authority.fs_write,
        operator.to_str().unwrap()
    ));

    let mut edit = saved.edit();
    let mut readonly = profile;
    readonly.preset = PermissionPreset::ReadOnly;
    edit.set_profile(&workspace, readonly).unwrap();
    durable_grants::commit(&store, &edit.review().unwrap(), &key, &encryption).unwrap();
    assert_eq!(
        live.policy(&workspace, &configured).unwrap(),
        Some(authority)
    );
    let restarted = WorkspaceSession::load(&workspace).unwrap();
    assert_eq!(
        restarted
            .policy(&workspace, &configured)
            .unwrap()
            .unwrap()
            .exec,
        Scope::none()
    );

    // A substituted store fails before it can choose a workspace or policy.
    std::fs::write(&store, b"substituted policy").unwrap();
    assert!(resolve_code_workspace(None).is_err());
    assert!(WorkspaceSession::load(&workspace).is_err());
}
