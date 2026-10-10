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

#[cfg(unix)]
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
    // The runner's temporary directory may itself use an 8.3 alias. Start
    // from its canonical name so this test varies only the verbatim prefix.
    let root = temp.path().canonicalize().unwrap();
    let ordinary = dunce::simplified(&root).join("live");
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

/// #2842: an unsynced backup name must never permit replacement of approve.toml.
#[cfg(unix)]
#[test]
fn backup_publication_failure_keeps_the_original_store() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("config.toml");
    let dir = temp.path().join("ocap");
    std::fs::create_dir(&dir).unwrap();
    let original = format!(
        "[[fs]]\npath = {:?}\nwrite = true\n",
        temp.path().join("missing").to_str().unwrap()
    );
    let store = dir.join("approve.toml");
    std::fs::write(&store, &original).unwrap();
    let reached = std::cell::Cell::new(false);
    let result =
        prune_with_backup_sync(&config, &original, &[(CapabilityClass::Fs, 0)], |backup| {
            reached.set(true);
            assert_eq!(std::fs::read_to_string(backup).unwrap(), original);
            anyhow::bail!("injected backup directory sync failure")
        });
    assert!(
        result.is_err(),
        "store was replaced despite failed backup publication"
    );
    assert!(
        reached.get(),
        "failpoint did not exercise backup publication"
    );
    assert_eq!(std::fs::read_to_string(store).unwrap(), original);
}

/// #2842: refuse the write path even if invoked directly on Windows.
#[cfg(windows)]
#[test]
fn windows_repair_is_report_only_without_private_backup_support() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("config.toml");
    let dir = temp.path().join("ocap");
    std::fs::create_dir(&dir).unwrap();
    let original = "# approval snapshot must not change\n";
    let store = dir.join("approve.toml");
    std::fs::write(&store, original).unwrap();
    let error = prune(&config, original, &[(CapabilityClass::Fs, 0)]).unwrap_err();
    assert!(
        error.to_string().contains("report-only on Windows"),
        "{error}"
    );
    assert_eq!(std::fs::read_to_string(store).unwrap(), original);
    assert_eq!(
        std::fs::read_dir(dir).unwrap().count(),
        1,
        "no backup or lock is created"
    );
}

/// #2842: a real junction alias still drifts from its signed name on Windows.
#[cfg(windows)]
#[test]
fn windows_junction_alias_remains_signed_name_drift() {
    let temp = tempfile::tempdir().unwrap();
    let canonical = temp.path().canonicalize().unwrap();
    let root = dunce::simplified(&canonical);
    let target = root.join("target");
    let alias = root.join("alias");
    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("live"), "exists").unwrap();
    // Junction creation does not require the symlink privilege. All names are
    // owned by this fixture; no inherited shell profile is evaluated.
    let output = std::process::Command::new("cmd.exe")
        .args(["/d", "/c", "mklink", "/J"])
        .arg(&alias)
        .arg(&target)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let named = alias.join("live");
    assert_eq!(
        named.canonicalize().unwrap(),
        target.join("live").canonicalize().unwrap()
    );
    let finding = stale_target(CapabilityClass::Fs, named.to_str().unwrap());
    assert!(finding.is_some_and(|(reason, _)| reason.contains("signed name")));
}
