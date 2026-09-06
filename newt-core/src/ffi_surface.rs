//! The `knowledge_base` technique (R1) as a memory provider.
//!
//! When a profile lists `knowledge_base` (`docs/design/technique-library.md`),
//! this provider injects the authoritative PyO3 import surface — the FFI manifest
//! ([`crate::ffi_manifest`]) — into the frozen system prompt, so the model imports
//! real paths (`newt_agent._newt_agent.core`) instead of guessing `newt_core` from
//! the crate name (the cross-family confabulation, `docs/findings/`). It rides the
//! same provider seam as [`crate::AgentsProvider`], so the block survives every
//! system-prompt rebuild. A **no-op on a non-PyO3 workspace** (empty manifest → no
//! block) — the technique generalizes safely.
//!
//! **Compression role (#661 group E).** Because the surface lives in the frozen
//! system prompt — the compressor's protected head ([`super::agentic::compress`]
//! `head_len`) — it is a **stable base that is NEVER summarized away**: the model
//! keeps an exact import surface even after the middle is compacted, so the lossy
//! summary has less it must preserve, and an import detail can't be lost to
//! compression. This is the "inject the authoritative import surface as a stable
//! base so there's less to summarize" half of the summarizer-effectiveness suite,
//! verified by `compress::retained_context_tests::knowledge_base_stable_base_survives_compression`.

use async_trait::async_trait;
use std::path::Path;

use crate::ffi_manifest::FfiManifest;
use crate::memory::{MemMessage, MemoryProvider, SessionContext};
use crate::metrics::TurnMetrics;

/// Injects the workspace's FFI import surface into the system prompt.
#[derive(Default)]
pub struct FfiSurfaceProvider {
    /// The rendered surface block, computed once at [`initialize`](Self::initialize).
    /// `None` when the workspace has no PyO3 bindings.
    block: Option<String>,
}

impl FfiSurfaceProvider {
    /// A provider that resolves its surface from the session workspace.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl MemoryProvider for FfiSurfaceProvider {
    fn name(&self) -> &str {
        "knowledge_base"
    }

    async fn initialize(&mut self, ctx: &SessionContext) -> anyhow::Result<()> {
        let manifest = FfiManifest::from_workspace(Path::new(&ctx.workspace)).unwrap_or_default();
        let n = manifest.len();
        self.block = if manifest.is_empty() {
            None
        } else {
            Some(manifest.render_block())
        };
        if self.block.is_some() {
            tracing::info!(crates = n, "knowledge_base: FFI import surface injected");
        }
        Ok(())
    }

    fn system_prompt_block(&self) -> Option<String> {
        self.block.clone()
    }

    fn build_messages(&self, _system_prompt: &str, _new_task: &str) -> Vec<MemMessage> {
        // System-prompt-only provider; history is managed elsewhere.
        Vec::new()
    }

    async fn sync_turn(&mut self, _user: &str, _assistant: &str, _metrics: &TurnMetrics) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(workspace: &str) -> SessionContext {
        SessionContext {
            workspace: workspace.to_string(),
            session_id: "s".into(),
        }
    }

    fn write_binding(root: &Path, krate: &str, submodule: &str) {
        let dir = root.join(krate).join("src");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("pyo3_module.rs"),
            format!("#[pyclass(name = \"X\", module = \"newt_agent._newt_agent.{submodule}\")] struct X;"),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn injects_surface_for_a_pyo3_workspace() {
        let dir = tempfile::tempdir().unwrap();
        write_binding(dir.path(), "newt-core", "core");
        let mut p = FfiSurfaceProvider::new();
        p.initialize(&ctx(dir.path().to_str().unwrap()))
            .await
            .unwrap();
        let block = p.system_prompt_block().expect("a surface block");
        assert!(block.contains("newt_agent._newt_agent.core"));
        assert!(block.contains("authoritative"));
    }

    #[tokio::test]
    async fn no_op_on_non_pyo3_workspace() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("README.md"), "no bindings here").unwrap();
        let mut p = FfiSurfaceProvider::new();
        p.initialize(&ctx(dir.path().to_str().unwrap()))
            .await
            .unwrap();
        assert!(p.system_prompt_block().is_none(), "should inject nothing");
    }
}
