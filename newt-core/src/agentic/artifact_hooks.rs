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
use super::compress::{CompressAction, CompressTrigger};
#[cfg(test)]
use super::prompt_intake::PROMPT_COMPREHENSION_SCHEMA_CURRENT;
use super::prompt_intake::{
    PromptIntake, PROMPT_COMPREHENSION_SCHEMA_V1, PROMPT_COMPREHENSION_SCHEMA_V2,
    PROMPT_COMPREHENSION_SCHEMA_V3,
};
use super::scheduled::{PlanSnapshot, Step, StepStatus, MAX_STEPS, STEP_DESC_CAP};
use crate::artifact::{ArtifactKind, ArtifactRelation, NewPromptArtifact};
use crate::{TokenUsage, TurnEndReason};

const COMPACTION_REASON_CHARS: usize = 512;
const GIT_OID_CHARS: usize = 128;
const GIT_BRANCH_CHARS: usize = 512;
const PROMPT_COMPREHENSION_LOCATOR: &str = "prompt-comprehension";
const MAX_ATOMIC_ASK_DIGESTS: usize = 64;
const MAX_CLARIFICATION_DIGESTS: usize = 16;
const MAX_DECISION_AGGREGATE_COUNT: u64 = 1_024;

/// Cap on one carried informational clause. Matches `prompt_intake`'s own
/// `MAX_ASK_BYTES`, so extraction has already truncated to it and this is a
/// second, independent bound on what reaches the ledger.
const MAX_INFORMATIONAL_ASK_BYTES: usize = 4_096;

const PROMPT_COMPREHENSION_FIELDS: [&str; 13] = [
    "schema",
    "disposition",
    "atomic_ask_count",
    "clarification_count",
    "decision_count",
    "decision_status_counts",
    "decision_source_counts",
    "atomic_ask_digests",
    "clarification_digests",
    "authorized_assumption_digests",
    // #1971, v3. Optional: records written before v3 have none.
    "informational_ask_count",
    "informational_asks",
    "atomic_ask_kinds",
];
const DECISION_STATUS_FIELDS: [&str; 2] = ["pending", "locked"];
const DECISION_SOURCE_FIELDS: [&str; 3] = ["operator", "policy", "authorized_assumption"];
const DIGEST_FIELDS: [&str; 2] = ["digest", "bytes"];

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

/// Record the harness-validated prompt-comprehension result without retaining
/// any operator or clarification text.
///
/// [`PromptIntake::artifact_metadata`] is deliberately treated as untrusted at
/// this persistence boundary. The hook accepts one exact scalar/aggregate
/// field shape with version-specific disposition enums, validates every
/// digest, and reconstructs a fresh JSON object from a whitelist. Unknown or
/// missing fields fail closed instead of creating a second prompt transcript
/// through artifact metadata.
pub fn record_prompt_comprehension_manifest(
    sink: &dyn PromptArtifactSink,
    context: ArtifactReadContext<'_>,
    intake: &PromptIntake,
) -> anyhow::Result<ArtifactReadRecord> {
    let metadata = intake.artifact_metadata();
    record_prompt_comprehension_metadata(sink, context, &metadata)
}

fn record_prompt_comprehension_metadata(
    sink: &dyn PromptArtifactSink,
    context: ArtifactReadContext<'_>,
    metadata: &Value,
) -> anyhow::Result<ArtifactReadRecord> {
    let metadata = bounded_prompt_comprehension_metadata(metadata)?;
    append(
        sink,
        context,
        NewPromptArtifact::new(ArtifactKind::Decision, ArtifactRelation::DerivedFrom)
            .with_locator(PROMPT_COMPREHENSION_LOCATOR)
            .with_metadata(metadata),
    )
}

