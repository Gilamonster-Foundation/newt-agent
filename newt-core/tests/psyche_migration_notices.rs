//! Real-process grounding for the migration reporter's library/host split.
//! The unit tier will assert diagnostic values; these cases prove a library
//! read itself cannot write below a host-owned editor or permission prompt.

use std::process::Command;

const CHILD: &str = "NEWT_MIGRATION_NOTICE_CHILD";

#[test]
fn migration_library_child() {
    let Ok(scenario) = std::env::var(CHILD) else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.toml");
    let old = "[tenacity]\ndefault = \"standard\"\n";
    let mut notices = Vec::new();
    match scenario.as_str() {
        "config" => {
            std::fs::write(&path, old).unwrap();
            let loaded = newt_core::psyche_import::read_config_file(&path, false, &mut |notice| {
                notices.push(notice);
            })
            .unwrap();
            assert_eq!(
                loaded,
                newt_core::psyche_import::migrate_config_text(old)
                    .unwrap()
                    .text
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), old);
        }
        "decode-error" => {
            std::fs::write(&path, format!("default_backend = 7\n{old}")).unwrap();
            assert!(newt_core::Config::load(&path, &mut |notice| notices.push(notice)).is_err());
        }
        "layered-error" => {
            let own = dir.path().join("user");
            let workspace = dir.path().join("workspace");
            std::fs::create_dir_all(&own).unwrap();
            std::fs::create_dir_all(workspace.join(".newt")).unwrap();
            let base = own.join("config.toml");
            let overlay = workspace.join(".newt").join("config.toml");
            // Memory policy is retained from project data; default_backend
            // would be stripped by the existing control-plane trust boundary.
            let invalid = format!("memory = 7\n{old}");
            std::fs::write(&base, old).unwrap();
            std::fs::write(&overlay, &invalid).unwrap();
            // Process-isolated child: pin both layers before runtime resolution.
            unsafe {
                std::env::set_var("NEWT_CONFIG_DIR", &own);
                std::env::set_var("NEWT_CONFIG", &base);
            }
            std::env::set_current_dir(&workspace).unwrap();
            // The product DISCOVERS the overlay from current_dir() and joins
            // component-wise, and Windows can respell a short tempdir name, so
            // build the expectation from what it sees rather than from the
            // path the test wrote.
            let discovered = std::env::current_dir()
                .unwrap()
                .join(".newt")
                .join("config.toml");
            let result = newt_core::Config::resolve_runtime_unpublished(&mut |n| notices.push(n));
            assert!(result.is_err());
            assert_eq!(
                notices.len(),
                2,
                "both files survive the later decode failure"
            );
            assert!(notices
                .iter()
                .any(|n| n.line().contains(base.to_str().unwrap())));
            assert!(notices
                .iter()
                .any(|n| n.line().contains(discovered.to_str().unwrap())));
            assert_eq!(
                std::fs::read_to_string(&base).unwrap(),
                newt_core::psyche_import::migrate_config_text(old)
                    .unwrap()
                    .text
            );
            assert_eq!(std::fs::read_to_string(&overlay).unwrap(), invalid);
            notices.clear();
            assert!(
                newt_core::Config::resolve_runtime_unpublished(&mut |n| notices.push(n)).is_err()
            );
            assert_eq!(
                notices.len(),
                1,
                "rewritten base is quiet; in-memory overlay is reportable again"
            );
            assert!(notices[0].line().contains(discovered.to_str().unwrap()));
        }
        "persona" => {
            let old = "+++\ncognition = \"pondering\"\n+++\nKeep this body.\n";
            std::fs::write(&path, old).unwrap();
            let loaded = newt_core::psyche_import::read_persona_file(&path, &mut |notice| {
                notices.push(notice);
            })
            .unwrap();
            assert_eq!(
                loaded,
                newt_core::psyche_import::migrate_persona_text(old)
                    .unwrap()
                    .text
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), loaded);
        }
        _ => panic!("unknown child scenario"),
    }
    assert_eq!(
        notices.len(),
        1,
        "the caller retains the migration report even on decode failure"
    );
}

/// A diagnostic belongs to the caller even when typed decode subsequently
/// fails. The library must return/report values, never print behind its host.
fn assert_library_does_not_print(scenario: &str) {
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "migration_library_child", "--nocapture"])
        .env(CHILD, scenario)
        .env_remove("NEWT_CONFIG_DIR")
        .output()
        .unwrap();
    assert!(output.status.success(), "{scenario}: {output:?}");
    assert!(
        output.stderr.is_empty(),
        "{scenario}: library printed without a host reporter: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn migration_library_config_read_does_not_print() {
    assert_library_does_not_print("config");
}

#[test]
fn migration_library_decode_failure_does_not_print() {
    assert_library_does_not_print("decode-error");
}

#[test]
fn migration_library_persona_read_does_not_print() {
    assert_library_does_not_print("persona");
}

#[test]
fn migration_library_layered_decode_failure_retains_both_reports() {
    assert_library_does_not_print("layered-error");
}

/// The operator is shown the path they named, not the one the lock and the
/// atomic write resolved it to. A symlinked directory reproduces on Linux what
/// Windows canonicalization (`\\?\` prefix, 8.3 names) does: the resolved path
/// differs from the caller's. Grounds the reporter's display/operate split.
#[cfg(unix)]
#[test]
fn migration_notice_names_the_callers_path_not_the_resolved_one() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let old = "[tenacity]\ndefault = \"standard\"\n";
    std::fs::write(real.join("config.toml"), old).unwrap();
    let via_link = link.join("config.toml");

    let mut notices = Vec::new();
    newt_core::psyche_import::read_config_file(&via_link, true, &mut |notice| {
        notices.push(notice);
    })
    .unwrap();

    assert_eq!(notices.len(), 1);
    let line = notices[0].line();
    assert!(line.contains(via_link.to_str().unwrap()), "{line}");
    assert!(
        !line.contains(real.to_str().unwrap()),
        "the resolved path leaked into the notice: {line}"
    );
    // The rewrite still landed on the file behind the link.
    assert_eq!(
        std::fs::read_to_string(real.join("config.toml")).unwrap(),
        newt_core::psyche_import::migrate_config_text(old)
            .unwrap()
            .text
    );
}
