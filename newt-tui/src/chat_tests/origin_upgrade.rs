use super::*;

fn ctx() -> newt_core::TurnPromptContext {
    newt_core::TurnPromptContext::ephemeral_operator(
        "conv",
        b"extract the module and open a PR".to_vec(),
        b"extract the module and open a PR".to_vec(),
    )
}

#[test]
fn bare_continue_after_round_cap_links_to_the_interrupted_objective() {
    let parent = ctx();
    let got = upgrade_origin_for_interrupted_objective(
        ModelInputOrigin::Operator,
        "continue",
        Some(&parent),
    );
    match got {
        ModelInputOrigin::OperatorContinuation { parent: linked } => assert_eq!(
            linked.submitted_prompt().id(),
            parent.submitted_prompt().id(),
            "the nudge must re-enter the interrupted objective's lineage"
        ),
        other => panic!("bare continue must link, got {other:?}"),
    }
}

#[test]
fn substantive_input_stays_fresh_even_with_an_interrupted_objective() {
    let parent = ctx();
    let got = upgrade_origin_for_interrupted_objective(
        ModelInputOrigin::Operator,
        "now refactor newt-tui/src/lib.rs instead and open a PR",
        Some(&parent),
    );
    assert!(
        matches!(got, ModelInputOrigin::Operator),
        "a new ask must never be silently chained to a stale objective"
    );
}

#[test]
fn no_interrupted_objective_means_no_upgrade() {
    let got =
        upgrade_origin_for_interrupted_objective(ModelInputOrigin::Operator, "continue", None);
    assert!(matches!(got, ModelInputOrigin::Operator));
}

#[test]
fn pending_clarification_continuations_are_left_untouched() {
    let parent = ctx();
    let pending = ModelInputOrigin::OperatorContinuation {
        parent: Box::new(ctx()),
    };
    let before_id = match &pending {
        ModelInputOrigin::OperatorContinuation { parent } => parent.submitted_prompt().id(),
        _ => unreachable!(),
    };
    let got = upgrade_origin_for_interrupted_objective(pending, "continue", Some(&parent));
    match got {
        ModelInputOrigin::OperatorContinuation { parent: kept } => assert_eq!(
            kept.submitted_prompt().id(),
            before_id,
            "a pending-clarification link outranks the round-cap link"
        ),
        other => panic!("existing continuation must be preserved, got {other:?}"),
    }
}

#[test]
fn durable_substantive_operator_prompt_consumes_the_round_cap_link() {
    let mut interrupted = Some(ctx());
    consume_interrupted_objective_for_accepted_prompt(
        &mut interrupted,
        &ModelInputOrigin::Operator,
    );
    assert!(
        interrupted.is_none(),
        "a fresh accepted objective must not leave the old cap link armed"
    );
}

#[test]
fn accepted_continuations_and_harness_retries_keep_the_round_cap_link() {
    for origin in [
        ModelInputOrigin::OperatorContinuation {
            parent: Box::new(ctx()),
        },
        ModelInputOrigin::HarnessRetry {
            parent: Box::new(ctx()),
        },
    ] {
        let mut interrupted = Some(ctx());
        consume_interrupted_objective_for_accepted_prompt(&mut interrupted, &origin);
        assert!(
            interrupted.is_some(),
            "continuations and derived input must preserve the objective link"
        );
    }
}

#[test]
fn accepted_web_objective_consumes_the_old_round_cap_link() {
    let mut interrupted = Some(ctx());
    consume_interrupted_objective_for_accepted_prompt(
        &mut interrupted,
        &ModelInputOrigin::WebInjected {
            inbox_id: "inbox".to_string(),
        },
    );
    assert!(interrupted.is_none());
}

#[test]
fn round_cap_footer_is_deterministic_and_only_decorates_capped_replies() {
    let reply = "Completed the parser; tests remain.";
    let capped = decorate_round_cap_reply(reply, Some(newt_core::TurnEndReason::RoundCap));
    assert!(capped.starts_with(reply), "{capped}");
    assert!(capped.contains("If work remains"), "{capped}");
    assert!(capped.contains("`continue`"), "{capped}");
    assert!(capped.contains("`/rounds <n>`"), "{capped}");
    assert_eq!(
        decorate_round_cap_reply(reply, None),
        reply,
        "ordinary replies must remain byte-for-byte unchanged"
    );
}

#[test]
fn capped_progress_is_persistable_without_duplicate_notices_and_resumes_its_objective() {
    let parent = ctx();
    let core_handoff = "Progress captured.\n\nPaused at the tool-round limit (40 rounds).";
    let persisted =
        decorate_round_cap_reply(core_handoff, Some(newt_core::TurnEndReason::RoundCap));
    assert_eq!(
        persisted.matches("tool-round limit").count(),
        1,
        "the TUI adds only the interactive affordance: {persisted}"
    );
    assert_eq!(persisted.matches('⏸').count(), 1, "{persisted}");
    assert!(persisted.contains("`continue`"), "{persisted}");

    let resumed = upgrade_origin_for_interrupted_objective(
        ModelInputOrigin::Operator,
        "continue",
        Some(&parent),
    );
    match resumed {
        ModelInputOrigin::OperatorContinuation { parent: linked } => assert_eq!(
            linked.submitted_prompt().id(),
            parent.submitted_prompt().id(),
            "the persisted capped turn must resume the interrupted objective"
        ),
        other => panic!("capped progress must resume as a continuation, got {other:?}"),
    }
}
