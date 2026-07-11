//! #1082 roadmap-as-code: the on-repo TOML codec for a roadmap.
//!
//! The `roadmaps` table in the conversation store is a per-machine *working
//! copy*; the file this module reads and writes — checked into the repo at
//! [`DEFAULT_ROADMAP_FILE`] — is the *authority*. `/roadmap export` serializes
//! the active roadmap here; `/roadmap import` on a fresh checkout bootstraps
//! the working copy back (upsert by roadmap id, so re-import updates in
//! place). This module is the pure codec only: no filesystem, no store —
//! those edges live at the command layer in `newt-tui`.

use serde::{Deserialize, Serialize};

use crate::plan::Plan;

/// The on-file schema version. Independent of the store's
/// `ROADMAP_SCHEMA_VERSION` column (same tree shape today, but the file
/// format and the DB column can evolve separately); bumped only on a
/// forward-incompatible change to this file's shape.
pub const ROADMAP_FILE_SCHEMA_VERSION: i64 = 1;

/// Where the roadmap file lives inside a workspace, alongside the repo's
/// other checked-in newt state (`.newt/bundled-skills/`, …).
pub const DEFAULT_ROADMAP_FILE: &str = ".newt/roadmap.toml";

/// A roadmap as it appears on disk: id + title + the Roadmap→Phase→Plan→Task
/// tree. `deny_unknown_fields` keeps the schema loud, matching `plan.rs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoadmapFile {
    /// [`ROADMAP_FILE_SCHEMA_VERSION`] at write time; imports refuse a
    /// version newer than they understand rather than mis-reading it.
    pub schema_version: i64,
    /// The roadmap's store id, preserved across export/import so the repo
    /// file and every machine's working copy name the same roadmap.
    pub id: String,
    /// Human title, as shown by `/roadmap list`.
    pub title: String,
    /// The node tree. Serialized under `[tree]` / `[[tree.subtask]]`.
    pub tree: Plan,
}

impl RoadmapFile {
    /// Wrap a store roadmap for export, stamping the current schema version.
    #[must_use]
    pub fn new(id: impl Into<String>, title: impl Into<String>, tree: Plan) -> Self {
        Self {
            schema_version: ROADMAP_FILE_SCHEMA_VERSION,
            id: id.into(),
            title: title.into(),
            tree,
        }
    }

    /// Serialize to the on-repo TOML text.
    ///
    /// # Errors
    /// Returns the `toml` serialization error (should not occur for a
    /// well-formed tree).
    pub fn to_toml_string(&self) -> anyhow::Result<String> {
        Ok(toml::to_string_pretty(self)?)
    }

    /// Parse and validate on-repo TOML text. Fail-loud by design: a malformed
    /// or future-versioned file is a hard error so an import never writes a
    /// garbage tree into the store.
    ///
    /// # Errors
    /// On TOML that does not match the schema, a `schema_version` newer than
    /// this build understands, an id outside the record-id alphabet
    /// (ASCII alphanumeric + `-`), or a blank title.
    pub fn from_toml_str(s: &str) -> anyhow::Result<Self> {
        let file: Self = toml::from_str(s)?;
        if file.schema_version > ROADMAP_FILE_SCHEMA_VERSION {
            anyhow::bail!(
                "roadmap file schema_version {} is newer than this newt understands ({}) — \
                 upgrade newt to import it",
                file.schema_version,
                ROADMAP_FILE_SCHEMA_VERSION
            );
        }
        if file.id.is_empty()
            || !file
                .id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            anyhow::bail!("roadmap file has invalid id `{}`", file.id);
        }
        if file.title.trim().is_empty() {
            anyhow::bail!("roadmap file has an empty title");
        }
        Ok(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{NodeKind, Subtask};

    fn demo_tree() -> Plan {
        let mut plan = Plan::default();
        plan.subtasks.push(Subtask::node(
            "node-1",
            "v0.1.0 Keys & doctrine",
            NodeKind::Phase,
            None,
        ));
        plan.subtasks.push(Subtask::node(
            "node-2",
            "pane API as cockpit MCP tools",
            NodeKind::Plan,
            Some("node-1".into()),
        ));
        plan
    }

    #[test]
    fn roadmap_file_round_trips_byte_identical() {
        let file = RoadmapFile::new("rm-1", "Gilamonster 0.x", demo_tree());
        let first = file.to_toml_string().unwrap();
        let reparsed = RoadmapFile::from_toml_str(&first).unwrap();
        assert_eq!(reparsed, file);
        let second = reparsed.to_toml_string().unwrap();
        // export → import → export is byte-identical (#1082 regression).
        assert_eq!(first, second);
    }

    #[test]
    fn import_refuses_future_schema_version() {
        let mut text = RoadmapFile::new("rm-1", "T", Plan::default())
            .to_toml_string()
            .unwrap();
        text = text.replace("schema_version = 1", "schema_version = 99");
        let err = RoadmapFile::from_toml_str(&text).unwrap_err().to_string();
        assert!(err.contains("newer than this newt"), "{err}");
    }

    #[test]
    fn import_refuses_bad_id_and_blank_title() {
        let good = RoadmapFile::new("rm-1", "T", Plan::default())
            .to_toml_string()
            .unwrap();
        let bad_id = good.replace("id = \"rm-1\"", "id = \"rm 1!\"");
        assert!(RoadmapFile::from_toml_str(&bad_id)
            .unwrap_err()
            .to_string()
            .contains("invalid id"));
        let blank_title = good.replace("title = \"T\"", "title = \"  \"");
        assert!(RoadmapFile::from_toml_str(&blank_title)
            .unwrap_err()
            .to_string()
            .contains("empty title"));
    }

    #[test]
    fn import_refuses_unknown_fields_loudly() {
        let mut text = RoadmapFile::new("rm-1", "T", Plan::default())
            .to_toml_string()
            .unwrap();
        text.push_str("\nsurprise = true\n");
        assert!(RoadmapFile::from_toml_str(&text).is_err());
    }

    #[test]
    fn import_refuses_garbage_toml() {
        assert!(RoadmapFile::from_toml_str("not = [toml").is_err());
        assert!(RoadmapFile::from_toml_str("").is_err());
    }
}
