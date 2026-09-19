//! One-time importer for the psyche vocabulary change: the "rewrite once"
//! step of `docs/design/psyche-effort-dials.md` (*Migration*).
//!
//! Slice 1a renamed the cognition levels. Slice 1b split tenacity: its
//! read-before-acting half became the initiative dial, and persona and config
//! tenacity went away (tenacity is now set only explicitly). A persona file or
//! config written before either change is rewritten once, on load, and the
//! importer reports what it changed. After that the file holds only the new
//! vocabulary, and the strict parsers never see the old one.
//!
//! **Deletable after one release.** The level parsers accept only the new
//! labels; they read [`renamed_cognition`] and [`split_tenacity`] only to word
//! their errors.

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

/// A file's text after migration, with one line per change made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Migration {
    pub text: String,
    pub changes: Vec<String>,
}

/// Migrate a persona's `+++` front-matter: rewrite a pre-rename top-level
/// `cognition` value, and turn a top-level `tenacity` entry into `initiative`
/// (persona tenacity no longer exists; if the file already names an
/// initiative, the tenacity line is dropped).
///
/// Only the edited bytes change; comments, alignment, every other key and the
/// body are kept byte-for-byte. `None` when there is nothing to change: no
/// front-matter, no old cognition label, no `tenacity` key, or front-matter
/// that does not parse (the strict parser reports that on use). Migrated text
/// yields `None`, so the rewrite is idempotent.
#[must_use]
pub fn migrate_persona_text(text: &str) -> Option<Migration> {
    let fm = crate::markup::split_newt_metadata(text)
        .ok()?
        .front_matter?;
    // The front matter starts right after the opening fence line.
    let start = text.find('\n')? + 1;
    debug_assert_eq!(text.get(start..start + fm.len()), Some(fm));
    let doc = toml_edit::Document::parse(fm).ok()?;
    let root = doc.as_table();
    let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    let mut changes = Vec::new();
    if let Some(value) = root.get("cognition").and_then(toml_edit::Item::as_value) {
        if let (Some(old), Some(span)) = (value.as_str(), value.span()) {
            if let Some(new) = renamed_cognition(old) {
                edits.push((span, format!("\"{new}\"")));
                changes.push(format!("cognition '{old}' -> '{new}'"));
            }
        }
    }
    if let Some((key, item)) = root.get_key_value("tenacity") {
        if let (Some(key_span), Some(value_span)) = (key.span(), item.span()) {
            let old = item.as_str().unwrap_or("?");
            match split_tenacity(old).filter(|_| !root.contains_key("initiative")) {
                Some(new) => {
                    edits.push((key_span, "initiative".to_string()));
                    edits.push((value_span, format!("\"{new}\"")));
                    changes.push(format!("tenacity '{old}' -> initiative '{new}'"));
                }
                None => {
                    edits.push((
                        line_around(fm, key_span.start, value_span.end),
                        String::new(),
                    ));
                    changes.push(format!(
                        "dropped tenacity '{old}' (persona tenacity no longer exists)"
                    ));
                }
            }
        }
    }
    if edits.is_empty() {
        return None;
    }
    // Apply back to front so earlier spans stay valid.
    edits.sort_by_key(|(span, _)| std::cmp::Reverse(span.start));
    let mut out = text.to_string();
    for (span, new) in edits {
        out.replace_range(start + span.start..start + span.end, &new);
    }
    Some(Migration { text: out, changes })
}

/// The whole line(s) holding `from..to`, newline included.
fn line_around(text: &str, from: usize, to: usize) -> std::ops::Range<usize> {
    let begin = text[..from].rfind('\n').map_or(0, |i| i + 1);
    let end = text[to..].find('\n').map_or(text.len(), |i| to + i + 1);
    begin..end
}

