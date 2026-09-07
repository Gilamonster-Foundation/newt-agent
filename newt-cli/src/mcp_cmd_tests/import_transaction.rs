use super::*;

#[test]
#[ignore = "real filesystem lock acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn import_and_ordinary_writers_share_sorted_resolved_locks() {
    let dir = tempfile::tempdir().unwrap();
    let mcp_path = dir.path().join("mcp.toml");
    let mcp = ResolvedPath::resolve(&mcp_path).unwrap();
    let config = ResolvedPath::resolve(&dir.path().join("config.toml")).unwrap();
    let mcp_alias = ResolvedPath::resolve(&dir.path().join(".").join("mcp.toml")).unwrap();

    let targets = sorted_unique_import_targets(vec![mcp, mcp_alias, config]);
    let paths: Vec<&Path> = targets.iter().map(ResolvedPath::as_path).collect();

    assert_eq!(paths.len(), 2);
    assert!(paths[0].ends_with("config.toml"));
    assert!(paths[1].ends_with("mcp.toml"));

    let guards = acquire_import_target_locks(vec![
        targets[1].clone(),
        targets[0].clone(),
        targets[1].clone(),
    ])
    .unwrap();
    assert_eq!(guards.len(), 2);
    assert!(targets.iter().all(|target| target.lock_path().is_file()));
    drop(guards);
    assert!(targets.iter().all(|target| !target.lock_path().exists()));

    let (ordinary_target, _ordinary_guard) = resolve_and_lock_write_target(&mcp_path).unwrap();
    assert_eq!(ordinary_target, targets[1]);
}

#[test]
#[ignore = "real filesystem durability acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn post_rename_sync_failure_never_restores_committed_import() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "before").unwrap();
    let destination = ResolvedPath::resolve(&path).unwrap();
    let _guard = newt_core::atomic_fs::acquire_lock(&destination.lock_path()).unwrap();

    let error = atomic_write_back_with(
        &destination,
        Some("before"),
        "after",
        || {},
        |destination, staged| {
            destination.durable_replace_with_sync(staged, |_| {
                Err(std::io::Error::other("injected parent fsync failure"))
            })
        },
    )
    .unwrap_err();

    assert_eq!(std::fs::read_to_string(path).unwrap(), "after");
    assert!(error.to_string().contains("could not durably sync"));
}

#[cfg(unix)]
/// Grounds the import adapter's use of a once-resolved destination. Even if
/// an ancestor symlink changes after lock acquisition, commit cannot escape
/// to the new parent.
#[test]
#[ignore = "real filesystem symlink acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn import_transaction_stays_bound_when_parent_symlink_is_retargeted() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("first");
    let second = dir.path().join("second");
    let parent_link = dir.path().join("active");
    std::fs::create_dir_all(&first).unwrap();
    std::fs::create_dir_all(&second).unwrap();
    symlink(&first, &parent_link).unwrap();

    let logical = parent_link.join("mcp.toml");
    let destination = ResolvedPath::resolve(&logical).unwrap();
    let _guard = newt_core::atomic_fs::acquire_lock(&destination.lock_path()).unwrap();
    std::fs::remove_file(&parent_link).unwrap();
    symlink(&second, &parent_link).unwrap();
    atomic_write_back(&destination, None, "# imported\n").unwrap();

    assert_eq!(
        std::fs::read_to_string(first.join("mcp.toml")).unwrap(),
        "# imported\n"
    );
    assert!(!second.join("mcp.toml").exists());
    assert_eq!(
        std::fs::canonicalize(&parent_link).unwrap(),
        std::fs::canonicalize(&second).unwrap()
    );
}

/// Grounds the pure staged-write tests above against the platform's actual
/// shared durable replacement behavior. Weekly/release acceptance only.
#[test]
#[ignore = "real filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn atomic_import_write_replaces_an_existing_target_without_temp_debris() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("mcp.toml");
    std::fs::write(&target, "before").unwrap();
    let destination = ResolvedPath::resolve(&target).unwrap();
    let _guard = newt_core::atomic_fs::acquire_lock(&destination.lock_path()).unwrap();

    atomic_write_back(&destination, Some("before"), "after").unwrap();

    assert_eq!(std::fs::read_to_string(&target).unwrap(), "after");
    assert!(std::fs::read_dir(dir.path()).unwrap().all(|entry| !entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .ends_with(".tmp")));
}