fn bounded_prompt_comprehension_metadata(metadata: &Value) -> anyhow::Result<Value> {
    // `authorized_assumption_digests` postdates the v1/v2 records already on
    // disk, so it is ALLOWED but not REQUIRED: a stored manifest written before
    // adjudication existed has no authorized locks, and an absent list is
    // exactly equivalent to an empty one. Every other field stays required.
    let object = exact_object(
        metadata,
        &PROMPT_COMPREHENSION_FIELDS,
        &[
            "authorized_assumption_digests",
            "informational_ask_count",
            "informational_asks",
            "atomic_ask_kinds",
        ],
        "prompt-comprehension metadata",
    )?;

    let schema = required_string(object, "schema", "prompt-comprehension schema")?;
    let disposition = required_string(object, "disposition", "prompt-comprehension disposition")?;
    match schema {
        PROMPT_COMPREHENSION_SCHEMA_V1 => {
            if !matches!(disposition, "ask" | "act" | "explain" | "research") {
                anyhow::bail!("prompt-comprehension metadata has an invalid v1 disposition");
            }
        }
        PROMPT_COMPREHENSION_SCHEMA_V2 => {
            if !matches!(disposition, "ask" | "act" | "explain" | "research" | "plan") {
                anyhow::bail!("prompt-comprehension metadata has an invalid v2 disposition");
            }
        }
        PROMPT_COMPREHENSION_SCHEMA_V3 => {
            if !matches!(disposition, "ask" | "act" | "explain" | "research" | "plan") {
                anyhow::bail!("prompt-comprehension metadata has an invalid v3 disposition");
            }
        }
        _ => anyhow::bail!("prompt-comprehension metadata has an unsupported schema"),
    }

    let atomic_ask_count = required_count(object, "atomic_ask_count")?;
    let clarification_count = required_count(object, "clarification_count")?;
    let decision_count = required_count(object, "decision_count")?;
    if decision_count > MAX_DECISION_AGGREGATE_COUNT {
        anyhow::bail!("prompt-comprehension decision count exceeds the artifact bound");
    }

    let status_counts = bounded_aggregate(
        object.get("decision_status_counts"),
        &DECISION_STATUS_FIELDS,
        "decision-status aggregate",
    )?;
    let source_counts = bounded_aggregate(
        object.get("decision_source_counts"),
        &DECISION_SOURCE_FIELDS,
        "decision-source aggregate",
    )?;
    let pending = status_counts["pending"].as_u64().unwrap_or(0);
    let locked = status_counts["locked"].as_u64().unwrap_or(0);
    let status_total = pending
        .checked_add(locked)
        .context("prompt-comprehension decision-status aggregate overflow")?;
    if status_total != decision_count {
        anyhow::bail!("prompt-comprehension decision-status aggregate does not match its count");
    }
    let source_total = DECISION_SOURCE_FIELDS
        .iter()
        .try_fold(0_u64, |total, key| {
            total
                .checked_add(source_counts[*key].as_u64().unwrap_or(0))
                .context("prompt-comprehension decision-source aggregate overflow")
        })?;
    if source_total != locked {
        anyhow::bail!(
            "prompt-comprehension decision-source aggregate does not match locked decisions"
        );
    }

    let atomic_ask_digests = bounded_digests(
        object.get("atomic_ask_digests"),
        MAX_ATOMIC_ASK_DIGESTS,
        "atomic-ask digests",
    )?;
    let clarification_digests = bounded_digests(
        object.get("clarification_digests"),
        MAX_CLARIFICATION_DIGESTS,
        "clarification digests",
    )?;
    if atomic_ask_count != u64::try_from(atomic_ask_digests.len()).unwrap_or(u64::MAX) {
        anyhow::bail!("prompt-comprehension atomic-ask digests do not match their count");
    }
    if clarification_count != u64::try_from(clarification_digests.len()).unwrap_or(u64::MAX) {
        anyhow::bail!("prompt-comprehension clarification digests do not match their count");
    }
    // One digest per model-authorized lock. The assumption text itself never
    // enters the ledger — the digest proves WHICH interpretation was
    // authorized without copying prompt-derived text into durable storage.
    let authorized_assumption_digests = match object.get("authorized_assumption_digests") {
        None => Vec::new(),
        present => bounded_digests(
            present,
            MAX_CLARIFICATION_DIGESTS,
            "authorized-assumption digests",
        )?,
    };
    if source_counts["authorized_assumption"].as_u64().unwrap_or(0)
        != u64::try_from(authorized_assumption_digests.len()).unwrap_or(u64::MAX)
    {
        anyhow::bail!(
            "prompt-comprehension authorized-assumption digests do not match their source count"
        );
    }

    // #1971 — the one place prompt-derived TEXT enters the ledger, and the
    // narrowest one available.
    //
    // Everything above is a count or a digest, and that stays true for every
    // clause an operator INSTRUCTED or DECIDED. Stated facts are carried
    // because the bug being fixed is that the durable record of a dropped fact
    // could not say what was dropped: the evidencing artifact held
    // `atomic_ask_count=1` and a digest, and the fact itself was unrecoverable
    // from it. A digest cannot be read back.
    //
    // It discloses nothing new. `prompt_receipts.raw_text` already persists the
    // ENTIRE prompt verbatim, in the same database — so this classifies bytes
    // that are already durable rather than copying new ones in, and it is
    // doubly bounded: informational clauses only, capped in count and in bytes.
    let informational_asks = bounded_informational_asks(object.get("informational_asks"))?;
    let informational_ask_count = match object.get("informational_ask_count") {
        None => u64::try_from(informational_asks.len()).unwrap_or(u64::MAX),
        Some(_) => required_count(object, "informational_ask_count")?,
    };
    if informational_ask_count != u64::try_from(informational_asks.len()).unwrap_or(u64::MAX) {
        anyhow::bail!("prompt-comprehension informational asks do not match their count");
    }
    let atomic_ask_kinds = bounded_ask_kinds(object.get("atomic_ask_kinds"))?;
    if !atomic_ask_kinds.is_empty()
        && atomic_ask_count != u64::try_from(atomic_ask_kinds.len()).unwrap_or(u64::MAX)
    {
        anyhow::bail!("prompt-comprehension ask kinds do not match the atomic-ask count");
    }

    Ok(json!({
        "informational_ask_count": informational_ask_count,
        "informational_asks": informational_asks,
        "atomic_ask_kinds": atomic_ask_kinds,
        "schema": schema,
        "disposition": disposition,
        "atomic_ask_count": atomic_ask_count,
        "clarification_count": clarification_count,
        "decision_count": decision_count,
        "decision_status_counts": status_counts,
        "decision_source_counts": source_counts,
        "atomic_ask_digests": atomic_ask_digests,
        "clarification_digests": clarification_digests,
        "authorized_assumption_digests": authorized_assumption_digests,
    }))
}

