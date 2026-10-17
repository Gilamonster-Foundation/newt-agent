//! Filesystem migration with caller-owned diagnostic delivery.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::tty::{Level, Notice};

use super::{migrate_config_text, migrate_persona_text, Migration};

/// A config lock-contention warning fires at most once per process. A single
/// command (e.g. `mcp import`, which re-reads the same authoritative config
/// while already holding its transaction lock) can hit lock contention on
/// the same target more than once; the operator needs to be told once that
/// this run's config changes were not durably saved, not the identical
/// sentence per attempt.
static LOCK_CONTENTION_REPORTED: AtomicBool = AtomicBool::new(false);

/// Test-only: reset the once-per-process dedupe... gated behind `cfg(test)`
/// or the `test-util` feature so downstream tests can use it without
/// shipping it in production builds.
#[cfg(any(test, feature = "test-util"))]
pub fn reset_lock_contention_warned_for_test() {
    LOCK_CONTENTION_REPORTED.store(false, Ordering::Relaxed);
}

/// Read and atomically migrate a persona. The caller owns presentation of the
/// report, including when a later persona parse fails.
pub fn read_persona_file(
    path: &Path,
    report: &mut dyn FnMut(Notice<'static>),
) -> std::io::Result<String> {
    read_migrating(
        MigratingPath {
            shown: path,
            operated: path,
        },
        "persona",
        migrate_persona_text,
        true,
        |p| std::fs::read_to_string(p),
        |p, text| crate::atomic_fs::atomic_write(p, text.as_bytes()),
        report,
    )
}

/// Read config under the same resolved-destination lock as Config::save.
///
/// `held_locks` lets a caller that already holds a config lock (for example,
/// `newt mcp import`'s transaction locks) re-read the same target without
/// self-contention. The lock is matched by resolved lock path, not by PID,
/// so same-process exclusion stays honest for unrelated locks.
pub fn read_config_file(
    path: &Path,
    rewrite: bool,
    held_locks: &[&crate::atomic_fs::LockGuard],
    report: &mut dyn FnMut(Notice<'static>),
) -> std::io::Result<String> {
    let destination = rewrite
        .then(|| crate::atomic_fs::ResolvedPath::resolve(path))
        .transpose()
        .map_err(|error| {
            std::io::Error::other(format!(
                "resolving config destination {}: {error:#}",
                path.display()
            ))
        })?;
    let mut lock_failure: Option<String> = None;
    // `locked` is the resolved destination we will read/migrate/write through.
    // It is `Some` for both a lock we acquired and a lock the caller already
    // holds, so migration writes under the right lock.
    let mut locked: Option<crate::atomic_fs::ResolvedPath> = None;
    // A guard we acquired ourselves must stay alive for the whole migration —
    // binding it here keeps it alive until after `read_migrating` returns.
    // The caller's own held lock (passed in `held_locks`) needs no extra guard.
    let mut _own_guard: Option<crate::atomic_fs::LockGuard> = None;
    if let Some(destination) = &destination {
        let held = held_locks
            .iter()
            .any(|guard| guard.lock_path() == destination.lock_path());
        if !held {
            match crate::atomic_fs::acquire_lock(&destination.lock_path()) {
                Ok(guard) => {
                    _own_guard = Some(guard);
                    locked = Some(destination.clone());
                }
                Err(error) if !LOCK_CONTENTION_REPORTED.swap(true, Ordering::Relaxed) => {
                    lock_failure = Some(format!(
                        "cannot lock config {} for migration: {error:#}",
                        path.display()
                    ));
                }
                Err(_) => {}
            }
        } else {
            locked = Some(destination.clone());
        }
    }
    let operated = locked
        .as_ref()
        .map_or(path, crate::atomic_fs::ResolvedPath::as_path);
    let mut migration_reported = false;
    let result = read_migrating(
        MigratingPath {
            shown: path,
            operated,
        },
        "config",
        migrate_config_text,
        locked.is_some(),
        |p| std::fs::read_to_string(p),
        |_, text| {
            let destination = locked.as_ref().expect("rewrite holds the config lock");
            destination.atomic_write(text.as_bytes())
        },
        &mut |notice| {
            if notice.text.contains("translated original text")
                || notice.text.contains("migrated config")
                || notice.text.contains("migration")
            {
                migration_reported = true;
            }
            let mut notice = notice;
            if let Some(error) = &lock_failure {
                notice.text = format!("{}; {error}", notice.text).into();
            }
            report(notice);
        },
    );
    if !migration_reported {
        if let Some(lock_failure) = lock_failure.as_ref() {
            let message = match &result {
                Ok(_) => format!("newt: warning: {lock_failure}; loaded original text in memory"),
                Err(read_error) => format!(
                    "newt: warning: {lock_failure}; could not read original text: {read_error}"
                ),
            };
            report(Notice::new(Level::Warn, "", message));
        }
    }
    result
}

/// The path shown to the operator and the path actually read/written.
/// Canonicalization can add a verbatim `\\?\` prefix (Windows), expand 8.3
/// short names, or follow a symlinked directory; `shown` is what the
/// operator named, `operated` is what the filesystem calls resolved to.
pub(super) struct MigratingPath<'a> {
    pub(super) shown: &'a Path,
    pub(super) operated: &'a Path,
}

/// Injected filesystem seam. Reporting does not own stdout, stderr, a tracing
/// subscriber, or a terminal. A host may retain the value until a safe point.
pub(super) fn read_migrating(
    path: MigratingPath<'_>,
    kind: &str,
    migrate: fn(&str) -> Option<Migration>,
    rewrite: bool,
    mut read: impl FnMut(&Path) -> std::io::Result<String>,
    write: impl FnOnce(&Path, &str) -> anyhow::Result<()>,
    report: &mut dyn FnMut(Notice<'static>),
) -> std::io::Result<String> {
    let MigratingPath { shown, operated } = path;
    let raw = read(operated)?;
    let Some(migration) = migrate(&raw) else {
        return Ok(raw);
    };
    let changes = migration.changes.join(", ");
    let shown = shown.display();
    if !rewrite {
        report(Notice::new(
            Level::Warn,
            "",
            format!(
                "newt: warning: {kind} {shown} uses old psyche labels ({changes}); \
             loading translated original text in memory, the file is unchanged"
            ),
        ));
        return Ok(migration.text);
    }
    // Preserve the changed-source check: a non-cooperating editor may write
    // despite the lock. The latest bytes always win over our original snapshot.
    match read(operated) {
        Ok(current) if current != raw => {
            let latest = migrate(&current);
            let changes = latest.as_ref().map(|m| m.changes.join(", "));
            let detail = changes.map(|c| format!(" ({c})")).unwrap_or_default();
            report(Notice::new(
                Level::Warn,
                "",
                format!(
                    "newt: warning: {kind} {shown} changed during migration; \
                 loading latest text in memory{detail}, the file is unchanged"
                ),
            ));
            return Ok(latest.map_or(current, |latest| latest.text));
        }
        Err(error) => {
            report(Notice::new(
                Level::Warn,
                "",
                format!(
                    "newt: warning: cannot re-read {kind} {shown} before migration: {error}; \
                 loading translated original text ({changes}), the file is unchanged"
                ),
            ));
            return Ok(migration.text);
        }
        Ok(_) => {}
    }
    let notice = match write(operated, &migration.text) {
        Ok(()) => Notice::new(
            Level::Ok,
            "",
            format!("newt: migrated {kind} {shown}: {changes}; loaded translated original text"),
        ),
        Err(error) => Notice::new(
            Level::Warn,
            "",
            format!(
                "newt: warning: {kind} {shown} uses old psyche labels ({changes}); \
             loading translated original text, but migration write did not complete: {error:#}; \
             config changes from this command were not saved"
            ),
        ),
    };
    report(notice);
    Ok(migration.text)
}

#[cfg(test)]
#[path = "psyche_import_notice_tests.rs"]
mod tests;
