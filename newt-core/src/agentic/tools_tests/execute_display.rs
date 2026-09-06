use super::*;
use crate::agentic::{ArtifactReadContext, PromptArtifactSink, SessionArtifactStore};
use crate::artifact::{ArtifactKind, ArtifactRelation, NewPromptArtifact};
use crate::PromptId;

/// #1235: every tool invocation goes through one display boundary. The
/// operator sees the command plus a bounded head+tail, while the
/// model-facing result remains complete.
///
/// #1973 declared amendment: this golden MOVED from tail-only
/// (c.rs/d.rs/e.rs) to head+tail (a.rs .. e.rs) — see the module doc on
/// `display::spill_view_lines`. This test's own property (the operator's
/// bounded excerpt vs. the model's complete result) is unaffected.
#[tokio::test]
async fn find_command_and_full_result_share_the_spill_boundary() {
    let ws = tempfile::TempDir::new().unwrap();
    for f in ["e.rs", "b.rs", "d.rs", "a.rs", "c.rs"] {
        touch(ws.path(), f);
    }
    let args = serde_json::json!({
        "path": ".",
        "name": "*.rs",
        "type": "f",
    });
    let caveats = caveats_rw(ws.path());
    let (out, rendered) = run_tool_captured("find", args, ws.path(), &caveats, &mut NoMcp).await;

    assert_eq!(out, "a.rs\nb.rs\nc.rs\nd.rs\ne.rs");
    assert_eq!(
        rendered,
        "⚙  find: . (name=*.rs, type=f)\n\
             ▒ a.rs\n\
             ▲ 3 lines hidden  [/spill N raises this view]\n\
             ▓ e.rs\n\
             …\n"
    );
}

#[tokio::test]
async fn routed_find_uses_the_governed_tool_in_the_audit_header() {
    let ws = tempfile::TempDir::new().unwrap();
    for f in ["b.rs", "a.rs"] {
        touch(ws.path(), f);
    }
    let caveats = caveats_rw(ws.path());
    let (out, rendered) = run_tool_captured(
        "run_command",
        serde_json::json!({"command": "find . -name '*.rs' -type f"}),
        ws.path(),
        &caveats,
        &mut NoMcp,
    )
    .await;

    assert_eq!(out, "a.rs\nb.rs");
    assert!(
        rendered.starts_with("⚙  find: . (name=*.rs, type=f)\n"),
        "routed action was not audited canonically: {rendered}"
    );
    assert!(!rendered.contains("⚙  run_command:"));
}

#[tokio::test]
async fn correction_alias_header_never_echoes_file_content() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path());
    let secret = "PRIVATE_BODY_MUST_NOT_APPEAR_IN_HEADER";
    let (out, rendered) = run_tool_captured(
        "create_file",
        serde_json::json!({"path": "secret.txt", "content": secret}),
        ws.path(),
        &caveats,
        &mut NoMcp,
    )
    .await;

    assert!(out.contains("write_file"), "got: {out}");
    assert!(
        rendered.starts_with(&format!(
            "⚙  create_file: secret.txt ({} bytes)\n",
            secret.len()
        )),
        "unsafe or unhelpful alias audit: {rendered}"
    );
    assert!(!rendered.contains(secret));
}

#[test]
fn lifecycle_audit_names_the_resolved_command() {
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(
        ws.path().join("Cargo.toml"),
        "[package]\nname='audit-fixture'\n",
    )
    .unwrap();
    let (name, detail) = tool_presentation(
        "lifecycle",
        &serde_json::json!({"phase": "test", "action": "run"}),
        ws.path(),
    );
    let resolved = crate::tooling::resolved_phase_commands(ws.path(), crate::tooling::Phase::Test);

    assert_eq!(name, "lifecycle");
    assert!(!resolved.is_empty());
    assert_eq!(detail, format!("test (run) → {}", resolved.join(" && ")));
}

#[test]
fn audit_preserves_whitespace_in_real_paths() {
    let ws = tempfile::TempDir::new().unwrap();
    let (name, detail) = tool_presentation(
        "read_file",
        &serde_json::json!({"path": " leading and trailing "}),
        ws.path(),
    );

    assert_eq!(name, "read_file");
    assert_eq!(detail, " leading and trailing ");

    let (name, detail) = tool_presentation(
        "run_command",
        &serde_json::json!({"command": "cd nested && printf exact-command"}),
        ws.path(),
    );
    assert_eq!(name, "run_command");
    assert_eq!(detail, "cd nested && printf exact-command");
}

#[tokio::test]
async fn find_error_uses_the_same_spill_boundary() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path());
    let (out, rendered) = run_tool_captured(
        "find",
        serde_json::json!({"path": "missing", "name": "*.rs", "type": "f"}),
        ws.path(),
        &caveats,
        &mut NoMcp,
    )
    .await;

    assert_eq!(out, "error: no such path 'missing'");
    assert_eq!(
        rendered,
        "⚙  find: missing (name=*.rs, type=f)\n\
             ▒ error: no such path 'missing'\n\
             …\n"
    );
}

