//! Validate the bounded pair transported from one trusted managed handle.
use crate::{
    AdmittedFenceBody, Caveats, ExecutionEventKind, SandboxKind, ToolEnvelope, ToolError,
    ToolResult,
};

impl ToolEnvelope {
    /// Validate the actual Started/terminal pair. `Some` proves a root started
    /// under its verified fence; `None` means a pre-spawn Failed/Denied terminal,
    /// never a HostExec. Missing managed evidence is an error for this verifier.
    /// Local execution IDs correlate the pair, but do not authenticate a replay.
    pub fn verify_named_root_execution(
        &self,
        expected_root: &str,
        effective: &Caveats,
        protected_roots: &std::collections::BTreeSet<String>,
    ) -> ToolResult<Option<&AdmittedFenceBody>> {
        let invalid = || ToolError::denied("invalid named-root Started/terminal evidence pair");
        let terminal = self.execution.as_ref().ok_or_else(invalid)?;
        if !terminal.is_terminal() {
            return Err(invalid());
        }
        let Some(started) = self.execution_started.as_ref() else {
            if matches!(terminal.kind, ExecutionEventKind::Exited(_))
                || self.sandbox_kind != SandboxKind::None
                || !self.enforcement.is_empty()
            {
                return Err(invalid());
            }
            return Ok(None);
        };
        if started.execution != terminal.execution
            || started.sequence >= terminal.sequence
            || matches!(terminal.kind, ExecutionEventKind::Denied { .. })
        {
            return Err(invalid());
        }
        let ExecutionEventKind::Started { fence, .. } = &started.kind else {
            return Err(invalid());
        };
        let body = fence.verify_named_root(expected_root, effective, protected_roots)?;
        if self.sandbox_kind != fence.sandbox_kind
            || self.enforcement != crate::enforcement_report(effective, body.mechanism)
        {
            return Err(invalid());
        }
        if let ExecutionEventKind::Exited(exit) = &terminal.kind {
            if exit.fence != *fence {
                return Err(invalid());
            }
        }
        Ok(Some(body))
    }
}

// Model: gpt-6-astra | Harness: Codex 0.153.4 | Operator: Shawn Hartsock | Time: 22:29 UTC | Date: 2026-09-12
