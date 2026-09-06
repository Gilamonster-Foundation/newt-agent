//! SQLite `ConversationStore` suite — Phase 17.1a/17.1b (issue #246).
//!
//! Part 1 ports the retired JSON backend's suite unchanged semantically
//! (the backend swap must be invisible through the public API; the two
//! storage-format-specific tests are ported to their SQLite analogues),
//! plus the shared free-function tests that moved here when 17.1b deleted
//! tests/conversation_store.rs. Part 2 covers what is new in 17.1a: §6
//! causal ordering (MRU = activity tick, never a timestamp), the clock-skew
//! case, BLAKE3 chain integrity and tamper detection, two-writer
//! `busy_timeout` concurrency, and the schema-diff migration. Part 3 covers
//! 17.1b: the one-time legacy JSON import, per-row `encoding_version` (N1),
//! and byte-case-exact prefix resolution (N5).

pub(crate) use newt_core::{
    new_conversation_id, session_plan_dir, session_plan_path, ArtifactKind, ArtifactRelation,
    ConversationRecord, ConversationTurn, NewPrompt, NewPromptArtifact, PhantomReach,
    PhantomResolution, ToolEvent, MAX_ARTIFACT_BODY_BYTES, MAX_ARTIFACT_LOCATOR_BYTES,
    MAX_ARTIFACT_METADATA_BYTES,
};
// The canonical (root re-exported) store IS the SQLite backend as of 17.1a.
pub(crate) use newt_core::ConversationStore;

fn db_path(root: &std::path::Path) -> std::path::PathBuf {
    root.join("conversations.db")
}

/// Open the store's database directly — the tests' tamper/skew/inspect hatch.
fn raw(root: &std::path::Path) -> rusqlite::Connection {
    rusqlite::Connection::open(db_path(root)).unwrap()
}

fn common_prefix<'a>(a: &'a str, b: &str) -> &'a str {
    let len = a
        .bytes()
        .zip(b.bytes())
        .take_while(|(left, right)| left == right)
        .count();
    assert!(len > 0, "test ids should share the unix timestamp prefix");
    &a[..len]
}

// The families below are the ten sections this suite has always declared in
// its own banner comments; each is now its own file under `store/`. Helpers
// stay here only when more than one family calls them — the single-family
// ones (`common_prefix`, `reopen_as_new_writer`) moved with their family.
#[path = "store/fts_recall_index.rs"]
mod fts_recall_index;
#[path = "store/legacy_import_and_encoding.rs"]
mod legacy_import_and_encoding;
#[path = "store/ordering_chain_concurrency.rs"]
mod ordering_chain_concurrency;
#[path = "store/ported_public_api.rs"]
mod ported_public_api;
#[path = "store/prompt_artifacts.rs"]
mod prompt_artifacts;
#[path = "store/provenance_sources.rs"]
mod provenance_sources;
#[path = "store/store_recall_source.rs"]
mod store_recall_source;
#[path = "store/tool_events_and_tokens.rs"]
mod tool_events_and_tokens;
#[path = "store/workspace_identity_v2.rs"]
mod workspace_identity_v2;
#[path = "store/writer_witnesses.rs"]
mod writer_witnesses;

/// Write one legacy-format record exactly where the JSON backend kept it:
/// `<root>/conversations/<workspace_id>/<id>.json`, pretty-printed.
fn write_legacy_record(root: &std::path::Path, record: &ConversationRecord) {
    let dir = root.join("conversations").join(&record.workspace_id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{}.json", record.id)),
        serde_json::to_string_pretty(record).unwrap(),
    )
    .unwrap();
}

fn legacy_record(
    id: &str,
    title: &str,
    workspace: &std::path::Path,
    turns: &[(&str, &str)],
    created: u128,
    updated: u128,
) -> ConversationRecord {
    // Legacy records carry the retired v1 (UUIDv5) key in their body and
    // dir name — that is the format under test.
    #[allow(deprecated)]
    let workspace_id = ConversationStore::workspace_id_for_path(workspace).unwrap();
    ConversationRecord {
        id: id.to_string(),
        title: title.to_string(),
        workspace: workspace.to_string_lossy().into_owned(),
        workspace_id,
        persona: Some("coder".to_string()),
        turns: turns
            .iter()
            .map(|(u, a)| ConversationTurn::new(*u, *a))
            .collect(),
        scratchpad: std::collections::BTreeMap::new(),
        plan: newt_core::PlanSnapshot::default(),
        roadmap_id: None,
        node_id: None,
        created_at_unix_nanos: created,
        updated_at_unix_nanos: updated,
    }
}

/// The events the agentic loop would record for a small two-tool turn.
fn sample_events() -> Vec<ToolEvent> {
    vec![
        ToolEvent::from_call(
            "read_file",
            &serde_json::json!({"path": "src/store.rs"}),
            true,
            Some(4),
        ),
        ToolEvent::from_call(
            "run_command",
            &serde_json::json!({"command": "cargo test -q"}),
            false,
            Some(2_500),
        ),
    ]
}
