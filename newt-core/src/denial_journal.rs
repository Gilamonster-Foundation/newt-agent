//! Durable repair evidence for capability denials.
//!
//! Prompt decisions answer "what did the operator allow?". This journal answers
//! the separate repair question: "what did the confinement boundary refuse,
//! under which exact command shape, and did it refuse again after a grant?".
//! Records are evidence only; nothing reads them back into authority.
//!
//! # The chain, and why this is not its own
//!
//! Until #2085's chain reached it, this file appended plain JSON lines with no
//! address of any kind:
//! a record could be edited, deleted, or reordered and nothing could tell. It
//! now writes [`event_journal`](crate::event_journal)'s chained line —
//! literally that module's [`Journal`], [`JournalLine`] and [`verify_chain`]
//! over a `DenialRecord` payload — because that module's own doc names it *"the
//! chained shape both are meant to migrate onto"*. Adopting the shape by
//! reusing the code is the only reading of that sentence that does not end in
//! a third journal.
//!
//! What stays here is what genuinely differs: the record, the repair
//! classification, and the **separate opt-in lifecycle**
//! ([`DENIAL_JOURNAL_PATH_ENV`], armed by the CLI, `off`/`0` to decline). The
//! streams are not merged for exactly that reason — merging would change *when*
//! denial evidence is written, which is a behavioural change, not a refactor.
//!
//! A denied command is the record an operator has motive to remove — it is the
//! evidence that a grant did not work, or that a target was refused. Per-row
//! addressing cannot see a removal; the chain can. See
//! [`verify_chain`](crate::event_journal::verify_chain) for what it can and
//! cannot prove, truncation included.
//!
//! # One encoding, not two
//!
//! A journal written before the chain is **rotated aside** to `.pre-chain` on
//! the first chained append (see [`append_record`]), and a fresh chain starts.
//! Those bytes stay on disk and readable; what this module does not carry is a
//! second decode arm for them forever.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::event_journal::{self, JournalLine};

/// The chain's reader-side vocabulary, re-exported so the one thing that reads
/// this journal back (`newt ocap denials`) does not have to know it is built on
/// the event journal's machinery.
pub use crate::event_journal::{head_path, read_head, verify_chain, ChainBreak};

/// Opt-in path used by dispatch. The CLI arms it for normal interactive runs.
pub const DENIAL_JOURNAL_PATH_ENV: &str = "NEWT_DENIAL_JOURNAL";

/// One chained denial line: the record, its parent link, and the address that
/// covers both.
pub type DenialLine = JournalLine<DenialRecord>;

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
    /// Wall-clock display claim. The CHAIN is the ordering ground truth — this
    /// file used to claim append order was, while nothing checked append order
    /// at all.
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
}

/// Where a pre-chain journal is moved to when the chain is adopted.
#[must_use]
pub fn pre_chain_path(path: &Path) -> PathBuf {
    path.with_extension("pre-chain")
}

/// Move a pre-chain journal aside, once, so the chain starts from a clean file.
///
/// The bytes are kept — an operator can still read them — but they are not
/// mixed into a chain they were never part of, and no second decode arm is
/// carried here to parse them. Skipped entirely once a head ref exists, which
/// is the steady state after the first chained append.
fn rotate_pre_chain(path: &Path) -> std::io::Result<()> {
    if event_journal::read_head(path).is_some() {
        return Ok(());
    }
    let Ok(body) = std::fs::read_to_string(path) else {
        return Ok(());
    };
    let Some(first) = body.lines().find(|line| !line.trim().is_empty()) else {
        return Ok(());
    };
    if serde_json::from_str::<DenialLine>(first).is_ok() {
        return Ok(()); // Already a chain, just missing its ref.
    }
    std::fs::rename(path, pre_chain_path(path))
}

/// Append one record as a chained line, advancing the head ref beside it.
///
/// # Concurrency
///
/// Two processes journalling into the same file can read the same head and
/// append two lines claiming the same parent, which reads back as a
/// [`ChainBreak::BrokenLink`]. That is the same over-reporting direction
/// [`event_journal::append_to`] already chose deliberately — a false alarm an
/// operator can inspect, rather than a real removal that verifies clean. It is
/// not fixed here because the fix is a lock, and a lock on the observability
/// path could make a denial wait on a journal.
///
/// # Errors
///
/// Propagates a filesystem or encoding failure. Callers on the enforcement
/// path must discard it — see [`record_envelope`].
pub fn append_record(path: &Path, record: DenialRecord) -> anyhow::Result<DenialLine> {
    rotate_pre_chain(path)?;
    let mut journal = event_journal::resume(path);
    event_journal::append_to(&mut journal, path, record)
}

/// Record a denial when the CLI armed the journal. Recording is deliberately
/// best-effort: an observability failure must never alter enforcement, so
/// minting an id — which is fallible — cannot introduce a failure path here.
pub fn record_envelope(command: &str, cwd: &str, stage: DenialStage, envelope: &serde_json::Value) {
    let Some(path) = std::env::var_os(DENIAL_JOURNAL_PATH_ENV) else {
        return;
    };
    let Some(record) = DenialRecord::from_envelope(command, cwd, stage, envelope) else {
        return;
    };
    let _ = append_record(Path::new(&path), record);
}

