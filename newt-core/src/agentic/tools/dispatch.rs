//! Tool-dispatch entry points, collaborator construction, and cancellation/display lifetime.

use crate::agentic;
use crate::agentic::artifact_read::ArtifactReadContext;
use crate::agentic::content_spill::SpillStore;
use crate::agentic::crew_tool::CrewRunner;
use crate::agentic::display::ToolDisplay;
use crate::agentic::git_tool::GitTool;
use crate::agentic::mcp::McpTools;
use crate::agentic::memory_fetch::MemorySource;
use crate::agentic::note_sink::NoteSink;
use crate::agentic::permissions::PermissionGate;
use crate::agentic::prompt_intake::PromptDisposition;
use crate::agentic::prompt_read::PromptReadContext;
use crate::agentic::recall::RecallSource;
use crate::agentic::tools::live_output::ToolSpinner;
use crate::agentic::tools::{execute_tool_inner, tool_presentation};

#[allow(clippy::too_many_arguments)]
/// The optional collaborator seams a tool dispatch may carry, bundled into ONE
/// value (reuse discipline: "prefer making a bug unrepresentable"). The
/// positional form threaded ~19 `Option` params through every facade layer —
/// and a bare-`None` run misaligned by one slot compiles fine while silently
/// disabling the wrong seam (the exact hazard hit while threading `where_is`,
/// #1285). Named fields + `..Default::default()` make that miswiring
/// impossible, and a NEW seam is one field plus its construction sites, not a
/// signature change through six layers.
///
/// `Default` is all-`None`: the bare dispatch a test or embedder starts from.
#[derive(Default)]
pub(crate) struct ToolCollaborators<'a> {
    pub(crate) build_check_cmd: Option<&'a str>,
    /// #1947: the turn's tool ledger, distilled — what `render_report`'s
    /// capability claims are checked against.
    ///
    /// `Option` is load-bearing and not a convenience. `None` means there is
    /// no RECORDER (eval, headless), which is not the same fact as an empty
    /// ledger; conflating them would refute every report in those tiers for
    /// a reason that has nothing to do with the report.
    pub(crate) tool_evidence: Option<&'a agentic::capability_check::Evidence>,
    pub(crate) note_sink: Option<&'a mut dyn NoteSink>,
    pub(crate) recall_source: Option<&'a dyn RecallSource>,
    pub(crate) memory_source: Option<&'a dyn MemorySource>,
    pub(crate) prompt_context: Option<PromptReadContext<'a>>,
    pub(crate) artifact_context: Option<ArtifactReadContext<'a>>,
    pub(crate) artifact_sink: Option<&'a dyn agentic::artifact_read::PromptArtifactSink>,
    pub(crate) permission_gate: Option<&'a mut dyn PermissionGate>,
    pub(crate) exec_floor: Option<&'a crate::caveats::Scope<String>>,
    pub(crate) git_tool: Option<&'a dyn GitTool>,
    pub(crate) crew_runner: Option<&'a dyn CrewRunner>,
    pub(crate) scratchpad_store: Option<&'a dyn agentic::scratchpad::ScratchpadStore>,
    pub(crate) code_search: Option<agentic::semantic::CodeSearch<'a>>,
    pub(crate) where_is: Option<&'a crate::where_is::WhereIsIndex>,
    /// #1387 navigator tool context (usage/graph/project). `None` ⇒ tools degrade.
    pub(crate) nav: Option<crate::navigator::NavToolCtx<'a>>,
    pub(crate) experience_store: Option<&'a dyn agentic::experiential::ExperienceStore>,
    pub(crate) step_ledger: Option<&'a dyn agentic::scheduled::StepLedger>,
    pub(crate) operating_mode_control: Option<&'a dyn agentic::OperatingModeControl>,
    pub(crate) plan_mode_control: Option<&'a dyn agentic::PlanModeControl>,
    pub(crate) spill_store: Option<&'a dyn SpillStore>,
    pub(crate) persona_tools: Option<&'a [String]>,
    pub(crate) live_tool_output: Option<std::sync::Arc<dyn crate::agentic::LiveToolOutput>>,
    /// Optional completed spill renderer for Rich TUI interactive viewport (#1640).
    pub(crate) completed_spill_renderer:
        Option<std::sync::Arc<dyn crate::agentic::CompletedSpillRenderer>>,
}

