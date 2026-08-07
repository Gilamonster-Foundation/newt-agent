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

use newt_core::{permits_path, Caveats, CaveatsExt, CountBoundExt};
use newt_identity::AgentKey;
use newt_inference::{ChatRequest, InferenceBackend};

use crate::emission::{normalize_emission, Emission};
use crate::error::{CoderError, Result};
use crate::prompt::{build_prompt, build_reprompt, CoderPrompt};

/// The coder. Holds the inference backend the orchestrator uses for
/// each `run` call; the backend is `Arc<dyn …>` so callers can share
/// one backend across coder + non-coder paths.
///
/// # Issue #93 — operator-rooted parent key
///
/// `Coder` optionally holds the worker's operator-rooted parent
/// [`AgentKey`] (from `WorkerIdentity::Operator { root }`). When a
/// dispatch needs to spawn a subprocess plugin (today: future
/// `ProviderPluginBackend::complete`), it derives a delegated child
/// via [`AgentKey::delegate`] so the child's cert chain roots back to
/// the operator's `UserKey` from `~/.newt/identity.pem` — never a
/// synthetic key.
pub struct Coder {
    backend: Arc<dyn InferenceBackend>,
    /// Operator-rooted parent key (issue #93). The acp-worker plumbs
    /// this from `WorkerIdentity::Operator { root }`; when present, any
    /// subprocess plugin the dispatch spawns inherits a delegated child
    /// from this parent so the cert chain walks back to the operator.
    /// `None` for the `WorkerIdentity::AllowNoKey` debug fallback and
    /// for legacy tests that don't yet plumb identity.
    parent_key: Option<Arc<AgentKey>>,
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
    /// NOTE: when the whole-file re-prompt fallback fires this becomes a
    /// composite first+retry transcript — use [`Self::first_emission`]
    /// when you need just the model's initial output.
    pub raw_reply: String,
    /// The model's *first* raw emission, before any re-prompt fallback.
    /// Always the initial reply (never a composite), so the eval
    /// scorecard can judge it with `git apply --check` (#30B) to tell a
    /// clean diff from a sloppy one the fuzzy worker merely rescued.
    pub first_emission: String,
}

impl Coder {
    /// Build a coder bound to `backend`.
    pub fn new(backend: Arc<dyn InferenceBackend>) -> Self {
        Self {
            backend,
            parent_key: None,
        }
    }

    /// Builder: attach the operator-rooted parent [`AgentKey`] the coder
    /// will use to delegate per-spawn children for subprocess plugins
    /// (issue #93).
    ///
    /// The acp-worker plumbs this in from
    /// `WorkerIdentity::Operator { root }` via
    /// [`WorkerIdentity::parent_key`]; the `AllowNoKey` debug fallback
    /// leaves it `None`. When present, [`Self::plugin_envelope_for`]
    /// can be called by future dispatch paths that need to spawn a
    /// provider plugin without first synthesizing a key.
    #[must_use]
    pub fn with_parent_key(mut self, parent: Arc<AgentKey>) -> Self {
        self.parent_key = Some(parent);
        self
    }

    /// Borrow the parent [`AgentKey`], if one is configured. Tests use
    /// this to assert the operator-rooted threading; future dispatch
    /// paths that spawn provider plugins use it to delegate per-spawn
    /// children.
    #[must_use]
    pub fn parent_key(&self) -> Option<&Arc<AgentKey>> {
        self.parent_key.as_ref()
    }

    /// Mint a plugin-side envelope for a subprocess running under
    /// `role` with `child_caveats`, by delegating from the coder's
    /// operator-rooted parent key.
    ///
    /// Returns:
    /// - `Some(Ok(envelope))` on the happy path — the envelope's cert
    ///   chain roots back to the operator's `UserKey` (issue #93).
    /// - `Some(Err(_))` if delegation refused (`child_caveats` would
    ///   amplify the parent's authority, etc.).
    /// - `None` when no parent key is configured (the
    ///   `WorkerIdentity::AllowNoKey` debug path or a legacy test that
    ///   didn't thread identity).
    ///
    /// The returned envelope is the same shape `newt-mesh`'s
    /// `plugin_envelope::serialize_for_plugin` produces — base64'd JSON
    /// of an `agent_mesh_protocol::CertChain` — so plugins decode it
    /// identically.
    pub fn plugin_envelope_for(
        &self,
        role: &str,
        child_caveats: Caveats,
    ) -> Option<std::result::Result<String, newt_identity::EnvelopeError>> {
        let parent = self.parent_key.as_ref()?;
        Some(newt_identity::delegate_for_plugin(
            parent.as_ref(),
            role,
            child_caveats,
        ))
    }

