use super::*;
use crate::agentic::mcp::LeasedMcpCall;
use crate::agentic::NoMcp;
use crate::caveats::{Caveats, CountBound, Scope};

/// fs read everywhere, fs write scoped to the workspace (skips the y/N
/// confirm — the scoped preset is the consent), nothing else.
fn caveats_rw(ws: &std::path::Path) -> Caveats {
    Caveats {
        fs_read: Scope::All,
        fs_write: Scope::only([ws.to_string_lossy().into_owned()]),
        exec: Scope::none(),
        net: Scope::none(),
        max_calls: CountBound::Unlimited,
        valid_for_generation: Scope::All,
    }
}

fn touch(root: &std::path::Path, rel: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, b"x").unwrap();
}

async fn run_tool(
    name: &str,
    args: serde_json::Value,
    ws: &std::path::Path,
    caveats: &Caveats,
    build_check: Option<&str>,
) -> String {
    execute_tool(
        name,
        &args,
        &ws.to_string_lossy(),
        false,
        20,
        caveats,
        &mut NoMcp,
        build_check,
        None,
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
    .await
}

// -- #263 prompted permission grants through execute_tool ---------------

/// Scripted gate: records every request it is asked about and answers
/// allow (with caveats widened by exactly the requested grants) or deny.
struct MockGate {
    allow: bool,
    base: Caveats,
    asks: Vec<(String, String)>,
}

impl MockGate {
    fn new(allow: bool, base: &Caveats) -> Self {
        Self {
            allow,
            base: base.clone(),
            asks: Vec::new(),
        }
    }
}

impl super::PermissionGate for MockGate {
    fn ask(&mut self, requests: &[super::PermissionRequest]) -> super::PermissionDecision {
        for r in requests {
            self.asks
                .push((r.tool.clone(), format!("{}:{}", r.kind.as_str(), r.target)));
        }
        if self.allow {
            let grants: Vec<_> = requests
                .iter()
                .map(|r| (r.kind, r.target.clone()))
                .collect();
            super::PermissionDecision::Allow(crate::agentic::widen_caveats(&self.base, &grants))
        } else {
            super::PermissionDecision::Deny
        }
    }
    // #728: this gate exercises the GRANT path only; it has no human to
    // answer free-text questions, so it reports no operator available.
    fn ask_question(&mut self, _question: &str) -> HumanQuestionOutcome {
        HumanQuestionOutcome::Unavailable
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_tool_gated(
    name: &str,
    args: serde_json::Value,
    ws: &std::path::Path,
    caveats: &Caveats,
    gate: &mut MockGate,
) -> String {
    execute_tool(
        name,
        &args,
        &ws.to_string_lossy(),
        false,
        20,
        caveats,
        &mut NoMcp,
        None,
        None,
        None,
        None, // memory_source
        Some(gate),
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

/// FR-2 (#1001): a one-tool remote MCP server for testing the remote-tool
/// leash — records whether `call` actually dispatched.
struct OneRemoteTool {
    name: &'static str,
    called: bool,
    resource_url_prefixes: &'static [&'static str],
}

impl OneRemoteTool {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            called: false,
            resource_url_prefixes: &[],
        }
    }

    fn with_resource_url_prefixes(
        mut self,
        resource_url_prefixes: &'static [&'static str],
    ) -> Self {
        self.resource_url_prefixes = resource_url_prefixes;
        self
    }
}

#[async_trait::async_trait]
impl McpTools for OneRemoteTool {
    fn handles(&self, name: &str) -> bool {
        name == self.name
    }
    fn tool_defs(&self) -> Vec<serde_json::Value> {
        let mut definition = serde_json::json!({
            "type": "function",
            "function": { "name": self.name, "description": "", "parameters": {} }
        });
        preserve_mcp_resource_url_affinity(
            &mut definition,
            Some(&serde_json::json!({
                "newt/resourceUrlPrefixes": self.resource_url_prefixes
            })),
        );
        vec![definition]
    }
    async fn call(&mut self, _leased: &LeasedMcpCall<'_>) -> String {
        self.called = true;
        "remote-tool-ran".to_string()
    }
}

async fn run_remote_gated(
    name: &str,
    ws: &std::path::Path,
    caveats: &Caveats,
    persona_tools: Option<&[String]>,
    mcp: &mut dyn McpTools,
    gate: Option<&mut MockGate>,
) -> String {
    let gate = gate.map(|g| g as &mut dyn super::PermissionGate);
    execute_tool_with_offload(
        name,
        &serde_json::json!({}),
        &ws.to_string_lossy(),
        false,
        20,
        caveats,
        mcp,
        None,
        None,
        None,
        None,
        gate,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        false,
        None,
        persona_tools,
    )
    .await
}

/// Directly exercise the disposition-aware dispatcher. Unlike
/// [`execute_tool_with_offload`], this reaches the new required boundary
/// argument while fixing all unrelated optional seams to their inert shape.
#[allow(clippy::too_many_arguments)]
async fn run_tool_with_disposition(
    name: &str,
    args: serde_json::Value,
    ws: &std::path::Path,
    caveats: &Caveats,
    mcp: &mut dyn McpTools,
    gate: Option<&mut dyn PermissionGate>,
    step_ledger: Option<&dyn crate::agentic::scheduled::StepLedger>,
    disposition: PromptDisposition,
) -> String {
    execute_tool_with_offload_and_prompt_and_artifacts(
        name,
        &args,
        &ws.to_string_lossy(),
        false,
        20,
        caveats,
        mcp,
        None, // build_check_cmd
        None, // note_sink
        None, // recall_source
        None, // memory_source
        None, // prompt_context
        None, // artifact_context
        None, // artifact_sink
        gate,
        None, // exec_floor
        None, // git_tool
        None, // crew_runner
        None, // scratchpad_store
        None, // code_search
        None, // where_is
        None, // experience_store
        step_ledger,
        false, // tool_offload
        None,  // spill_store
        None,  // persona_tools
        disposition,
    )
    .await
}

#[cfg(test)]
#[path = "execute_git.rs"]
mod git;

#[cfg(test)]
#[path = "execute_plan_disposition.rs"]
mod plan_disposition;

#[cfg(test)]
#[path = "execute_crew.rs"]
mod crew;

#[cfg(test)]
#[path = "execute_find.rs"]
mod find;

#[cfg(test)]
#[path = "execute_display.rs"]
mod display;

#[cfg(test)]
#[path = "execute_filesystem.rs"]
mod filesystem;

#[cfg(test)]
#[path = "execute_file_artifacts.rs"]
mod file_artifacts;

#[cfg(test)]
#[path = "execute_aliases.rs"]
mod aliases;

#[cfg(test)]
#[path = "execute_mcp_authority.rs"]
mod mcp_authority;

#[cfg(test)]
#[path = "execute_permissions.rs"]
mod permissions;

#[cfg(test)]
#[path = "execute_user_input.rs"]
mod user_input;

#[cfg(test)]
#[path = "execute_web_recovery.rs"]
mod web_recovery;

#[cfg(test)]
#[path = "execute_memory.rs"]
mod memory;