/// Migrate a config file's pre-split `[tenacity]` `default` and `families`
/// into `[initiative]`, mapping each level through [`LEGACY_TENACITY`]. Other
/// `[tenacity]` keys are left alone, and the table is dropped once empty. An
/// `[initiative]` value the file already sets wins over the moved one.
///
/// Comments and formatting are kept (`toml_edit`); when nothing else shares
/// the table it is renamed in place. `None` when there is nothing to move, or
/// the text does not parse (the typed decode reports that).
#[must_use]
pub fn migrate_config_text(text: &str) -> Option<Migration> {
    use toml_edit::{DocumentMut, Item, Table};
    let mut doc: DocumentMut = text.parse().ok()?;
    let tenacity = doc.get("tenacity")?.as_table_like()?;
    if !tenacity.contains_key("default") && !tenacity.contains_key("families") {
        return None;
    }
    let only_moved = tenacity
        .iter()
        .all(|(k, _)| k == "default" || k == "families");
    let mut moved = if only_moved && !doc.contains_key("initiative") {
        doc.remove("tenacity")?
    } else {
        let tenacity = doc.get_mut("tenacity")?.as_table_like_mut()?;
        let mut table = Table::new();
        for key in ["default", "families"] {
            if let Some(item) = tenacity.remove(key) {
                table.insert(key, item);
            }
        }
        if tenacity.is_empty() {
            doc.remove("tenacity");
        }
        Item::Table(table)
    };
    let mut changes = Vec::new();
    let moved_table = moved.as_table_like_mut()?;
    if let Some(item) = moved_table.get_mut("default") {
        map_tenacity_value(item, "default", &mut changes);
    }
    if let Some(families) = moved_table
        .get_mut("families")
        .and_then(Item::as_table_like_mut)
    {
        for (family, item) in families.iter_mut() {
            map_tenacity_value(item, &format!("families.{}", family.get()), &mut changes);
        }
    }
    match doc.get_mut("initiative").and_then(Item::as_table_like_mut) {
        None => {
            doc.insert("initiative", moved);
        }
        Some(initiative) => {
            let moved = moved.as_table_like_mut()?;
            if let Some(default) = moved.remove("default") {
                if initiative.contains_key("default") {
                    changes.push("kept the existing [initiative] default".to_string());
                } else {
                    initiative.insert("default", default);
                }
            }
            if let Some(mut families) = moved.remove("families") {
                let target = initiative
                    .entry("families")
                    .or_insert(Item::Table(Table::new()))
                    .as_table_like_mut()?;
                for (family, item) in families.as_table_like_mut()?.iter_mut() {
                    if !target.contains_key(family.get()) {
                        target.insert(family.get(), std::mem::take(item));
                    }
                }
            }
        }
    }
    Some(Migration {
        text: doc.to_string(),
        changes,
    })
}

/// Rewrite one old tenacity label to its initiative level, keeping the
/// value's comments. An unknown value is left for the typed decode to refuse.
fn map_tenacity_value(item: &mut toml_edit::Item, key: &str, changes: &mut Vec<String>) {
    let Some(value) = item.as_value_mut() else {
        return;
    };
    let Some(new) = value.as_str().and_then(split_tenacity) else {
        return;
    };
    changes.push(format!(
        "tenacity.{key} '{}' -> initiative.{key} '{new}'",
        value.as_str().unwrap_or_default()
    ));
    let decor = value.decor().clone();
    *value = toml_edit::Value::from(new);
    *value.decor_mut() = decor;
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
    read_migrating(
        path,
        "persona",
        migrate_persona_text,
        true,
        |p| std::fs::read_to_string(p),
        |p, text| crate::atomic_fs::atomic_write(p, text.as_bytes()),
    )
}

/// Read a config file, migrating it: rewritten in place when `rewrite` (the
/// operator's own config), otherwise translated in memory with a warning,
/// because a project or ambient config is shared or untrusted and `/etc` is
/// the system's.
///
/// # Errors
///
/// Only the read's own I/O error.
pub fn read_config_file(path: &Path, rewrite: bool) -> std::io::Result<String> {
    read_migrating(
        path,
        "config",
        migrate_config_text,
        rewrite,
        |p| std::fs::read_to_string(p),
        |p, text| crate::atomic_fs::atomic_write(p, text.as_bytes()),
    )
}