/// Informational clause text, bounded in count and in bytes. Anything else in
/// the list is a malformed record, not a clause.
fn bounded_informational_asks(value: Option<&Value>) -> anyhow::Result<Vec<String>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let items = value
        .as_array()
        .context("informational asks must be a JSON array")?;
    if items.len() > MAX_ATOMIC_ASK_DIGESTS {
        anyhow::bail!("informational asks exceed the artifact bound");
    }
    items
        .iter()
        .map(|item| {
            let text = item
                .as_str()
                .context("an informational ask must be a string")?;
            if text.len() > MAX_INFORMATIONAL_ASK_BYTES {
                anyhow::bail!("an informational ask exceeds the artifact byte bound");
            }
            Ok(text.to_string())
        })
        .collect()
}

/// Per-clause kinds. A closed vocabulary, so an unknown kind is a malformed
/// record rather than a silently accepted new authority category.
fn bounded_ask_kinds(value: Option<&Value>) -> anyhow::Result<Vec<String>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let items = value.as_array().context("ask kinds must be a JSON array")?;
    if items.len() > MAX_ATOMIC_ASK_DIGESTS {
        anyhow::bail!("ask kinds exceed the artifact bound");
    }
    items
        .iter()
        .map(|item| {
            let kind = item.as_str().context("an ask kind must be a string")?;
            if !matches!(kind, "instruction" | "informational") {
                anyhow::bail!("an ask kind is outside the closed vocabulary");
            }
            Ok(kind.to_string())
        })
        .collect()
}

fn exact_object<'a>(
    value: &'a Value,
    fields: &[&str],
    optional: &[&str],
    label: &str,
) -> anyhow::Result<&'a serde_json::Map<String, Value>> {
    let object = value
        .as_object()
        .with_context(|| format!("{label} must be a JSON object"))?;
    if fields
        .iter()
        .filter(|field| !optional.contains(*field))
        .any(|field| !object.contains_key(*field))
    {
        anyhow::bail!("{label} is missing a required field");
    }
    if object.keys().any(|field| !fields.contains(&field.as_str())) {
        anyhow::bail!("{label} contains an unexpected field");
    }
    Ok(object)
}

fn required_string<'a>(
    object: &'a serde_json::Map<String, Value>,
    field: &str,
    label: &str,
) -> anyhow::Result<&'a str> {
    object
        .get(field)
        .and_then(Value::as_str)
        .with_context(|| format!("{label} must be a string"))
}

fn required_count(object: &serde_json::Map<String, Value>, field: &str) -> anyhow::Result<u64> {
    object
        .get(field)
        .and_then(Value::as_u64)
        .with_context(|| format!("prompt-comprehension `{field}` must be an unsigned integer"))
}

fn bounded_aggregate(value: Option<&Value>, fields: &[&str], label: &str) -> anyhow::Result<Value> {
    let value = value.with_context(|| format!("{label} is required"))?;
    let object = exact_object(value, fields, &[], label)?;
    let mut bounded = serde_json::Map::new();
    for field in fields {
        let count = required_count(object, field)?;
        if count > MAX_DECISION_AGGREGATE_COUNT {
            anyhow::bail!("{label} count exceeds the artifact bound");
        }
        bounded.insert((*field).to_string(), Value::from(count));
    }
    Ok(Value::Object(bounded))
}

