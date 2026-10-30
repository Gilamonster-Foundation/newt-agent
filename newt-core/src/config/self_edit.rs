//! #144: newt proposes an edit to its own `config.toml`; the operator applies it.
//!
//! [`Config::with_config_edit`] is the pure half: a comment-preserving
//! `toml_edit` rewrite that refuses keys the schema does not know.
//! [`Config::apply_config_edit`] is the fs half: under the config lock it
//! refuses a file that changed since the operator saw the diff, backs the
//! file up to `<path>.bak`, then writes atomically.

use std::path::Path;

use super::Config;
use crate::error::{NewtError, Result};

/// One proposed edit: set (or, with `append`, extend) the value at a dotted
/// key path such as `shell.env_passthrough`.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigEdit {
    pub key: String,
    pub value: serde_json::Value,
    pub append: bool,
}

impl Config {
    /// Apply `edit` to config `text`, keeping comments and layout. Append
    /// dedupes. Refuses a key the schema does not know, a value of the wrong
    /// type, and an edit that would replace a whole table.
    pub fn with_config_edit(text: &str, edit: &ConfigEdit) -> Result<String> {
        let segments: Vec<&str> = edit.key.split('.').collect();
        if segments.iter().any(|segment| {
            segment.is_empty()
                || !segment
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        }) {
            return Err(invalid(format!(
                "'{}' is not a dotted config key",
                edit.key
            )));
        }
        let updated = edit_document(text, &segments, toml_value(&edit.value)?, edit.append)?;
        require_known_key(&updated, &segments)?;
        Ok(updated)
    }

    /// Write `edit` to the config at `path` if, under the config lock, the file
    /// still reads as `expected` (what the operator was shown). Returns the
    /// text written.
    pub fn apply_config_edit(path: &Path, expected: &str, edit: &ConfigEdit) -> Result<String> {
        let mut written = String::new();
        Self::update_config_file(path, |current| {
            if current != expected {
                return Err(invalid(format!(
                    "{} changed after the edit was shown; config unchanged",
                    path.display()
                )));
            }
            written = Self::with_config_edit(current, edit)?;
            Ok(written.clone())
        })?;
        Ok(written)
    }

    /// The one locked read-modify-write for `config.toml`: resolve the path,
    /// take `config.toml.lock`, read (missing = empty), rewrite, copy the old
    /// file to `<path>.bak`, then write atomically.
    pub(super) fn update_config_file(
        path: &Path,
        update: impl FnOnce(&str) -> Result<String>,
    ) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(NewtError::Io)?;
        }
        let destination = crate::atomic_fs::ResolvedPath::resolve(path).map_err(|error| {
            NewtError::Config(format!(
                "resolve config destination for {}: {error:#}",
                path.display()
            ))
        })?;
        let _lock = crate::atomic_fs::acquire_lock(&destination.lock_path())
            .map_err(|error| NewtError::Config(format!("lock {}: {error:#}", path.display())))?;
        let text = match std::fs::read_to_string(destination.as_path()) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(NewtError::Io(error)),
        };
        let updated = update(&text)?;
        if let Ok(metadata) = std::fs::metadata(destination.as_path()) {
            write_backup(destination.as_path(), &text, &metadata.permissions()).map_err(
                |error| NewtError::Config(format!("back up {}: {error:#}", path.display())),
            )?;
        }
        destination
            .atomic_write(updated.as_bytes())
            .map_err(|error| NewtError::Config(format!("write {}: {error:#}", path.display())))
    }
}

fn invalid(message: String) -> NewtError {
    NewtError::Config(message)
}

/// Set or append `value` at `segments` in `text`. Errors carry no config text:
/// a TOML diagnostic quotes the source, and the source can hold secrets.
fn edit_document(
    text: &str,
    segments: &[&str],
    value: toml_edit::Value,
    append: bool,
) -> Result<String> {
    let key = segments.join(".");
    let (last, parents) = segments.split_last().expect("split yields a segment");
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|_| invalid("config is not valid TOML".into()))?;
    let mut table: &mut dyn toml_edit::TableLike = doc.as_table_mut();
    for segment in parents {
        table = table
            .entry(segment)
            .or_insert_with(|| {
                let mut created = toml_edit::Table::new();
                created.set_implicit(true);
                toml_edit::Item::Table(created)
            })
            .as_table_like_mut()
            .ok_or_else(|| invalid(format!("'{segment}' is not a table")))?;
    }
    match table.get_mut(last) {
        Some(item) if item.is_table_like() => {
            return Err(invalid(format!("'{key}' is a table; name a key inside it")))
        }
        Some(item) if append => {
            let array = item
                .as_array_mut()
                .ok_or_else(|| invalid(format!("'{key}' is not a list")))?;
            append_new(array, value);
        }
        Some(item) => {
            let mut value = value;
            if let Some(previous) = item.as_value() {
                *value.decor_mut() = previous.decor().clone();
            }
            *item = toml_edit::Item::Value(value);
        }
        None => {
            let value = if append {
                let mut array = toml_edit::Array::new();
                append_new(&mut array, value);
                array.into()
            } else {
                value
            };
            table.insert(last, toml_edit::Item::Value(value));
        }
    }
    Ok(doc.to_string())
}

