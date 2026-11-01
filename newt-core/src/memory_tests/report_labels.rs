//! #2933: providers are reported by their component id; providers that share one
//! id (the two API surfaces) keep a span each, so none of them goes unattributed.

use super::*;
use crate::agentic::request_report::{ReportSink, RequestReport};

/// A provider that contributes one fixed block to the system prompt.
struct Block(&'static str, &'static str);

#[async_trait::async_trait]
impl MemoryProvider for Block {
    fn name(&self) -> &str {
        self.0
    }
    fn system_prompt_block(&self) -> Option<String> {
        Some(self.1.to_string())
    }
    fn build_messages(&self, _system_prompt: &str, _new_task: &str) -> Vec<MemMessage> {
        Vec::new()
    }
    async fn sync_turn(&mut self, _user: &str, _assistant: &str, _metrics: &TurnMetrics) {}
}

fn manager() -> MemoryManager {
    let mut m = MemoryManager::new();
    m.add_provider(Block("agents", "## AGENTS.md\nUse four spaces."));
    // The `knowledge_base` profile installs the FFI surface first, then the API one.
    m.add_provider(Block(
        "knowledge_base",
        "## FFI surface\nReviewWidget imports.",
    ));
    m.add_provider(Block("note_store", "## Agent Notes\nprefers vi"));
    m.add_provider(Block("api_surface", "## API surface\npub fn review()."));
    m
}

#[test]
fn providers_sharing_a_component_id_keep_a_span_each() {
    let m = manager();
    let sink = ReportSink::new();
    m.annotate_system_prompt_additions_where(&sink, &|_| true);
    // Re-annotating (every rebuild) replaces, never accumulates.
    m.annotate_system_prompt_additions_where(&sink, &|_| true);
    let notes = sink.snapshot_notes();
    let labels: Vec<&str> = notes.iter().map(|n| n.label.as_str()).collect();
    assert_eq!(
        labels,
        [
            "project-instructions",
            "api-surface",
            "notes",
            "api-surface"
        ]
    );

    // The prompt carries the blocks as the real builder joins them; nothing is left
    // unattributed but the joins.
    let prompt = m.context_additions();
    let report = RequestReport::build_from_parts(&[prompt.as_str()], None, &notes);
    let rows: Vec<(&str, usize)> = report.system[0]
        .components
        .iter()
        .map(|c| (c.label.as_str(), c.chars))
        .collect();
    assert!(
        rows.iter()
            .all(|(label, _)| !label.contains("unattributed")),
        "{rows:?}"
    );
    let api: Vec<usize> = rows
        .iter()
        .filter(|(l, _)| *l == "api-surface")
        .map(|(_, n)| *n)
        .collect();
    assert_eq!(
        api,
        [
            "## FFI surface\nReviewWidget imports.",
            "## API surface\npub fn review()."
        ]
        .map(|t| t.chars().count()),
        "both spans are reported, in prompt order"
    );
}

#[test]
fn a_provider_that_is_off_is_neither_sent_nor_reported() {
    let m = manager();
    let sink = ReportSink::new();
    let keep = |name: &str| name != "knowledge_base";
    m.annotate_system_prompt_additions_where(&sink, &keep);
    let labels: Vec<String> = sink.snapshot_notes().into_iter().map(|n| n.label).collect();
    assert_eq!(labels, ["project-instructions", "notes", "api-surface"]);
    assert!(!m.context_additions_where(&keep).contains("FFI surface"));
}
