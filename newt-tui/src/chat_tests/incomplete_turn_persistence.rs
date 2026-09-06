use super::*;
use std::sync::Mutex;

/// Test-only [`newt_core::agentic::PromptArtifactSink`] that records what
/// was written instead of persisting it — mirrors the shape of
/// `artifact_hooks.rs`'s own private `RecordingSink` (that one cannot be
/// reused directly: it is private to a different crate's test module).
#[derive(Default)]
struct RecordingArtifactSink {
    writes: Mutex<Vec<newt_core::NewPromptArtifact>>,
}

impl RecordingArtifactSink {
    fn artifacts(&self) -> Vec<newt_core::NewPromptArtifact> {
        self.writes.lock().unwrap().clone()
    }
}

impl newt_core::agentic::PromptArtifactSink for RecordingArtifactSink {
    fn append_artifact(
        &self,
        originating_prompt_id: newt_core::PromptId,
        objective_root_id: newt_core::PromptId,
        artifact: newt_core::NewPromptArtifact,
    ) -> anyhow::Result<newt_core::agentic::ArtifactReadRecord> {
        let mut writes = self.writes.lock().unwrap();
        writes.push(artifact.clone());
        Ok(newt_core::agentic::ArtifactReadRecord {
            id: newt_core::ArtifactId::new(),
            prompt_id: originating_prompt_id,
            root_prompt_id: objective_root_id,
            writer_fingerprint: "test-writer".to_string(),
            seq: writes.len() as u64,
            prev_hash: "prev".to_string(),
            kind: format!("{:?}", artifact.kind()),
            relation: format!("{:?}", artifact.relation()),
            locator: artifact.locator().map(str::to_string),
            body: artifact.body().map(str::to_string),
            metadata: artifact.metadata().clone(),
            ts_claim: 1,
            artifact_hash: "hash".to_string(),
        })
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    _ws: tempfile::TempDir,
    store: newt_core::ConversationStore,
    conversation_id: String,
    memory: newt_core::MemoryManager,
    scratchpad_store: newt_core::SessionScratchpadStore,
    step_ledger: newt_core::SessionStepLedger,
    pricing: newt_core::PricingConfig,
    sink: RecordingArtifactSink,
    turn: newt_core::TurnPromptContext,
}

fn fixture(conversation_id: &str) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let store = newt_core::ConversationStore::new(root.path(), ws.path(), 100).unwrap();
    let turn = newt_core::TurnPromptContext::ephemeral_operator(
        conversation_id.to_string(),
        b"continue".to_vec(),
        b"continue".to_vec(),
    );
    Fixture {
        _root: root,
        _ws: ws,
        store,
        conversation_id: conversation_id.to_string(),
        memory: newt_core::MemoryManager::new(),
        scratchpad_store: newt_core::SessionScratchpadStore::default(),
        step_ledger: newt_core::SessionStepLedger::default(),
        pricing: newt_core::PricingConfig::default(),
        sink: RecordingArtifactSink::default(),
        turn,
    }
}

/// The exact shape of the operator's forensic evidence: real tool calls
/// happened before the interrupt landed.
fn a_tool_event() -> newt_core::ToolEvent {
    newt_core::ToolEvent {
        tool: "read_file".to_string(),
        args_digest: "keys=path;abc123".to_string(),
        ok: true,
        duration_ms: Some(42),
    }
}

/// Test-only [`newt_core::MemoryProvider`] that records what it was
/// synced with, sharing its log via `Arc` so the test can still read it
/// after the provider moves into the [`newt_core::MemoryManager`] by
/// value. Exists to pin the OTHER half of #1963's finding: "memory.sync_all
/// is also Ok-only, so the segment is lost to resume context, not just
/// forensics" — a persisted `turns` row with no memory sync still loses
/// the interrupted segment from what the NEXT turn's context sees.
#[derive(Clone, Default)]
struct RecordingMemoryProvider(std::sync::Arc<Mutex<Vec<(String, String)>>>);

