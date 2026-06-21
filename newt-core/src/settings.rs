//! Sticky user preferences (`~/.newt/settings.toml`).
//!
//! A small, hand-editable file that records the *last-active* session
//! selections so the next `newt` invocation starts where you left off
//! (issue #545). Today that means two axes, both under `[session]`:
//!
//! ```toml
//! # newt — sticky user preferences (hand-editable).
//! [session]
//! provider = "dgx1"      # last `/backends <name>`
//! model = "gpt-4.1"      # last `/model <name>`
//! ```
//!
//! It is written automatically when you switch the backend (`/backends
//! <name>`) or the model (`/model <name>`) in the TUI, and read once at
//! startup to seed `NEWT_PROVIDER` / `NEWT_DGX_MODEL`.
//!
//! # Design
//!
//! * **Lowest precedence on restore.** An explicit `NEWT_PROVIDER` /
//!   `NEWT_DGX_MODEL` in the environment, or a `--loadout`'s provider/model
//!   axis, always wins. `settings.toml` only fills an axis nothing else
//!   pinned ([`Session::restore`]).
//! * **Defensive.** A `provider` naming a `[[backends]]` entry that no longer
//!   exists is ignored, so editing config out from under the file can never
//!   break startup.
//! * **Hand-editable + room to grow.** Reads/writes go through a dynamic
//!   [`toml::Table`], so unknown keys and tables the user (or a future newt)
//!   adds are preserved across an in-app rewrite. The one thing a rewrite
//!   does *not* preserve is inline comments — keep durable notes in
//!   `config.toml`; this file's header says so.
//!
//! This is deliberately separate from the hand-authored `~/.newt/config.toml`:
//! config is where you *declare* backends/loadouts; `settings.toml` is the
//! lean, machine-managed memory of what you last picked.

use std::path::{Path, PathBuf};

/// Path to `~/.newt/settings.toml` (sibling of `config.toml`).
pub fn settings_path() -> Option<PathBuf> {
    crate::config::Config::user_config_path().map(|p| p.with_file_name("settings.toml"))
}

/// The last-active selections read from the `[session]` table.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Session {
    /// Last `[[backends]]` name chosen via `/backends <name>`.
    pub provider: Option<String>,
    /// Last model override chosen via `/model <name>` (the `NEWT_DGX_MODEL` axis).
    pub model: Option<String>,
}

/// The startup environment a [`Session`] asks for: each field is `Some` only
/// when that axis should actually be set (nothing already pinned it, and — for
/// `provider` — the named backend still exists).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Restore {
    pub provider: Option<String>,
    pub model: Option<String>,
}

impl Session {
    /// Extract `[session]` selections from a parsed settings document. Tolerant
    /// of a missing table, missing keys, non-string values, and empty strings
    /// (all read as `None`) — a malformed file must never break startup.
    pub fn from_doc(doc: &toml::Table) -> Self {
        let session = doc.get("session").and_then(toml::Value::as_table);
        let read = |key: &str| {
            session
                .and_then(|t| t.get(key))
                .and_then(toml::Value::as_str)
                .map(str::to_string)
                .filter(|s| !s.is_empty())
        };
        Self {
            provider: read("provider"),
            model: read("model"),
        }
    }

    /// True when nothing is recorded (skip the whole restore/write dance).
    pub fn is_empty(&self) -> bool {
        self.provider.is_none() && self.model.is_none()
    }