#[allow(clippy::too_many_arguments)]
pub async fn execute_tool(
    name: &str,
    args: &serde_json::Value,
    workspace: &str,
    color: bool,
    tool_output_lines: usize,
    caveats: &crate::caveats::Caveats,
    mcp: &mut dyn McpTools,
    build_check_cmd: Option<&str>,
    note_sink: Option<&mut dyn NoteSink>,
    recall_source: Option<&dyn RecallSource>,
    memory_source: Option<&dyn MemorySource>,
    permission_gate: Option<&mut dyn PermissionGate>,
    exec_floor: Option<&crate::caveats::Scope<String>>,
    git_tool: Option<&dyn GitTool>,
    crew_runner: Option<&dyn CrewRunner>,
    scratchpad_store: Option<&dyn agentic::scratchpad::ScratchpadStore>,
    code_search: Option<agentic::semantic::CodeSearch<'_>>,
    where_is: Option<&crate::where_is::WhereIsIndex>,
    experience_store: Option<&dyn agentic::experiential::ExperienceStore>,
    step_ledger: Option<&dyn agentic::scheduled::StepLedger>,
) -> String {
    // The convenience wrapper carries no offload/persona/prompt surface —
    // callers that need those seams use the wider entry points.
    let collab = ToolCollaborators {
        build_check_cmd,
        // Reborrow the invariant `&mut dyn` seams to the local region (the
        // same coercion the loop's call sites perform on ChatCtx fields).
        note_sink: note_sink.map(|s| &mut *s as &mut dyn NoteSink),
        recall_source,
        memory_source,
        permission_gate: permission_gate.map(|g| &mut *g as &mut dyn PermissionGate),
        exec_floor,
        git_tool,
        crew_runner,
        scratchpad_store,
        code_search,
        where_is,
        nav: None,
        experience_store,
        step_ledger,
        ..Default::default()
    };
    execute_tool_with_collaborators(
        name,
        args,
        workspace,
        color,
        tool_output_lines,
        caveats,
        mcp,
        collab,
        false,
        PromptDisposition::Act,
        None,
    )
    .await
    .expect("tool execution without a cancellation flag cannot be interrupted")
}

#[allow(clippy::too_many_arguments)]
pub async fn execute_tool_with_offload(
    name: &str,
    args: &serde_json::Value,
    workspace: &str,
    color: bool,
    tool_output_lines: usize,
    caveats: &crate::caveats::Caveats,
    mcp: &mut dyn McpTools,
    build_check_cmd: Option<&str>,
    note_sink: Option<&mut dyn NoteSink>,
    recall_source: Option<&dyn RecallSource>,
    memory_source: Option<&dyn MemorySource>,
    permission_gate: Option<&mut dyn PermissionGate>,
    exec_floor: Option<&crate::caveats::Scope<String>>,
    git_tool: Option<&dyn GitTool>,
    crew_runner: Option<&dyn CrewRunner>,
    scratchpad_store: Option<&dyn agentic::scratchpad::ScratchpadStore>,
    code_search: Option<agentic::semantic::CodeSearch<'_>>,
    where_is: Option<&crate::where_is::WhereIsIndex>,
    experience_store: Option<&dyn agentic::experiential::ExperienceStore>,
    step_ledger: Option<&dyn agentic::scheduled::StepLedger>,
    tool_offload: bool,
    spill_store: Option<&dyn SpillStore>,
    persona_tools: Option<&[String]>,
) -> String {
    let collab = ToolCollaborators {
        build_check_cmd,
        // Reborrow the invariant `&mut dyn` seams to the local region (the
        // same coercion the loop's call sites perform on ChatCtx fields).
        note_sink: note_sink.map(|s| &mut *s as &mut dyn NoteSink),
        recall_source,
        memory_source,
        permission_gate: permission_gate.map(|g| &mut *g as &mut dyn PermissionGate),
        exec_floor,
        git_tool,
        crew_runner,
        scratchpad_store,
        code_search,
        where_is,
        nav: None,
        experience_store,
        step_ledger,
        spill_store,
        persona_tools,
        ..Default::default()
    };
    execute_tool_with_collaborators(
        name,
        args,
        workspace,
        color,
        tool_output_lines,
        caveats,
        mcp,
        collab,
        tool_offload,
        PromptDisposition::Act,
        None,
    )
    .await
    .expect("tool execution without a cancellation flag cannot be interrupted")
}

