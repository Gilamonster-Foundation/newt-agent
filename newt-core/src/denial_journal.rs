//! Durable repair evidence for capability denials.
//!
//! Prompt decisions answer "what did the operator allow?". This journal answers
//! the separate repair question: "what did the confinement boundary refuse,
//! under which exact command shape, and did it refuse again after a grant?".
//! Records are evidence only; nothing reads them back into authority.

use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::path::Path;

/// Opt-in path used by dispatch. The CLI arms it for normal interactive runs.
pub const DENIAL_JOURNAL_PATH_ENV: &str = "NEWT_DENIAL_JOURNAL";

/// Which attempt produced a denial.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DenialStage {
    /// The command's first confined dispatch.
    Initial,
    /// The single retry after the operator granted the structured request.
    AfterGrant,
}

/// One structured entry from agent-bridle's denial envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalDenial {
    pub kind: String,
    pub target: String,
    pub reason: String,
}

/// One denied command attempt, persisted as a JSON line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DenialRecord {
    /// Wall-clock display claim; append order remains the ordering ground truth.
    pub ts_claim: String,
    /// Secret-redacted raw command: the replay fixture missing from turn events.
    pub command: String,
    /// Working directory used by `run_command`.
    pub cwd: String,
    pub stage: DenialStage,
    pub denials: Vec<JournalDenial>,
}

impl DenialRecord {
    /// Lift a structured confined-shell envelope into durable repair evidence.
    pub fn from_envelope(
        command: &str,
        cwd: &str,
        stage: DenialStage,
        envelope: &serde_json::Value,
    ) -> Option<Self> {
        let denials = envelope
            .get("denials")?
            .as_array()?
            .iter()
            .filter_map(|d| {
                let kind = d.get("kind")?.as_str()?.to_string();
                let target = d.get("target")?.as_str()?.to_string();
                let reason = d
                    .get("reason")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                Some(JournalDenial {
                    kind,
                    target,
                    reason,
                })
            })
            .collect::<Vec<_>>();
        if denials.is_empty() {
            return None;
        }
        Some(Self {
            ts_claim: chrono::Utc::now().to_rfc3339(),
            command: crate::agentic::compress::redact_secrets(command),
            cwd: cwd.to_string(),
            stage,
            denials,
        })
    }

    /// Append one record, creating parent directories as necessary.
    pub fn append_jsonl(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let line = serde_json::to_string(self).map_err(std::io::Error::other)?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        writeln!(file, "{line}")
    }
}

/// Record a denial when the CLI armed the journal. Recording is deliberately
/// best-effort: an observability failure must never alter enforcement.
pub fn record_envelope(command: &str, cwd: &str, stage: DenialStage, envelope: &serde_json::Value) {
    let Some(path) = std::env::var_os(DENIAL_JOURNAL_PATH_ENV) else {
        return;
    };
    let Some(record) = DenialRecord::from_envelope(command, cwd, stage, envelope) else {
        return;
    };
    let _ = record.append_jsonl(Path::new(&path));
}

/// Parse an append-only journal. Corrupt/partial lines are skipped so one
/// interrupted append cannot hide the remaining repair evidence.
pub fn read_jsonl(body: &str) -> Vec<DenialRecord> {
    body.lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// What kind of repair a denial most likely needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairClass {
    /// A normal denied target that may warrant a reviewed policy addition.
    PolicyGap,
    /// A parser limitation/malformed or deliberately unsupported construct.
    Structural,
    /// The same call was denied after a grant; granting cannot repair it.
    GrantRetryFailure,
}

impl RepairClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PolicyGap => "policy",
            Self::Structural => "implementation",
            Self::GrantRetryFailure => "grant-retry",
        }
    }
}

/// Fold repeated `(kind, target)` evidence while keeping the first replay
/// fixture and escalating its repair class if stronger evidence arrives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DenialSummary {
    pub kind: String,
    pub target: String,
    pub count: u64,
    pub classification: RepairClass,
    pub example_command: String,
    pub reason: String,
}

fn class_for(stage: DenialStage, reason: &str) -> RepairClass {
    if stage == DenialStage::AfterGrant {
        RepairClass::GrantRetryFailure
    } else if reason.contains("refused by design:")
        || reason.contains("not yet supported by the confined shell")
        || reason.contains("malformed command:")
    {
        RepairClass::Structural
    } else {
        RepairClass::PolicyGap
    }
}