    /// Run one turn against `workspace` under the authority carried by
    /// `caveats`.
    ///
    /// `caveats` is the peer's signed, verified attenuated authority — see
    /// `docs/decisions/agentic_object_capability_security.md` and the
    /// 35a [`caveats_for_peer`] extractor in `newt-mesh`. Every tool
    /// dispatch this method makes (`fs_read` for the prompt scan,
    /// `net` for the inference call, `fs_write` for the apply, plus the
    /// `max_calls` budget for inference turns) goes through the
    /// enforcement helpers below — no path bypasses the check, even when
    /// `caveats == Caveats::top()`. That symmetry is load-bearing: 35c
    /// will tighten authority per peer, and a "skip checks if top"
    /// shortcut would silently break that tightening.
    ///
    /// On any caveat refusal we return [`CoderError::CapabilityDenied`]
    /// carrying the axis name and the concrete target the dispatch tried
    /// to touch — enough context for the arbiter scorecard to count this
    /// as a scrubbed sortie rather than a model failure.
    ///
    /// Happy path: build prompt -> infer -> normalize -> apply.
    ///
    /// Weak-model fallback: when the model emits a [`Emission::UnifiedDiff`]
    /// (even under the whole-file directive) and that diff fails to apply
    /// — its line numbers / context are too far off even for the fuzzy
    /// matcher in `newt-tools::apply_patch` — we issue exactly ONE
    /// re-prompt asking for the COMPLETE file(s) in `FILE:`/`END-FILE`
    /// form, then apply via the hardened `apply_whole_files` path. The
    /// retry counts as a *second* inference call against the
    /// `max_calls` budget; if that budget would be exhausted we return
    /// the original apply error rather than escalating to a denial.
    ///
    /// [`caveats_for_peer`]: https://docs.rs/newt-mesh/latest/newt_mesh/caveats/fn.caveats_for_peer.html
    pub async fn run(&self, workspace: &Path, task: &str, caveats: &Caveats) -> Result<CoderRun> {
        // 1. Build the prompt. `build_prompt` is what *reads* the
        //    workspace, so the fs_read check is gated on the files the
        //    prompt actually injected, not on the candidate set the
        //    scanner considered.
        let prompt = build_prompt(workspace, task)?;
        check_fs_read(caveats, workspace, &prompt)?;
        tracing::info!(
            files_included = prompt.included_files.len(),
            user_chars = prompt.user.len(),
            "newt-coder prompt built"
        );

        // 2. First inference call — guarded by the net + max_calls axes.
        let mut calls_used: u64 = 0;
        check_call_budget(caveats, calls_used)?;
        check_net(caveats, self.backend.as_ref())?;
        let req = ChatRequest::new().system(prompt.system).user(prompt.user);
        let reply = self
            .backend
            .complete(req)
            .await
            .map_err(|e| CoderError::Inference(e.to_string()))?;
        calls_used += 1;
        let raw = reply.content.clone();
        let model_id = reply.model_id.clone();

        let emission = normalize_emission(&raw)?;
        let shape_label = emission.shape_label().to_string();

        // 3. Try to apply the first emission — `apply` consults the
        //    fs_write axis before each write.
        match self.apply(&emission, workspace, caveats) {
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
                    first_emission: raw.clone(),
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
                self.reprompt_whole_files(workspace, task, raw, first_err, calls_used, caveats)
                    .await
            }
            Err(other) => Err(other),
        }
    }

    /// Single-retry fallback: re-prompt the model for the complete
    /// file(s) and apply via `apply_whole_files`.
    ///
    /// Bounded to ONE additional inference call — there is no loop. The
    /// retry counts as a *second* tool call against
    /// [`Caveats::max_calls`], and if the budget would be exhausted by
    /// that second call we fall through to `original_err` (the apply
    /// failure from the first attempt). On any failure of the retry
    /// (inference error, the model returning yet another diff / prose,
    /// or the whole-file apply failing the shape guards or fs_write
    /// caveat) we return `original_err`, so the caller sees the root
    /// cause rather than a confusing second-order failure.
    async fn reprompt_whole_files(
        &self,
        workspace: &Path,
        task: &str,
        first_raw: String,
        original_err: CoderError,
        calls_used: u64,
        caveats: &Caveats,
    ) -> Result<CoderRun> {
        // The retry would be the (calls_used + 1)-th call; if the
        // budget can't cover it, don't degrade the diagnostic by
        // surfacing a fresh capability denial — keep the original
        // apply failure, which is more actionable.
        if !caveats.max_calls.permits_one_more(calls_used) {
            tracing::warn!(
                calls_used,
                "newt-coder: re-prompt skipped, max_calls budget exhausted"
            );
            return Err(original_err);
        }

        let prompt = match build_reprompt(workspace, task) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "newt-coder: re-prompt build failed");
                return Err(original_err);
            }
        };
        // The re-prompt re-reads the same workspace; fs_read scope must
        // still permit every file the second pass would inject.
        if let Err(e) = check_fs_read(caveats, workspace, &prompt) {
            tracing::warn!(error = %e, "newt-coder: re-prompt fs_read denied");
            return Err(original_err);
        }

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
        match self.apply(&emission, workspace, caveats) {
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
                    // The first emission is the diff the model actually
                    // produced for the task; the scorecard judges *that*,
                    // not the rescued retry.
                    first_emission: first_raw.clone(),
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

    /// Apply one classified emission to `workspace`, under `caveats`.
    /// Returns the list of relative paths written, where known.
    ///
    /// Every filesystem write goes through the `fs_write` axis first,
    /// under **prefix (containment)** semantics: the model-supplied path
    /// is joined to `workspace` and checked with
    /// [`newt_core::permits_path`], the same shared gate the interactive
    /// tool sites use. A production dispatch fences `fs_write` to the
    /// session workspace (`Scope::only([workspace])`, step-4.3), so an
    /// in-workspace target is permitted and a `..`/absolute escape is
    /// denied *before* any write.
    ///
    /// For a [`Emission::WholeFiles`] emission we know every target path
    /// up front, so the check happens before any write touches disk —
    /// partial-apply is never possible under a denied caveat. For a
    /// [`Emission::UnifiedDiff`] we cannot enumerate paths without
    /// re-parsing, so we gate on the **workspace root** itself (the fence
    /// must authorise the session workspace); each hunk is then object-
    /// bound *beneath* the workspace by `apply_patch` (#522), so a hunk
    /// escaping the fence is rejected at the write primitive regardless.
    fn apply(
        &self,
        emission: &Emission,
        workspace: &Path,
        caveats: &Caveats,
    ) -> Result<Vec<String>> {
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
                // Caveat check: every target path must be permitted on
                // the fs_write axis. The emission key is workspace-relative;
                // join it to `workspace` and gate with prefix (containment)
                // semantics so a workspace fence permits in-workspace targets
                // and denies `..`/absolute escapes. We loop *all* paths before
                // committing any write so a denial on the second file can't
                // leave the first file half-written.
                for path in files.keys() {
                    let full = workspace.join(path);
                    if !permits_path(&caveats.fs_write, &full.to_string_lossy()) {
                        return Err(CoderError::CapabilityDenied {
                            kind: "fs_write",
                            target: path.clone(),
                        });
                    }
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
                // We can't enumerate the touched paths without re-parsing the
                // diff, so gate on the workspace root itself: the fence must
                // authorise the session workspace (`Scope::All`, or an
                // `Only([…])` that contains it). A fence that doesn't cover the
                // workspace — or a deny-all `none()` — denies the dispatch up
                // front. Each hunk is then object-bound *beneath* the workspace
                // by `apply_patch` (#522), so a hunk targeting `../escape` is
                // rejected at the write primitive even though this coarse check
                // passed. Target the diff blob itself so the error message
                // points at the can't-enumerate-paths reason.
                if !permits_path(&caveats.fs_write, &workspace.to_string_lossy()) {
                    return Err(CoderError::CapabilityDenied {
                        kind: "fs_write",
                        target: "<unified_diff: paths not enumerable>".to_string(),
                    });
                }
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

// ── Enforcement helpers ────────────────────────────────────────────────────
//
// One helper per axis the dispatch sites consult. Every helper goes through
// `Caveats::permits_*` even when the caveat is `top` — there is no fast-path
// bypass, by design. See the module/`Coder::run` doc comments.

/// Check whether `caveats.max_calls` permits one more inference call
/// given `used_so_far` calls already counted against this run.
fn check_call_budget(caveats: &Caveats, used_so_far: u64) -> Result<()> {
    if caveats.max_calls.permits_one_more(used_so_far) {
        Ok(())
    } else {
        Err(CoderError::CapabilityDenied {
            kind: "max_calls",
            target: format!("turn #{}", used_so_far + 1),
        })
    }
}

/// Check whether `caveats.net` permits the network call the backend
/// would make on `complete()`. Backends with no endpoint (mocks,
/// in-process plugins) skip the check vacuously — there is no host to
/// consult.
fn check_net(caveats: &Caveats, backend: &dyn InferenceBackend) -> Result<()> {
    let endpoint = match backend.endpoint() {
        Some(e) => e,
        None => return Ok(()),
    };
    let host = host_from_endpoint(endpoint);
    if caveats.permits_net(host) {
        Ok(())
    } else {
        Err(CoderError::CapabilityDenied {
            kind: "net",
            target: host.to_string(),
        })
    }
}

/// Check whether `caveats.fs_read` permits every file the prompt
/// actually injected. We gate on `included_files` (what was read), not
/// on the wider candidate set the scanner considered, so the denial
/// fires only when the model would have *seen* a forbidden path.
fn check_fs_read(caveats: &Caveats, workspace: &Path, prompt: &CoderPrompt) -> Result<()> {
    for path in &prompt.included_files {
        // `included_files` are workspace-relative; join to `workspace` and gate
        // with prefix (containment) semantics, matching the fs_write side and
        // the interactive tool gate. A workspace-scoped fs_read fence then
        // permits every in-workspace file the prompt injected.
        let full = workspace.join(path);
        if !permits_path(&caveats.fs_read, &full.to_string_lossy()) {
            return Err(CoderError::CapabilityDenied {
                kind: "fs_read",
                target: path.to_string_lossy().into_owned(),
            });
        }
    }
    Ok(())
}

/// Extract the host portion of an HTTP(S) URL — enough for the
/// `caveats.net` exact-match check, without dragging in a `url` crate
/// dependency. Strips `scheme://`, then takes everything up to the
/// first `/`, `?`, or port `:`. Returns the input unchanged if no
/// scheme prefix is present (treating it as already a bare host).
fn host_from_endpoint(endpoint: &str) -> &str {
    let after_scheme = endpoint
        .find("://")
        .map(|i| &endpoint[i + 3..])
        .unwrap_or(endpoint);
    let end = after_scheme
        .find(['/', ':', '?'])
        .unwrap_or(after_scheme.len());
    &after_scheme[..end]
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
            .apply(&Emission::WholeFiles(files), tmp.path(), &Caveats::top())
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
            .apply(
                &Emission::Prose("I've updated it.".to_string()),
                tmp.path(),
                &Caveats::top(),
            )
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
            .apply(
                &Emission::UnifiedDiff(diff.to_string()),
                tmp.path(),
                &Caveats::top(),
            )
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
        let err = coder.apply(&bad, tmp.path(), &Caveats::top()).unwrap_err();
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
            .apply(
                &whole_files("src/lib.rs", new_body),
                tmp.path(),
                &Caveats::top(),
            )
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
            .apply(&whole_files("a.txt", diff), tmp.path(), &Caveats::top())
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
            .apply(&whole_files("a.txt", hunk), tmp.path(), &Caveats::top())
            .unwrap_err();
        assert!(matches!(err, CoderError::LooksLikeDiff { .. }));
    }

    #[test]
    fn apply_whole_files_rejects_empty_contents() {
        let tmp = TempDir::new().unwrap();
        let coder = coder_with_no_backend_used();
        let err = coder
            .apply(&whole_files("a.txt", ""), tmp.path(), &Caveats::top())
            .unwrap_err();
        assert!(matches!(err, CoderError::EmptyEmission { ref path } if path == "a.txt"));
    }

    #[test]
    fn apply_whole_files_rejects_whitespace_only_contents() {
        let tmp = TempDir::new().unwrap();
        let coder = coder_with_no_backend_used();
        let err = coder
            .apply(
                &whole_files("a.txt", "   \n\t\n"),
                tmp.path(),
                &Caveats::top(),
            )
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
            .apply(
                &whole_files("src/lib.rs", body),
                tmp.path(),
                &Caveats::top(),
            )
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

    // ── Caveat enforcement at the apply boundary ─────────────────────────

    // ── Caveat enforcement at the apply boundary ─────────────────────────
    //
    // step-4.3: the fs_write scope now stores absolute roots and is matched by
    // containment (`newt_core::permits_path`), mirroring production dispatch
    // (`Scope::only([workspace])`). The pre-step-4.3 per-file relative-exact
    // fixtures are retired — see `apply_under_workspace_fence_permits_inside_
    // denies_escape` above for the core permit/deny property.

    #[test]
    fn apply_whole_files_denies_when_fence_excludes_workspace() {
        // A fs_write fence pointing at some OTHER directory must deny a write
        // into this workspace — the fence is a real gate, not vacuous.
        let tmp = TempDir::new().unwrap();
        let elsewhere = TempDir::new().unwrap();
        let coder = coder_with_no_backend_used();
        let caveats = Caveats {
            fs_write: newt_core::Scope::only([elsewhere.path().to_string_lossy().into_owned()]),
            ..Caveats::top()
        };

        let err = coder
            .apply(
                &whole_files("forbidden.rs", "fn evil() {}\n"),
                tmp.path(),
                &caveats,
            )
            .unwrap_err();
        match err {
            CoderError::CapabilityDenied { kind, target } => {
                assert_eq!(kind, "fs_write");
                assert_eq!(target, "forbidden.rs");
            }
            other => panic!("expected CapabilityDenied, got {other:?}"),
        }
        // And the file was never created.
        assert!(!tmp.path().join("forbidden.rs").exists());
    }

    #[test]
    fn apply_whole_files_denies_atomically_on_partial_scope() {
        // A multi-file emission where one path ESCAPES the workspace fence must
        // write NOTHING — the check loops every path before committing any
        // write. Regression for the "wrote half the emission then refused" mode.
        let tmp = TempDir::new().unwrap();
        let coder = coder_with_no_backend_used();
        let caveats = Caveats {
            fs_write: newt_core::Scope::only([tmp.path().to_string_lossy().into_owned()]),
            ..Caveats::top()
        };
        let mut files = BTreeMap::new();
        files.insert("a.rs".to_string(), "fn a() {}\n".to_string()); // inside
        files.insert("../b.rs".to_string(), "fn b() {}\n".to_string()); // escape

        let err = coder
            .apply(&Emission::WholeFiles(files), tmp.path(), &caveats)
            .unwrap_err();
        assert!(matches!(
            err,
            CoderError::CapabilityDenied {
                kind: "fs_write",
                ..
            }
        ));
        // Neither the in-workspace file nor the escape landed.
        assert!(!tmp.path().join("a.rs").exists());
        assert!(!tmp.path().parent().unwrap().join("b.rs").exists());
    }

    #[test]
    fn apply_unified_diff_gated_on_workspace_fence() {
        // We can't enumerate diff paths up front, so the dispatch is gated on
        // the workspace root itself: a fence that does NOT cover the workspace
        // denies; a fence that DOES cover it permits (each hunk is then object-
        // bound beneath the workspace by `apply_patch`).
        let tmp = TempDir::new().unwrap();
        let coder = coder_with_no_backend_used();
        let diff = Emission::UnifiedDiff(
            "--- a/whatever.rs\n+++ b/whatever.rs\n@@ -1 +1 @@\n-x\n+y\n".to_string(),
        );

        // Deny-all fence → denied up front.
        let foreign = Caveats {
            fs_write: newt_core::Scope::none(),
            ..Caveats::top()
        };
        let err = coder.apply(&diff, tmp.path(), &foreign).unwrap_err();
        assert!(matches!(
            err,
            CoderError::CapabilityDenied {
                kind: "fs_write",
                ..
            }
        ));

        // Workspace fence → the caveat gate permits it. Any error here is
        // downstream diff-apply mechanics (the target file doesn't exist), a
        // different error class — NOT a fs_write capability denial.
        let fenced = Caveats {
            fs_write: newt_core::Scope::only([tmp.path().to_string_lossy().into_owned()]),
            ..Caveats::top()
        };
        if let Err(e) = coder.apply(&diff, tmp.path(), &fenced) {
            assert!(
                !matches!(
                    e,
                    CoderError::CapabilityDenied {
                        kind: "fs_write",
                        ..
                    }
                ),
                "workspace fence must not raise a fs_write capability denial: {e:?}"
            );
        }
    }

    #[test]
    fn apply_under_workspace_fence_permits_inside_denies_escape() {
        // step-4.3 (`acp-worker-fs-scope`): production ACP dispatch fences
        // fs_write to the session workspace (`Scope::only([workspace_abs])`)
        // instead of `Scope::All`. The coder's apply gate must therefore PERMIT
        // a write to a file INSIDE the workspace and DENY a `..`-escape that
        // resolves outside — the adversarial case a hostile model would emit.
        //
        // Regression: before the exact-match → prefix switch, a workspace fence
        // denied EVERY in-workspace write (the relative emission key never
        // string-matched the absolute root), so the fence could not be activated
        // without breaking the coder's own legitimate writes. This test is red
        // until `apply` joins to the workspace and gates with prefix semantics.
        let tmp = TempDir::new().unwrap();
        let coder = coder_with_no_backend_used();
        let ws = tmp.path().to_str().unwrap().to_string();
        let caveats = Caveats {
            fs_write: newt_core::Scope::only([ws]),
            ..Caveats::top()
        };

        // Inside the workspace → permitted, and the file lands.
        let written = coder
            .apply(&whole_files("lib.rs", "fn ok() {}\n"), tmp.path(), &caveats)
            .expect("in-workspace write must be permitted under a workspace fence");
        assert_eq!(written, vec!["lib.rs".to_string()]);
        assert!(tmp.path().join("lib.rs").exists());

        // `..`-escape → denied before any write, and nothing lands outside.
        let err = coder
            .apply(
                &whole_files("../evil.rs", "fn evil() {}\n"),
                tmp.path(),
                &caveats,
            )
            .unwrap_err();
        assert!(matches!(
            err,
            CoderError::CapabilityDenied {
                kind: "fs_write",
                ..
            }
        ));
        assert!(!tmp.path().parent().unwrap().join("evil.rs").exists());
    }

    // ── host_from_endpoint ───────────────────────────────────────────────

    #[test]
    fn host_from_endpoint_strips_scheme_and_path() {
        assert_eq!(
            super::host_from_endpoint("http://localhost:11434/api/chat"),
            "localhost"
        );
        assert_eq!(
            super::host_from_endpoint("https://allowed.example.com/v1/chat"),
            "allowed.example.com"
        );
        // No scheme — treated as a bare host.
        assert_eq!(
            super::host_from_endpoint("bare.host.local"),
            "bare.host.local"
        );
        // No path, just host:port.
        assert_eq!(super::host_from_endpoint("http://h:8080"), "h");
        // Empty path component.
        assert_eq!(super::host_from_endpoint("https://only.host/"), "only.host");
    }

    // ── check_call_budget ────────────────────────────────────────────────

    #[test]
    fn check_call_budget_passes_under_unlimited() {
        super::check_call_budget(&Caveats::top(), 0).unwrap();
        super::check_call_budget(&Caveats::top(), 999_999).unwrap();
    }

    #[test]
    fn check_call_budget_passes_within_bound() {
        let caveats = Caveats {
            max_calls: newt_core::CountBound::AtMost(3),
            ..Caveats::top()
        };
        super::check_call_budget(&caveats, 0).unwrap();
        super::check_call_budget(&caveats, 2).unwrap();
    }

    #[test]
    fn check_call_budget_denies_at_bound() {
        let caveats = Caveats {
            max_calls: newt_core::CountBound::AtMost(2),
            ..Caveats::top()
        };
        let err = super::check_call_budget(&caveats, 2).unwrap_err();
        match err {
            CoderError::CapabilityDenied { kind, target } => {
                assert_eq!(kind, "max_calls");
                assert!(target.contains("#3"));
            }
            other => panic!("expected CapabilityDenied, got {other:?}"),
        }
    }
}
