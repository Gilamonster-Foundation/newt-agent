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
    let loaded = read_config_file(&path, true).unwrap();
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        old,
        "a locked config must not be rewritten"
    );
    assert_eq!(loaded, migrate_config_text(old).unwrap().text);
    drop(lock);
    assert_eq!(read_config_file(&path, true).unwrap(), loaded);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), loaded);
    assert!(
        !destination.lock_path().exists(),
        "release the migration lock"
    );
}
