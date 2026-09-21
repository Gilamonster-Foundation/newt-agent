//! Filesystem migration with caller-owned diagnostic delivery.

use std::path::Path;

use crate::tty::{Level, Notice};

use super::{migrate_config_text, migrate_persona_text, Migration};

/// Read and atomically migrate a persona. The caller owns presentation of the
/// report, including when a later persona parse fails.
pub fn read_persona_file(
    path: &Path,
    report: &mut dyn FnMut(Notice<'static>),
) -> std::io::Result<String> {
    read_migrating(
        path,
        "persona",
        migrate_persona_text,
        true,
        |p| std::fs::read_to_string(p),
        |p, text| crate::atomic_fs::atomic_write(p, text.as_bytes()),
        report,
    )
}

/// Read config under the same resolved-destination lock as Config::save.
/// A lock failure and the resulting in-memory load form one report. The lock
/// remains held through revalidation and replacement, exactly as before.
pub fn read_config_file(
    path: &Path,
    rewrite: bool,
    report: &mut dyn FnMut(Notice<'static>),
) -> std::io::Result<String> {
    let locked = rewrite.then(|| {
        let destination = crate::atomic_fs::ResolvedPath::resolve(path)?;
        let lock = crate::atomic_fs::acquire_lock(&destination.lock_path())?;
        anyhow::Ok((destination, lock))
    });
    let (locked, lock_failure) = match locked.transpose() {
        Ok(locked) => (locked, None),
        Err(error) => (
            None,
            Some(format!(
                "cannot lock config {} for migration: {error:#}",
                path.display()
            )),
        ),
    };
    let destination = locked.as_ref().map(|(destination, _)| destination);
    // Display the caller's path; operate on the resolved one. Canonicalization
    // can add a verbatim `\\?\` prefix (Windows), expand 8.3 short names, or
    // follow a symlinked directory, and the operator should be shown the path
    // they actually named.
    let operated = destination.map_or(path, crate::atomic_fs::ResolvedPath::as_path);
    let mut reported = false;
    let result = read_migrating(
        path,
        "config",
        migrate_config_text,
        destination.is_some(),
        |_| std::fs::read_to_string(operated),
        |_, text| {
            destination
                .expect("rewrite holds the config lock")
                .atomic_write(text.as_bytes())
        },
        &mut |mut notice| {
            if let Some(error) = &lock_failure {
                notice.text = format!("{}; {error}", notice.text).into();
            }
            reported = true;
            report(notice);
        },
    );
    if let Some(error) = lock_failure.filter(|_| !reported) {
        let loaded = match &result {
            Ok(_) => "loaded original text in memory; the file was not rewritten".to_string(),
            Err(read_error) => {
                format!("could not read original text: {read_error}; the file was not rewritten")
            }
        };
        report(Notice::new(
            Level::Warn,
            "",
            format!("newt: warning: {error}; {loaded}"),
        ));
    }
    result
}

/// Injected filesystem seam. Reporting does not own stdout, stderr, a tracing
/// subscriber, or a terminal. A host may retain the value until a safe point.
pub(super) fn read_migrating(
    path: &Path,
    kind: &str,
    migrate: fn(&str) -> Option<Migration>,
    rewrite: bool,
    mut read: impl FnMut(&Path) -> std::io::Result<String>,
    write: impl FnOnce(&Path, &str) -> anyhow::Result<()>,
    report: &mut dyn FnMut(Notice<'static>),
) -> std::io::Result<String> {
    let raw = read(path)?;
    let Some(migration) = migrate(&raw) else {
        return Ok(raw);
    };
    let changes = migration.changes.join(", ");
    let shown = path.display();
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
    match read(path) {
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
    let notice = match write(path, &migration.text) {
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
             loading translated original text, but migration write did not complete: {error:#}"
            ),
        ),
    };
    report(notice);
    Ok(migration.text)
}

#[cfg(test)]
#[path = "psyche_import_notice_tests.rs"]
mod tests;
