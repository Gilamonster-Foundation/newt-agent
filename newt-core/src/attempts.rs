//! Per-attempt inference usage ledger (#2313).
//!
//! Every inference attempt is counted once, attributed, and reconcilable, and
//! missing usage stays missing. Attempt identity is a [`ContentId`] over
//! [`AttemptKey`], minted through `content-addressable`; the ledger's lines
//! are [`crate::event_journal::JournalLine`]s on the existing chained journal,
//! so an emitted trace verifies with [`crate::event_journal::verify_chain`].
//! It writes no file of its own.
//!
//! [`ContentId`]: content_addressable::ContentId

use serde::{Deserialize, Serialize};

use crate::metrics::TokenUsage;

/// Terminal state of one inference attempt (#2313): how the attempt ended,
/// and nothing else. Whether usage was reported is a separate fact —
/// `AttemptRecord::usage` is `None`, counted by `UsageTotals::usage_missing`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptState {
    Ok,
    Failed,
    Cancelled,
}

/// What an attempt IS, and so what its identity is minted from: the turn, the
/// role, the exact request bytes and the attempt's position among attempts
/// sharing those three, in dispatch order. Outcome (usage, state) is excluded,
/// so re-observing one attempt keeps its id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptKey {
    pub turn: String,
    pub role: String,
    pub request: content_addressable::RawContentId,
    pub ordinal: u32,
}

impl content_addressable::ContentAddressable for AttemptKey {
    fn canonical_form(&self) -> Result<Vec<u8>, content_addressable::ContentError> {
        content_addressable::canonical::to_canonical_dagcbor(self)
    }
}

/// One observation of one attempt, attributed and addressed by its key.
///
/// Carries the whole `key`, not just its role, so a reader of a trace line can
/// recompute `id` from the line's own content and detect an edited key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttemptRecord {
    /// `key.content_id()`, kept for indexing.
    pub id: content_addressable::ContentId,
    pub key: AttemptKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    pub model: String,
    pub backend: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
    pub state: AttemptState,
}

impl AttemptRecord {
    /// Attribute an observation to `key`, minting the id from the key alone.
    ///
    /// # Errors
    /// Propagates a canonical-encoding failure.
    pub fn new(
        key: &AttemptKey,
        model: &str,
        backend: &str,
        usage: Option<TokenUsage>,
        state: AttemptState,
    ) -> Result<Self, content_addressable::ContentError> {
        use content_addressable::ContentAddressable as _;
        Ok(Self {
            id: key.content_id()?,
            key: key.clone(),
            tier: None,
            model: model.to_string(),
            backend: backend.to_string(),
            usage,
            state,
        })
    }
}

/// Run totals over the latest observation of every distinct attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageTotals {
    pub attempts: u32,
    /// Attempts whose usage is `None`, whatever their state.
    pub usage_missing: u32,
    pub in_tokens: u64,
    pub out_tokens: u64,
    pub usage_complete: bool,
}

/// Per-attempt usage ledger (#2313), chained on [`crate::event_journal`].
///
/// Holds the chain head plus the latest observation per attempt id: the fold
/// the totals are computed from, not a copy of the emitted lines. Those lines
/// go to a trace (`--events`), where `verify_chain` is the check on the head.
#[derive(Debug, Default)]
pub struct AttemptLedger {
    journal: crate::event_journal::Journal,
    ordinals: std::collections::BTreeMap<(String, String, content_addressable::RawContentId), u32>,
    observed: std::collections::BTreeMap<content_addressable::ContentId, AttemptRecord>,
}

impl AttemptLedger {
    /// Mint the key for the next attempt at `{turn, role, request_body}`. Its
    /// ordinal is the attempt's position among attempts sharing those three,
    /// in dispatch order, so a retry of identical bytes is a distinct attempt.
    pub fn dispatch(&mut self, turn: &str, role: &str, request_body: &[u8]) -> AttemptKey {
        let request = content_addressable::RawContentId::from_content(request_body);
        let next = self
            .ordinals
            .entry((turn.to_string(), role.to_string(), request))
            .or_default();
        let ordinal = *next;
        *next += 1;
        AttemptKey {
            turn: turn.to_string(),
            role: role.to_string(),
            request,
            ordinal,
        }
    }

    /// Record an observation and return the chain line for an `--events` trace.
    /// A re-observation of the same attempt replaces its earlier one in the
    /// totals and is appended to the chain as a new line.
    ///
    /// # Errors
    /// Propagates a canonical-encoding failure; the ledger is then unchanged.
    pub fn observe(
        &mut self,
        record: AttemptRecord,
    ) -> Result<crate::event_journal::JournalLine<AttemptRecord>, content_addressable::ContentError>
    {
        let line = self.journal.append(record.clone())?;
        self.observed.insert(record.id, record);
        Ok(line)
    }

