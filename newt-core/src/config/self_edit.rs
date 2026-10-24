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
        let (last, parents) = segments.split_last().expect("split yields a segment");
        let mut doc = text
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| invalid(format!("config is not valid TOML: {e}")))?;
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
        let value = toml_value(&edit.value)?;
        match table.get_mut(last) {
            Some(item) if item.is_table_like() => {
                return Err(invalid(format!(
                    "'{}' is a table; name a key inside it",
                    edit.key
                )))
            }
            Some(item) if edit.append => {
                let array = item
                    .as_array_mut()
                    .ok_or_else(|| invalid(format!("'{}' is not a list", edit.key)))?;
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
                let value = if edit.append {
                    let mut array = toml_edit::Array::new();
                    append_new(&mut array, value);
                    array.into()
                } else {
                    value
                };
                table.insert(last, toml_edit::Item::Value(value));
            }
        }
        let updated = doc.to_string();
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
        if destination.as_path().exists() {
            // `copy` keeps the file mode, so a private config stays private.
            std::fs::copy(destination.as_path(), backup_path(destination.as_path()))
                .map_err(NewtError::Io)?;
        }
        destination
            .atomic_write(updated.as_bytes())
            .map_err(|error| NewtError::Config(format!("write {}: {error:#}", path.display())))
    }
}

fn invalid(message: String) -> NewtError {
    NewtError::Config(message)
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
fn append_new(array: &mut toml_edit::Array, value: toml_edit::Value) {
    let bare = |value: &toml_edit::Value| {
        let mut value = value.clone();
        value.decor_mut().clear();
        value.to_string()
    };
    let items = match value {
        toml_edit::Value::Array(items) => items.into_iter().collect(),
        single => vec![single],
    };
    for item in items {
        if !array.iter().any(|existing| bare(existing) == bare(&item)) {
            array.push(item);
        }
    }
}

/// The schema's own answer to "is this a key": the edited text must parse as
/// `Config`, and the key must survive a serialize round trip. A key `Config`
/// ignores is dropped by that trip.
fn require_known_key(text: &str, segments: &[&str]) -> Result<()> {
    let config: Config =
        toml::from_str(text).map_err(|e| invalid(format!("edit makes config invalid: {e}")))?;
    let mut value =
        toml::Value::try_from(&config).map_err(|e| invalid(format!("serialize config: {e}")))?;
    for segment in segments {
        value = value
            .get(segment)
            .cloned()
            .ok_or_else(|| invalid(format!("'{}' is not a config key", segments.join("."))))?;
    }
    Ok(())
}

/// `<path>.bak`, beside the config it backs up.
#[must_use]
pub fn backup_path(path: &Path) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".bak");
    name.into()
}
