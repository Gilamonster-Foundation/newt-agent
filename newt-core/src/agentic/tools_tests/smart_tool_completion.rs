//! Completion publication occurs before presentation can discard the return.
use super::*;
use crate::agentic::smart_harness::SmartHarness;
use std::sync::Arc;

const RETURNED: &str = "actual observed result before storage failure";
struct BreakCheckpoint(std::path::PathBuf);
#[async_trait::async_trait]
impl McpTools for BreakCheckpoint {
    fn handles(&self, name: &str) -> bool {
        name == "fixture__read"
    }
    fn tool_defs(&self) -> Vec<serde_json::Value> {
        Vec::new()
    }
    async fn call(&mut self, _: &crate::agentic::mcp::LeasedMcpCall<'_>) -> String {
        std::fs::remove_file(&self.0).unwrap();
        std::fs::create_dir(&self.0).unwrap();
        RETURNED.into()
    }
}

async fn failed_return<W: std::io::Write + Send>(writer: W) -> (anyhow::Error, W) {
    let workspace = tempfile::tempdir().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let session = agent_harness::Session::open(directory.path(), Default::default()).unwrap();
    let mut mcp = BreakCheckpoint(session.checkpoint_path().unwrap());
    let harness = SmartHarness::new(
        session,
        Arc::new(|_| panic!("no inference")),
        Default::default(),
    )
    .unwrap();
    let batch = harness.fixture_tool_batch("fixture__read", serde_json::json!({}));
    let invocation = batch.start(0, None).unwrap();
    let caveats = crate::confined_exec::workspace_confined_caveats(workspace.path());
    let mut display = crate::agentic::display::ToolDisplay::new(writer, false, 80, 20, false);
    let error = execute_tool_with_display_cancellable(
        &mut display,
        "fixture__read",
        &serde_json::json!({}),
        workspace.path().to_str().unwrap(),
        false,
        20,
        &caveats,
        &mut mcp,
        ToolCollaborators {
            invocation: Some(&invocation),
            persona_tools: Some(&["fixture__read".to_string()]),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
        None,
    )
    .await
    .unwrap_err();
    (error, display.into_inner())
}

/// Grounds the failure-reporting contract with a real obstructed checkpoint:
/// both a captured display and an output-discarding host retain the return and
/// original storage error, without calling it an observed tool failure.
#[tokio::test]
#[serial_test::serial]
async fn persistence_failure_preserves_result_with_captured_or_discarded_display() {
    let (error, rendered) = failed_return(Vec::new()).await;
    let rendered = String::from_utf8(rendered).unwrap();
    assert!(
        rendered.find(RETURNED).unwrap() < rendered.find("error: tool completion failed").unwrap(),
        "{rendered}"
    );
    assert!(
        rendered.contains("frame storage"),
        "the display must retain the actionable storage reason: {rendered}"
    );
    assert!(format!("{error:#}").contains(RETURNED));
    assert!(format!("{error:#}").contains("frame storage"));
    let (error, _) = failed_return(std::io::sink()).await;
    assert!(format!("{error:#}").contains(RETURNED));
    assert!(format!("{error:#}").contains("frame storage"));
}

/// Grounds pre-dispatch persona and permission refusals in the real dispatcher:
/// denied file writes and an ungranted remote call never execute, and their
/// authored refusal is retained as Harness material rather than tool evidence.
#[tokio::test]
#[serial_test::serial]
async fn persona_and_permission_refusals_retain_host_origin() {
    for name in ["write_file", "fixture__read"] {
        let workspace = tempfile::tempdir().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let session = agent_harness::Session::open(directory.path(), Default::default()).unwrap();
        let mut mcp = BreakCheckpoint(session.checkpoint_path().unwrap());
        let harness = SmartHarness::new(
            session,
            Arc::new(|_| panic!("no inference")),
            Default::default(),
        )
        .unwrap();
        let args = serde_json::json!({"path":"must-not-exist","content":"denied write"});
        let batch = harness.fixture_tool_batch(name, args.clone());
        let invocation = batch.start(0, None).unwrap();
        let caveats = crate::confined_exec::workspace_confined_caveats(workspace.path());
        let mut display =
            crate::agentic::display::ToolDisplay::new(Vec::new(), false, 80, 20, false);
        let output = execute_tool_with_display_cancellable(
            &mut display,
            name,
            &args,
            workspace.path().to_str().unwrap(),
            false,
            20,
            &caveats,
            &mut mcp,
            ToolCollaborators {
                invocation: Some(&invocation),
                persona_tools: Some(&["read_file".to_string()]),
                ..Default::default()
            },
            false,
            PromptDisposition::Act,
            None,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!output.is_empty());
        assert!(!workspace.path().join("must-not-exist").exists());
        let store = agent_harness::store::FrameStore::open(directory.path()).unwrap();
        let record = agent_harness::forensics::inspect_from_store(
            &store,
            harness.head().unwrap(),
            Default::default(),
        )
        .unwrap()
        .unwrap();
        let returned = record
            .references
            .iter()
            .find(|reference| reference.relation == "return")
            .expect("dispatch publishes its return before presentation")
            .cid
            .parse()
            .unwrap();
        let event =
            agent_harness::forensics::inspect_from_store(&store, returned, Default::default())
                .unwrap()
                .unwrap();
        assert_eq!(
            event.record["origin"], "harness",
            "{name}: known policy refusal must not become tool evidence"
        );
    }
}
