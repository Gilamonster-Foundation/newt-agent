use super::*;

#[test]
fn stale_targets_distinguish_missing_aliases_and_symbolic_commands() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let live = root.join("live");
    std::fs::write(&live, "exists").unwrap();
    assert!(stale_target(CapabilityClass::Fs, live.to_str().unwrap()).is_none());
    assert!(
        stale_target(CapabilityClass::Fs, root.join("dead").to_str().unwrap())
            .unwrap()
            .1
    );
    assert!(
        stale_target(
            CapabilityClass::Exec,
            root.join("dead-program").to_str().unwrap()
        )
        .unwrap()
        .1
    );
    assert!(
        stale_target(
            CapabilityClass::Exec,
            "newt-doctor-nonexistent-program-fixture"
        )
        .unwrap()
        .1
    );
    #[cfg(unix)]
    {
        let alias = root.join("alias");
        std::os::unix::fs::symlink(&live, &alias).unwrap();
        assert!(stale_target(CapabilityClass::Fs, alias.to_str().unwrap())
            .unwrap()
            .0
            .contains("signed name"));
    }
}

#[test]
fn confirmation_cannot_prune_a_concurrently_changed_store() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("config.toml");
    let dir = temp.path().join("ocap");
    std::fs::create_dir(&dir).unwrap();
    let original = "# reviewed original\n";
    let changed = "# operator changed this while the prompt was open\n";
    std::fs::write(dir.join("approve.toml"), changed).unwrap();
    let error = prune(&config, original, &[(CapabilityClass::Fs, 0)]).unwrap_err();
    assert!(error.to_string().contains("changed during confirmation"));
    assert_eq!(
        std::fs::read_to_string(dir.join("approve.toml")).unwrap(),
        changed
    );
    assert!(!std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .any(|e| e.file_name().to_string_lossy().contains("backup-")));
}

#[cfg(windows)]
#[test]
fn windows_verbatim_prefix_is_not_signed_name_drift() {
    let temp = tempfile::tempdir().unwrap();
    let ordinary = std::path::absolute(temp.path()).unwrap().join("live");
    std::fs::write(&ordinary, "exists").unwrap();
    let canonical = ordinary.canonicalize().unwrap();
    for name in [&ordinary, &canonical] {
        let finding = stale_target(CapabilityClass::Fs, name.to_str().unwrap());
        assert!(
            finding.is_none(),
            "named={name:?}, canonical={canonical:?}, finding={finding:?}"
        );
    }
}
