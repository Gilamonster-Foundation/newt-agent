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
fn migration_lock_failure_reports_nonmigrating_and_unreadable_originals() {
    for missing in [false, true] {
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
        assert!(message.contains("the file was not rewritten"), "{message}");
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
