//! The Coder orchestrator — prompt -> infer -> normalize -> apply.
//!
//! One method, [`Coder::run`], wires the four pieces together:
//!
//! 1. [`build_prompt`](crate::prompt::build_prompt) scans the workspace
//!    for relevant files and composes a `(system, user)` pair around
//!    the S5 whole-file directive.
//! 2. The injected [`InferenceBackend`] runs one `complete` turn.
//! 3. [`normalize_emission`](crate::emission::normalize_emission)
//!    classifies the raw reply as `WholeFiles` / `UnifiedDiff` /
//!    `Prose`.
//! 4. The classified emission is applied to the workspace:
//!    `apply_whole_files` for the directive's happy path,
//!    `apply_patch` for the diff fallback, no-op + warn for prose.
//!
//! The caller (newt-acp-worker) then runs `git diff` to capture the
//! real workspace diff — the foreman's empty-diff signal is computed
//! from `git diff`, not from anything in this struct.

use std::path::Path;
use std::sync::Arc;

use newt_inference::{ChatRequest, InferenceBackend};

use crate::emission::{normalize_emission, Emission};
use crate::error::{CoderError, Result};
use crate::prompt::build_prompt;

/// The coder. Holds the inference backend the orchestrator uses for
/// each `run` call; the backend is `Arc<dyn …>` so callers can share
/// one backend across coder + non-coder paths.
pub struct Coder {
    backend: Arc<dyn InferenceBackend>,
}

/// Outcome of one `Coder::run` turn. Surfaced via the ACP worker's
/// `TaskReply.emission_shape` so the foreman's scorecard can
/// distinguish T0a / T0b / T0c instead of lumping them as "empty
/// diff".
#[derive(Debug, Clone)]
pub struct CoderRun {
    /// Wire-stable shape label: "whole_files", "unified_diff", "prose".
    pub emission_shape: String,
    /// Model id the inference backend returned.
    pub model_id: String,
    /// Relative paths of files the run wrote (empty for prose / diff
    /// — the diff path doesn't tell us which files it touched without
    /// re-parsing).
    pub files_written: Vec<String>,
    /// The raw model reply. Useful for audit logs and post-mortem.
    pub raw_reply: String,
}

impl Coder {
    /// Build a coder bound to `backend`.
    pub fn new(backend: Arc<dyn InferenceBackend>) -> Self {
        Self { backend }
    }

    /// Run one turn against `workspace`.
    pub async fn run(&self, workspace: &Path, task: &str) -> Result<CoderRun> {
        let prompt = build_prompt(workspace, task)?;
        tracing::info!(
            files_included = prompt.included_files.len(),
            user_chars = prompt.user.len(),
            "newt-coder prompt built"
        );

        let req = ChatRequest::new().system(prompt.system).user(prompt.user);
        let reply = self
            .backend
            .complete(req)
            .await
            .map_err(|e| CoderError::Inference(e.to_string()))?;
        let raw = reply.content.clone();
        let model_id = reply.model_id.clone();

        let emission = normalize_emission(&raw)?;
        let shape_label = emission.shape_label().to_string();
        let files_written = self.apply(&emission, workspace)?;

        tracing::info!(
            emission_shape = %shape_label,
            files_written = files_written.len(),
            "newt-coder run complete"
        );

        Ok(CoderRun {
            emission_shape: shape_label,
            model_id,
            files_written,
            raw_reply: raw,
        })
    }

    /// Apply one classified emission to `workspace`. Returns the
    /// list of relative paths written, where known.
    fn apply(&self, emission: &Emission, workspace: &Path) -> Result<Vec<String>> {
        match emission {
            Emission::WholeFiles(files) => {
                // `apply_whole_files` wants `(String, String)` tuples;
                // collect to give it owned values without leaking the
                // BTreeMap iterator's lifetime into the call.
                let pairs: Vec<(String, String)> =
                    files.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                let written = newt_tools::apply_whole_files(workspace, pairs)
                    .map_err(|e| CoderError::FileWrite(e.to_string()))?;
                Ok(written)
            }
            Emission::UnifiedDiff(diff) => {
                // Legacy path: model emitted a real diff. We don't
                // know which files it touched without re-parsing, so
                // return an empty `files_written` — the caller's
                // `git diff` capture is the source of truth.
                newt_tools::apply_patch(workspace, diff)
                    .map_err(|e| CoderError::FileWrite(e.to_string()))?;
                Ok(Vec::new())
            }
            Emission::Prose(prose) => {
                tracing::warn!(
                    prose_len = prose.len(),
                    "newt-coder: prose-only emission, no edits"
                );
                Ok(Vec::new())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;
    use tempfile::TempDir;

    // Apply-only tests; the end-to-end smoke (build_prompt -> backend
    // -> normalize -> apply) lives in tests/coder_smoke.rs.

    fn coder_with_no_backend_used() -> Coder {
        // The `apply` method does not call the backend, so we can use
        // any backend here. We construct one only so the type checks.
        // Tests in tests/ use a real MockBackend for the run() path.
        struct Stub;
        #[async_trait::async_trait]
        impl InferenceBackend for Stub {
            fn name(&self) -> &str {
                "stub"
            }
            fn model_id(&self) -> &str {
                "stub-model"
            }
            fn supports_tier(&self, _t: newt_core::router::Tier) -> bool {
                false
            }
            async fn complete(
                &self,
                _req: ChatRequest,
            ) -> anyhow::Result<newt_inference::ChatReply> {
                unreachable!("apply tests do not call the backend")
            }
        }
        Coder::new(Arc::new(Stub))
    }

    #[test]
    fn apply_whole_files_writes_to_workspace() {
        let tmp = TempDir::new().unwrap();
        let coder = coder_with_no_backend_used();

        let mut files = BTreeMap::new();
        files.insert("src/lib.rs".to_string(), "pub fn hello() {}\n".to_string());

        let written = coder
            .apply(&Emission::WholeFiles(files), tmp.path())
            .unwrap();
        assert_eq!(written, vec!["src/lib.rs".to_string()]);
        let content = fs::read_to_string(tmp.path().join("src/lib.rs")).unwrap();
        assert_eq!(content, "pub fn hello() {}\n");
    }

    #[test]
    fn apply_prose_writes_nothing() {
        let tmp = TempDir::new().unwrap();
        let coder = coder_with_no_backend_used();
        let written = coder
            .apply(&Emission::Prose("I've updated it.".to_string()), tmp.path())
            .unwrap();
        assert!(written.is_empty());
    }

    #[test]
    fn apply_unified_diff_returns_empty_files_written() {
        let tmp = TempDir::new().unwrap();
        // Seed a file so the diff actually applies.
        fs::write(tmp.path().join("a.txt"), "old\n").unwrap();
        let diff = "\
--- a/a.txt
+++ b/a.txt
@@ -1 +1 @@
-old
+new
";
        let coder = coder_with_no_backend_used();
        let written = coder
            .apply(&Emission::UnifiedDiff(diff.to_string()), tmp.path())
            .unwrap();
        assert!(written.is_empty(), "diff path returns empty files_written");
        let content = fs::read_to_string(tmp.path().join("a.txt")).unwrap();
        assert_eq!(content, "new\n");
    }

    #[test]
    fn apply_bad_diff_surfaces_filewrite_error() {
        let tmp = TempDir::new().unwrap();
        let coder = coder_with_no_backend_used();
        let bad = Emission::UnifiedDiff("not a real diff".to_string());
        let err = coder.apply(&bad, tmp.path()).unwrap_err();
        assert!(matches!(err, CoderError::FileWrite(_)));
    }
}
