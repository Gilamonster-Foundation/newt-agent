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

use newt_core::Caveats;
use newt_inference::{ChatRequest, InferenceBackend};

use crate::emission::{normalize_emission, Emission};
use crate::error::{CoderError, Result};
use crate::prompt::{build_prompt, build_reprompt, CoderPrompt};

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
        Self { backend }
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
        check_fs_read(caveats, &prompt)?;
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
        if let Err(e) = check_fs_read(caveats, &prompt) {
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
    /// Every filesystem write goes through the `fs_write` axis first.
    /// For a [`Emission::WholeFiles`] emission we know every target
    /// path up front, so the check happens before any write touches
    /// disk — partial-apply is never possible under a denied caveat.
    /// For a [`Emission::UnifiedDiff`] we cannot enumerate paths
    /// without re-parsing, so we require `fs_write` to be
    /// [`Scope::All`](newt_core::Scope::All) — bounded fs_write +
    /// diff emission is a denial. This is conservative on purpose:
    /// 35c will swap diff dispatch for a parser that knows the paths,
    /// and the conservative rule is easier to weaken later than to
    /// retrofit a "we already wrote half the diff" rollback.
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
                // the fs_write axis. We loop *all* paths before
                // committing any write so a denial on the second file
                // can't leave the first file half-written.
                for path in files.keys() {
                    if !caveats.permits_fs_write(path) {
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
                // We can't enumerate the touched paths without
                // re-parsing the diff. Be conservative: require
                // `fs_write = All`. Anything narrower denies the
                // dispatch up front. Target the diff blob itself so
                // the error message points the reader at the
                // can't-enumerate-paths reason.
                if !matches!(caveats.fs_write, newt_core::Scope::All) {
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
fn check_fs_read(caveats: &Caveats, prompt: &CoderPrompt) -> Result<()> {
    for path in &prompt.included_files {
        let s = path.to_string_lossy();
        if !caveats.permits_fs_read(&s) {
            return Err(CoderError::CapabilityDenied {
                kind: "fs_read",
                target: s.into_owned(),
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

    #[test]
    fn apply_whole_files_denies_path_outside_fs_write_scope() {
        let tmp = TempDir::new().unwrap();
        let coder = coder_with_no_backend_used();
        let caveats = Caveats {
            fs_write: newt_core::Scope::only(["allowed.rs".to_string()]),
            ..Caveats::top()
        };

        // Allowed write succeeds.
        let allowed = coder
            .apply(
                &whole_files("allowed.rs", "fn ok() {}\n"),
                tmp.path(),
                &caveats,
            )
            .expect("permitted write must succeed");
        assert_eq!(allowed, vec!["allowed.rs".to_string()]);

        // Forbidden write returns CapabilityDenied.
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
        // A multi-file emission where one path is denied must write
        // NOTHING — the check loops every path before committing any
        // write. Regression for the "wrote half the emission then
        // refused" failure mode.
        let tmp = TempDir::new().unwrap();
        let coder = coder_with_no_backend_used();
        let caveats = Caveats {
            fs_write: newt_core::Scope::only(["a.rs".to_string()]),
            ..Caveats::top()
        };
        let mut files = BTreeMap::new();
        files.insert("a.rs".to_string(), "fn a() {}\n".to_string());
        files.insert("b.rs".to_string(), "fn b() {}\n".to_string());

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
        // Neither file landed.
        assert!(!tmp.path().join("a.rs").exists());
        assert!(!tmp.path().join("b.rs").exists());
    }

    #[test]
    fn apply_unified_diff_denied_under_bounded_fs_write() {
        // We can't enumerate diff paths up front, so any non-`All`
        // fs_write scope conservatively denies the dispatch.
        let tmp = TempDir::new().unwrap();
        let coder = coder_with_no_backend_used();
        let caveats = Caveats {
            fs_write: newt_core::Scope::only(["whatever.rs".to_string()]),
            ..Caveats::top()
        };
        let diff = Emission::UnifiedDiff(
            "--- a/whatever.rs\n+++ b/whatever.rs\n@@ -1 +1 @@\n-x\n+y\n".to_string(),
        );
        let err = coder.apply(&diff, tmp.path(), &caveats).unwrap_err();
        assert!(matches!(
            err,
            CoderError::CapabilityDenied {
                kind: "fs_write",
                ..
            }
        ));
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
