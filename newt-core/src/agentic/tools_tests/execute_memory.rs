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

// -- read_file accepts a memory address -------------------------------------
//
// A spill teaser tells the model to call `memory_fetch`, but a weak model
// reaches for `read_file` with the `spill:<cid>` handle instead. Live
// 2026-09-23: four identical `read_file spill:bafy…` calls, each answered
// "No such file or directory" — a lie (the payload existed, the tool was
// wrong), logged `ok:true`, so the repeat guard never fired. `read_file` now
// resolves any memory address through the same resolver `memory_fetch` uses.
// Fully mocked: the workspace path is never touched on this branch.

async fn read_file_via(
    args: serde_json::Value,
    source: Option<&dyn crate::agentic::MemorySource>,
) -> String {
    let caveats = Caveats::top();
    execute_tool(
        "read_file",
        &args,
        "/nonexistent-workspace-never-touched",
        false,
        20,
        &caveats,
        &mut NoMcp,
        None,
        None,
        None,
        source,
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
    .await
}

#[tokio::test]
async fn read_file_of_a_spill_address_serves_the_spilled_payload() {
    use crate::agentic::memory_fetch::tests::MockSource;
    use crate::agentic::MemAddr;
    let source = MockSource {
        body: Some("EXACT_SPILLED_DETAIL".to_string()),
        ..Default::default()
    };
    let out = read_file_via(
        serde_json::json!({"path": "spill:bafyexample"}),
        Some(&source),
    )
    .await;
    assert_eq!(out, "EXACT_SPILLED_DETAIL");
    assert_eq!(
        *source.calls.lock().unwrap(),
        vec![MemAddr::Spill {
            id: "bafyexample".into()
        }]
    );
}

#[tokio::test]
async fn read_file_of_a_memory_address_honours_offset_and_limit() {
    use crate::agentic::memory_fetch::tests::MockSource;
    let body = (1..=10)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    let source = MockSource {
        body: Some(body),
        ..Default::default()
    };
    let out = read_file_via(
        serde_json::json!({"path": "spill:bafyexample", "offset": 4, "limit": 2}),
        Some(&source),
    )
    .await;
    assert!(
        out.contains("line 4") && out.contains("line 5"),
        "got: {out}"
    );
    assert!(
        !out.contains("line 3") && !out.contains("line 6"),
        "got: {out}"
    );
}

#[tokio::test]
async fn read_file_of_a_memory_address_without_a_source_names_memory_fetch_not_enoent() {
    let out = read_file_via(serde_json::json!({"path": "spill:bafyexample"}), None).await;
    assert!(
        !out.contains("os error"),
        "must not look like a missing file: {out}"
    );
    assert!(out.contains("memory address"), "got: {out}");
}

/// `read_file`'s normal cap (~30k chars) is larger than the spill cap (16k).
/// A big spilled payload read back through `read_file` must come back paged
/// UNDER the spill cap, or the answer is itself spilled into a new handle.
#[tokio::test]
async fn read_file_of_a_big_spill_pages_under_the_spill_cap() {
    use crate::agentic::memory_fetch::tests::MockSource;
    let body = (1..=2_000)
        .map(|n| format!("line {n} {}", "x".repeat(40)))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(body.len() > 60_000);
    let source = MockSource {
        body: Some(body),
        ..Default::default()
    };
    let out = read_file_via(
        serde_json::json!({"path": "spill:bafyexample"}),
        Some(&source),
    )
    .await;
    let chars = out.chars().count();
    assert!(
        chars <= crate::agentic::content_spill::TOOL_RESULT_SPILL_CAP,
        "{chars} chars would re-spill"
    );
    assert!(out.starts_with("line 1 "), "got: {out:.80}");
    let tail = &out[out.len() - 200..];
    assert!(out.contains("offset"), "must say how to continue: {tail}");
}