fn class_rank(class: RepairClass) -> u8 {
    match class {
        RepairClass::PolicyGap => 0,
        RepairClass::Structural => 1,
        RepairClass::GrantRetryFailure => 2,
    }
}

pub fn summarize(records: &[DenialRecord]) -> Vec<DenialSummary> {
    let mut out: Vec<DenialSummary> = Vec::new();
    for record in records {
        for denial in &record.denials {
            let class = class_for(record.stage, &denial.reason);
            if let Some(existing) = out
                .iter_mut()
                .find(|s| s.kind == denial.kind && s.target == denial.target)
            {
                existing.count += 1;
                if class_rank(class) > class_rank(existing.classification) {
                    existing.classification = class;
                    existing.reason.clone_from(&denial.reason);
                }
            } else {
                out.push(DenialSummary {
                    kind: denial.kind.clone(),
                    target: denial.target.clone(),
                    count: 1,
                    classification: class,
                    example_command: record.command.clone(),
                    reason: denial.reason.clone(),
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exec_envelope(target: &str, reason: &str) -> serde_json::Value {
        serde_json::json!({
            "denied": true,
            "denials": [{
                "kind": "exec",
                "target": target,
                "reason": reason,
            }],
        })
    }

    #[test]
    fn journal_preserves_the_redacted_command_and_structured_denial() {
        let record = DenialRecord::from_envelope(
            "wc -l src/lib.rs; curl -H 'Authorization: Bearer eyJabcdefgh.abcdefgh.abcdefgh'",
            "/workspace",
            DenialStage::Initial,
            &exec_envelope(
                "confinement",
                "exec of \"confinement\" is not within the granted authority",
            ),
        )
        .expect("structured denial");

        assert!(record.command.starts_with("wc -l src/lib.rs"));
        assert!(!record.command.contains("eyJabcdefgh"));
        assert!(record.command.contains("[REDACTED]"));
        assert_eq!(record.cwd, "/workspace");
        assert_eq!(record.stage, DenialStage::Initial);
        assert_eq!(record.denials[0].target, "confinement");
    }

    #[test]
    fn summaries_separate_policy_structural_and_retry_repairs() {
        let policy = DenialRecord::from_envelope(
            "bacon check",
            "/workspace",
            DenialStage::Initial,
            &exec_envelope(
                "bacon",
                "exec of \"bacon\" is not within the granted authority",
            ),
        )
        .unwrap();
        let structural = DenialRecord::from_envelope(
            "echo $(date)",
            "/workspace",
            DenialStage::Initial,
            &exec_envelope(
                "command substitution `$(`",
                "refused by design: command substitution is a dynamic construct",
            ),
        )
        .unwrap();
        let retry = DenialRecord::from_envelope(
            "wc -l src/lib.rs",
            "/workspace",
            DenialStage::AfterGrant,
            &exec_envelope(
                "confinement",
                "exec of \"confinement\" is not within the granted authority",
            ),
        )
        .unwrap();

        let summaries = summarize(&[policy, structural, retry]);
        assert_eq!(summaries.len(), 3);
        assert_eq!(summaries[0].classification, RepairClass::PolicyGap);
        assert_eq!(summaries[1].classification, RepairClass::Structural);
        assert_eq!(summaries[2].classification, RepairClass::GrantRetryFailure);
    }

    #[test]
    fn jsonl_reader_folds_repeated_targets_without_losing_a_fixture() {
        let first = DenialRecord::from_envelope(
            "wc -l a.rs",
            "/workspace",
            DenialStage::Initial,
            &exec_envelope(
                "confinement",
                "exec of \"confinement\" is not within the granted authority",
            ),
        )
        .unwrap();
        let second = DenialRecord::from_envelope(
            "wc -l b.rs",
            "/workspace",
            DenialStage::Initial,
            &exec_envelope(
                "confinement",
                "exec of \"confinement\" is not within the granted authority",
            ),
        )
        .unwrap();
        let body = format!(
            "{}\nnot-json\n{}\n",
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&second).unwrap()
        );

        let summaries = summarize(&read_jsonl(&body));
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].count, 2);
        assert_eq!(summaries[0].example_command, "wc -l a.rs");
    }
}
