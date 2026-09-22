use super::*;
use std::cell::{Cell, RefCell};

const OLD: &str = "[tenacity]\ndefault = \"standard\" # preserve me\n";

/// Diagnostic delivery is a value seam, independent of tracing or process
/// output. Every branch preserves the exact migrator bytes and reports once.
#[test]
fn migration_notice_values_cover_read_revalidation_and_write_outcomes() {
    for scenario in ["rewrite", "memory", "changed", "reread", "write"] {
        let calls = Cell::new(0);
        let written = RefCell::new(None);
        let mut notices = Vec::new();
        let latest = OLD.replace("standard", "insistent");
        let result = read_migrating(
            Path::new("operator.toml"),
            "config",
            migrate_config_text,
            scenario != "memory",
            |_| {
                let call = calls.get();
                calls.set(call + 1);
                if call > 0 {
                    match scenario {
                        "changed" => return Ok(latest.clone()),
                        "reread" => return Err(std::io::Error::other("reread sentinel")),
                        _ => {}
                    }
                }
                Ok(OLD.to_string())
            },
            |_, text| {
                if scenario == "write" {
                    anyhow::bail!("write sentinel");
                }
                *written.borrow_mut() = Some(text.to_string());
                Ok(())
            },
            &mut |notice| notices.push(notice),
        )
        .unwrap();
        let source = if scenario == "changed" { &latest } else { OLD };
        assert_eq!(
            result,
            migrate_config_text(source).unwrap().text,
            "{scenario}"
        );
        assert_eq!(notices.len(), 1, "{scenario}");
        let notice = &notices[0];
        let message = notice.line();
        assert!(message.contains("operator.toml"), "{message}");
        match scenario {
            "rewrite" => {
                assert_eq!(notice.level, Level::Ok);
                assert!(message.contains("migrated config"), "{message}");
                assert_eq!(*written.borrow(), Some(result));
            }
            "memory" => {
                assert!(
                    message.contains("translated original text in memory"),
                    "{message}"
                );
                assert!(message.contains("standard"), "{message}");
                assert!(message.contains("unchanged"), "{message}");
            }
            "changed" => {
                assert!(message.contains("latest text"), "{message}");
                assert!(message.contains("insistent"), "{message}");
                assert!(message.contains("unchanged"), "{message}");
            }
            "reread" => {
                assert!(message.contains("reread sentinel"), "{message}");
                assert!(message.contains("translated original text"), "{message}");
                assert!(message.contains("unchanged"), "{message}");
            }
            "write" => {
                assert!(message.contains("write sentinel"), "{message}");
                assert!(message.contains("write did not complete"), "{message}");
                assert!(!message.contains("migrated config"), "{message}");
            }
            _ => unreachable!(),
        }
        if scenario != "rewrite" {
            assert_eq!(notice.level, Level::Warn);
            assert!(written.borrow().is_none(), "{scenario}");
        }
    }
}

#[test]
fn migration_notice_clean_reads_are_quiet_and_failed_attempts_are_not_cached() {
    let mut notices = Vec::new();
    for text in ["# clean\n", OLD, OLD] {
        read_migrating(
            Path::new("same.toml"),
            "config",
            migrate_config_text,
            false,
            |_| Ok(text.to_string()),
            |_, _| panic!("in-memory reads never write"),
            &mut |notice| notices.push(notice),
        )
        .unwrap();
    }
    assert_eq!(
        notices.len(),
        2,
        "a later unchanged legacy read remains reportable"
    );
}

#[test]
fn migration_notice_initial_read_error_never_runs_the_writer() {
    let mut notices = Vec::new();
    let result = read_migrating(
        Path::new("missing.toml"),
        "config",
        migrate_config_text,
        true,
        |_| Err(std::io::Error::other("read sentinel")),
        |_, _| panic!("failed reads never write"),
        &mut |notice| notices.push(notice),
    );
    assert!(result.unwrap_err().to_string().contains("read sentinel"));
    assert!(
        notices.is_empty(),
        "no migration occurred; caller owns the read error"
    );
}

/// Grounds the lock-only warning fallback when there is no translation to
/// supply a report. Real destination locks preserve both current bytes and the
/// primary read error; neither outcome may silently disappear.
#[test]
#[serial_test::serial(lock_contention_notice)]
fn migration_lock_failure_reports_nonmigrating_and_unreadable_originals() {
    for missing in [false, true] {
        crate::psyche_import::reset_lock_contention_warned_for_test();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let current = "# current operator bytes\n";
        if !missing {
            std::fs::write(&path, current).unwrap();
        }
        let destination = crate::atomic_fs::ResolvedPath::resolve(&path).unwrap();
        let _lock = crate::atomic_fs::acquire_lock(&destination.lock_path()).unwrap();
        let mut notices = Vec::new();
        let result = read_config_file(&path, true, &mut |notice| notices.push(notice));
        if missing {
            assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::NotFound);
            assert!(!path.exists(), "a failed read must not create the file");
        } else {
            assert_eq!(result.unwrap(), current);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), current);
        }
        assert_eq!(notices.len(), 1, "one lock failure is one read outcome");
        assert_eq!(notices[0].level, Level::Warn);
        let message = notices[0].line();
        assert!(message.contains("cannot lock config"), "{message}");
        assert!(message.contains(path.to_str().unwrap()), "{message}");
        assert!(
            message.contains("config changes from this command were not saved"),
            "{message}"
        );
        assert!(!message.contains("migrated config"), "{message}");
        assert!(
            message.contains(if missing {
                "could not read original text"
            } else {
                "loaded original text in memory"
            }),
            "{message}"
        );
    }
}

/// #2487 review round 2, item 2: a single command that re-reads the same
/// locked config more than once (e.g. `mcp import`'s namespace-sanitization
/// pass re-loading the config it already holds the transaction lock on) must
/// warn about the lock contention ONCE per process, not once per attempt.
/// RED before the fix: two lock-contention reads produced two notices.
#[test]
#[serial_test::serial(lock_contention_notice)]
fn lock_contention_warning_fires_once_per_process_not_once_per_attempt() {
    crate::psyche_import::reset_lock_contention_warned_for_test();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    // No old psyche labels here — the ONLY notice either read can produce is
    // the lock-contention fallback, so `second.is_empty()` below isolates
    // that signal instead of conflating it with the (legitimate, per-read)
    // migration notice `OLD` would also trigger.
    std::fs::write(&path, "# current operator bytes\n").unwrap();
    let destination = crate::atomic_fs::ResolvedPath::resolve(&path).unwrap();
    let _lock = crate::atomic_fs::acquire_lock(&destination.lock_path()).unwrap();

    let mut first = Vec::new();
    read_config_file(&path, true, &mut |n| first.push(n)).unwrap();
    assert_eq!(first.len(), 1, "first attempt reports the contention");

    let mut second = Vec::new();
    read_config_file(&path, true, &mut |n| second.push(n)).unwrap();
    assert!(
        second.is_empty(),
        "a second attempt in the same process must not repeat the warning: {second:?}"
    );
}
