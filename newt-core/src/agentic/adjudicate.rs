//! One bounded, tool-less adjudication of heuristic decision candidates.
//!
//! The deterministic detector in [`super::prompt_intake`] decides WHAT might be
//! a decision; this module asks the model the one question the detector cannot
//! answer from wording alone — *did the operator delegate resolution of this
//! decision to the agent, and if so under what stated interpretation?*
//!
//! Authority is deliberately lopsided. The model returns verdicts; the harness
//! ([`PromptIntake::apply_adjudications`]) is the only thing that may change
//! state, and the only transition it will perform is
//! `Pending -> Locked(AuthorizedAssumption)`. Every failure path here — a side
//! call that errors, times out, returns prose, returns malformed JSON, or
//! addresses a decision that was never a candidate — leaves the candidates
//! `Pending` and asks the operator. This is a filter over the heuristic's
//! output, never a second agent turn: no tools, no repository access, no
//! network beyond the single completion, and no retry loop.

use super::compress::Summarizer;
use super::prompt_intake::{
    AdjudicationRefusal, AdjudicationVerdict, PromptIntake, MAX_ADJUDICATION_BATCH,
};

/// Instruction for the adjudication side call. It states the delegated-vs-
/// operator-owned distinction directly, because that distinction — not the
/// imperative mood of "choose" — is the whole question.
pub(super) fn build_adjudication_prompt(intake: &PromptIntake) -> String {
    let mut prompt = String::from(
        "You are adjudicating whether an operator delegated a decision to the agent.\n\
         For each numbered item, decide ONE thing: did the operator leave this choice \
         to the agent's judgement, or does it remain the operator's to make?\n\n\
         Mark `delegated_to_agent: true` ONLY when the instruction hands the agent a \
         criterion it can apply on its own (\"choose the smallest coherent fix\", \
         \"whichever needs the least change\"), and state the interpretation you would \
         proceed under in `assumption`.\n\
         Mark `delegated_to_agent: false` when the operator poses a real alternative \
         without a deciding criterion (\"choose SQLite or Postgres\"), asks to be asked \
         (\"ask me whether to use X or Y\"), or states that they have not decided \
         (\"I haven't decided whether we should use X or Y\").\n\
         When uncertain, answer false. A wrong `false` costs one question; a wrong \
         `true` silently substitutes your judgement for the operator's.\n\n\
         Reply with ONLY a JSON array, no prose and no code fence:\n\
         [{\"decision_id\": 1, \"delegated_to_agent\": true, \"assumption\": \"…\"}]\n\
         `assumption` must be a non-empty sentence when delegated_to_agent is true, \
         and \"\" otherwise.\n\n\
         Items:\n",
    );
    for candidate in intake.adjudication_candidates() {
        prompt.push_str(&format!("{}. {}\n", candidate.id, candidate.question));
    }
    prompt
}

/// Strict parse of the adjudication reply.
///
/// Tolerates only what a well-behaved model incidentally adds — surrounding
/// whitespace and a ```json fence. It does NOT hunt for a decision inside
/// prose: an unparseable reply is a refusal, not an invitation to guess.
pub(super) fn parse_adjudication_reply(reply: &str) -> Option<Vec<AdjudicationVerdict>> {
    let trimmed = strip_code_fence(reply.trim());
    let start = trimmed.find('[')?;
    let end = trimmed.rfind(']')?;
    if end < start {
        return None;
    }
    serde_json::from_str::<Vec<AdjudicationVerdict>>(&trimmed[start..=end]).ok()
}

fn strip_code_fence(reply: &str) -> &str {
    let Some(rest) = reply.strip_prefix("```") else {
        return reply;
    };
    let rest = rest.strip_prefix("json").unwrap_or(rest);
    rest.trim_start_matches('\n')
        .trim_end()
        .strip_suffix("```")
        .unwrap_or(rest)
}

