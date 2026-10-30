use super::*;
use crate::tty::Level;
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
        MigratingPath {
            shown: Path::new("config.toml"),
            operated: Path::new("config.toml"),
        },
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
        MigratingPath {
            shown: Path::new("config.toml"),
            operated: Path::new("config.toml"),
        },
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

/// On Windows, `ResolvedPath::as_path` (`std::fs::canonicalize`'s output)
/// carries the `\\?\` extended-length prefix, so `read_migrating` takes the
/// operator's own path separately and reads/writes through the resolved one.
/// Regression: reading and writing must still hit the resolved path, and the
/// notice text shown to the operator must name the path they typed, not the
/// canonicalized one — swapping `shown` for `operated` in the notice would
/// still pass every assertion below except the last.
#[test]
fn read_migrating_operates_on_the_resolved_path_not_the_shown_one() {
    let shown = Path::new(r"C:\Users\bob\.newt\config.toml");
    let resolved = Path::new(r"\\?\C:\Users\bob\.newt\config.toml");
    let reads: RefCell<Vec<std::path::PathBuf>> = RefCell::new(Vec::new());
    let writes: RefCell<Vec<std::path::PathBuf>> = RefCell::new(Vec::new());
    let notices: RefCell<Vec<String>> = RefCell::new(Vec::new());
    let loaded = read_migrating(
        MigratingPath {
            shown,
            operated: resolved,
        },
        "config",
        migrate_config_text,
        true,
        |p| {
            reads.borrow_mut().push(p.to_path_buf());
            Ok("[tenacity]\ndefault = \"standard\"\n".to_string())
        },
        |p, text| {
            writes.borrow_mut().push(p.to_path_buf());
            let _ = text;
            Ok(())
        },
        &mut |notice| notices.borrow_mut().push(notice.text.to_string()),
    )
    .unwrap();
    assert!(reads.borrow().iter().all(|p| p == resolved), "{reads:?}");
    assert_eq!(*writes.borrow(), [resolved.to_path_buf()]);
    assert!(loaded.contains("[initiative]"), "{loaded}");
    let notice = notices.borrow().join("\n");
    assert!(
        notice.contains(r"C:\Users\bob\.newt\config.toml"),
        "notice must show the operator's path: {notice}"
    );
    assert!(
        !notice.contains(r"\\?\"),
        "notice must not show the canonicalized path: {notice}"
    );
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
    let loaded = read_config_file(&path, true, &[], &mut |_| {}).unwrap();
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        old,
        "a locked config must not be rewritten"
    );
    assert_eq!(loaded, migrate_config_text(old).unwrap().text);
    drop(lock);
    assert_eq!(
        read_config_file(&path, true, &[], &mut |_| {}).unwrap(),
        loaded
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), loaded);
    assert!(
        !destination.lock_path().exists(),
        "release the migration lock"
    );
}

/// MCP import holds the transaction lock before it re-reads consulted configs.
/// A read that passes the held lock must not re-acquire it: no retry loop, no
/// false contention warning, and no "changes were not saved" claim.
#[test]
#[serial_test::serial(lock_contention_notice)]
fn import_read_respects_held_lock_no_retry() {
    crate::atomic_fs::reset_lock_attempts_for_test();
    crate::psyche_import::reset_lock_contention_warned_for_test();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "# current operator bytes\n").unwrap();
    let destination = crate::atomic_fs::ResolvedPath::resolve(&path).unwrap();
    let guard = crate::atomic_fs::acquire_lock(&destination.lock_path()).unwrap();
    crate::atomic_fs::reset_lock_attempts_for_test();
    let held: &[&crate::atomic_fs::LockGuard] = &[&guard];
    let mut notices = Vec::new();
    let loaded = read_config_file(&path, true, held, &mut |n| notices.push(n)).unwrap();
    assert_eq!(loaded, "# current operator bytes\n");
    assert!(notices.is_empty(), "clean read under a held lock is quiet");
    assert_eq!(
        crate::atomic_fs::lock_attempts_for_test(),
        0,
        "the held lock must bypass acquire_lock entirely"
    );
    drop(guard);
    assert!(!destination.lock_path().exists());
}

/// A reader that does NOT hold the lock still contends normally. This is the
/// negative case for the held-lock optimization: genuine exclusion is
/// preserved, and the caller still gets the current bytes in memory once the
/// retry budget exhausts.
#[test]
#[serial_test::serial(lock_contention_notice)]
fn unheld_config_reader_still_exhausts_lock_budget() {
    crate::atomic_fs::reset_lock_attempts_for_test();
    crate::psyche_import::reset_lock_contention_warned_for_test();
    let _zero_delay = crate::atomic_fs::set_lock_retry_delay_for_test(std::time::Duration::ZERO);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "# current operator bytes\n").unwrap();
    let destination = crate::atomic_fs::ResolvedPath::resolve(&path).unwrap();
    let guard = crate::atomic_fs::acquire_lock(&destination.lock_path()).unwrap();
    crate::atomic_fs::reset_lock_attempts_for_test();
    let mut notices = Vec::new();
    let loaded = read_config_file(&path, true, &[], &mut |n| notices.push(n)).unwrap();
    assert_eq!(loaded, "# current operator bytes\n");
    assert_eq!(
        crate::atomic_fs::lock_attempts_for_test(),
        100,
        "a reader without the held lock must retry to exhaustion"
    );
    assert_eq!(notices.len(), 1);
    assert!(notices[0].line().contains("cannot lock config"));
    drop(guard);
}

/// A migration that actually writes under a held lock succeeds silently; there
/// is no "changes were not saved" warning. This is the positive side of the
/// false-warning fix.
#[test]
#[serial_test::serial(lock_contention_notice)]
fn held_lock_migration_reports_success_not_unsaved_warning() {
    crate::psyche_import::reset_lock_contention_warned_for_test();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let old = "[tenacity]\ndefault = \"standard\"\n";
    std::fs::write(&path, old).unwrap();
    let destination = crate::atomic_fs::ResolvedPath::resolve(&path).unwrap();
    let guard = crate::atomic_fs::acquire_lock(&destination.lock_path()).unwrap();
    let held: &[&crate::atomic_fs::LockGuard] = &[&guard];
    let mut notices = Vec::new();
    let loaded = read_config_file(&path, true, held, &mut |n| notices.push(n)).unwrap();
    let migrated = migrate_config_text(old).unwrap().text;
    assert_eq!(loaded, migrated);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), migrated);
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].level, Level::Ok);
    assert!(notices[0].line().contains("migrated config"));
    assert!(
        !notices[0]
            .line()
            .contains("config changes from this command were not saved"),
        "successful migration must not claim changes were not saved"
    );
    drop(guard);
}
