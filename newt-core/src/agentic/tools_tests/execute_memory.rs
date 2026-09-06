use super::*;

// -- save_note dispatch through execute_tool (Step 19.3) ----------------

#[tokio::test]
async fn save_note_without_sink_is_unknown_tool() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path());
    // run_tool passes note_sink: None — the no-sink (headless) shape.
    let out = run_tool(
        "save_note",
        serde_json::json!({"action": "add", "text": "a fact"}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert!(out.starts_with("unknown tool: save_note"), "got: {out}");
}

#[tokio::test]
async fn save_note_with_sink_routes_through_execute_tool() {
    use crate::agentic::note_sink::tests::MockSink;
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path());
    let mut sink = MockSink::default();
    let out = execute_tool(
        "save_note",
        &serde_json::json!({"action": "add", "text": "workspace builds with just check"}),
        &ws.path().to_string_lossy(),
        false,
        20,
        &caveats,
        &mut NoMcp,
        None,
        Some(&mut sink),
        None,
        None, // memory_source
        None,
        None,
        None, // git_tool
        None, // crew_runner
        None, // scratchpad_store
        None, // code_search
        None, // where_is
        None, // experience_store
        None, // step_ledger
    )
    .await;
    assert_eq!(sink.calls, vec!["add:workspace builds with just check"]);
    assert!(
        out.starts_with("note saved: workspace builds"),
        "got: {out}"
    );
}

// -- recall dispatch through execute_tool (Step 17.5) -------------------

#[tokio::test]
async fn recall_without_source_is_unknown_tool() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path());
    // run_tool passes recall_source: None — the no-store (headless) shape.
    let out = run_tool(
        "recall",
        serde_json::json!({"query": "tokio panic"}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert!(out.starts_with("unknown tool: recall"), "got: {out}");
}

#[tokio::test]
async fn recall_with_source_routes_through_execute_tool() {
    use crate::agentic::recall::tests::{hit, MockSource};
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path());
    let source = MockSource {
        hits: vec![hit(
            "123456789012-abcd",
            "past work",
            3,
            ">>>tokio<<< panic",
        )],
        ..Default::default()
    };
    let out = execute_tool(
        "recall",
        &serde_json::json!({"query": "tokio panic"}),
        &ws.path().to_string_lossy(),
        false,
        20,
        &caveats,
        &mut NoMcp,
        None,
        None,
        Some(&source),
        None, // memory_source
        None,
        None,
        None, // git_tool
        None, // crew_runner
        None, // scratchpad_store
        None, // code_search
        None, // where_is
        None, // experience_store
        None, // step_ledger
    )
    .await;
    assert_eq!(
        *source.calls.lock().unwrap(),
        vec![("tokio panic".to_string(), 5)]
    );
    assert!(out.contains("«tokio» panic"), "got: {out}");
    assert!(out.contains("past work"), "got: {out}");
}

// -- memory_fetch dispatch through execute_tool (#319) ------------------

/// FLAG OFF (no source): a `memory_fetch` call is treated like any unknown
/// tool — the inert-by-default shape (the tool was never advertised, so a
/// call here is a hallucination). Mirrors `recall_without_source`.
#[tokio::test]
async fn memory_fetch_without_source_is_unknown_tool() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path());
    // run_tool passes memory_source: None — the no-source (headless) shape.
    let out = run_tool(
        "memory_fetch",
        serde_json::json!({"address": "note:1"}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert!(out.starts_with("unknown tool: memory_fetch"), "got: {out}");
}

/// FLAG ON (source present): a `memory_fetch` call routes through the
/// injected `MemorySource` and returns its body. Mirrors
/// `recall_with_source_routes_through_execute_tool`.
#[tokio::test]
async fn memory_fetch_with_source_routes_through_execute_tool() {
    use crate::agentic::memory_fetch::tests::MockSource;
    use crate::agentic::MemAddr;
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path());
    let source = MockSource {
        body: Some("the exact note body".to_string()),
        ..Default::default()
    };
    let out = execute_tool(
        "memory_fetch",
        &serde_json::json!({"address": "note:1"}),
        &ws.path().to_string_lossy(),
        false,
        20,
        &caveats,
        &mut NoMcp,
        None,
        None,
        None,
        Some(&source),
        None,
        None,
        None, // git_tool
        None, // crew_runner
        None, // scratchpad_store
        None, // code_search
        None, // where_is
        None, // experience_store
        None, // step_ledger
    )
    .await;
    assert_eq!(out, "the exact note body");
    assert_eq!(
        *source.calls.lock().unwrap(),
        vec![MemAddr::Note { id: "1".into() }]
    );
}
