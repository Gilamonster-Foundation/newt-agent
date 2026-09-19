//! One-time importer for the psyche vocabulary rename: the "rewrite once"
//! step of `docs/design/psyche-effort-dials.md` (*Migration*).
//!
//! A persona file or a stored preference pin written before the rename
//! carries an old cognition label. The importer rewrites it to the new label
//! once, on load, and reports what it changed. After that the file holds only
//! the new vocabulary, and the strict parsers never see the old one.
//!
//! **Deletable after one release.** [`Cognition`](crate::role_profile::Cognition)
//! accepts only the new labels; its `FromStr` reads [`renamed_cognition`] only
//! to word its error, as the tenacity and initiative parsers read
//! [`split_tenacity`].

use std::path::Path;

/// Old cognition label → new label. The single legacy table: every rewrite
/// and every rename hint reads it.
pub const LEGACY_COGNITION: [(&str, &str); 4] = [
    ("glancing", "zen"),
    ("pondering", "rational"),
    ("deliberating", "thoughtful"),
    ("contemplating", "meticulous"),
];

/// Old tenacity label → the initiative level that inherits its
/// read-before-acting half. `relentless` also kept tenacity's round-cap lift,
/// but only an explicit operator choice ever used that, and the importer only
/// sees persona, config and pinned values.
pub const LEGACY_TENACITY: [(&str, &str); 4] = [
    ("relaxed", "patient"),
    ("standard", "measured"),
    ("insistent", "decisive"),
    ("relentless", "eager"),
];

fn lookup(table: &[(&str, &'static str)], old: &str) -> Option<&'static str> {
    table
        .iter()
        .find(|(legacy, _)| *legacy == old)
        .map(|(_, new)| *new)
}

/// The new label for a pre-rename cognition label, if `old` is one.
#[must_use]
pub fn renamed_cognition(old: &str) -> Option<&'static str> {
    lookup(&LEGACY_COGNITION, old)
}

/// The initiative level for a pre-split tenacity label, if `old` is one.
#[must_use]
pub fn split_tenacity(old: &str) -> Option<&'static str> {
    lookup(&LEGACY_TENACITY, old)
}

/// A persona file's text after migration, with one line per change made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonaMigration {
    pub text: String,
    pub changes: Vec<String>,
}

/// Rewrite a pre-rename top-level `cognition` value in a persona's `+++`
/// front-matter.
///
/// Only the value's bytes change; comments, alignment, every other key and
/// the body are kept byte-for-byte. `None` when there is nothing to change:
/// no front-matter, no `cognition` string, a current or unknown value, or
/// front-matter that does not parse (the strict parser reports that on use).
/// Migrated text yields `None`, so the rewrite is idempotent.
#[must_use]
pub fn migrate_persona_text(text: &str) -> Option<PersonaMigration> {
    let fm = crate::markup::split_newt_metadata(text)
        .ok()?
        .front_matter?;
    // The front matter starts right after the opening fence line.
    let start = text.find('\n')? + 1;
    debug_assert_eq!(text.get(start..start + fm.len()), Some(fm));
    let doc = toml_edit::Document::parse(fm).ok()?;
    let value = doc.as_table().get("cognition")?.as_value()?;
    let old = value.as_str()?;
    let new = renamed_cognition(old)?;
    let span = value.span()?;
    let text = format!(
        "{}\"{new}\"{}",
        &text[..start + span.start],
        &text[start + span.end..]
    );
    Some(PersonaMigration {
        text,
        changes: vec![format!("cognition '{old}' -> '{new}'")],
    })
}

/// Read a persona file, migrating it once. Every production persona read goes
/// through here. A migrated file is rewritten atomically (temp file + rename
/// in the same directory) and one stderr line names the changes. If the
/// rewrite fails the migrated text is still returned, with a warning, so the
/// persona loads either way.
///
/// # Errors
///
/// Only the read's own I/O error.
pub fn read_persona_file(path: &Path) -> std::io::Result<String> {
    read_persona_file_with(
        path,
        |p| std::fs::read_to_string(p),
        |p, text| crate::atomic_fs::atomic_write(p, text.as_bytes()),
    )
}