/// Parse an append-only journal. Corrupt/partial lines are skipped so one
/// interrupted append cannot hide the remaining repair evidence — and, unlike
/// before, skipping one no longer hides the *gap*: the record after it loses
/// its parent, which [`verify_chain`] reports.
#[must_use]
pub fn read_jsonl(body: &str) -> Vec<DenialLine> {
    event_journal::read_jsonl(body)
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

/// Fold the chain's records into repair evidence.
///
/// Takes lines rather than bare records so a caller cannot summarize evidence
/// it never had the chance to verify — the id and the parent link travel with
/// the record all the way to the presentation layer.
#[must_use]
pub fn summarize(lines: &[DenialLine]) -> Vec<DenialSummary> {
    let mut out: Vec<DenialSummary> = Vec::new();
    for record in lines.iter().map(|line| line.node.payload()) {
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
    use crate::event_journal::Journal;

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

    fn denied(command: &str, stage: DenialStage) -> DenialRecord {
        DenialRecord::from_envelope(
            command,
            "/workspace",
            stage,
            &exec_envelope(
                "confinement",
                "exec of \"confinement\" is not within the granted authority",
            ),
        )
        .expect("structured denial")
    }

    /// A chained run of `n` records — what `append_record` writes, without a
    /// file. The unit tier stays fs-free; `tests/denial_journal_fs.rs` grounds
    /// this against a real one.
    fn chain(n: usize) -> (Journal, Vec<DenialLine>) {
        let mut journal = Journal::new();
        let lines = (0..n)
            .map(|i| {
                journal
                    .append(denied(&format!("wc -l {i}.rs"), DenialStage::Initial))
                    .expect("append")
            })
            .collect();
        (journal, lines)
    }

    // --- the tamper suite: the properties an unaddressed JSON line has none of ---

    #[test]
    fn an_intact_chain_of_denials_verifies() {
        let (journal, lines) = chain(4);
        let head = journal.head().expect("head").to_string();
        assert_eq!(verify_chain(&lines, Some(&head)), vec![]);
    }

    /// The case a per-row address would also catch — kept so the chain is not
    /// shown to be better at the hard cases while being worse at the basic one.
    #[test]
    fn a_tampered_record_fails_verification() {
        let (_, mut lines) = chain(3);
        lines[1].node.payload.command = "wc -l innocent.rs".to_string();
        assert!(verify_chain(&lines, None).contains(&ChainBreak::Edited { index: 1 }));
    }

    /// **The case a per-row address cannot see, and the reason this is a chain.**
    /// Deleting the evidence of a denial is the tamper an operator repairing a
    /// policy actually has motive for.
    #[test]
    fn a_deleted_record_fails_verification_though_every_survivor_is_intact() {
        let (_, mut lines) = chain(4);
        lines.remove(1);
        let breaks = verify_chain(&lines, None);
        assert!(
            breaks.contains(&ChainBreak::BrokenLink { index: 1 }),
            "removing a record must break the link at the record after it: {breaks:?}"
        );
        assert!(
            lines.iter().all(DenialLine::is_intact),
            "every surviving record is still individually intact — which is \
             exactly why addressing each row on its own is not enough"
        );
    }

    /// Also invisible per-row: the same records, in the wrong order. Stage
    /// ordering is what separates a policy gap from a grant-retry failure, so a
    /// reorder is a repair-class forgery.
    #[test]
    fn a_reordered_pair_fails_verification() {
        let (_, mut lines) = chain(4);
        lines.swap(1, 2);
        assert!(
            !verify_chain(&lines, None).is_empty(),
            "a reorder must fail"
        );
        assert!(lines.iter().all(DenialLine::is_intact));
    }

    /// The honest limit: lopping records off the END leaves a shorter chain
    /// that is valid on its own terms. Only the separately stored head sees it.
    #[test]
    fn a_truncated_tail_is_caught_only_by_the_head_ref() {
        let (journal, mut lines) = chain(5);
        let head = journal.head().expect("head").to_string();
        lines.truncate(2);

        assert_eq!(
            verify_chain(&lines, None),
            vec![],
            "the chain alone cannot see its own truncation — the limit that \
             makes the stored head necessary, not a bug"
        );
        assert_eq!(
            verify_chain(&lines, Some(&head)),
            vec![ChainBreak::Truncated {
                expected_head: head
            }],
        );
    }

    /// One encoding, not two. A pre-chain line is not a denial record this
    /// reader will accept — `append_record` rotates such a file aside rather
    /// than leaving a second decode arm here forever.
    #[test]
    fn a_pre_chain_line_is_not_read_as_a_denial() {
        let legacy = serde_json::to_string(&denied("wc -l old.rs", DenialStage::Initial))
            .expect("serialize");
        assert!(read_jsonl(&legacy).is_empty());
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

        let mut journal = Journal::new();
        let lines: Vec<_> = [policy, structural, retry]
            .into_iter()
            .map(|r| journal.append(r).expect("append"))
            .collect();

        let summaries = summarize(&lines);
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
        // Three appends, but the middle one was interrupted mid-write and is
        // on disk as a partial line.
        let mut journal = Journal::new();
        let first = journal.append(first).expect("append");
        let _interrupted = journal
            .append(denied("wc -l torn.rs", DenialStage::Initial))
            .expect("append");
        let second = journal.append(second).expect("append");
        let body = format!(
            "{}\nnot-js\n{}\n",
            first.render_line().unwrap(),
            second.render_line().unwrap()
        );

        let lines = read_jsonl(&body);
        let summaries = summarize(&lines);
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].count, 2);
        assert_eq!(summaries[0].example_command, "wc -l a.rs");
        // Skipping the torn line keeps the rest of the evidence readable — but
        // it does not let the gap pass unremarked, which is what the old
        // "corrupt lines are skipped" contract quietly did.
        assert_eq!(
            verify_chain(&lines, None),
            vec![ChainBreak::BrokenLink { index: 1 }]
        );
    }
}
