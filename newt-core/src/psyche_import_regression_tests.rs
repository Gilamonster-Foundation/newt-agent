use super::*;
use std::cell::{Cell, RefCell};

/// PR #2445: a config edited after the migration read must keep the new bytes.
/// The injected read models a writer that does not honor Newt's advisory lock.
#[test]
fn psyche_split_fix_preserves_a_concurrent_config_edit() {
    let old = "default_backend = \"old\"\n[tenacity]\ndefault = \"standard\"\n";
    let edited = old.replace("old", "new");
    let disk = RefCell::new(old.to_string());
    let writes = Cell::new(0);
    let first_read = Cell::new(true);
    let loaded = read_migrating(
        Path::new("config.toml"),
        "config",
        migrate_config_text,
        true,
        |_| {
            let observed = disk.borrow().clone();
            if first_read.replace(false) {
                *disk.borrow_mut() = edited.clone();
            }
            Ok(observed)
        },
        |_, text| {
            writes.set(writes.get() + 1);
            *disk.borrow_mut() = text.to_string();
            Ok(())
        },
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(
        *disk.borrow(),
        edited,
        "migration must not overwrite the concurrent edit"
    );
    assert_eq!(writes.get(), 0, "a stale migration must not be written");
    assert_eq!(
        loaded,
        migrate_config_text(&edited).unwrap().text,
        "load the latest config in memory"
    );
}

/// PR #2445: if the pre-write reread fails, ownership of the bytes is unknown.
/// Loading may continue with the migrated snapshot, but persistence must stop.
#[test]
fn psyche_split_fix_skips_write_when_config_cannot_be_revalidated() {
    let old = "[tenacity]\ndefault = \"standard\"\n";
    let first_read = Cell::new(true);
    let writes = Cell::new(0);
    let loaded = read_migrating(
        Path::new("config.toml"),
        "config",
        migrate_config_text,
        true,
        |_| {
            if first_read.replace(false) {
                Ok(old.to_string())
            } else {
                Err(std::io::ErrorKind::PermissionDenied.into())
            }
        },
        |_, _| {
            writes.set(writes.get() + 1);
            Ok(())
        },
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(writes.get(), 0, "unverified bytes must not be replaced");
    assert_eq!(loaded, migrate_config_text(old).unwrap().text);
}

/// Real-filesystem grounding for the injected race checks: migration must obey
/// the same resolved destination lock as Config::save, or it can lose a save.
#[test]
#[ignore = "real-resource: touches the filesystem and waits for lock contention"]
#[serial_test::serial(real_fs)]
fn psyche_split_fix_config_migration_respects_the_save_lock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let old = "[tenacity]\ndefault = \"standard\"\n";
    std::fs::write(&path, old).unwrap();
    let destination = crate::atomic_fs::ResolvedPath::resolve(&path).unwrap();
    let lock = crate::atomic_fs::acquire_lock(&destination.lock_path()).unwrap();
    let loaded = read_config_file(&path, true, &mut |_| {}).unwrap();
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        old,
        "a locked config must not be rewritten"
    );
    assert_eq!(loaded, migrate_config_text(old).unwrap().text);
    drop(lock);
    assert_eq!(read_config_file(&path, true, &mut |_| {}).unwrap(), loaded);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), loaded);
    assert!(
        !destination.lock_path().exists(),
        "release the migration lock"
    );
}

/// #2451: a versioned current persona declaration must survive the importer.
#[test]
fn resolute_2451_marked_persona_tenacity_is_not_deleted() {
    for level in ["normal", "resolute", "relentless"] {
        let text = format!("+++\npsyche_version = 2\ntenacity = \"{level}\"\n+++\nBody\n");
        assert_eq!(migrate_persona_text(&text), None, "{level}");
    }
}

/// #2451: versioned pursuit defaults must never move into initiative.
#[test]
fn resolute_2451_marked_config_tenacity_is_not_moved() {
    for level in ["normal", "resolute", "relentless"] {
        let text = format!("[tenacity]\nversion = 2\ndefault = \"{level}\"\n[tenacity.families]\nexample = \"{level}\"\n");
        assert_eq!(migrate_config_text(&text), None, "{level}");
    }
}

/// #2451: normal/resolute are current labels, so unversioned declarations
/// require a format hint rather than destructive legacy migration.
#[test]
fn resolute_2451_unversioned_current_labels_are_preserved_and_refused() {
    for level in ["normal", "resolute"] {
        let persona = format!("+++\ntenacity = \"{level}\"\n+++\nBody\n");
        assert_eq!(migrate_persona_text(&persona), None);
        let error = crate::RoleProfile::parse(&persona).unwrap_err().to_string();
        assert!(error.contains("psyche_version"), "{error}");
        let config = format!("[tenacity]\ndefault = \"{level}\"\n");
        assert_eq!(migrate_config_text(&config), None);
        let error = toml::from_str::<crate::Config>(&config)
            .unwrap_err()
            .to_string();
        assert!(error.contains("version"), "{error}");
    }
}

/// #2451: an unsupported version is not permission to rewrite the file.
#[test]
fn resolute_2451_unknown_versions_are_preserved_and_refused() {
    let persona = "+++\npsyche_version = 3\ntenacity = \"relentless\"\n+++\nBody\n";
    assert_eq!(migrate_persona_text(persona), None);
    assert!(crate::RoleProfile::parse(persona).is_err());
    let config = "[tenacity]\nversion = 3\ndefault = \"relentless\"\n";
    assert_eq!(migrate_config_text(config), None);
    assert!(toml::from_str::<crate::Config>(config).is_err());
}

/// #2451: the existing metadata writer must retain restored tenacity and stamp
/// the discriminator, so its output survives the next file load.
#[test]
fn resolute_2451_persona_save_round_trips_current_tenacity() {
    let text = "+++\npsyche_version = 2\ntenacity = \"relentless\"\n+++\nBody\n";
    let profile = crate::RoleProfile::parse(text).unwrap();
    let saved = profile.to_markdown().unwrap();
    assert!(saved.contains("psyche_version = 2"), "{saved}");
    assert!(saved.contains("tenacity = \"relentless\""), "{saved}");
    assert_eq!(migrate_persona_text(&saved), None);
    assert_eq!(crate::RoleProfile::parse(&saved).unwrap(), profile);
}