/// Run the bounded adjudication and return the harness-applied intake.
///
/// Returns the intake UNCHANGED on every failure path. The batch bound is
/// checked before the side call, so an oversized batch costs nothing.
pub async fn adjudicate_decisions(intake: &PromptIntake, complete: &Summarizer) -> PromptIntake {
    let candidates = intake.adjudication_candidates();
    if candidates.is_empty() {
        return intake.clone();
    }
    if candidates.len() > MAX_ADJUDICATION_BATCH {
        tracing::warn!(
            candidates = candidates.len(),
            bound = MAX_ADJUDICATION_BATCH,
            "decision adjudication refused an oversized batch; asking the operator"
        );
        return intake.clone();
    }

    let reply = match complete(build_adjudication_prompt(intake)).await {
        Ok(reply) => reply,
        Err(error) => {
            tracing::warn!(%error, "decision adjudication unavailable; asking the operator");
            return intake.clone();
        }
    };
    let Some(verdicts) = parse_adjudication_reply(&reply) else {
        tracing::warn!("decision adjudication returned malformed output; asking the operator");
        return intake.clone();
    };
    match intake.apply_adjudications(&verdicts) {
        Ok(resolved) => resolved,
        Err(AdjudicationRefusal::BatchTooLarge { candidates, bound }) => {
            tracing::warn!(
                candidates,
                bound,
                "decision adjudication exceeded its bound"
            );
            intake.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agentic::{DecisionSource, DecisionStatus, PromptDisposition};

    /// A candidate prompt whose decisions the heuristic detects deterministically.
    const DELEGATED: &str = "Choose the smallest coherent fix consistent with existing design.";
    const OPERATOR_OWNED: &str = "Choose SQLite or Postgres.";

    fn summarizer(reply: &'static str) -> Summarizer {
        Box::new(move |_prompt: String| {
            Box::pin(async move { Ok(reply.to_string()) })
                as super::super::compress::SummarizeFuture
        })
    }

    fn failing_summarizer() -> Summarizer {
        Box::new(move |_prompt: String| {
            Box::pin(async move { Err(anyhow::anyhow!("backend down")) })
                as super::super::compress::SummarizeFuture
        })
    }

    /// The detector is the sole source of candidates and does not consult the
    /// model: both shapes are detected identically, and the model's job is only
    /// to say which one the operator delegated.
    #[test]
    fn candidate_generation_is_deterministic_and_model_free() {
        for prompt in [DELEGATED, OPERATOR_OWNED] {
            let first = PromptIntake::analyze(prompt);
            let second = PromptIntake::analyze(prompt);
            assert_eq!(
                first.adjudication_candidates(),
                second.adjudication_candidates(),
                "{prompt:?}"
            );
            assert_eq!(first.adjudication_candidates().len(), 1, "{prompt:?}");
            assert_eq!(first.disposition(), PromptDisposition::Ask, "{prompt:?}");
        }
    }

    /// The model may only filter the heuristic's list. A verdict naming an
    /// ordinal that was never offered is discarded, so adjudication can never
    /// manufacture — or silently widen — the decision set.
    #[test]
    fn adjudicator_cannot_manufacture_candidates() {
        let intake = PromptIntake::analyze(DELEGATED);
        let before = intake.manifest().decision_count();
        let resolved = intake
            .apply_adjudications(&[
                AdjudicationVerdict {
                    decision_id: 7,
                    delegated_to_agent: true,
                    assumption: "invented".to_string(),
                },
                AdjudicationVerdict {
                    decision_id: 0,
                    delegated_to_agent: true,
                    assumption: "also invented".to_string(),
                },
            ])
            .expect("batch is within bounds");
        assert_eq!(resolved.manifest().decision_count(), before);
        assert_eq!(resolved.manifest().pending_decision_count(), 1);
        assert_eq!(resolved.authorized_assumption_count(), 0);
        assert_eq!(resolved.disposition(), PromptDisposition::Ask);
    }

    /// The only transition the model can drive is
    /// `Pending -> Locked(AuthorizedAssumption)`. It cannot unlock, cannot
    /// re-source an operator answer, and cannot mark anything `Operator`.
    #[test]
    fn only_pending_to_authorized_assumption_is_legal() {
        let intake = PromptIntake::analyze(DELEGATED);
        let resolved = intake
            .apply_adjudications(&[AdjudicationVerdict {
                decision_id: 1,
                delegated_to_agent: true,
                assumption: "Use the smallest coherent fix.".to_string(),
            }])
            .expect("within bounds");
        let decision = &resolved.manifest().decisions()[0];
        assert_eq!(decision.status(), DecisionStatus::Locked);
        assert_eq!(
            decision.source(),
            Some(DecisionSource::AuthorizedAssumption)
        );
        assert_eq!(
            decision.assumption(),
            Some("Use the smallest coherent fix.")
        );

        // An operator answer is already locked; a later adjudication of the
        // same batch must not touch it or restate its provenance.
        let by_operator =
            PromptIntake::analyze(OPERATOR_OWNED).resolve_with_operator_answer("1: use Postgres");
        assert_eq!(
            by_operator.manifest().decisions()[0].source(),
            Some(DecisionSource::Operator)
        );
        assert!(by_operator.adjudication_candidates().is_empty());
        let after = by_operator
            .apply_adjudications(&[AdjudicationVerdict {
                decision_id: 1,
                delegated_to_agent: true,
                assumption: "model tries to overwrite the operator".to_string(),
            }])
            .expect("within bounds");
        assert_eq!(
            after.manifest().decisions()[0].source(),
            Some(DecisionSource::Operator)
        );
        assert_eq!(after.manifest().decisions()[0].assumption(), None);
        assert_eq!(after.authorized_assumption_count(), 0);
    }

    /// Authorization requires a stated interpretation. A `true` verdict with no
    /// assumption is a silent guess, and is refused.
    #[test]
    fn authorization_requires_a_non_empty_assumption() {
        let intake = PromptIntake::analyze(DELEGATED);
        for assumption in ["", "   ", "\n\t "] {
            let resolved = intake
                .apply_adjudications(&[AdjudicationVerdict {
                    decision_id: 1,
                    delegated_to_agent: true,
                    assumption: assumption.to_string(),
                }])
                .expect("within bounds");
            assert_eq!(
                resolved.manifest().pending_decision_count(),
                1,
                "{assumption:?} must not authorize a lock"
            );
            assert_eq!(resolved.disposition(), PromptDisposition::Ask);
        }
    }

    /// `delegated_to_agent: false` is the model saying "this is the operator's
    /// call" — it stays pending and reaches the clarification batch.
    #[tokio::test]
    async fn genuine_choices_still_reach_the_clarification_batch() {
        let intake = PromptIntake::analyze(OPERATOR_OWNED);
        let resolved = adjudicate_decisions(
            &intake,
            &summarizer(r#"[{"decision_id":1,"delegated_to_agent":false,"assumption":""}]"#),
        )
        .await;
        assert_eq!(resolved.disposition(), PromptDisposition::Ask);
        assert!(resolved.clarification_batch().contains("SQLite"));
        assert!(resolved.authorized_assumption_notices().is_empty());
    }

    /// A delegated decision disappears from the batch entirely and the turn
    /// proceeds under its stated assumption.
    #[tokio::test]
    async fn delegated_choices_disappear_from_the_batch() {
        let intake = PromptIntake::analyze(DELEGATED);
        let resolved = adjudicate_decisions(
            &intake,
            &summarizer(
                r#"[{"decision_id":1,"delegated_to_agent":true,"assumption":"Use the smallest coherent fix consistent with the existing design."}]"#,
            ),
        )
        .await;
        assert_eq!(resolved.manifest().pending_decision_count(), 0);
        assert_ne!(resolved.disposition(), PromptDisposition::Ask);
        assert_eq!(resolved.clarification_batch(), "");
    }

    /// Every assumption is stated to the operator with the ordinal that
    /// reopens it, and is carried in the durable projection as its own digest.
    #[tokio::test]
    async fn assumptions_are_rendered_and_persisted() {
        let resolved = adjudicate_decisions(
            &PromptIntake::analyze(DELEGATED),
            &summarizer(
                r#"[{"decision_id":1,"delegated_to_agent":true,"assumption":"Use the smallest coherent fix."}]"#,
            ),
        )
        .await;
        assert_eq!(
            resolved.authorized_assumption_notices(),
            vec!["Assuming: Use the smallest coherent fix. — `/undo-lock 1` to reopen".to_string()]
        );
        let metadata = resolved.artifact_metadata();
        assert_eq!(
            metadata["decision_source_counts"]["authorized_assumption"],
            serde_json::json!(1)
        );
        assert_eq!(
            metadata["decision_source_counts"]["operator"],
            serde_json::json!(0)
        );
        assert_eq!(
            metadata["authorized_assumption_digests"]
                .as_array()
                .map(Vec::len),
            Some(1),
            "the assumption is provenanced without copying its text: {metadata}"
        );
    }

    /// `/undo-lock` reverses the harness's own inference and returns the
    /// decision to the operator.
    #[tokio::test]
    async fn undo_lock_reopens_an_authorized_assumption() {
        let resolved = adjudicate_decisions(
            &PromptIntake::analyze(DELEGATED),
            &summarizer(
                r#"[{"decision_id":1,"delegated_to_agent":true,"assumption":"Use the smallest coherent fix."}]"#,
            ),
        )
        .await;
        assert_eq!(resolved.authorized_assumption_count(), 1);

        let reopened = resolved.undo_lock(1).expect("ordinal 1 is an assumption");
        assert_eq!(reopened.manifest().pending_decision_count(), 1);
        assert_eq!(reopened.authorized_assumption_count(), 0);
        assert_eq!(reopened.disposition(), PromptDisposition::Ask);
        assert_eq!(reopened.manifest().decisions()[0].source(), None);
        assert_eq!(reopened.manifest().decisions()[0].assumption(), None);
        assert!(reopened
            .clarification_batch()
            .contains("smallest coherent fix"));
        assert!(resolved.undo_lock(2).is_none());
        assert!(resolved.undo_lock(0).is_none());

        // An operator answer is not the harness's to discard.
        let by_operator =
            PromptIntake::analyze(OPERATOR_OWNED).resolve_with_operator_answer("1: use Postgres");
        assert!(by_operator.undo_lock(1).is_none());
    }

    /// A mixed batch authorizes only what the model marked delegated; the rest
    /// is asked, and the surviving ordinals still renumber correctly.
    #[tokio::test]
    async fn mixed_batches_authorize_some_and_ask_others() {
        let intake = PromptIntake::analyze(
            "Choose the smallest coherent fix.\n\
             Choose SQLite or Postgres.\n\
             Pick whichever transport needs the least change.",
        );
        assert_eq!(intake.adjudication_candidates().len(), 3);
        let resolved = adjudicate_decisions(
            &intake,
            &summarizer(
                r#"[{"decision_id":1,"delegated_to_agent":true,"assumption":"Smallest coherent fix."},
                    {"decision_id":2,"delegated_to_agent":false,"assumption":""},
                    {"decision_id":3,"delegated_to_agent":true,"assumption":"Transport needing the least change."}]"#,
            ),
        )
        .await;
        assert_eq!(resolved.authorized_assumption_count(), 2);
        assert_eq!(resolved.manifest().pending_decision_count(), 1);
        assert_eq!(resolved.disposition(), PromptDisposition::Ask);

        let batch = resolved.clarification_batch();
        assert!(batch.contains("SQLite"), "{batch}");
        assert!(!batch.contains("smallest coherent fix"), "{batch}");
        assert!(batch.contains("1. "), "the survivor is renumbered: {batch}");
        assert!(!batch.contains("2. "), "{batch}");

        // The two assumption ordinals are their own numbering, 1 and 2.
        let notices = resolved.authorized_assumption_notices();
        assert!(notices[0].contains("/undo-lock 1"), "{notices:?}");
        assert!(notices[1].contains("/undo-lock 2"), "{notices:?}");

        // The still-pending operator decision answers on ordinal 1.
        let answered = resolved.resolve_with_operator_answer("1: use Postgres");
        assert_eq!(answered.manifest().pending_decision_count(), 0);
        assert_eq!(answered.authorized_assumption_count(), 2);
    }

    /// The batch bound refuses wholesale rather than adjudicating a prefix.
    ///
    /// `MAX_CONCRETE_DECISIONS` already caps parsing at exactly
    /// `MAX_ADJUDICATION_BATCH` candidates, so this bound is defense in depth
    /// and is unreachable through `analyze` — proving it requires constructing
    /// the oversized state directly.
    #[test]
    fn batch_size_bound_is_enforced() {
        let full = PromptIntake::analyze(
            &(0..MAX_ADJUDICATION_BATCH)
                .map(|i| format!("Choose the smallest fix for module-{i}."))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        assert_eq!(
            full.adjudication_candidates().len(),
            MAX_ADJUDICATION_BATCH,
            "a full batch is still adjudicable"
        );
        assert!(full.apply_adjudications(&[]).is_ok());

        let oversized = full.with_extra_pending_candidates(1);
        let verdicts = (1..=oversized.adjudication_candidates().len())
            .map(|id| AdjudicationVerdict {
                decision_id: id,
                delegated_to_agent: true,
                assumption: "delegated".to_string(),
            })
            .collect::<Vec<_>>();
        let refusal = oversized
            .apply_adjudications(&verdicts)
            .expect_err("an oversized batch is refused");
        assert!(matches!(
            refusal,
            AdjudicationRefusal::BatchTooLarge { candidates, bound }
                if bound == MAX_ADJUDICATION_BATCH && candidates == MAX_ADJUDICATION_BATCH + 1
        ));
        assert_eq!(oversized.authorized_assumption_count(), 0);
    }

    /// Every failure mode fails closed: the candidates stay pending and the
    /// operator is asked.
    #[tokio::test]
    async fn malformed_or_failed_adjudication_fails_closed() {
        let intake = PromptIntake::analyze(DELEGATED);
        let mut adjudicators: Vec<Summarizer> = vec![
            failing_summarizer(),
            summarizer("I think you should probably just pick the smallest one."),
            summarizer("[{\"decision_id\": \"one\", \"delegated_to_agent\": true}]"),
            summarizer("[{\"delegated_to_agent\": true, \"assumption\": \"no id\"}]"),
            summarizer("[truncated"),
            summarizer(""),
        ];
        for adjudicator in adjudicators.drain(..) {
            let resolved = adjudicate_decisions(&intake, &adjudicator).await;
            assert_eq!(
                resolved.manifest().pending_decision_count(),
                1,
                "a failed adjudication must leave the decision pending"
            );
            assert_eq!(resolved.disposition(), PromptDisposition::Ask);
            assert_eq!(resolved.authorized_assumption_count(), 0);
        }
    }

    /// The oversized batch never reaches the model at all.
    #[tokio::test]
    async fn an_oversized_batch_skips_the_side_call() {
        let intake = PromptIntake::analyze("Choose the smallest coherent fix.")
            .with_extra_pending_candidates(MAX_ADJUDICATION_BATCH);
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = calls.clone();
        let adjudicator: Summarizer = Box::new(move |_prompt: String| {
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move { Ok("[]".to_string()) }) as super::super::compress::SummarizeFuture
        });
        let resolved = adjudicate_decisions(&intake, &adjudicator).await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(resolved.disposition(), PromptDisposition::Ask);
    }

    /// Adjudication is one tool-less completion: the request carries the
    /// candidates and nothing else — no tool catalog, no transcript, no
    /// repository content — and the seam itself (a `String -> String`
    /// summarizer) has no capability to call a tool.
    #[test]
    fn adjudication_has_no_tool_capability() {
        let intake = PromptIntake::analyze(DELEGATED);
        let prompt = build_adjudication_prompt(&intake);
        assert!(prompt.contains(DELEGATED), "{prompt}");
        for forbidden in [
            "tool",
            "shell",
            "bash",
            "write_file",
            "read_file",
            "network",
            "http",
        ] {
            assert!(
                !prompt.to_ascii_lowercase().contains(forbidden),
                "the adjudication prompt must advertise no capability, found {forbidden:?}: {prompt}"
            );
        }
    }

    /// A fenced reply is the one incidental deviation tolerated; prose is not.
    #[test]
    fn only_fenced_json_is_tolerated() {
        let fenced =
            "```json\n[{\"decision_id\":1,\"delegated_to_agent\":true,\"assumption\":\"x\"}]\n```";
        assert_eq!(
            parse_adjudication_reply(fenced).map(|v| v.len()),
            Some(1),
            "{fenced}"
        );
        assert!(parse_adjudication_reply("no json here").is_none());
    }
}