/// The shared read-then-migrate step, with the filesystem injected so the
/// unit tier is fs-free.
fn read_migrating(
    path: &Path,
    kind: &str,
    migrate: fn(&str) -> Option<Migration>,
    rewrite: bool,
    read: impl FnOnce(&Path) -> std::io::Result<String>,
    write: impl FnOnce(&Path, &str) -> anyhow::Result<()>,
) -> std::io::Result<String> {
    let raw = read(path)?;
    let Some(migration) = migrate(&raw) else {
        return Ok(raw);
    };
    let changes = migration.changes.join(", ");
    let shown = path.display();
    if !rewrite {
        eprintln!(
            "newt: warning: {kind} {shown} uses old psyche labels ({changes}); \
             using the new ones in memory, the file is unchanged"
        );
        return Ok(migration.text);
    }
    match write(path, &migration.text) {
        Ok(()) => eprintln!("newt: migrated {kind} {shown}: {changes}"),
        Err(e) => eprintln!(
            "newt: warning: {kind} {shown} uses old psyche labels ({changes}); loaded the new \
             labels but could not rewrite the file: {e}"
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
tenacity  = \"relentless\"   # push
+++

# Bob

Body keeps cognition = \"contemplating\" and tenacity = \"relaxed\" as prose.
";

    /// Both old keys in one pass: `OLD` after the 1a and 1b rewrites.
    fn migrated_old() -> String {
        OLD.replacen("'contemplating'", "\"meticulous\"", 1)
            .replacen("tenacity  = \"relentless\"", "initiative  = \"eager\"", 1)
    }

    fn read_with(
        path: &Path,
        raw: &str,
        rewrite: bool,
        write: impl FnOnce(&Path, &str) -> anyhow::Result<()>,
    ) -> std::io::Result<String> {
        read_migrating(
            path,
            "persona",
            migrate_persona_text,
            rewrite,
            |_| Ok(raw.to_string()),
            write,
        )
    }

    #[test]
    fn migrates_the_values_and_preserves_everything_else() {
        let m = migrate_persona_text(OLD).expect("old labels migrate");
        assert_eq!(m.text, migrated_old(), "only the edited bytes change");
        assert_eq!(
            m.changes,
            [
                "cognition 'contemplating' -> 'meticulous'",
                "tenacity 'relentless' -> initiative 'eager'"
            ]
        );
        let rp = crate::RoleProfile::parse(&m.text).expect("strict parse accepts the result");
        assert_eq!(
            rp.cognition,
            Some(crate::role_profile::Cognition::Meticulous)
        );
        assert_eq!(rp.initiative, Some(crate::initiative::Initiative::Eager));
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
        for (old, new) in LEGACY_TENACITY {
            let text = format!("+++\ntenacity = \"{old}\"\n+++\nBody.\n");
            let m = migrate_persona_text(&text).unwrap();
            assert_eq!(m.text, format!("+++\ninitiative = \"{new}\"\n+++\nBody.\n"));
            assert_eq!(
                new.parse::<crate::initiative::Initiative>()
                    .unwrap()
                    .label(),
                new
            );
        }
    }

    /// Regression (slice 1b): persona tenacity no longer exists, so a
    /// `tenacity` key goes even when it cannot map: a file that already names
    /// an initiative keeps it, and an unknown level is dropped, not kept inert.
    #[test]
    fn a_tenacity_key_is_dropped_when_initiative_is_already_set_or_it_cannot_map() {
        let both = "+++\ninitiative = \"patient\"\ntenacity = \"insistent\"  # old\nrole = \"x\"\n+++\nB.\n";
        let m = migrate_persona_text(both).unwrap();
        assert_eq!(
            m.text,
            "+++\ninitiative = \"patient\"\nrole = \"x\"\n+++\nB.\n"
        );
        assert_eq!(
            m.changes,
            ["dropped tenacity 'insistent' (persona tenacity no longer exists)"]
        );
        let unknown = "+++\ntenacity = \"normal\"\n+++\nB.\n";
        assert_eq!(
            migrate_persona_text(unknown).unwrap().text,
            "+++\n+++\nB.\n"
        );
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
            "+++\ncognition = \"zen\"\ninitiative = \"eager\"\n+++\nBody.\n",
            "+++\nrole = \"x\"\n+++\ncognition = \"pondering\"\ntenacity = \"relaxed\"\n",
            "No front-matter; cognition = \"pondering\".\n",
            "+++\ncognition = \"pondering\n+++\nBody.\n",
        ] {
            assert_eq!(migrate_persona_text(text), None, "{text}");
        }
    }

    #[test]
    fn handles_crlf_and_bom() {
        let text = "\u{feff}+++\r\ncognition = \"glancing\"\r\ntenacity = \"standard\"\r\n+++\r\nBody.\r\n";
        let m = migrate_persona_text(text).unwrap();
        assert_eq!(
            m.text,
            "\u{feff}+++\r\ncognition = \"zen\"\r\ninitiative = \"measured\"\r\n+++\r\nBody.\r\n"
        );
    }

    const OLD_CONFIG: &str = "\
default_backend = \"sol\"

# how hard to push
[tenacity]
default = \"insistent\"   # most models

[tenacity.families]
nemotron = \"relentless\"  # small
qwen3 = \"standard\"
";

    /// Regression (slice 1b): a pre-split config moves to `[initiative]` with
    /// its comments. Before the importer the typed decode of `[tenacity]
    /// default = \"insistent\"` failed the whole config load.
    #[test]
    fn config_tenacity_table_is_renamed_to_initiative_with_mapped_levels() {
        let m = migrate_config_text(OLD_CONFIG).expect("old table migrates");
        assert_eq!(
            m.text,
            OLD_CONFIG
                .replace("[tenacity]", "[initiative]")
                .replace("[tenacity.families]", "[initiative.families]")
                .replace("\"insistent\"", "\"decisive\"")
                .replace("\"relentless\"", "\"eager\"")
                .replace("\"standard\"", "\"measured\"")
        );
        assert_eq!(
            m.changes,
            [
                "tenacity.default 'insistent' -> initiative.default 'decisive'",
                "tenacity.families.nemotron 'relentless' -> initiative.families.nemotron 'eager'",
                "tenacity.families.qwen3 'standard' -> initiative.families.qwen3 'measured'",
            ]
        );
        let cfg: crate::config::Config = toml::from_str(&m.text).unwrap();
        let initiative = cfg.initiative.expect("[initiative] decoded");
        assert_eq!(
            initiative.resolve(Some("nemotron")),
            crate::initiative::Initiative::Eager
        );
        assert_eq!(
            initiative.resolve(None),
            crate::initiative::Initiative::Decisive
        );
        assert_eq!(migrate_config_text(&m.text), None, "idempotent");
    }

    #[test]
    fn config_merge_keeps_existing_initiative_values_and_other_tenacity_keys() {
        let text = "\
[initiative]
default = \"patient\"
[initiative.families]
qwen3 = \"eager\"
[tenacity]
default = \"standard\"
budgets = 3
[tenacity.families]
qwen3 = \"relaxed\"
kimi = \"insistent\"
";
        let m = migrate_config_text(text).unwrap();
        let v: toml::Value = toml::from_str(&m.text).unwrap();
        assert_eq!(v["initiative"]["default"].as_str(), Some("patient"));
        assert_eq!(v["initiative"]["families"]["qwen3"].as_str(), Some("eager"));
        assert_eq!(
            v["initiative"]["families"]["kimi"].as_str(),
            Some("decisive")
        );
        assert_eq!(
            v["tenacity"]
                .as_table()
                .map(|t| t.keys().map(String::as_str).collect::<Vec<_>>()),
            Some(vec!["budgets"]),
            "keys the split does not own stay put"
        );
        assert_eq!(
            migrate_config_text("[tenacity]\nbudgets = 3\n"),
            None,
            "nothing to move"
        );
        assert_eq!(migrate_config_text("default_backend = \"x\"\n"), None);
    }

    #[test]
    fn read_writes_back_once_through_the_seam() {
        let writes = RefCell::new(Vec::new());
        let path = Path::new("/personas/bob.md");
        let got = read_with(path, OLD, true, |p, text| {
            writes
                .borrow_mut()
                .push((p.to_path_buf(), text.to_string()));
            Ok(())
        })
        .unwrap();
        assert_eq!(got, migrated_old());
        assert_eq!(*writes.borrow(), [(path.to_path_buf(), got.clone())]);

        // The migrated file reads back with no second write.
        let got_again = read_with(path, &got, true, |_, _| {
            panic!("an already-migrated file must not be rewritten")
        })
        .unwrap();
        assert_eq!(got_again, got);
    }

    #[test]
    fn a_file_that_must_not_be_rewritten_migrates_in_memory_only() {
        let got = read_migrating(
            Path::new("/repo/.newt/config.toml"),
            "config",
            migrate_config_text,
            false,
            |_| Ok(OLD_CONFIG.to_string()),
            |_, _| panic!("a shared config is never rewritten"),
        )
        .unwrap();
        assert!(got.contains("[initiative]"), "{got}");
    }

    #[test]
    fn a_failed_write_still_returns_the_migrated_text() {
        let got = read_with(Path::new("/ro/bob.md"), OLD, true, |_, _| {
            Err(anyhow::anyhow!("read-only filesystem"))
        })
        .unwrap();
        assert_eq!(got, migrated_old());
    }

    #[test]
    fn a_read_error_propagates_without_a_write() {
        let err = read_migrating(
            Path::new("/missing.md"),
            "persona",
            migrate_persona_text,
            true,
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
        assert_eq!(got, migrated_old());
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["bob.md"], "no temp file or lock left behind");
        assert_eq!(read_persona_file(&path).unwrap(), got, "idempotent on disk");
    }
}