fn bounded_digests(value: Option<&Value>, max: usize, label: &str) -> anyhow::Result<Vec<Value>> {
    let digests = value
        .and_then(Value::as_array)
        .with_context(|| format!("{label} must be an array"))?;
    if digests.len() > max {
        anyhow::bail!("{label} exceeds the artifact bound");
    }
    digests
        .iter()
        .map(|entry| {
            let object = exact_object(entry, &DIGEST_FIELDS, &[], label)?;
            let digest = required_string(object, "digest", label)?;
            if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                anyhow::bail!("{label} contains an invalid digest");
            }
            let bytes = required_count(object, "bytes")?;
            Ok(json!({
                "digest": digest,
                "bytes": bytes,
            }))
        })
        .collect()
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
///
/// Automatic callers pass their original [`CompressTrigger`] so the artifact
/// can explain the decision in bounded scalar metadata. Recovery and manual
/// callers pass `None`; `send_budget_authoritative` is then intentionally
/// ignored and no automatic-trigger metadata is written.
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
    trigger: Option<&CompressTrigger>,
    send_budget_authoritative: bool,
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
    // A successful checkpoint is necessarily rooted (the append below
    // validates it too). Put that durable selector in the bounded body so a
    // recovered artifact makes its objective scope clear without copying any
    // prompt, message, or tool-result content.
    let root_selector = context
        .root_prompt_id()
        .map(|root| format!("root:{root}"))
        .unwrap_or_else(|| "root:unavailable".to_string());
    let body = format!(
        "Context checkpoint for {root_selector}: {action}; ~{tokens_before} → ~{tokens_after} tokens against a \
         ~{budget}-token budget at round {round}. Reason: {reason}"
    );
    let mut metadata = json!({
        "action": action,
        "tokens_before": tokens_before,
        "tokens_after": tokens_after,
        "tokens_reclaimed": tokens_before.saturating_sub(tokens_after),
        "budget": budget,
        "round": round,
        "reason": reason,
    });
    if let Some(trigger) = trigger {
        // The trigger is deliberately a bounded, scalar-only projection. It
        // explains why automatic compaction happened without retaining prompt
        // text, message payloads, tool output, or a mutable context dump.
        metadata["trigger"] = compaction_trigger_metadata(trigger, send_budget_authoritative);
    }
    let record = append(
        sink,
        context,
        NewPromptArtifact::new(
            ArtifactKind::CompactionCheckpoint,
            ArtifactRelation::Summarizes,
        )
        .with_body(body)
        .with_metadata(metadata),
    )?;
    Ok(Some(record))
}

/// Bounded decision diagnostics for an automatic compaction checkpoint.
///
/// This is intentionally separate from the general checkpoint metadata so
/// recovery, overflow, and manual callers can pass `None` and retain their
/// established semantics. The values are all scalar configuration or measured
/// token/count figures; no model-facing context is copied into the ledger.
fn compaction_trigger_metadata(
    trigger: &CompressTrigger,
    send_budget_authoritative: bool,
) -> Value {
    json!({
        "policy": trigger.policy.as_str(),
        "message_count": trigger.message_count,
        "message_count_threshold": trigger.message_count_threshold,
        "current_tokens": trigger.current_tokens,
        "token_threshold": trigger.token_threshold,
        "send_budget": trigger.send_budget,
        "send_budget_authoritative": send_budget_authoritative,
        "has_authoritative_headroom": trigger.has_authoritative_headroom,
        "causes": {
            "message_count": trigger.count_fired,
            "token_threshold": trigger.token_fired,
            "send_budget": trigger.send_budget_fired,
        },
        "primary_cause": trigger.primary_cause.as_str(),
    })
}

