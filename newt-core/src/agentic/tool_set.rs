//! #2959: the one derivation of the tool set a request carries, shared by the
//! four wire loops and the TUI `/context request` preview so they cannot
//! disagree. Pure over borrowed inputs: no store, sink or client in scope.

use super::tools::{
    filter_advertised_tools, filter_tools_for_disposition, merged_tool_definitions, select_exposed,
    select_openai_compatible_tools, ExposureSettings, HiddenTools,
};
use super::{smart_harness, McpTools, PromptDisposition};
use serde_json::Value;

/// Which optional tools the session offers. The loops derive each flag from
/// the presence of the collaborator behind it.
#[derive(Clone, Copy, Debug, Default)]
pub struct Advertise<'a> {
    pub save_note: bool,
    pub recall: bool,
    pub memory_fetch: bool,
    /// `Some` advertises the embedded `git` tool, scoped to this read scope.
    pub git_read_scope: Option<&'a crate::caveats::Scope<String>>,
    pub team: bool,
    pub scratchpad: bool,
    pub code_search: bool,
    pub experiential: bool,
    pub scheduled: bool,
    pub operating_mode: bool,
    pub plan_mode: bool,
    pub plan_mode_active: bool,
}

pub struct ToolSetInputs<'a> {
    pub mcp: &'a dyn McpTools,
    pub advertise: Advertise<'a>,
    pub command_budget: crate::RunCommandBudget,
    pub shell_dialect: &'a str,
    pub persona_tools: Option<&'a [String]>,
    pub smart_harness: Option<&'a smart_harness::SmartHarness>,
    pub disposition: PromptDisposition,
    pub exposure: &'a ExposureSettings,
    pub exposure_budget_tokens: Option<usize>,
    pub estimation: crate::tokens::TokenEstimation,
    /// The OpenAI-compatible function envelope applies (Chat Completions and
    /// Responses wires, not Ollama or Anthropic).
    pub openai_compatible: bool,
}

/// The exposed catalog plus the schemas held back for promotion.
pub(crate) fn derive_tool_set(i: &ToolSetInputs<'_>) -> (Value, HiddenTools) {
    let a = &i.advertise;
    let tools = merged_tool_definitions(
        i.mcp,
        a.save_note,
        a.recall,
        a.memory_fetch,
        a.git_read_scope,
        a.team,
        a.scratchpad,
        a.code_search,
        a.experiential,
        a.scheduled,
        a.operating_mode,
        a.plan_mode,
        a.plan_mode_active,
        i.command_budget,
        i.shell_dialect,
    );
    // Persona preferences rank tools; dispatch still authorizes separately.
    let tools = filter_advertised_tools(tools, i.persona_tools);
    let tools = filter_tools_for_disposition(
        smart_harness::advertise(tools, i.smart_harness),
        i.disposition,
    );
    // #TEC Pass 1: clip to the operator's filters and the live budget.
    let (tools, hidden) = select_exposed(
        tools,
        i.exposure,
        i.exposure_budget_tokens,
        &std::collections::BTreeSet::new(),
        i.estimation,
    );
    if i.openai_compatible {
        let tools = select_openai_compatible_tools(tools, &hidden);
        (tools, hidden)
    } else {
        (tools, hidden)
    }
}

/// What the next request would advertise, for previews.
pub fn exposed_tool_set(i: &ToolSetInputs<'_>) -> Value {
    derive_tool_set(i).0
}
