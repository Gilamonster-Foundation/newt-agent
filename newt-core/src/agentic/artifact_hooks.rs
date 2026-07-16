//! Pure lifecycle hooks that turn successful harness work into prompt artifacts.
//!
//! The hooks in this module are deliberately narrower than general tool
//! telemetry. They retain bounded harness-authored state, or locators plus
//! digests for external state. Raw file contents, assistant replies, and tool
//! streams never enter an artifact.

use anyhow::Context as _;
use serde_json::{json, Value};
use std::path::{Component, Path, PathBuf};

use super::artifact_read::{ArtifactReadContext, ArtifactReadRecord, PromptArtifactSink};
use super::compress::CompressAction;
use super::scheduled::{PlanSnapshot, Step, StepStatus, MAX_STEPS, STEP_DESC_CAP};
use crate::artifact::{ArtifactKind, ArtifactRelation, NewPromptArtifact};
use crate::{TokenUsage, TurnEndReason};

const COMPACTION_REASON_CHARS: usize = 512;
const GIT_OID_CHARS: usize = 128;
const GIT_BRANCH_CHARS: usize = 512;

/// Append one artifact using only harness-owned prompt identities.
///
/// `originating_prompt_id` is the submitted receipt, including a harness retry;
/// `root_prompt_id` is the validated objective root. The active-operator id is
/// intentionally not used here: it is a read selector, not the event origin.
fn append(
    sink: &dyn PromptArtifactSink,
    context: ArtifactReadContext<'_>,
    artifact: NewPromptArtifact,
) -> anyhow::Result<ArtifactReadRecord> {
    let originating = context
        .originating_prompt_id()
        .context("cannot record a prompt artifact without an originating prompt receipt")?;
    let root = context
        .root_prompt_id()
        .context("cannot record a prompt artifact without an objective root receipt")?;
    sink.append_artifact(originating, root, artifact)
}

/// Record the normalized plan state produced by a successful `update_plan`.
///
/// The plan body is a bounded JSON snapshot: no more than the plan ledger's
/// normal step cap, and no more than its normal per-description display cap.
pub(crate) fn record_plan_revision(
    sink: &dyn PromptArtifactSink,
    context: ArtifactReadContext<'_>,
    plan: &PlanSnapshot,
) -> anyhow::Result<ArtifactReadRecord> {
    let bounded = bounded_plan(plan);
    if bounded.steps.is_empty() {
        anyhow::bail!("cannot record an empty plan revision");
    }
    let body = serde_json::to_string(&bounded)?;
    let todo = bounded
        .steps
        .iter()
        .filter(|step| step.status == StepStatus::Todo)
        .count();
    let active = bounded
        .steps
        .iter()
        .filter(|step| step.status == StepStatus::Active)
        .count();
    let done = bounded
        .steps
        .iter()
        .filter(|step| step.status == StepStatus::Done)
        .count();
    append(
        sink,
        context,
        NewPromptArtifact::new(ArtifactKind::PlanRevision, ArtifactRelation::Updates)
            .with_locator("plan")
            .with_body(body)
            .with_metadata(json!({
                "body_encoding": "plan_snapshot_json_v1",
                "step_count": bounded.steps.len(),
                "todo_steps": todo,
                "active_steps": active,
                "done_steps": done,
            })),
    )
}