/// [`read_persona_file`] with the filesystem injected, so the unit tier is
/// fs-free.
fn read_persona_file_with(
    path: &Path,
    read: impl FnOnce(&Path) -> std::io::Result<String>,
    write: impl FnOnce(&Path, &str) -> anyhow::Result<()>,
) -> std::io::Result<String> {
    let raw = read(path)?;
    let Some(migration) = migrate_persona_text(&raw) else {
        return Ok(raw);
    };
    let changes = migration.changes.join(", ");
    match write(path, &migration.text) {
        Ok(()) => eprintln!("newt: migrated persona {}: {changes}", path.display()),
        Err(e) => eprintln!(
            "newt: warning: persona {} uses renamed labels ({changes}); loaded the new \
             labels but could not rewrite the file: {e}",
            path.display()
        ),
    }
    Ok(migration.text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const OLD: &str = "\
+++
role      = \"researcher\"   # who
# the dial below was renamed
cognition = 'contemplating'  # deep
tenacity  = \"relentless\"
+++

# Bob

Body keeps cognition = \"contemplating\" as prose.
";

    #[test]
    fn migrates_the_value_and_preserves_everything_else() {
        let m = migrate_persona_text(OLD).expect("old label migrates");
        let expected = OLD.replacen("'contemplating'", "\"meticulous\"", 1);
        assert_eq!(m.text, expected, "only the value's bytes change");
        assert_eq!(m.changes, ["cognition 'contemplating' -> 'meticulous'"]);
        let rp = crate::RoleProfile::parse(&m.text).expect("strict parse accepts the result");
        assert_eq!(
            rp.cognition,
            Some(crate::role_profile::Cognition::Meticulous)
        );
    }

    #[test]
    fn every_legacy_label_maps_to_a_parseable_level() {
        for (old, new) in LEGACY_COGNITION {
            let text = format!("+++\ncognition = \"{old}\"\n+++\nBody.\n");
            let m = migrate_persona_text(&text).unwrap();
            assert_eq!(m.text, format!("+++\ncognition = \"{new}\"\n+++\nBody.\n"));
            assert_eq!(
                new.parse::<crate::role_profile::Cognition>()
                    .unwrap()
                    .label(),
                new
            );
        }
    }

    #[test]
    fn is_idempotent_and_leaves_unknown_or_absent_alone() {
        let once = migrate_persona_text(OLD).unwrap().text;
        assert_eq!(
            migrate_persona_text(&once),
            None,
            "second pass changes nothing"
        );
        for text in [
            "+++\ncognition = \"telepathic\"\n+++\nBody.\n",
            "+++\ncognition = \"zen\"\n+++\nBody.\n",
            "+++\nrole = \"x\"\n+++\ncognition = \"pondering\"\n",
            "No front-matter; cognition = \"pondering\".\n",
            "+++\ncognition = \"pondering\n+++\nBody.\n",
        ] {
            assert_eq!(migrate_persona_text(text), None, "{text}");
        }
    }

    #[test]
    fn handles_crlf_and_bom() {
        let text = "\u{feff}+++\r\ncognition = \"glancing\"\r\n+++\r\nBody.\r\n";
        let m = migrate_persona_text(text).unwrap();
        assert_eq!(
            m.text,
            "\u{feff}+++\r\ncognition = \"zen\"\r\n+++\r\nBody.\r\n"
        );
    }

    #[test]
    fn read_writes_back_once_through_the_seam() {
        let writes = RefCell::new(Vec::new());
        let path = Path::new("/personas/bob.md");
        let got = read_persona_file_with(
            path,
            |_| Ok(OLD.to_string()),
            |p, text| {
                writes
                    .borrow_mut()
                    .push((p.to_path_buf(), text.to_string()));
                Ok(())
            },
        )
        .unwrap();
        assert!(got.contains("cognition = \"meticulous\""), "{got}");
        assert_eq!(*writes.borrow(), [(path.to_path_buf(), got.clone())]);

        // The migrated file reads back with no second write.
        let got_again = read_persona_file_with(
            path,
            |_| Ok(got.clone()),
            |_, _| panic!("an already-migrated file must not be rewritten"),
        )
        .unwrap();
        assert_eq!(got_again, got);
    }

    #[test]
    fn a_failed_write_still_returns_the_migrated_text() {
        let got = read_persona_file_with(
            Path::new("/ro/bob.md"),
            |_| Ok(OLD.to_string()),
            |_, _| Err(anyhow::anyhow!("read-only filesystem")),
        )
        .unwrap();
        assert!(got.contains("cognition = \"meticulous\""), "{got}");
    }

    #[test]
    fn a_read_error_propagates_without_a_write() {
        let err = read_persona_file_with(
            Path::new("/missing.md"),
            |_| Err(std::io::ErrorKind::NotFound.into()),
            |_, _| panic!("nothing to write"),
        )
        .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    /// Real-filesystem grounding for the injected write seam: the production
    /// writer replaces the file atomically and leaves no temp file behind.
    #[test]
    #[ignore = "real-resource: weekly/release tier; touches the filesystem"]
    #[serial_test::serial(real_fs)]
    fn read_persona_file_rewrites_the_real_file_atomically() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("bob.md");
        std::fs::write(&path, OLD).unwrap();
        let got = read_persona_file(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), got);
        assert!(got.contains("cognition = \"meticulous\""), "{got}");
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["bob.md"], "no temp file or lock left behind");
        assert_eq!(read_persona_file(&path).unwrap(), got, "idempotent on disk");
    }
}