    /// Decide what to restore. `current_provider` is the value `NEWT_PROVIDER`
    /// already holds (an explicit env var or a `--loadout`'s provider) — `None`
    /// if unset; a pinned provider is left untouched. `model_pinned` is whether
    /// `NEWT_DGX_MODEL` is already set. `known_backend` guards the recorded
    /// provider so a stale name (backend since removed from config) is dropped
    /// rather than producing a dangling pin. Pure — the caller owns the env and
    /// config lookups, keeping this fully unit-testable.
    ///
    /// A recorded model belongs to the provider it was chosen under
    /// (`self.provider`), so it is restored only when that provider is the one
    /// that will actually be active. This keeps a model picked for one backend
    /// from bleeding onto a *different* backend selected by env or a `--loadout`
    /// — matching the in-app rule that picking a provider clears the model
    /// override. (When both are `None` the model was chosen on the default
    /// backend, which is also what's active, so a bare `/model` choice still
    /// sticks.)
    pub fn restore(
        &self,
        current_provider: Option<&str>,
        model_pinned: bool,
        known_backend: impl Fn(&str) -> bool,
    ) -> Restore {
        // Provider: fill it only when nothing already pinned one and the
        // recorded name still resolves to a configured backend.
        let provider = match current_provider {
            Some(_) => None,
            None => self
                .provider
                .as_deref()
                .filter(|name| known_backend(name))
                .map(str::to_string),
        };
        // The provider that will be active once our decision lands: an existing
        // pin, else the one we just restored, else the default resolution.
        let effective = current_provider.or(provider.as_deref());
        let model = self
            .model
            .as_deref()
            .filter(|_| !model_pinned)
            .filter(|_| effective == self.provider.as_deref())
            .map(str::to_string);
        Restore { provider, model }
    }
}

/// Whether a `/backends`/`/model` switch should be persisted to
/// `settings.toml`. `false` in an ephemeral session (`--ephemeral` /
/// `NEWT_EPHEMERAL`), which must leave no trace on disk — the switch still
/// applies live for the session, it just isn't recorded. Pure mirror of the
/// `should_extract_on_close` rule; the caller computes `ephemeral` from the env.
pub fn should_persist(ephemeral: bool) -> bool {
    !ephemeral
}

/// The self-documenting header re-emitted on every write. (Comments inside the
/// body are not round-tripped through [`toml::Table`]; this header always is.)
const HEADER: &str = "\
# newt — sticky user preferences (hand-editable).
#
# Auto-updated when you switch the backend (/backends <name>) or model
# (/model <name>) in the TUI, so your last choice is restored on the next
# start. Restore is the LOWEST precedence: an explicit NEWT_PROVIDER /
# NEWT_DGX_MODEL in the environment, or a --loadout, always wins. Delete this
# file to forget the last selection.
#
# Hand-editable, but note: an in-app /backends or /model write preserves other
# keys/tables yet drops inline comments — keep durable notes in config.toml.
";

/// Set (`Some`) or clear (`None`) `session.<key>` in a settings document,
/// preserving every other key and table. Clearing the last key in `[session]`
/// drops the now-empty table so the file stays tidy. Pure (operates on the
/// passed document) so it unit-tests without touching the filesystem.
pub fn apply_session_key(doc: &mut toml::Table, key: &str, value: Option<&str>) {
    match value {
        Some(v) => {
            let entry = doc
                .entry("session".to_string())
                .or_insert_with(|| toml::Value::Table(toml::Table::new()));
            // Heal a `session` key the user mistyped as a non-table.
            if !entry.is_table() {
                *entry = toml::Value::Table(toml::Table::new());
            }
            if let Some(table) = entry.as_table_mut() {
                table.insert(key.to_string(), toml::Value::String(v.to_string()));
            }
        }
        None => {
            if let Some(toml::Value::Table(table)) = doc.get_mut("session") {
                table.remove(key);
                if table.is_empty() {
                    doc.remove("session");
                }
            }
        }
    }
}

/// Render a settings document to TOML text with the header. Returns `Err` (so
/// the caller skips the write rather than truncating the file) if the document
/// cannot be serialized.
pub fn to_toml_string(doc: &toml::Table) -> Result<String, toml::ser::Error> {
    Ok(format!("{HEADER}{}", toml::to_string_pretty(doc)?))
}

// ---------------------------------------------------------------------------
// Filesystem seam (thin; integration-tier only — never exercised by unit tests
// per the repo test-fs policy, mirroring `tuning.rs`).
// ---------------------------------------------------------------------------

/// Parse the settings document at `path` into a dynamic table. Any error
/// (absent file, parse failure) yields an empty document.
fn read_doc(path: &Path) -> toml::Table {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str::<toml::Table>(&text).ok())
        .unwrap_or_default()
}