fn bounded_plan(plan: &PlanSnapshot) -> PlanSnapshot {
    PlanSnapshot {
        steps: plan
            .steps
            .iter()
            .filter_map(|step| {
                let description = truncate_chars(step.description.trim(), STEP_DESC_CAP);
                (!description.is_empty()).then_some(Step {
                    description,
                    status: step.status,
                })
            })
            .take(MAX_STEPS)
            .collect(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileDigest {
    digest: String,
    bytes: u64,
}

impl FileDigest {
    fn from_bytes(bytes: &[u8]) -> Self {
        Self {
            digest: blake3::hash(bytes).to_hex().to_string(),
            bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        }
    }

    fn metadata(&self) -> Value {
        json!({
            "available": true,
            "exists": true,
            "digest": self.digest,
            "bytes": self.bytes,
        })
    }
}

/// Digest-only state on one side of a file transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ArtifactFileState {
    Present(FileDigest),
    Absent,
    /// The state exists outside the model's current read authority. Persist a
    /// reason, never a digest/equality oracle.
    Unavailable(&'static str),
}

impl ArtifactFileState {
    pub(crate) fn from_bytes(bytes: &[u8]) -> Self {
        Self::Present(FileDigest::from_bytes(bytes))
    }

    pub(crate) fn from_digest(digest: String, bytes: u64) -> Self {
        Self::Present(FileDigest { digest, bytes })
    }

    pub(crate) const fn absent() -> Self {
        Self::Absent
    }

    pub(crate) const fn unavailable(reason: &'static str) -> Self {
        Self::Unavailable(reason)
    }

    fn metadata(&self) -> Value {
        match self {
            Self::Present(digest) => digest.metadata(),
            Self::Absent => json!({
                "available": true,
                "exists": false,
                "digest": Value::Null,
                "bytes": 0,
            }),
            Self::Unavailable(reason) => json!({
                "available": false,
                "reason": reason,
            }),
        }
    }
}

/// Record one verified governed file transition without retaining bytes.
///
/// The caller performs the operation and immediate postcondition check inside
/// the authorized tool arm, before arbitrary build hooks run. A write-only
/// grant passes an unavailable preimage so `artifact_read` cannot become a
/// read-side digest oracle.
pub(crate) fn record_file_change(
    sink: &dyn PromptArtifactSink,
    context: ArtifactReadContext<'_>,
    locator: &str,
    operation: &'static str,
    before: ArtifactFileState,
    after: ArtifactFileState,
) -> anyhow::Result<Option<ArtifactReadRecord>> {
    let relative = normalize_relative_path(locator)
        .context("file-change artifact requires a workspace-relative locator")?;
    let locator = path_locator(&relative)
        .context("file-change artifact requires a non-empty workspace-relative locator")?;
    if before == after {
        return Ok(None);
    }
    append(
        sink,
        context,
        NewPromptArtifact::new(ArtifactKind::FileChange, ArtifactRelation::Realizes)
            .with_locator(locator)
            .with_metadata(json!({
                "operation": operation,
                "digest_algorithm": "blake3",
                "before": before.metadata(),
                "after": after.metadata(),
            })),
    )
    .map(Some)
}

/// Record the retry verifier's compensating mutation. The revert machinery
/// has already proven the path belongs to Newt's per-turn write ledger, but it
/// may restore pre-existing bytes outside the model's fs_read authority. Keep
/// the terminal transition discoverable without exposing either digest.
pub fn record_retry_revert_file(
    sink: &dyn PromptArtifactSink,
    context: ArtifactReadContext<'_>,
    locator: &str,
) -> anyhow::Result<ArtifactReadRecord> {
    let relative = normalize_relative_path(locator)
        .context("retry-revert artifact requires a workspace-relative locator")?;
    let locator = path_locator(&relative)
        .context("retry-revert artifact requires a non-empty workspace-relative locator")?;
    append(
        sink,
        context,
        NewPromptArtifact::new(ArtifactKind::FileChange, ArtifactRelation::Realizes)
            .with_locator(locator)
            .with_metadata(json!({
                "operation": "retry_revert",
                "terminal_state": "restored_pre_turn",
                "before": {
                    "available": false,
                    "reason": "compensating_transition_not_exposed",
                },
                "after": {
                    "available": false,
                    "reason": "restored_preimage_outside_artifact_authority",
                },
            })),
    )
}

fn normalize_relative_path(raw: &str) -> Option<PathBuf> {
    if raw.trim().is_empty() {
        return None;
    }
    let input = Path::new(raw);
    if input.is_absolute() {
        return None;
    }
    let mut normalized = PathBuf::new();
    for component in input.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!normalized.as_os_str().is_empty()).then_some(normalized)
}

fn path_locator(path: &Path) -> Option<String> {
    let parts: Vec<String> = path
        .components()
        .map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Option<_>>()?;
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// Record a context transformation that actually replaced or pruned material.
/// Fit/refusal/pass-through outcomes are explicitly not checkpoints.
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_compaction_checkpoint(
    sink: &dyn PromptArtifactSink,
    context: ArtifactReadContext<'_>,
    action: CompressAction,
    tokens_before: usize,
    tokens_after: usize,
    budget: usize,
    round: usize,
    reason: &str,
) -> anyhow::Result<Option<ArtifactReadRecord>> {
    let action = match action {
        CompressAction::Pruned => "pruned",
        CompressAction::Summarized => "summarized",
        CompressAction::StaticFallback => "static_fallback",
        CompressAction::Fit | CompressAction::Refused | CompressAction::DispatchedOverBudget => {
            return Ok(None)
        }
    };
    let reason = truncate_chars(reason.trim(), COMPACTION_REASON_CHARS);
    let body = format!(
        "Context checkpoint: {action}; ~{tokens_before} → ~{tokens_after} tokens against a \
         ~{budget}-token budget at round {round}. Reason: {reason}"
    );
    let record = append(
        sink,
        context,
        NewPromptArtifact::new(
            ArtifactKind::CompactionCheckpoint,
            ArtifactRelation::Summarizes,
        )
        .with_body(body)
        .with_metadata(json!({
            "action": action,
            "tokens_before": tokens_before,
            "tokens_after": tokens_after,
            "tokens_reclaimed": tokens_before.saturating_sub(tokens_after),
            "budget": budget,
            "round": round,
            "reason": reason,
        })),
    )?;
    Ok(Some(record))
}

/// Record a completed turn without duplicating the assistant transcript.
pub fn record_turn_outcome(
    sink: &dyn PromptArtifactSink,
    context: ArtifactReadContext<'_>,
    reply: &str,
    usage: Option<TokenUsage>,
    end_reason: Option<TurnEndReason>,
    elapsed_ms: u64,
) -> anyhow::Result<ArtifactReadRecord> {
    let reply_digest = blake3::hash(reply.as_bytes()).to_hex().to_string();
    append(
        sink,
        context,
        NewPromptArtifact::new(ArtifactKind::TurnOutcome, ArtifactRelation::DerivedFrom)
            .with_metadata(json!({
                "reply_digest_algorithm": "blake3",
                "reply_digest": reply_digest,
                "reply_bytes": u64::try_from(reply.len()).unwrap_or(u64::MAX),
                "usage": usage,
                "end_reason": end_reason,
                "elapsed_ms": elapsed_ms,
            })),
    )
}

/// Record conversation-memory compaction without copying its generated
/// summary into a second transcript. The full summary already has its own
/// durable conversation record; this artifact is only a prompt-rooted index.
pub fn record_memory_compaction_checkpoint(
    sink: &dyn PromptArtifactSink,
    context: ArtifactReadContext<'_>,
    summary: &str,
) -> anyhow::Result<ArtifactReadRecord> {
    append(
        sink,
        context,
        NewPromptArtifact::new(
            ArtifactKind::CompactionCheckpoint,
            ArtifactRelation::Summarizes,
        )
        .with_metadata(json!({
            "source": "conversation_memory",
            "summary_digest_algorithm": "blake3",
            "summary_digest": blake3::hash(summary.as_bytes()).to_hex().to_string(),
            "summary_bytes": u64::try_from(summary.len()).unwrap_or(u64::MAX),
        })),
    )
}

/// Record a user-triggered `/compress` or `/compact` operation that actually
/// changed the in-memory working set. No-op commands deliberately emit no
/// artifact, matching the compressor's own honesty contract.
pub fn record_manual_compaction_checkpoint(
    sink: &dyn PromptArtifactSink,
    context: ArtifactReadContext<'_>,
    outcome: &super::ManualCompressOutcome,
) -> anyhow::Result<Option<ArtifactReadRecord>> {
    if !outcome.fired {
        return Ok(None);
    }
    let body = format!(
        "Operator-requested context checkpoint: {}; ~{} -> ~{} tokens ({} -> {} messages).",
        outcome.how,
        outcome.tokens_before,
        outcome.tokens_after,
        outcome.messages_before,
        outcome.messages_after,
    );
    append(
        sink,
        context,
        NewPromptArtifact::new(
            ArtifactKind::CompactionCheckpoint,
            ArtifactRelation::Summarizes,
        )
        .with_body(body)
        .with_metadata(json!({
            "source": "operator_command",
            "how": outcome.how,
            "tokens_before": outcome.tokens_before,
            "tokens_after": outcome.tokens_after,
            "tokens_reclaimed": outcome.tokens_before.saturating_sub(outcome.tokens_after),
            "messages_before": outcome.messages_before,
            "messages_after": outcome.messages_after,
        })),
    )
    .map(Some)
}

/// Record an observed repository HEAD transition.
///
/// This is intentionally an observation, not an authorship claim: the change
/// may have come from the embedded git tool, a shell command, a rebase/checkout,
/// or another process. No record is emitted when HEAD is absent or unchanged.
pub fn record_observed_head_transition(
    sink: &dyn PromptArtifactSink,
    context: ArtifactReadContext<'_>,
    before: Option<&str>,
    after: Option<&str>,
    branch: Option<&str>,
) -> anyhow::Result<Option<ArtifactReadRecord>> {
    let Some(after) = after.filter(|value| !value.trim().is_empty()) else {
        return Ok(None);
    };
    if before == Some(after) {
        return Ok(None);
    }
    let before = before.map(|value| truncate_chars(value, GIT_OID_CHARS));
    let after = truncate_chars(after, GIT_OID_CHARS);
    let branch = branch.map(|value| truncate_chars(value, GIT_BRANCH_CHARS));
    let record = append(
        sink,
        context,
        NewPromptArtifact::new(ArtifactKind::Commit, ArtifactRelation::Realizes)
            .with_locator(format!("git:{after}"))
            .with_metadata(json!({
                "observation": "head_transition",
                "authorship": "unattributed",
                "before_head": before,
                "after_head": after,
                "branch": branch,
            })),
    )?;
    Ok(Some(record))
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::ArtifactId;
    use crate::PromptId;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingSink {
        writes: Mutex<Vec<(PromptId, PromptId, NewPromptArtifact)>>,
        error: Option<&'static str>,
    }

    impl RecordingSink {
        fn failing(message: &'static str) -> Self {
            Self {
                writes: Mutex::new(Vec::new()),
                error: Some(message),
            }
        }

        fn artifacts(&self) -> Vec<NewPromptArtifact> {
            self.writes
                .lock()
                .unwrap()
                .iter()
                .map(|x| x.2.clone())
                .collect()
        }
    }

    impl PromptArtifactSink for RecordingSink {
        fn append_artifact(
            &self,
            originating_prompt_id: PromptId,
            objective_root_id: PromptId,
            artifact: NewPromptArtifact,
        ) -> anyhow::Result<ArtifactReadRecord> {
            if let Some(error) = self.error {
                anyhow::bail!(error);
            }
            let mut writes = self.writes.lock().unwrap();
            writes.push((originating_prompt_id, objective_root_id, artifact.clone()));
            Ok(ArtifactReadRecord {
                id: ArtifactId::new(),
                prompt_id: originating_prompt_id,
                root_prompt_id: objective_root_id,
                writer_fingerprint: "test-writer".to_string(),
                seq: writes.len() as u64,
                prev_hash: "prev".to_string(),
                kind: format!("{:?}", artifact.kind()),
                relation: format!("{:?}", artifact.relation()),
                locator: artifact.locator().map(str::to_string),
                body: artifact.body().map(str::to_string),
                metadata: artifact.metadata().clone(),
                ts_claim: 1,
                artifact_hash: "hash".to_string(),
            })
        }
    }

    fn context() -> (PromptId, PromptId, ArtifactReadContext<'static>) {
        let originating = PromptId::new();
        let active = PromptId::new();
        let root = PromptId::new();
        (
            originating,
            root,
            ArtifactReadContext::new(Some(originating), Some(active), Some(root), None),
        )
    }

    #[test]
    fn append_uses_submitted_origin_not_active_selector() {
        let sink = RecordingSink::default();
        let (originating, root, context) = context();
        record_turn_outcome(&sink, context, "ok", None, None, 1).unwrap();
        let writes = sink.writes.lock().unwrap();
        assert_eq!(writes[0].0, originating);
        assert_eq!(writes[0].1, root);
    }

    #[test]
    fn missing_prompt_authority_and_sink_errors_are_returned() {
        let sink = RecordingSink::default();
        let missing_origin = ArtifactReadContext::new(None, None, Some(PromptId::new()), None);
        let error = record_turn_outcome(&sink, missing_origin, "ok", None, None, 1)
            .unwrap_err()
            .to_string();
        assert!(error.contains("originating prompt"), "{error}");
        assert!(sink.artifacts().is_empty());

        let failing = RecordingSink::failing("disk full");
        let (_, _, context) = context();
        let error = record_turn_outcome(&failing, context, "ok", None, None, 1)
            .unwrap_err()
            .to_string();
        assert_eq!(error, "disk full");
    }

    #[test]
    fn plan_revision_is_bounded_normalized_and_counted() {
        let sink = RecordingSink::default();
        let (_, _, context) = context();
        let plan = PlanSnapshot {
            steps: (0..(MAX_STEPS + 5))
                .map(|index| Step {
                    description: if index == 0 {
                        "   ".to_string()
                    } else {
                        "🦀".repeat(STEP_DESC_CAP + 50)
                    },
                    status: if index == 1 {
                        StepStatus::Active
                    } else if index % 2 == 0 {
                        StepStatus::Done
                    } else {
                        StepStatus::Todo
                    },
                })
                .collect(),
        };
        record_plan_revision(&sink, context, &plan).unwrap();
        let artifacts = sink.artifacts();
        let artifact = &artifacts[0];
        assert_eq!(artifact.kind(), ArtifactKind::PlanRevision);
        assert_eq!(artifact.relation(), ArtifactRelation::Updates);
        assert_eq!(artifact.locator(), Some("plan"));
        let decoded: PlanSnapshot = serde_json::from_str(artifact.body().unwrap()).unwrap();
        assert_eq!(decoded.steps.len(), MAX_STEPS);
        assert!(decoded
            .steps
            .iter()
            .all(|step| step.description.chars().count() <= STEP_DESC_CAP));
        assert_eq!(artifact.metadata()["step_count"], MAX_STEPS);
        assert_eq!(artifact.metadata()["active_steps"], 1);
    }

    #[test]
    fn empty_plan_is_not_recorded() {
        let sink = RecordingSink::default();
        let (_, _, context) = context();
        let error = record_plan_revision(&sink, context, &PlanSnapshot::default())
            .unwrap_err()
            .to_string();
        assert!(error.contains("empty plan"));
        assert!(sink.artifacts().is_empty());
    }

    #[test]
    fn file_change_rejects_non_relative_locators() {
        for path in ["", "../outside.txt", "/tmp/outside.txt"] {
            let sink = RecordingSink::default();
            let (_, _, context) = context();
            assert!(record_file_change(
                &sink,
                context,
                path,
                "write_file",
                ArtifactFileState::absent(),
                ArtifactFileState::from_bytes(b"after"),
            )
            .is_err());
            assert!(sink.artifacts().is_empty());
        }
    }

    #[test]
    fn successful_file_change_records_relative_locator_and_digests_without_bytes() {
        let before_secret = "SECRET_PREIMAGE_BYTES";
        let after_secret = "SECRET_POSTIMAGE_BYTES";
        let sink = RecordingSink::default();
        let (_, _, context) = context();
        let record = record_file_change(
            &sink,
            context,
            "src/./old/../file.txt",
            "edit_file",
            ArtifactFileState::from_bytes(before_secret.as_bytes()),
            ArtifactFileState::from_bytes(after_secret.as_bytes()),
        )
        .unwrap();
        assert!(record.is_some());
        let artifacts = sink.artifacts();
        let artifact = &artifacts[0];
        assert_eq!(artifact.kind(), ArtifactKind::FileChange);
        assert_eq!(artifact.relation(), ArtifactRelation::Realizes);
        assert_eq!(artifact.locator(), Some("src/file.txt"));
        assert!(artifact.body().is_none());
        assert_eq!(artifact.metadata()["operation"], "edit_file");
        assert_eq!(artifact.metadata()["before"]["bytes"], before_secret.len());
        assert_eq!(artifact.metadata()["after"]["bytes"], after_secret.len());
        assert_ne!(
            artifact.metadata()["before"]["digest"],
            artifact.metadata()["after"]["digest"]
        );
        let metadata = artifact.metadata().to_string();
        assert!(!metadata.contains(before_secret));
        assert!(!metadata.contains(after_secret));
    }

    #[test]
    fn unavailable_preimage_and_delete_poststate_do_not_expose_digests() {
        let sink = RecordingSink::default();
        let (_, _, context) = context();
        record_file_change(
            &sink,
            context,
            "private.txt",
            "delete_file",
            ArtifactFileState::unavailable("fs_read_not_granted"),
            ArtifactFileState::absent(),
        )
        .unwrap();
        let artifacts = sink.artifacts();
        assert_eq!(artifacts[0].metadata()["before"]["available"], false);
        assert_eq!(
            artifacts[0].metadata()["before"]["reason"],
            "fs_read_not_granted"
        );
        assert!(artifacts[0].metadata()["before"].get("digest").is_none());
        assert_eq!(artifacts[0].metadata()["after"]["exists"], false);
    }

    #[test]
    fn unchanged_file_state_emits_nothing() {
        let sink = RecordingSink::default();
        let (_, _, context) = context();
        let same = ArtifactFileState::from_bytes(b"same");
        assert!(
            record_file_change(&sink, context, "file.txt", "write_file", same.clone(), same,)
                .unwrap()
                .is_none()
        );
        assert!(sink.artifacts().is_empty());
    }

    #[test]
    fn compaction_records_only_transforming_actions_and_bounds_reason() {
        for skipped in [
            CompressAction::Fit,
            CompressAction::Refused,
            CompressAction::DispatchedOverBudget,
        ] {
            let sink = RecordingSink::default();
            let (_, _, context) = context();
            assert!(record_compaction_checkpoint(
                &sink,
                context,
                skipped,
                100,
                100,
                80,
                2,
                "not transformed",
            )
            .unwrap()
            .is_none());
            assert!(sink.artifacts().is_empty());
        }

        for action in [
            CompressAction::Pruned,
            CompressAction::Summarized,
            CompressAction::StaticFallback,
        ] {
            let sink = RecordingSink::default();
            let (_, _, context) = context();
            let reason = "r".repeat(COMPACTION_REASON_CHARS + 100);
            record_compaction_checkpoint(&sink, context, action, 1_000, 700, 800, 3, &reason)
                .unwrap();
            let artifacts = sink.artifacts();
            assert_eq!(artifacts[0].kind(), ArtifactKind::CompactionCheckpoint);
            assert_eq!(artifacts[0].relation(), ArtifactRelation::Summarizes);
            assert_eq!(
                artifacts[0].metadata()["reason"]
                    .as_str()
                    .unwrap()
                    .chars()
                    .count(),
                COMPACTION_REASON_CHARS
            );
            assert_eq!(artifacts[0].metadata()["tokens_reclaimed"], 300);
        }
    }

    #[test]
    fn turn_outcome_keeps_digest_and_metrics_but_not_reply() {
        let sink = RecordingSink::default();
        let (_, _, context) = context();
        let reply = "sensitive assistant reply";
        record_turn_outcome(
            &sink,
            context,
            reply,
            Some(TokenUsage {
                input_tokens: 10,
                output_tokens: 4,
            }),
            Some(TurnEndReason::Completed),
            55,
        )
        .unwrap();
        let artifacts = sink.artifacts();
        let artifact = &artifacts[0];
        assert_eq!(artifact.kind(), ArtifactKind::TurnOutcome);
        assert!(artifact.body().is_none());
        assert_eq!(artifact.metadata()["reply_bytes"], reply.len());
        assert_eq!(artifact.metadata()["usage"]["input_tokens"], 10);
        assert_eq!(artifact.metadata()["end_reason"], "completed");
        assert_eq!(artifact.metadata()["elapsed_ms"], 55);
        assert!(!artifact.metadata().to_string().contains(reply));
    }

    #[test]
    fn memory_compaction_indexes_digest_without_copying_summary() {
        let sink = RecordingSink::default();
        let (_, _, context) = context();
        let summary = "private generated compaction summary";
        record_memory_compaction_checkpoint(&sink, context, summary).unwrap();
        let artifacts = sink.artifacts();
        let artifact = &artifacts[0];
        assert_eq!(artifact.kind(), ArtifactKind::CompactionCheckpoint);
        assert_eq!(artifact.relation(), ArtifactRelation::Summarizes);
        assert!(artifact.body().is_none());
        assert_eq!(artifact.metadata()["source"], "conversation_memory");
        assert_eq!(artifact.metadata()["summary_bytes"], summary.len());
        assert!(!artifact.metadata().to_string().contains(summary));
    }

    #[test]
    fn manual_compaction_records_only_a_fired_transformation() {
        let (_, _, context) = context();
        let mut outcome = super::super::ManualCompressOutcome {
            messages: Vec::new(),
            fired: false,
            messages_before: 12,
            messages_after: 12,
            tokens_before: 1_000,
            tokens_after: 1_000,
            how: "already fits",
            notice: None,
        };
        let sink = RecordingSink::default();
        assert!(
            record_manual_compaction_checkpoint(&sink, context, &outcome)
                .unwrap()
                .is_none()
        );
        assert!(sink.artifacts().is_empty());

        outcome.fired = true;
        outcome.messages_after = 5;
        outcome.tokens_after = 400;
        outcome.how = "prune + summary";
        record_manual_compaction_checkpoint(&sink, context, &outcome).unwrap();
        let artifacts = sink.artifacts();
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].kind(), ArtifactKind::CompactionCheckpoint);
        assert_eq!(artifacts[0].metadata()["source"], "operator_command");
        assert_eq!(artifacts[0].metadata()["tokens_reclaimed"], 600);
    }

    #[test]
    fn head_transition_is_observed_without_authorship_claim() {
        for (before, after) in [
            (None, None),
            (Some("abc"), None),
            (Some("abc"), Some("abc")),
        ] {
            let sink = RecordingSink::default();
            let (_, _, context) = context();
            assert!(
                record_observed_head_transition(&sink, context, before, after, Some("main"),)
                    .unwrap()
                    .is_none()
            );
        }

        for before in [None, Some("abc")] {
            let sink = RecordingSink::default();
            let (_, _, context) = context();
            record_observed_head_transition(
                &sink,
                context,
                before,
                Some("def"),
                Some(&"branch".repeat(GIT_BRANCH_CHARS)),
            )
            .unwrap();
            let artifacts = sink.artifacts();
            let artifact = &artifacts[0];
            assert_eq!(artifact.kind(), ArtifactKind::Commit);
            assert_eq!(artifact.locator(), Some("git:def"));
            assert_eq!(artifact.metadata()["observation"], "head_transition");
            assert_eq!(artifact.metadata()["authorship"], "unattributed");
            assert!(
                artifact.metadata()["branch"]
                    .as_str()
                    .unwrap()
                    .chars()
                    .count()
                    <= GIT_BRANCH_CHARS
            );
        }
    }
}