/// Write `<config>.bak` by staging a fresh file and renaming it over the
/// entry, so an existing `.bak` symlink or hard link is replaced, never
/// written through.
fn write_backup(
    config: &Path,
    text: &str,
    permissions: &std::fs::Permissions,
) -> anyhow::Result<()> {
    let backup = crate::atomic_fs::ResolvedPath::resolve_entry(&backup_path(config))?;
    let staged = backup.stage_with_permissions(text.as_bytes(), Some(permissions), true)?;
    if let Err(error) = backup.durable_replace(&staged) {
        let _ = std::fs::remove_file(&staged);
        return Err(error.into());
    }
    Ok(())
}

/// A JSON scalar or list as a TOML value. Objects and null have no edit
/// meaning here.
fn toml_value(json: &serde_json::Value) -> Result<toml_edit::Value> {
    use serde_json::Value as Json;
    Ok(match json {
        Json::String(text) => text.as_str().into(),
        Json::Bool(flag) => (*flag).into(),
        Json::Number(number) => match (number.as_i64(), number.as_f64()) {
            (Some(integer), _) => integer.into(),
            (None, Some(float)) => float.into(),
            (None, None) => return Err(invalid(format!("{number} is out of range"))),
        },
        Json::Array(items) => {
            let mut array = toml_edit::Array::new();
            for item in items {
                array.push(toml_value(item)?);
            }
            array.into()
        }
        Json::Null | Json::Object(_) => {
            return Err(invalid(
                "value must be a string, number, boolean or list".into(),
            ))
        }
    })
}

/// Push each entry of `value` (or `value` itself) not already in `array`.
/// Entries compare by value, not spelling: `'PATH'` and `"PATH"` are one.
fn append_new(array: &mut toml_edit::Array, value: toml_edit::Value) {
    let items = match value {
        toml_edit::Value::Array(items) => items.into_iter().collect(),
        single => vec![single],
    };
    for item in items {
        if !array.iter().any(|existing| same_value(existing, &item)) {
            array.push(item);
        }
    }
}

fn same_value(a: &toml_edit::Value, b: &toml_edit::Value) -> bool {
    use toml_edit::Value as V;
    match (a, b) {
        (V::String(a), V::String(b)) => a.value() == b.value(),
        (V::Integer(a), V::Integer(b)) => a.value() == b.value(),
        (V::Float(a), V::Float(b)) => a.value() == b.value(),
        (V::Boolean(a), V::Boolean(b)) => a.value() == b.value(),
        (V::Array(a), V::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b.iter()).all(|(a, b)| same_value(a, b))
        }
        (a, b) => a.to_string().trim() == b.to_string().trim(),
    }
}

/// The schema's own answer to "is this a key". `Config` ignores unknown keys,
/// so an unknown key parses with ANY value and never serializes back. A known
/// key either serializes back, or rejects (or keeps) one of a few probe values
/// of different types; probing makes the check independent of a value that
/// `skip_serializing_if` drops, such as `[]` or `""`.
fn require_known_key(text: &str, segments: &[&str]) -> Result<()> {
    let key = segments.join(".");
    let present = |text: &str| -> Option<()> {
        let config: Config = toml::from_str(text).ok()?;
        let mut value = toml::Value::try_from(&config).ok()?;
        for segment in segments {
            value = value.get(segment)?.clone();
        }
        Some(())
    };
    if toml::from_str::<Config>(text).is_err() {
        return Err(invalid(format!(
            "'{key}' would make the config invalid (wrong type or value)"
        )));
    }
    if present(text).is_some() {
        return Ok(());
    }
    let probes: [toml_edit::Value; 4] = [
        "newt-probe".into(),
        1.into(),
        true.into(),
        toml_edit::Array::from_iter(["newt-probe"]).into(),
    ];
    for probe in probes {
        let probed = edit_document(text, segments, probe, false)?;
        if toml::from_str::<Config>(&probed).is_err() || present(&probed).is_some() {
            return Ok(());
        }
    }
    Err(invalid(format!("'{key}' is not a config key")))
}

/// `<path>.bak`, beside the config it backs up.
#[must_use]
pub fn backup_path(path: &Path) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".bak");
    name.into()
}