/// Prompt- and artifact-aware tool dispatcher used by inference loops.
#[allow(clippy::too_many_arguments)]
pub async fn execute_tool_with_offload_and_prompt_and_artifacts(
    name: &str,
    args: &serde_json::Value,
    workspace: &str,
    color: bool,
    tool_output_lines: usize,
    caveats: &crate::caveats::Caveats,
    mcp: &mut dyn McpTools,
    build_check_cmd: Option<&str>,
    note_sink: Option<&mut dyn NoteSink>,
    recall_source: Option<&dyn RecallSource>,
    memory_source: Option<&dyn MemorySource>,
    prompt_context: Option<PromptReadContext<'_>>,
    artifact_context: Option<ArtifactReadContext<'_>>,
    artifact_sink: Option<&dyn agentic::artifact_read::PromptArtifactSink>,
    permission_gate: Option<&mut dyn PermissionGate>,
    exec_floor: Option<&crate::caveats::Scope<String>>,
    git_tool: Option<&dyn GitTool>,
    crew_runner: Option<&dyn CrewRunner>,
    scratchpad_store: Option<&dyn agentic::scratchpad::ScratchpadStore>,
    code_search: Option<agentic::semantic::CodeSearch<'_>>,
    where_is: Option<&crate::where_is::WhereIsIndex>,
    experience_store: Option<&dyn agentic::experiential::ExperienceStore>,
    step_ledger: Option<&dyn agentic::scheduled::StepLedger>,
    tool_offload: bool,
    spill_store: Option<&dyn SpillStore>,
    persona_tools: Option<&[String]>,
    disposition: PromptDisposition,
) -> String {
    let collab = ToolCollaborators {
        build_check_cmd,
        // Reborrow the invariant `&mut dyn` seams to the local region (the
        // same coercion the loop's call sites perform on ChatCtx fields).
        note_sink: note_sink.map(|s| &mut *s as &mut dyn NoteSink),
        recall_source,
        memory_source,
        prompt_context,
        artifact_context,
        artifact_sink,
        permission_gate: permission_gate.map(|g| &mut *g as &mut dyn PermissionGate),
        exec_floor,
        git_tool,
        crew_runner,
        scratchpad_store,
        code_search,
        where_is,
        nav: None,
        experience_store,
        step_ledger,
        spill_store,
        persona_tools,
        ..Default::default()
    };
    execute_tool_with_collaborators(
        name,
        args,
        workspace,
        color,
        tool_output_lines,
        caveats,
        mcp,
        collab,
        tool_offload,
        disposition,
        None,
    )
    .await
    .expect("tool execution without a cancellation flag cannot be interrupted")
}

/// Cancellation-aware loop entry point — the collaborator-struct core every
/// public wrapper above flattens into. The header is written synchronously
/// before the cancel-first race begins; an already-set interrupt therefore
/// closes a complete audit block without ever polling the tool body.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_tool_with_collaborators(
    name: &str,
    args: &serde_json::Value,
    workspace: &str,
    color: bool,
    tool_output_lines: usize,
    caveats: &crate::caveats::Caveats,
    mcp: &mut dyn McpTools,
    collab: ToolCollaborators<'_>,
    tool_offload: bool,
    disposition: PromptDisposition,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Option<String> {
    let mut display = ToolDisplay::new(
        std::io::stdout(),
        color,
        agentic::display::term_cols(),
        agentic::display::spill_lines(),
        agentic::display::spill_summary(),
    );
    // Thread the completed spill renderer for Rich TUI interactive viewport (#1640)
    if let Some(ref renderer) = collab.completed_spill_renderer {
        display.set_completed_spill_renderer(renderer.clone());
    }
    execute_tool_with_display_cancellable(
        &mut display,
        name,
        args,
        workspace,
        color,
        tool_output_lines,
        caveats,
        mcp,
        collab,
        tool_offload,
        disposition,
        cancel,
    )
    .await
}

async fn wait_for_tool_cancellation(cancel: Option<&std::sync::atomic::AtomicBool>) {
    match cancel {
        None => std::future::pending::<()>().await,
        Some(flag) => {
            while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                tokio::time::sleep(std::time::Duration::from_millis(15)).await;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn execute_tool_with_display_cancellable<W: std::io::Write + Send>(
    display: &mut ToolDisplay<W>,
    name: &str,
    args: &serde_json::Value,
    workspace: &str,
    color: bool,
    tool_output_lines: usize,
    caveats: &crate::caveats::Caveats,
    mcp: &mut dyn McpTools,
    collab: ToolCollaborators<'_>,
    tool_offload: bool,
    disposition: PromptDisposition,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Option<String> {
    let (presentation_name, presentation_detail) =
        tool_presentation(name, args, std::path::Path::new(workspace));
    display.call(&presentation_name, &presentation_detail);
    let result = {
        // #1727: the row under the header is never silent while the tool is
        // in flight. The spinner is scoped to this block, so it is erased
        // before `display.result` below on every path — return, cancel, or
        // panic — and the session's live sink is wrapped so the FIRST live
        // chunk takes the row over from it. See `ToolSpinner`.
        let spinner = ToolSpinner::start(&presentation_name, color);
        let collab = ToolCollaborators {
            live_tool_output: spinner.wrap(collab.live_tool_output),
            ..collab
        };
        let execution = execute_tool_inner(
            display,
            name,
            args,
            workspace,
            color,
            tool_output_lines,
            caveats,
            mcp,
            collab,
            tool_offload,
            disposition,
        );
        tokio::pin!(execution);
        tokio::select! {
            biased;
            _ = wait_for_tool_cancellation(cancel) => None,
            result = &mut execution => Some(result),
        }
    };
    match result {
        Some(result) => {
            display.result(&result);
            Some(result)
        }
        None => {
            // The turn is being torn down — an interactive viewport painted
            // here would outlive every dismiss hook (the provider loops
            // return immediately) and strand a dead frame above the caller's
            // interrupt notice. Static excerpt only.
            display.drop_completed_spill_renderer();
            let result = format!("error: {name} interrupted — tool cancelled before completion");
            display.result(&result);
            None
        }
    }
}