impl RecordingMemoryProvider {
    fn calls(&self) -> Vec<(String, String)> {
        self.0.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl newt_core::MemoryProvider for RecordingMemoryProvider {
    fn name(&self) -> &str {
        "recording_memory_provider"
    }
    fn build_messages(&self, _system_prompt: &str, _new_task: &str) -> Vec<newt_core::MemMessage> {
        Vec::new()
    }
    async fn sync_turn(&mut self, user: &str, assistant: &str, _metrics: &newt_core::TurnMetrics) {
        self.0
            .lock()
            .unwrap()
            .push((user.to_string(), assistant.to_string()));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelled_turn_persists_a_row_and_outcome_with_real_partial_usage() {
    let mut f = fixture("conv-cancel-test");
    // Anti-vacuous half: nothing is there before the call — the assertions
    // below are about what THIS call produced, not ambient state.
    assert!(
        !f.store.exists(&f.conversation_id).unwrap(),
        "the conversation must not exist before the interrupted turn is persisted"
    );

    let memory_calls = RecordingMemoryProvider::default();
    f.memory.add_provider(memory_calls.clone());

    let tool_events = vec![a_tool_event()];
    let real_usage = newt_core::TokenUsage {
        input_tokens: 12_345,
        output_tokens: 678,
    };
    let rt = tokio::runtime::Handle::current();

    persist_incomplete_turn(
        Some(&f.store),
        &f.conversation_id,
        None,
        "continue",
        "partial streamed answer before the interrupt",
        &tool_events,
        &[],
        Some(real_usage),
        0,
        newt_core::TurnEndReason::Cancelled,
        std::time::Duration::from_millis(4200),
        "test-model",
        "http://test-endpoint",
        &f.pricing,
        &mut f.memory,
        &f.scratchpad_store,
        &f.step_ledger,
        Some(&f.sink as &dyn newt_core::agentic::PromptArtifactSink),
        Some(&f.turn),
        None,
        None,
        &rt,
        false,
        false,
    );

    let record = f.store.load(&f.conversation_id).unwrap();
    assert_eq!(
        record.turns.len(),
        1,
        "exactly one turns row — not zero (the #1963 bug) and not two (a double write)"
    );
    let saved = &record.turns[0];
    assert_eq!(
        saved.assistant,
        "partial streamed answer before the interrupt"
    );
    assert_eq!(
        saved.tokens_in,
        Some(12_345),
        "real accumulated usage, not NULL"
    );
    assert_eq!(saved.tokens_out, Some(678));
    assert_eq!(
        saved.events.len(),
        1,
        "the real tool-event ledger, not dropped"
    );
    assert_eq!(saved.events[0].tool, "read_file");

    let artifacts = f.sink.artifacts();
    let outcome = artifacts
        .iter()
        .find(|a| a.kind() == newt_core::ArtifactKind::TurnOutcome)
        .expect("a turn_outcome artifact must be recorded for a cancelled turn");
    assert_eq!(outcome.metadata()["end_reason"], "cancelled");
    assert_eq!(outcome.metadata()["usage"]["input_tokens"], 12_345);
    assert_eq!(outcome.metadata()["usage"]["output_tokens"], 678);

    let synced = memory_calls.calls();
    assert_eq!(
            synced.len(),
            1,
            "memory.sync_all_with_active_task must run on the cancel path too — it used to be Ok-only, \
             which lost the interrupted segment from resume context, not just forensics"
        );
    assert_eq!(synced[0].1, "partial streamed answer before the interrupt");
}

/// Anti-fabrication twin: a genuine backend error has no accumulated-usage
/// channel back to the caller (unlike a cancel, which usually does — see
/// the sibling test). `None` must reach the artifact as JSON `null`, never
/// a manufactured `0` — a persisted zero would poison the tuner (#1967).
#[tokio::test(flavor = "multi_thread")]
async fn failed_turn_persists_with_null_usage_never_a_fabricated_zero() {
    let mut f = fixture("conv-err-test");
    let tool_events = vec![a_tool_event(), a_tool_event()];
    let rt = tokio::runtime::Handle::current();

    persist_incomplete_turn(
        Some(&f.store),
        &f.conversation_id,
        None,
        "continue",
        "",
        &tool_events,
        &[],
        None,
        0,
        newt_core::TurnEndReason::Failed,
        std::time::Duration::from_millis(900),
        "test-model",
        "http://test-endpoint",
        &f.pricing,
        &mut f.memory,
        &f.scratchpad_store,
        &f.step_ledger,
        Some(&f.sink as &dyn newt_core::agentic::PromptArtifactSink),
        Some(&f.turn),
        None,
        None,
        &rt,
        false,
        false,
    );

    let record = f.store.load(&f.conversation_id).unwrap();
    assert_eq!(record.turns.len(), 1);
    let saved = &record.turns[0];
    assert_eq!(
        saved.tokens_in, None,
        "no fabricated usage on a genuine error"
    );
    assert_eq!(saved.tokens_out, None);
    assert_eq!(
        saved.events.len(),
        2,
        "the real tool-event ledger survives the failure"
    );

    let artifacts = f.sink.artifacts();
    let outcome = artifacts
        .iter()
        .find(|a| a.kind() == newt_core::ArtifactKind::TurnOutcome)
        .expect("a turn_outcome artifact must be recorded for a failed turn too");
    assert_eq!(outcome.metadata()["end_reason"], "failed");
    assert_eq!(
        outcome.metadata()["usage"],
        serde_json::Value::Null,
        "NULL usage is the honest value for an unrecoverable error, not 0"
    );
}