    /// Totals over the latest observation per attempt id. `in_tokens` is the
    /// per-attempt SUM (not the turn merge's max-of-rounds input), and usage is
    /// complete only when there was an attempt and none is missing usage.
    #[must_use]
    pub fn totals(&self) -> UsageTotals {
        let usage = self.observed.values().filter_map(|r| r.usage);
        let usage_missing = self.observed.values().filter(|r| r.usage.is_none()).count() as u32;
        UsageTotals {
            attempts: self.observed.len() as u32,
            usage_missing,
            in_tokens: usage.clone().map(|u| u64::from(u.input_tokens)).sum(),
            out_tokens: usage.map(|u| u64::from(u.output_tokens)).sum(),
            usage_complete: !self.observed.is_empty() && usage_missing == 0,
        }
    }

    /// The latest observation of every distinct attempt.
    pub fn records(&self) -> impl Iterator<Item = &AttemptRecord> {
        self.observed.values()
    }

    /// The chain head, reported only when the lines were emitted.
    #[must_use]
    pub fn head(&self) -> Option<&content_addressable::ContentId> {
        self.journal.head()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn used(input_tokens: u32, output_tokens: u32) -> Option<TokenUsage> {
        Some(TokenUsage {
            input_tokens,
            output_tokens,
        })
    }

    fn record(key: &AttemptKey, usage: Option<TokenUsage>, state: AttemptState) -> AttemptRecord {
        AttemptRecord::new(key, "fixture-model", "fixture-backend", usage, state).unwrap()
    }

    /// #2313 trap: completeness must not be vacuously true. Zero attempts is
    /// NOT complete; any attempt missing usage, in any state, makes it
    /// incomplete; the known totals sum only measured attempts.
    #[test]
    fn usage_complete_needs_an_attempt_and_no_missing_usage() {
        let mut ledger = AttemptLedger::default();
        assert!(
            !ledger.totals().usage_complete,
            "zero attempts is not complete"
        );

        let measured = ledger.dispatch("turn-1", "primary", b"round 0");
        ledger
            .observe(record(&measured, used(1_000, 10), AttemptState::Ok))
            .unwrap();
        assert!(
            ledger.totals().usage_complete,
            "one measured attempt is complete"
        );

        let unmeasured = ledger.dispatch("turn-1", "auxiliary", b"classify");
        ledger
            .observe(record(&unmeasured, None, AttemptState::Ok))
            .unwrap();
        let failed = ledger.dispatch("turn-1", "summarizer", b"summarize");
        ledger
            .observe(record(&failed, None, AttemptState::Failed))
            .unwrap();
        assert_eq!(
            ledger.totals(),
            UsageTotals {
                attempts: 3,
                usage_missing: 2,
                in_tokens: 1_000,
                out_tokens: 10,
                usage_complete: false,
            }
        );
    }

    /// #2313: state is TERMINAL only — ok, failed, cancelled. Whether usage
    /// was reported is a separate fact (`usage: None`, counted by
    /// `usage_missing`), so a failed call that reported nothing stays
    /// distinguishable from a completed one, and there is no fourth state that
    /// mixes the two.
    #[test]
    fn attempt_state_is_terminal_only_and_missing_usage_is_separate() {
        for state in ["ok", "failed", "cancelled"] {
            assert!(
                serde_json::from_str::<AttemptState>(&format!("\"{state}\"")).is_ok(),
                "{state}"
            );
        }
        assert!(
            serde_json::from_str::<AttemptState>("\"unmeasured\"").is_err(),
            "missing usage is not a terminal state"
        );

        let mut ledger = AttemptLedger::default();
        let failed = ledger.dispatch("turn-1", "primary", b"round 0");
        let line = ledger
            .observe(record(&failed, None, AttemptState::Failed))
            .unwrap();
        assert_eq!(line.node.payload().state, AttemptState::Failed);
        let totals = ledger.totals();
        assert_eq!(
            (totals.attempts, totals.usage_missing, totals.usage_complete),
            (1, 1, false)
        );
    }

    /// #2313 trap: turn usage takes the MAX input across rounds (the
    /// `merge_round_usage` rule), which under-counts what was sent. The
    /// ledger sums each attempt's input: 1,000 + 1,500 is 2,500, not 1,500.
    #[test]
    fn in_tokens_sum_every_attempt_not_the_max_merged_turn_input() {
        let mut ledger = AttemptLedger::default();
        for (body, input, output) in [(&b"round 0"[..], 1_000, 20), (&b"round 1"[..], 1_500, 30)] {
            let key = ledger.dispatch("turn-1", "primary", body);
            ledger
                .observe(record(&key, used(input, output), AttemptState::Ok))
                .unwrap();
        }
        let totals = ledger.totals();
        assert_eq!((totals.in_tokens, totals.out_tokens), (2_500, 50));
    }

    /// #2313 trap: identity is minted per ATTEMPT. Re-observing one attempt
    /// (a partial count, then the final one) counts it once with the latest
    /// usage; two dispatches of identical bytes are two attempts.
    #[test]
    fn a_reobserved_attempt_counts_once_and_identical_retries_count_twice() {
        let mut ledger = AttemptLedger::default();
        let key = ledger.dispatch("turn-1", "primary", b"same body");
        ledger
            .observe(record(&key, used(100, 5), AttemptState::Cancelled))
            .unwrap();
        ledger
            .observe(record(&key, used(100, 9), AttemptState::Ok))
            .unwrap();
        let totals = ledger.totals();
        assert_eq!((totals.attempts, totals.out_tokens), (1, 9));

        let retry = ledger.dispatch("turn-1", "primary", b"same body");
        assert_eq!((key.ordinal, retry.ordinal), (0, 1));
        assert_ne!(
            record(&key, None, AttemptState::Ok).id,
            record(&retry, None, AttemptState::Ok).id
        );
        ledger
            .observe(record(&retry, used(100, 4), AttemptState::Ok))
            .unwrap();
        let totals = ledger.totals();
        assert_eq!(
            (totals.attempts, totals.in_tokens, totals.out_tokens),
            (2, 200, 13)
        );
    }

    /// #2313: a trace line's attempt identity is checkable from the line
    /// alone. After a JSON round-trip the record still carries the key its id
    /// was minted from, and recomputing gives the same id. The twin shows a
    /// tampered ordinal no longer matches, so an edit to the key is detected.
    #[test]
    fn a_trace_line_recomputes_its_attempt_id_from_its_own_key() {
        use content_addressable::ContentAddressable as _;
        let mut ledger = AttemptLedger::default();
        let key = ledger.dispatch("turn-1", "primary", b"body");
        let line = ledger
            .observe(record(&key, used(10, 1), AttemptState::Ok))
            .unwrap();
        let decoded: crate::event_journal::JournalLine<AttemptRecord> =
            serde_json::from_str(&line.render_line().unwrap()).unwrap();
        let decoded = decoded.node.payload();
        assert_eq!(decoded.id, decoded.key.content_id().unwrap());

        let mut tampered = decoded.clone();
        tampered.key.ordinal += 1;
        assert_ne!(tampered.id, tampered.key.content_id().unwrap());
    }

    /// #2313: the lines an `--events` trace carries are the evidence for
    /// `ledger_head`. Interleaved with other trace lines they still verify as
    /// a chain reaching the head; a removed attempt line or a truncated tail
    /// is reported, never passed.
    #[test]
    fn emitted_attempt_lines_verify_as_a_chain_inside_a_mixed_trace() {
        use crate::event_journal::{read_jsonl, verify_chain, ChainBreak};
        let mut ledger = AttemptLedger::default();
        let mut trace = vec![r#"{"kind":"parse_signal"}"#.to_string()];
        for (role, body) in [("primary", "a"), ("auxiliary", "b"), ("summarizer", "c")] {
            let key = ledger.dispatch("turn-1", role, body.as_bytes());
            let line = ledger
                .observe(record(&key, used(10, 1), AttemptState::Ok))
                .unwrap();
            trace.push(line.render_line().unwrap());
            trace.push(r#"{"kind":"solve_result"}"#.to_string());
        }
        let head = ledger
            .head()
            .expect("emitted lines leave a head")
            .to_string();

        let lines = read_jsonl::<AttemptRecord>(&trace.join("\n"));
        assert_eq!(
            lines.len(),
            3,
            "non-attempt trace lines are not chain lines"
        );
        assert_eq!(verify_chain(&lines, Some(&head)), vec![]);

        let mut removed = lines.clone();
        removed.remove(1);
        assert_eq!(
            verify_chain(&removed, Some(&head)),
            vec![ChainBreak::BrokenLink { index: 1 }]
        );
        assert_eq!(
            verify_chain(&lines[..2], Some(&head)),
            vec![ChainBreak::Truncated {
                expected_head: head.clone()
            }]
        );
    }
}