/// Write the document back, creating `~/.newt/` if needed. A serialization
/// failure is surfaced as an error *before* any write, so a broken document
/// never truncates the file.
fn write_doc(path: &Path, doc: &toml::Table) -> std::io::Result<()> {
    let text = to_toml_string(doc).map_err(|e| std::io::Error::other(e.to_string()))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, text)
}

/// Load the recorded session selections from `~/.newt/settings.toml`. Empty on
/// any error.
pub fn load() -> Session {
    settings_path()
        .map(|path| Session::from_doc(&read_doc(&path)))
        .unwrap_or_default()
}

/// Read-modify-write `~/.newt/settings.toml`, preserving unknown content.
/// Best-effort: a path we can't resolve or a write we can't perform is a no-op.
fn update(mutate: impl FnOnce(&mut toml::Table)) {
    let Some(path) = settings_path() else {
        return;
    };
    let mut doc = read_doc(&path);
    mutate(&mut doc);
    let _ = write_doc(&path, &doc);
}

/// Persist the provider chosen via `/backends <name>`, clearing any stale model
/// override (mirrors the session's `NEWT_DGX_MODEL` reset so the named backend's
/// own default model applies on the next start). Best-effort.
pub fn record_provider(name: &str) {
    update(|doc| {
        apply_session_key(doc, "provider", Some(name));
        apply_session_key(doc, "model", None);
    });
}

/// Persist the model chosen via `/model <name>` (leaving the provider as-is).
/// Best-effort.
pub fn record_model(name: &str) {
    update(|doc| apply_session_key(doc, "model", Some(name)));
}