struct EmptyRemote;

#[async_trait::async_trait]
impl McpTools for EmptyRemote {
    fn handles(&self, name: &str) -> bool {
        name == "test__get_empty"
    }

    fn tool_defs(&self) -> Vec<serde_json::Value> {
        Vec::new()
    }

    async fn call(&mut self, _leased: &LeasedMcpCall<'_>) -> String {
        String::new()
    }
}

#[tokio::test]
async fn empty_tool_result_still_commits_a_complete_spill_block() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path());
    let (out, rendered) = run_tool_captured(
        "test__get_empty",
        serde_json::json!({}),
        ws.path(),
        &caveats,
        &mut EmptyRemote,
    )
    .await;

    assert!(out.is_empty());
    assert_eq!(
        rendered,
        "⚙  test__get_empty: {}\n\
             ▒ (no output)\n\
             …\n"
    );
}

#[tokio::test]
async fn unknown_tool_has_exactly_one_complete_audit_block() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path());
    let (out, rendered) = run_tool_captured(
        "definitely_unknown",
        serde_json::json!({}),
        ws.path(),
        &caveats,
        &mut NoMcp,
    )
    .await;

    assert!(out.contains("unknown tool"), "got: {out}");
    assert_eq!(rendered.matches("⚙  definitely_unknown:").count(), 1);
    assert_eq!(rendered.matches("…\n").count(), 1);
    assert_eq!(rendered.matches("▒ unknown tool:").count(), 1);
}

#[tokio::test]
async fn pre_set_cancellation_closes_the_block_without_polling_a_mutation() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path());
    let args = serde_json::json!({"path": "must-not-exist.txt", "content": "blocked"});
    let cancel = std::sync::atomic::AtomicBool::new(true);
    let mut display = crate::agentic::display::ToolDisplay::new(Vec::new(), false, 80, 3, false);

    let out = execute_tool_with_display_cancellable(
        &mut display,
        "write_file",
        &args,
        &ws.path().to_string_lossy(),
        false,
        20,
        &caveats,
        &mut NoMcp,
        ToolCollaborators::default(),
        false,
        PromptDisposition::Act,
        Some(&cancel),
    )
    .await;

    assert!(out.is_none());
    assert!(!ws.path().join("must-not-exist.txt").exists());
    assert_eq!(
        String::from_utf8(display.into_inner()).unwrap(),
        "⚙  write_file: must-not-exist.txt (7 bytes)\n\
             ▒ error: write_file interrupted — tool cancelled before completion\n\
             …\n"
    );
}

#[tokio::test]
async fn prompt_read_central_display_never_echoes_recovered_prompt_text() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path());
    let exact = "operator secret that must reach only the model";
    let context = PromptReadContext::new(None, exact, None);
    let (out, rendered) = run_tool_captured_with_context(
        "prompt_read",
        serde_json::json!({}),
        ws.path(),
        &caveats,
        &mut NoMcp,
        Some(context),
        None,
    )
    .await;

    let model: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(model["model_text"], exact);
    assert_eq!(rendered.matches("⚙  prompt_read:").count(), 1);
    assert!(rendered.contains("ephemeral prompt: returned"));
    assert!(!rendered.contains(exact));
}

#[tokio::test]
async fn artifact_read_central_display_never_echoes_recovered_body() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path());
    let prompt = PromptId::new();
    let secret = "artifact body that must reach only the model";
    let store = SessionArtifactStore::new("central-display-test").unwrap();
    let record = store
        .append_artifact(
            prompt,
            prompt,
            NewPromptArtifact::new(ArtifactKind::Decision, ArtifactRelation::DerivedFrom)
                .with_body(secret),
        )
        .unwrap();
    let context = ArtifactReadContext::new(Some(prompt), Some(prompt), Some(prompt), Some(&store));
    let (out, rendered) = run_tool_captured_with_context(
        "artifact_read",
        serde_json::json!({"address": record.id.to_string()}),
        ws.path(),
        &caveats,
        &mut NoMcp,
        None,
        Some(context),
    )
    .await;

    let model: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(model["artifact"]["body"], secret);
    assert_eq!(rendered.matches("⚙  artifact_read:").count(), 1);
    assert!(rendered.contains(&format!(
        "returned {} of {} body characters",
        secret.chars().count(),
        secret.chars().count()
    )));
    assert!(!rendered.contains(secret));
}

