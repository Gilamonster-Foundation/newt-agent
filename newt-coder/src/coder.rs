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
use crate::prompt::{build_prompt, build_reprompt};

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
    ///
    /// Happy path: build prompt -> infer -> normalize -> apply.
    ///
    /// Weak-model fallback: when the model emits a [`Emission::UnifiedDiff`]
    /// (even under the whole-file directive) and that diff fails to apply
    /// — its line numbers / context are too far off even for the fuzzy
    /// matcher in `newt-tools::apply_patch` — we issue exactly ONE
    /// re-prompt asking for the COMPLETE file(s) in `FILE:`/`END-FILE`
    /// form, then apply via the hardened `apply_whole_files` path. The
    /// retry is bounded to a single attempt; if it still doesn't yield
    /// usable whole-file output we return the original apply error.
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

        // Try to apply the first emission.
        match self.apply(&emission, workspace) {
            Ok(files_written) => {
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
            // The first emission was diff-shaped and did not apply: either a
            // unified diff whose context was too far off even for the fuzzy
            // matcher, or diff content the model wrapped in FILE:/END-FILE
            // markers (classified as whole-files but rejected by the
            // diff-shape guard). Both are recoverable with a single re-prompt
            // for proper whole-file output.
            Err(first_err)
                if matches!(emission, Emission::UnifiedDiff(_))
                    || matches!(first_err, CoderError::LooksLikeDiff { .. }) =>
            {
                tracing::warn!(
                    error = %first_err,
                    "newt-coder: diff-shaped emission did not apply, re-prompting for whole files"
                );
                self.reprompt_whole_files(workspace, task, raw, first_err)
                    .await
            }
            Err(other) => Err(other),
        }
    }

    /// Single-retry fallback: re-prompt the model for the complete
    /// file(s) and apply via `apply_whole_files`.
    ///
    /// Bounded to ONE additional inference call — there is no loop. On any
    /// failure of the retry (inference error, the model returning yet
    /// another diff / prose, or the whole-file apply failing the shape
    /// guards) we return `original_err`, the error from the first attempt,
    /// so the caller sees the root cause rather than a confusing
    /// second-order failure.
    async fn reprompt_whole_files(
        &self,
        workspace: &Path,
        task: &str,
        first_raw: String,
        original_err: CoderError,
    ) -> Result<CoderRun> {
        let prompt = match build_reprompt(workspace, task) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "newt-coder: re-prompt build failed");
                return Err(original_err);
            }
        };

        let req = ChatRequest::new().system(prompt.system).user(prompt.user);
        let reply = match self.backend.complete(req).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "newt-coder: re-prompt inference failed");
                return Err(original_err);
            }
        };
        let retry_raw = reply.content.clone();
        let model_id = reply.model_id.clone();

        // The retry must yield whole files; anything else (another diff,
        // prose) is not usable for this fallback.
        let emission = match normalize_emission(&retry_raw) {
            Ok(em @ Emission::WholeFiles(_)) => em,
            Ok(other) => {
                tracing::warn!(
                    emission_shape = %other.shape_label(),
                    "newt-coder: re-prompt did not return whole files"
                );
                return Err(original_err);
            }
            Err(e) => {
                tracing::warn!(error = %e, "newt-coder: re-prompt emission malformed");
                return Err(original_err);
            }
        };

        let shape_label = emission.shape_label().to_string();
        match self.apply(&emission, workspace) {
            Ok(files_written) => {
                tracing::info!(
                    emission_shape = %shape_label,
                    files_written = files_written.len(),
                    "newt-coder: re-prompt whole-file fallback applied"
                );
                Ok(CoderRun {
                    // Reflect what *actually* applied: the whole-file retry,
                    // not the original diff.
                    emission_shape: shape_label,
                    model_id,
                    files_written,
                    // Keep an audit trail of both turns: the first
                    // (rejected) diff and the retry that landed.
                    raw_reply: format!(
                        "[diff-apply failed, re-prompted for whole files]\n\
                         --- first reply ---\n{first_raw}\n\
                         --- retry reply ---\n{retry_raw}"
                    ),
                })
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "newt-coder: re-prompt whole-file apply failed"
                );
                Err(original_err)
            }
        }
    }

    /// Apply one classified emission to `workspace`. Returns the
    /// list of relative paths written, where known.
    fn apply(&self, emission: &Emission, workspace: &Path) -> Result<Vec<String>> {
        match emission {
            Emission::WholeFiles(files) => {
                // Shape guards before writing. A whole-file emission
                // legitimately rewrites every line (renames, signature
                // changes, new doc comments), so we do NOT compare the
                // body against what's on disk. We reject only bodies
                // whose *shape* is wrong; the real correctness gate is
                // the downstream `git diff` capture plus the eval
                // compile/test evaluators.
                for (path, contents) in files {
                    reject_bad_shape(path, contents)?;
                }
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

/// Reject a whole-file emission whose body has the wrong *shape*.
///
/// This replaces the old "first non-blank line must equal the file's
/// existing anchor line" check, which wrongly rejected correct output
/// whenever a rename or signature change altered line 1. Instead we
/// only refuse bodies that are:
///
/// - empty / whitespace-only ([`CoderError::EmptyEmission`]),
/// - diff-shaped — first non-blank line starts with `--- `, `+++ `, or
///   `@@` ([`CoderError::LooksLikeDiff`]), or
/// - still prefixed with a leaked `FILE:` marker as their first
///   non-blank line ([`CoderError::LeakedMarker`]) — defense in depth
///   in case [`crate::emission`] did not strip it.
fn reject_bad_shape(path: &str, contents: &str) -> Result<()> {
    let first_non_blank = contents.lines().find(|l| !l.trim().is_empty());
    match first_non_blank {
        None => Err(CoderError::EmptyEmission {
            path: path.to_string(),
        }),
        Some(first) => {
            let trimmed = first.trim_start();
            if trimmed.starts_with("--- ")
                || trimmed.starts_with("+++ ")
                || trimmed.starts_with("@@")
            {
                return Err(CoderError::LooksLikeDiff {
                    path: path.to_string(),
                });
            }
            if trimmed.starts_with("FILE:") {
                return Err(CoderError::LeakedMarker {
                    path: path.to_string(),
                });
            }
            Ok(())
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

    fn whole_files(path: &str, contents: &str) -> Emission {
        let mut m = BTreeMap::new();
        m.insert(path.to_string(), contents.to_string());
        Emission::WholeFiles(m)
    }

    #[test]
    fn apply_whole_files_accepts_line_one_change() {
        // Regression for failures 1 & 2 (rename / signature change):
        // the emitted first line differs from the existing first line,
        // which the old anchor check wrongly rejected. It must now apply.
        let tmp = TempDir::new().unwrap();
        let coder = coder_with_no_backend_used();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(
            tmp.path().join("src/lib.rs"),
            "pub fn hello(name: &str) -> String {\n    format!(\"hi {name}\")\n}\n",
        )
        .unwrap();

        let new_body = "pub fn greet(name: &str) -> String {\n    format!(\"hi {name}\")\n}\n";
        let written = coder
            .apply(&whole_files("src/lib.rs", new_body), tmp.path())
            .unwrap();
        assert_eq!(written, vec!["src/lib.rs".to_string()]);
        assert_eq!(
            fs::read_to_string(tmp.path().join("src/lib.rs")).unwrap(),
            new_body
        );
    }

    #[test]
    fn apply_whole_files_rejects_diff_shaped_contents() {
        let tmp = TempDir::new().unwrap();
        let coder = coder_with_no_backend_used();
        fs::write(tmp.path().join("a.txt"), "old\n").unwrap();
        let diff = "--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-old\n+new\n";
        let err = coder
            .apply(&whole_files("a.txt", diff), tmp.path())
            .unwrap_err();
        assert!(matches!(err, CoderError::LooksLikeDiff { ref path } if path == "a.txt"));
        // The file must not have been overwritten.
        assert_eq!(
            fs::read_to_string(tmp.path().join("a.txt")).unwrap(),
            "old\n"
        );
    }

    #[test]
    fn apply_whole_files_rejects_hunk_only_contents() {
        let tmp = TempDir::new().unwrap();
        let coder = coder_with_no_backend_used();
        let hunk = "@@ -1,2 +1,2 @@\n-old\n+new\n";
        let err = coder
            .apply(&whole_files("a.txt", hunk), tmp.path())
            .unwrap_err();
        assert!(matches!(err, CoderError::LooksLikeDiff { .. }));
    }

    #[test]
    fn apply_whole_files_rejects_empty_contents() {
        let tmp = TempDir::new().unwrap();
        let coder = coder_with_no_backend_used();
        let err = coder
            .apply(&whole_files("a.txt", ""), tmp.path())
            .unwrap_err();
        assert!(matches!(err, CoderError::EmptyEmission { ref path } if path == "a.txt"));
    }

    #[test]
    fn apply_whole_files_rejects_whitespace_only_contents() {
        let tmp = TempDir::new().unwrap();
        let coder = coder_with_no_backend_used();
        let err = coder
            .apply(&whole_files("a.txt", "   \n\t\n"), tmp.path())
            .unwrap_err();
        assert!(matches!(err, CoderError::EmptyEmission { .. }));
    }

    #[test]
    fn apply_whole_files_rejects_leaked_file_marker() {
        // Defense in depth (failures 3 & 4): even if a leaked FILE:
        // marker slipped past the parser, the writer must refuse it.
        let tmp = TempDir::new().unwrap();
        let coder = coder_with_no_backend_used();
        let body = "FILE: src/lib.rs\npub fn add(a: i32, b: i32) -> i32 { a + b }\n";
        let err = coder
            .apply(&whole_files("src/lib.rs", body), tmp.path())
            .unwrap_err();
        assert!(matches!(err, CoderError::LeakedMarker { ref path } if path == "src/lib.rs"));
    }

    #[test]
    fn reject_bad_shape_messages_start_with_file_write_failed() {
        for err in [
            super::reject_bad_shape("p", "").unwrap_err(),
            super::reject_bad_shape("p", "--- a/p\n").unwrap_err(),
            super::reject_bad_shape("p", "FILE: p\n").unwrap_err(),
        ] {
            assert!(
                err.to_string().starts_with("file write failed:"),
                "message did not start with prefix: {err}"
            );
        }
    }
}