// ---------------------------------------------------------------------------
// Tests — pure / in-memory only (no real filesystem, per the test-fs policy).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(text: &str) -> toml::Table {
        toml::from_str(text).expect("valid toml")
    }

    #[test]
    fn from_doc_reads_session_axes() {
        let d = doc("[session]\nprovider = \"dgx1\"\nmodel = \"gpt-4.1\"\n");
        let s = Session::from_doc(&d);
        assert_eq!(s.provider.as_deref(), Some("dgx1"));
        assert_eq!(s.model.as_deref(), Some("gpt-4.1"));
        assert!(!s.is_empty());
    }

    #[test]
    fn from_doc_is_tolerant_of_missing_empty_and_wrong_types() {
        assert!(Session::from_doc(&doc("")).is_empty());
        assert!(Session::from_doc(&doc("[other]\nx = 1\n")).is_empty());
        // Empty string and non-string value both read as None.
        let d = doc("[session]\nprovider = \"\"\nmodel = 42\n");
        assert!(Session::from_doc(&d).is_empty());
    }

    /// Regression for #545 (read half): a recorded provider/model is restored
    /// when nothing else pinned the axis. Before the fix there was no such
    /// restore path, so the last choice was silently forgotten on restart.
    #[test]
    fn restore_applies_recorded_axes_when_unpinned() {
        let s = Session {
            provider: Some("dgx1".into()),
            model: Some("gpt-4.1".into()),
        };
        let r = s.restore(None, false, |n| n == "dgx1");
        assert_eq!(r.provider.as_deref(), Some("dgx1"));
        assert_eq!(r.model.as_deref(), Some("gpt-4.1"));
    }

    /// Regression for #545 (second reported case): a bare `/model` choice with
    /// no recorded provider sticks on the default-resolved backend.
    #[test]
    fn restore_applies_a_bare_model_on_the_default_backend() {
        let s = Session {
            provider: None,
            model: Some("gpt-4.1".into()),
        };
        let r = s.restore(None, false, |_| false);
        assert_eq!(r.provider, None);
        assert_eq!(r.model.as_deref(), Some("gpt-4.1"));
    }

    #[test]
    fn restore_yields_to_an_explicit_pin() {
        let s = Session {
            provider: Some("dgx1".into()),
            model: Some("gpt-4.1".into()),
        };
        // Provider pinned (to the SAME backend) + model pinned → both untouched.
        assert_eq!(s.restore(Some("dgx1"), true, |_| true), Restore::default());
        // Provider pinned to the same backend, model unpinned → model restores.
        let r = s.restore(Some("dgx1"), false, |_| true);
        assert_eq!(r.provider, None);
        assert_eq!(r.model.as_deref(), Some("gpt-4.1"));
    }

    /// Regression for the cross-provider model bleed: a model chosen for one
    /// backend must NOT be re-applied to a different backend that env or a
    /// `--loadout` pinned this run.
    #[test]
    fn restore_does_not_bleed_model_onto_a_different_pinned_provider() {
        let s = Session {
            provider: Some("openai".into()),
            model: Some("gpt-4.1".into()),
        };
        // A --loadout/env pinned dgx1; the openai-era model must not ride along.
        let r = s.restore(Some("dgx1"), false, |_| true);
        assert_eq!(r.provider, None);
        assert_eq!(r.model, None);
    }

    #[test]
    fn restore_drops_both_axes_when_the_provider_no_longer_exists() {
        let s = Session {
            provider: Some("ghost".into()),
            model: Some("gpt-4.1".into()),
        };
        // Unknown backend → provider dropped; the model belonged to that gone
        // backend, so it is dropped too rather than bleeding onto the default.
        let r = s.restore(None, false, |_| false);
        assert_eq!(r, Restore::default());
    }

    /// Regression for the ephemeral "leave no trace" invariant: switches in an
    /// ephemeral session must not be persisted.
    #[test]
    fn ephemeral_sessions_do_not_persist() {
        assert!(!should_persist(true));
        assert!(should_persist(false));
    }

    #[test]
    fn apply_sets_and_round_trips_through_text() {
        let mut d = toml::Table::new();
        apply_session_key(&mut d, "provider", Some("dgx1"));
        apply_session_key(&mut d, "model", Some("gpt-4.1"));
        let text = to_toml_string(&d).unwrap();
        assert!(text.starts_with("# newt — sticky user preferences"));
        let back = Session::from_doc(&doc(&text));
        assert_eq!(back.provider.as_deref(), Some("dgx1"));
        assert_eq!(back.model.as_deref(), Some("gpt-4.1"));
    }

    /// Regression for #545 (write half): `/backends` clears the model override.
    #[test]
    fn provider_switch_clears_the_model() {
        let mut d = doc("[session]\nprovider = \"openai\"\nmodel = \"gpt-4.1\"\n");
        apply_session_key(&mut d, "provider", Some("dgx1"));
        apply_session_key(&mut d, "model", None);
        let s = Session::from_doc(&d);
        assert_eq!(s.provider.as_deref(), Some("dgx1"));
        assert_eq!(s.model, None);
    }

    /// Hand-edit safety: an in-app rewrite preserves unknown keys/tables.
    #[test]
    fn rewrite_preserves_unknown_content() {
        let mut d =
            doc("top_pref = true\n\n[session]\nmodel = \"a\"\n\n[mine]\nnote = \"keep me\"\n");
        // Simulate `/backends dgx1`: set provider, clear model.
        apply_session_key(&mut d, "provider", Some("dgx1"));
        apply_session_key(&mut d, "model", None);
        let text = to_toml_string(&d).unwrap();
        // Serializes without a ValueAfterTable error and keeps the foreign bits.
        let back = doc(&text);
        assert_eq!(
            back.get("top_pref").and_then(toml::Value::as_bool),
            Some(true)
        );
        assert_eq!(
            back.get("mine")
                .and_then(toml::Value::as_table)
                .and_then(|t| t.get("note"))
                .and_then(toml::Value::as_str),
            Some("keep me")
        );
        assert_eq!(Session::from_doc(&back).provider.as_deref(), Some("dgx1"));
    }

    #[test]
    fn clearing_the_last_session_key_drops_the_empty_table() {
        let mut d = doc("[session]\nmodel = \"a\"\n");
        apply_session_key(&mut d, "model", None);
        assert!(!d.contains_key("session"));
    }
}