/// Record a completed turn without duplicating the assistant transcript.
///
/// `tool_round_limit` is the cap this turn actually ran under, WITH its
/// derivation (#1965). It is stamped here because this is the only per-turn
/// durable row: the effective limit is recomputed per dispatch from config,
/// model tuning, tenacity and a session-local `/rounds` override, and slash
/// commands are excluded from prompt receipts by design — so an escalation
/// from 40 to effectively unlimited previously left no record in config, in
/// receipts, in turns, or in artifacts, and runs reaching rounds 145/236/285/320
/// were indistinguishable from runs under the announced cap.
///
/// Four bounded scalars, no text: the ledger's content-free rule is untouched.
/// `configured` is carried beside `rounds` so a reader sees the ESCALATION and
/// not merely the result.
pub fn record_turn_outcome(
    sink: &dyn PromptArtifactSink,
    context: ArtifactReadContext<'_>,
    reply: &str,
    usage: Option<TokenUsage>,
    end_reason: Option<TurnEndReason>,
    elapsed_ms: u64,
    tool_round_limit: Option<crate::tenacity::ToolRoundLimit>,
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
                "tool_round_limit": tool_round_limit.map(|l| l.rounds),
                "tool_round_limit_source": tool_round_limit.map(|l| l.source.as_str()),
                "configured_tool_round_limit": tool_round_limit.map(|l| l.configured),
                "tenacity": tool_round_limit.and_then(|l| l.tenacity).map(|t| t.label()),
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

    fn digest_metadata(text: &str) -> Value {
        json!({
            "digest": blake3::hash(text.as_bytes()).to_hex().to_string(),
            "bytes": text.len(),
        })
    }

    fn valid_prompt_comprehension_metadata_for_schema(schema: &str) -> Value {
        json!({
            "schema": schema,
            "disposition": "ask",
            "atomic_ask_count": 2,
            "clarification_count": 1,
            "decision_count": 3,
            "decision_status_counts": {
                "pending": 1,
                "locked": 2,
            },
            "decision_source_counts": {
                "operator": 1,
                "policy": 1,
                "authorized_assumption": 0,
            },
            "atomic_ask_digests": [
                digest_metadata("private atomic ask one"),
                digest_metadata("private atomic ask two"),
            ],
            "clarification_digests": [
                digest_metadata("private clarification question"),
            ],
        })
    }

    fn valid_prompt_comprehension_metadata() -> Value {
        valid_prompt_comprehension_metadata_for_schema(PROMPT_COMPREHENSION_SCHEMA_CURRENT)
    }

    fn assert_prompt_comprehension_metadata_rejected(metadata: &Value) {
        let sink = RecordingSink::default();
        let (_, _, context) = context();
        assert!(record_prompt_comprehension_metadata(&sink, context, metadata).is_err());
        assert!(sink.artifacts().is_empty());
    }

    #[test]
    fn append_uses_submitted_origin_not_active_selector() {
        let sink = RecordingSink::default();
        let (originating, root, context) = context();
        record_turn_outcome(&sink, context, "ok", None, None, 1, None).unwrap();
        let writes = sink.writes.lock().unwrap();
        assert_eq!(writes[0].0, originating);
        assert_eq!(writes[0].1, root);
    }

    #[test]
    fn prompt_comprehension_manifest_records_only_bounded_aggregates_and_digests() {
        let sink = RecordingSink::default();
        let (originating, root, context) = context();
        record_prompt_comprehension_metadata(
            &sink,
            context,
            &valid_prompt_comprehension_metadata(),
        )
        .unwrap();

        let writes = sink.writes.lock().unwrap();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].0, originating);
        assert_eq!(writes[0].1, root);
        let artifact = &writes[0].2;
        assert_eq!(artifact.kind(), ArtifactKind::Decision);
        assert_eq!(artifact.relation(), ArtifactRelation::DerivedFrom);
        assert_eq!(artifact.locator(), Some(PROMPT_COMPREHENSION_LOCATOR));
        assert!(artifact.body().is_none());
        assert_eq!(
            artifact.metadata()["schema"],
            PROMPT_COMPREHENSION_SCHEMA_CURRENT
        );
        assert_eq!(artifact.metadata()["disposition"], "ask");
        assert_eq!(artifact.metadata()["atomic_ask_count"], 2);
        assert_eq!(artifact.metadata()["clarification_count"], 1);
        assert_eq!(artifact.metadata()["decision_status_counts"]["pending"], 1);
        assert_eq!(artifact.metadata()["decision_source_counts"]["policy"], 1);
        assert_eq!(
            artifact.metadata()["atomic_ask_digests"]
                .as_array()
                .unwrap()
                .len(),
            2
        );

        let persisted = artifact.metadata().to_string();
        for private_text in [
            "private atomic ask one",
            "private atomic ask two",
            "private clarification question",
        ] {
            assert!(!persisted.contains(private_text), "{persisted}");
        }
    }

    #[test]
    fn prompt_comprehension_manifest_accepts_harness_selected_plan_disposition() {
        let mut intake = crate::agentic::PromptIntake::analyze("Implement the parser repair.");
        intake.enforce_read_only(crate::agentic::PromptDisposition::Plan);
        let sink = RecordingSink::default();
        let (_, _, context) = context();

        record_prompt_comprehension_metadata(&sink, context, &intake.artifact_metadata()).unwrap();

        let writes = sink.writes.lock().unwrap();
        assert_eq!(
            writes[0].2.metadata()["schema"],
            "prompt_comprehension_manifest_v3"
        );
        assert_eq!(writes[0].2.metadata()["disposition"], "plan");
    }

    /// **A v2 record already on disk still validates** (#1971). The three new
    /// fields are optional, and an absent list is exactly equivalent to an
    /// empty one — the same compatibility rule `authorized_assumption_digests`
    /// established. Without this, arming a schema bump would reject every
    /// manifest written before it.
    #[test]
    fn prompt_comprehension_manifest_accepts_a_v2_record_without_the_new_fields() {
        let sink = RecordingSink::default();
        let (_, _, context) = context();
        let v2 = serde_json::json!({
            "schema": "prompt_comprehension_manifest_v2",
            "disposition": "act",
            "atomic_ask_count": 1,
            "clarification_count": 0,
            "decision_count": 0,
            "decision_status_counts": { "pending": 0, "locked": 0 },
            "decision_source_counts": {
                "operator": 0, "policy": 0, "authorized_assumption": 0
            },
            "atomic_ask_digests": [{ "digest": "a".repeat(64), "bytes": 12 }],
            "clarification_digests": [],
        });

        record_prompt_comprehension_metadata(&sink, context, &v2).unwrap();

        let writes = sink.writes.lock().unwrap();
        let metadata = writes[0].2.metadata();
        assert_eq!(metadata["schema"], "prompt_comprehension_manifest_v2");
        assert_eq!(
            metadata["informational_ask_count"], 0,
            "an absent list normalizes to zero, not to a rejection"
        );
        assert_eq!(metadata["informational_asks"], serde_json::json!([]));
    }

    /// The carried text is bounded in both directions, and the kind vocabulary
    /// is closed — an unknown kind is a malformed record, never a silently
    /// accepted new authority category.
    #[test]
    fn the_v3_fields_are_bounded_and_their_vocabulary_is_closed() {
        let base = |informational: serde_json::Value, kinds: serde_json::Value| {
            serde_json::json!({
                "schema": "prompt_comprehension_manifest_v3",
                "disposition": "explain",
                "atomic_ask_count": 1,
                "clarification_count": 0,
                "decision_count": 0,
                "decision_status_counts": { "pending": 0, "locked": 0 },
                "decision_source_counts": {
                    "operator": 0, "policy": 0, "authorized_assumption": 0
                },
                "atomic_ask_digests": [{ "digest": "a".repeat(64), "bytes": 12 }],
                "clarification_digests": [],
                "informational_ask_count": 1,
                "informational_asks": informational,
                "atomic_ask_kinds": kinds,
            })
        };
        let ok = base(
            serde_json::json!(["the remote is X"]),
            serde_json::json!(["informational"]),
        );
        assert!(bounded_prompt_comprehension_metadata(&ok).is_ok());

        let oversized = base(
            serde_json::json!(["x".repeat(MAX_INFORMATIONAL_ASK_BYTES + 1)]),
            serde_json::json!(["informational"]),
        );
        assert!(
            bounded_prompt_comprehension_metadata(&oversized).is_err(),
            "an oversized clause must be refused, not truncated silently"
        );

        let unknown_kind = base(
            serde_json::json!(["the remote is X"]),
            serde_json::json!(["advisory"]),
        );
        assert!(
            bounded_prompt_comprehension_metadata(&unknown_kind).is_err(),
            "the kind vocabulary is closed"
        );

        let miscounted = base(
            serde_json::json!(["a", "b"]),
            serde_json::json!(["informational"]),
        );
        assert!(
            bounded_prompt_comprehension_metadata(&miscounted).is_err(),
            "the count must match the list it counts"
        );
    }

    #[test]
    fn prompt_comprehension_manifest_accepts_legacy_v1_without_rewriting_its_schema() {
        let sink = RecordingSink::default();
        let (_, _, context) = context();
        let metadata =
            valid_prompt_comprehension_metadata_for_schema("prompt_comprehension_manifest_v1");

        record_prompt_comprehension_metadata(&sink, context, &metadata).unwrap();

        let writes = sink.writes.lock().unwrap();
        assert_eq!(
            writes[0].2.metadata()["schema"],
            "prompt_comprehension_manifest_v1"
        );
        assert_eq!(writes[0].2.metadata()["disposition"], "ask");
    }

    #[test]
    fn prompt_comprehension_manifest_rejects_plan_under_legacy_v1_schema() {
        let mut metadata =
            valid_prompt_comprehension_metadata_for_schema("prompt_comprehension_manifest_v1");
        metadata["disposition"] = Value::from("plan");

        assert_prompt_comprehension_metadata_rejected(&metadata);
    }

    #[test]
    fn prompt_comprehension_manifest_rejects_text_bearing_or_unknown_fields() {
        let mut missing_field = valid_prompt_comprehension_metadata();
        missing_field.as_object_mut().unwrap().remove("schema");
        assert_prompt_comprehension_metadata_rejected(&missing_field);

        // v3 became a supported schema in #1971; the guard is that an
        // UNKNOWN schema is refused, so it moves to the next unminted version
        // rather than being deleted.
        let mut unsupported_schema = valid_prompt_comprehension_metadata();
        unsupported_schema["schema"] = Value::from("prompt_comprehension_manifest_v4");
        assert_prompt_comprehension_metadata_rejected(&unsupported_schema);

        let mut root_text = valid_prompt_comprehension_metadata();
        root_text["prompt_text"] = Value::from("private operator prompt");
        assert_prompt_comprehension_metadata_rejected(&root_text);

        let mut status_text = valid_prompt_comprehension_metadata();
        status_text["decision_status_counts"]["private decision text"] = Value::from(1);
        assert_prompt_comprehension_metadata_rejected(&status_text);

        let mut source_text = valid_prompt_comprehension_metadata();
        source_text["decision_source_counts"]["model_assertion"] = Value::from(1);
        assert_prompt_comprehension_metadata_rejected(&source_text);

        let mut digest_text = valid_prompt_comprehension_metadata();
        digest_text["atomic_ask_digests"][0]["text"] = Value::from("private atomic ask");
        assert_prompt_comprehension_metadata_rejected(&digest_text);

        // #1971 admits exactly ONE text-bearing field, by name and bounded.
        // Arbitrary prompt text is still refused — including under a plausible
        // sibling name — so the boundary moved by one named field rather than
        // opening.
        let mut plausible_sibling = valid_prompt_comprehension_metadata();
        plausible_sibling["informational_ask_text"] = Value::from("private operator prompt");
        assert_prompt_comprehension_metadata_rejected(&plausible_sibling);
    }

    #[test]
    fn prompt_comprehension_manifest_rejects_invalid_digests_counts_and_bounds() {
        let mut invalid_digest = valid_prompt_comprehension_metadata();
        invalid_digest["atomic_ask_digests"][0]["digest"] =
            Value::from("private atomic ask, not a digest");
        assert_prompt_comprehension_metadata_rejected(&invalid_digest);

        let mut ask_count_mismatch = valid_prompt_comprehension_metadata();
        ask_count_mismatch["atomic_ask_count"] = Value::from(1);
        assert_prompt_comprehension_metadata_rejected(&ask_count_mismatch);

        let mut decision_count_mismatch = valid_prompt_comprehension_metadata();
        decision_count_mismatch["decision_count"] = Value::from(4);
        assert_prompt_comprehension_metadata_rejected(&decision_count_mismatch);

        let mut source_count_mismatch = valid_prompt_comprehension_metadata();
        source_count_mismatch["decision_source_counts"]["operator"] = Value::from(0);
        assert_prompt_comprehension_metadata_rejected(&source_count_mismatch);

        let digest = digest_metadata("bounded ask");
        let mut too_many_asks = valid_prompt_comprehension_metadata();
        too_many_asks["atomic_ask_count"] = Value::from(MAX_ATOMIC_ASK_DIGESTS + 1);
        too_many_asks["atomic_ask_digests"] =
            Value::Array(vec![digest; MAX_ATOMIC_ASK_DIGESTS + 1]);
        assert_prompt_comprehension_metadata_rejected(&too_many_asks);
    }

    #[test]
    fn missing_prompt_authority_and_sink_errors_are_returned() {
        let sink = RecordingSink::default();
        let missing_origin = ArtifactReadContext::new(None, None, Some(PromptId::new()), None);
        let error = record_turn_outcome(&sink, missing_origin, "ok", None, None, 1, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("originating prompt"), "{error}");
        assert!(sink.artifacts().is_empty());

        let failing = RecordingSink::failing("disk full");
        let (_, _, context) = context();
        let error = record_turn_outcome(&failing, context, "ok", None, None, 1, None)
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
                None,
                false,
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
            record_compaction_checkpoint(
                &sink, context, action, 1_000, 700, 800, 3, &reason, None, false,
            )
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
            assert!(artifacts[0].metadata().get("trigger").is_none());
        }
    }

    #[test]
    fn automatic_compaction_checkpoint_records_bounded_trigger_diagnostics() {
        let sink = RecordingSink::default();
        let (_, root, context) = context();
        let trigger = CompressTrigger {
            budget: 9_500,
            max_messages: Some(20),
            hard_budget: true,
            policy: crate::CompactionTriggerPolicy::HeadroomAware,
            message_count: 41,
            message_count_threshold: 40,
            current_tokens: 10_100,
            token_threshold: Some(10_000),
            send_budget: Some(20_000),
            has_authoritative_headroom: true,
            count_fired: false,
            token_fired: true,
            send_budget_fired: false,
            primary_cause: super::super::compress::CompressTriggerCause::TokenThreshold,
        };

        record_compaction_checkpoint(
            &sink,
            context,
            CompressAction::Summarized,
            12_000,
            5_000,
            trigger.budget,
            4,
            "automatic_token_threshold",
            Some(&trigger),
            true,
        )
        .unwrap();

        let artifacts = sink.artifacts();
        let artifact = &artifacts[0];
        assert!(artifact.body().unwrap().contains(&format!("root:{root}")));
        let trigger = &artifact.metadata()["trigger"];
        assert_eq!(trigger["policy"], "headroom_aware");
        assert_eq!(trigger["message_count"], 41);
        assert_eq!(trigger["message_count_threshold"], 40);
        assert_eq!(trigger["current_tokens"], 10_100);
        assert_eq!(trigger["token_threshold"], 10_000);
        assert_eq!(trigger["send_budget"], 20_000);
        assert_eq!(trigger["send_budget_authoritative"], true);
        assert_eq!(trigger["has_authoritative_headroom"], true);
        assert_eq!(trigger["causes"]["message_count"], false);
        assert_eq!(trigger["causes"]["token_threshold"], true);
        assert_eq!(trigger["causes"]["send_budget"], false);
        assert_eq!(trigger["primary_cause"], "token_threshold");
    }

    /// **The twin** (#1965): a turn under the DEFAULT cap records 40 from
    /// config, and reports itself unescalated.
    ///
    /// Without this the escalated assertion above could pass against a record
    /// that always says "override" — and the field that matters most is the
    /// difference between the two, not either value alone.
    #[test]
    fn a_default_cap_turn_records_forty_from_config() {
        let sink = RecordingSink::default();
        let (_, _, context) = context();
        let limit = crate::tenacity::ToolRoundLimit {
            rounds: 40,
            source: crate::tenacity::ToolRoundLimitSource::Config,
            configured: 40,
            tenacity: None,
        };
        assert!(!limit.is_escalated());
        record_turn_outcome(&sink, context, "ok", None, None, 1, Some(limit)).unwrap();
        let artifacts = sink.artifacts();
        let metadata = artifacts[0].metadata();
        assert_eq!(metadata["tool_round_limit"], 40);
        assert_eq!(metadata["tool_round_limit_source"], "config");
        assert_eq!(metadata["configured_tool_round_limit"], 40);
        assert_eq!(metadata["tenacity"], serde_json::Value::Null);
    }

    /// A tenacity-driven escalation names TENACITY, not the override, and
    /// carries the level — so a reader can tell "the operator typed /rounds
    /// 320" apart from "a relentless posture raised it", which are different
    /// operator acts with different remedies.
    #[test]
    fn a_tenacity_escalation_names_the_level_that_caused_it() {
        let sink = RecordingSink::default();
        let (_, _, context) = context();
        let limit = crate::tenacity::resolve_tool_round_limit(
            40,
            Some(crate::tenacity::Tenacity::Relentless),
            None,
        );
        record_turn_outcome(&sink, context, "ok", None, None, 1, Some(limit)).unwrap();
        let artifacts = sink.artifacts();
        let metadata = artifacts[0].metadata();
        assert_eq!(
            metadata["tool_round_limit"],
            crate::tenacity::RELENTLESS_TOOL_ROUND_TARGET
        );
        assert_eq!(metadata["tool_round_limit_source"], "tenacity");
        assert_eq!(metadata["configured_tool_round_limit"], 40);
        assert_eq!(metadata["tenacity"], "relentless");
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
            Some(crate::tenacity::ToolRoundLimit {
                rounds: 320,
                source: crate::tenacity::ToolRoundLimitSource::Override,
                configured: 40,
                tenacity: None,
            }),
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
        // #1965 — the escalation is on the record. Before this, a turn that ran
        // 320 rounds under an announced cap of 40 was indistinguishable in the
        // ledger from one that ran under 40, because the effective limit is
        // recomputed per dispatch and slash commands never reach a receipt.
        assert_eq!(artifact.metadata()["tool_round_limit"], 320);
        assert_eq!(artifact.metadata()["tool_round_limit_source"], "override");
        assert_eq!(artifact.metadata()["configured_tool_round_limit"], 40);
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