#[tokio::test]
async fn render_report_has_one_header_document_and_ack_block() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path());
    let (out, rendered) = run_tool_captured(
        "render_report",
        serde_json::json!({
            "title": "Build status",
            "body": "All required checks passed."
        }),
        ws.path(),
        &caveats,
        &mut NoMcp,
    )
    .await;

    assert!(out.starts_with("report rendered:"), "got: {out}");
    assert_eq!(rendered.matches("⚙  render_report:").count(), 1);
    assert_eq!(rendered.matches("All required checks passed.").count(), 1);
    assert_eq!(rendered.matches("▒ report rendered:").count(), 1);
}

async fn run_tool_captured(
    name: &str,
    args: serde_json::Value,
    ws: &std::path::Path,
    caveats: &Caveats,
    mcp: &mut dyn McpTools,
) -> (String, String) {
    run_tool_captured_with_context(name, args, ws, caveats, mcp, None, None).await
}

async fn run_tool_captured_with_context(
    name: &str,
    args: serde_json::Value,
    ws: &std::path::Path,
    caveats: &Caveats,
    mcp: &mut dyn McpTools,
    prompt_context: Option<PromptReadContext<'_>>,
    artifact_context: Option<ArtifactReadContext<'_>>,
) -> (String, String) {
    run_tool_captured_with_context_and_live(
        name,
        args,
        ws,
        caveats,
        mcp,
        prompt_context,
        artifact_context,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_tool_captured_with_context_and_live(
    name: &str,
    args: serde_json::Value,
    ws: &std::path::Path,
    caveats: &Caveats,
    mcp: &mut dyn McpTools,
    prompt_context: Option<PromptReadContext<'_>>,
    artifact_context: Option<ArtifactReadContext<'_>>,
    live_tool_output: Option<std::sync::Arc<dyn crate::agentic::LiveToolOutput>>,
) -> (String, String) {
    let mut display = crate::agentic::display::ToolDisplay::new(Vec::new(), false, 80, 3, false);
    // Mechanics helper: authorize the tool it is told to run (the MCP-auth
    // tests use `run_remote_gated` instead). Post the `mcp-under-leash`
    // name-grant closure an MCP call needs a structural grant; for a built-in
    // tool `persona_tools` doesn't gate dispatch, so this is a no-op there.
    let persona_grant = [name.to_string()];
    let out = execute_tool_with_display_cancellable(
        &mut display,
        name,
        &args,
        &ws.to_string_lossy(),
        false,
        20,
        caveats,
        mcp,
        ToolCollaborators {
            prompt_context,
            artifact_context,
            live_tool_output,
            persona_tools: Some(&persona_grant),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
        None,
    )
    .await
    .expect("uncancelled test dispatch should complete");
    let rendered = String::from_utf8(display.into_inner()).unwrap();
    (out, rendered)
}

#[cfg(not(windows))]
#[tokio::test]
async fn live_shell_observation_does_not_change_headless_completion_bytes() {
    #[derive(Default)]
    struct CapturedLiveOutput {
        events: std::sync::Mutex<Vec<String>>,
    }
    impl crate::agentic::LiveToolOutput for CapturedLiveOutput {
        fn start(&self, _generation: u64) {
            self.events.lock().unwrap().push("start".into());
        }
        fn write(&self, _generation: u64, _stream: crate::agentic::ToolOutputStream, chunk: &[u8]) {
            self.events
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(chunk).into_owned());
        }
        fn finish(&self, _generation: u64) {
            self.events.lock().unwrap().push("finish".into());
        }
        fn abandon(&self, _generation: u64) {
            self.events.lock().unwrap().push("abandon".into());
        }
    }

    let ws = tempfile::TempDir::new().unwrap();
    let caveats = Caveats {
        exec: crate::caveats::Scope::only(["echo".to_string()]),
        ..caveats_rw(ws.path())
    };
    let args = serde_json::json!({"command": "echo byte-stable"});
    let (headless_out, headless_rendered) = run_tool_captured_with_context_and_live(
        "run_command",
        args.clone(),
        ws.path(),
        &caveats,
        &mut NoMcp,
        None,
        None,
        None,
    )
    .await;
    let sink = std::sync::Arc::new(CapturedLiveOutput::default());
    let (live_out, live_rendered) = run_tool_captured_with_context_and_live(
        "run_command",
        args,
        ws.path(),
        &caveats,
        &mut NoMcp,
        None,
        None,
        Some(sink.clone()),
    )
    .await;

    assert_eq!(live_out, headless_out);
    assert_eq!(live_rendered.as_bytes(), headless_rendered.as_bytes());
    assert!(
        !headless_rendered.as_bytes().contains(&0x1b),
        "headless completion emitted cursor-control bytes: {headless_rendered:?}"
    );
    let events = sink.events.lock().unwrap();
    assert_eq!(events.first().map(String::as_str), Some("start"));
    assert_eq!(events.last().map(String::as_str), Some("finish"));
    assert!(
        events.iter().any(|event| event.contains("byte-stable")),
        "live events: {events:?}; model output: {live_out:?}"
    );
}
