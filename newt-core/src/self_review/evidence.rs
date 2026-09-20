use content_addressable::{ContentAddressable, ContentId};
use serde::{Deserialize, Serialize};

use super::{CaptureFailure, PresentedSubject};
use crate::event_journal::{Journal, JournalLine};

/// Model review disposition, deliberately distinct from a verification pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Assessment {
    NoFindings,
    Findings,
}

/// Model result contract; prose alone never becomes structured review evidence.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelResult {
    subject: ContentId,
    objective: ContentId,
    disposition: Assessment,
    findings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum ReviewFailure {
    #[error("review response was malformed or inconsistent")]
    Malformed,
    #[error("review evidence identity is invalid")]
    Tampered,
    #[error("review evidence belongs to a different objective")]
    WrongObjective,
    #[error("review evidence belongs to a different or stale subject")]
    StaleSubject,
    #[error("review is incomplete")]
    Incomplete,
    #[error(transparent)]
    Capture(#[from] CaptureFailure),
    #[error("review evidence could not be encoded: {0}")]
    Encoding(String),
}

/// Append-only phase outcome; findings are never relabelled as tests passed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ReviewStatus {
    Reviewed {
        disposition: Assessment,
        findings: Vec<String>,
    },
    Incomplete {
        reason: ReviewFailure,
    },
    Denied,
    Cancelled,
    Exhausted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewEvent {
    pub objective: ContentId,
    pub subject: ContentId,
    pub coverage: super::ReviewCoverage,
    pub outcome: ReviewStatus,
}

fn valid_assessment(disposition: &Assessment, findings: &[String]) -> bool {
    !findings.iter().any(|finding| finding.trim().is_empty())
        && matches!(disposition, Assessment::NoFindings) == findings.is_empty()
}

fn parse_result(raw: &str, subject: &PresentedSubject, id: ContentId) -> ReviewStatus {
    let failed = |reason| ReviewStatus::Incomplete { reason };
    let Ok(result) = serde_json::from_str::<ModelResult>(raw) else {
        return failed(ReviewFailure::Malformed);
    };
    if result.objective != subject.objective() {
        return failed(ReviewFailure::WrongObjective);
    }
    if result.subject != id {
        return failed(ReviewFailure::StaleSubject);
    }
    if !valid_assessment(&result.disposition, &result.findings) {
        return failed(ReviewFailure::Malformed);
    }
    ReviewStatus::Reviewed {
        disposition: result.disposition,
        findings: result.findings,
    }
}

/// Validate a model reply and append either reviewed or incomplete evidence.
/// Existing journal history is never rewritten to turn a failure into success.
///
/// # Errors
/// Encoding failure preserves the journal's previous head.
pub fn record_reply(
    journal: &mut Journal,
    presented: &PresentedSubject,
    raw: &str,
) -> Result<JournalLine<ReviewEvent>, ReviewFailure> {
    let id = presented
        .content_id()
        .map_err(|error| ReviewFailure::Encoding(error.to_string()))?;
    record_status(journal, presented, parse_result(raw, presented, id))
}

pub(crate) fn record_status(
    journal: &mut Journal,
    presented: &PresentedSubject,
    outcome: ReviewStatus,
) -> Result<JournalLine<ReviewEvent>, ReviewFailure> {
    let subject = presented
        .content_id()
        .map_err(|error| ReviewFailure::Encoding(error.to_string()))?;
    journal
        .append(ReviewEvent {
            objective: presented.objective(),
            subject,
            coverage: presented.coverage().clone(),
            outcome,
        })
        .map_err(|error| ReviewFailure::Encoding(error.to_string()))
}

/// The shared production-consumption boundary: validate identity, objective and
/// a fresh host capture before accepting a reviewed outcome. The host must
/// recapture through its existing authorized effects owner, not a cached digest.
///
/// # Errors
/// Rejects tamper, wrong context, stale/unavailable material and non-review outcomes.
pub fn consume_evidence(
    line: &JournalLine<ReviewEvent>,
    expected_evidence: ContentId,
    objective: ContentId,
    recapture: impl FnOnce() -> Result<PresentedSubject, CaptureFailure>,
) -> Result<&ReviewStatus, ReviewFailure> {
    // Compare to the host-retained journal head as well as self-consistency;
    // otherwise a rewritten payload with its address recomputed could pass.
    if line.id != expected_evidence.to_string() || !line.is_intact() {
        return Err(ReviewFailure::Tampered);
    }
    let event = line.node.payload();
    if event.objective != objective {
        return Err(ReviewFailure::WrongObjective);
    }
    let fresh = recapture()?;
    if fresh.objective() != objective {
        return Err(ReviewFailure::WrongObjective);
    }
    let id = fresh
        .content_id()
        .map_err(|error| ReviewFailure::Encoding(error.to_string()))?;
    if event.subject != id || event.coverage != *fresh.coverage() {
        return Err(ReviewFailure::StaleSubject);
    }
    match &event.outcome {
        reviewed @ ReviewStatus::Reviewed {
            disposition,
            findings,
        } => {
            if !valid_assessment(disposition, findings) {
                return Err(ReviewFailure::Malformed);
            }
            Ok(reviewed)
        }
        _ => Err(ReviewFailure::Incomplete),
    }
}
