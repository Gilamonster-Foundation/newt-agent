use super::*;
use newt_core::PlanDraftSink as _;

/// #2424: the pure revision counter, exercised without any filesystem I/O
/// (the fully-mocked unit tier's rule — the actual `std::fs::write` in
/// `TurnPlanDraftSink::save_draft` is an integration-tier concern, not
/// covered here).
#[test]
fn revision_starts_at_one_and_increments_from_the_prior_draft() {
    assert_eq!(
        next_plan_draft_revision(None),
        1,
        "the first save is revision 1"
    );
    let three = newt_core::agentic::PlanDraft {
        revision: 3,
        markdown: String::new(),
    };
    assert_eq!(
        next_plan_draft_revision(Some(&three)),
        4,
        "revision N+1 follows revision N, never resets"
    );
}

/// `latest_draft()` reads back exactly what is held, and `clear()` (the
/// `/new` / restore / persona-rotation boundary handler) drops it — a fresh
/// conversation must never inherit a stale draft. Manipulates the private
/// `latest` field directly (same-crate visibility) instead of routing
/// through `save_draft`, so this stays filesystem-free.
#[test]
fn latest_draft_round_trips_and_clear_drops_it() {
    let state = PlanDraftState::default();
    assert!(
        state.bind("conv-1").latest_draft().is_none(),
        "a fresh state has no draft"
    );

    let draft = newt_core::agentic::PlanDraft {
        revision: 2,
        markdown: "# Refactor plan".to_string(),
    };
    *state.latest.lock().unwrap() = Some(draft.clone());

    assert_eq!(state.bind("conv-1").latest_draft(), Some(draft));

    state.clear();
    assert!(
        state.bind("conv-1").latest_draft().is_none(),
        "clear() must drop the draft, matching PlanModeState's own clear()"
    );
}

/// The turn-end presentation hook shows a revision exactly once: a second
/// call with no new save in between returns `None`, and a genuinely new
/// revision is offered again. Drives revisions by writing `latest` directly
/// (same-crate visibility) rather than through `save_draft`, which performs
/// a real filesystem write the fully-mocked unit tier must never touch.
#[test]
fn take_for_presentation_shows_each_revision_exactly_once() {
    let state = PlanDraftState::default();
    assert!(
        state.take_for_presentation().is_none(),
        "nothing to present before any draft exists"
    );

    *state.latest.lock().unwrap() = Some(newt_core::agentic::PlanDraft {
        revision: 1,
        markdown: "# v1".to_string(),
    });
    let shown = state
        .take_for_presentation()
        .expect("a fresh draft to show");
    assert_eq!(shown.revision, 1);
    assert!(
        state.take_for_presentation().is_none(),
        "the same revision must never be presented twice"
    );

    *state.latest.lock().unwrap() = Some(newt_core::agentic::PlanDraft {
        revision: 2,
        markdown: "# v2".to_string(),
    });
    let shown_again = state
        .take_for_presentation()
        .expect("a genuinely new revision is offered again");
    assert_eq!(shown_again.revision, 2);
}

/// `ConversationModeStates::clear()` must clear the draft alongside the
/// Plan-mode flag and the Auto-mode selection — the whole point of bundling
/// them (per the struct's own doc comment) is that no boundary handler can
/// clear one and forget the others.
#[test]
fn conversation_mode_states_clear_drops_the_plan_draft_too() {
    let states = ConversationModeStates::default();
    *states.plan_draft.latest.lock().unwrap() = Some(newt_core::agentic::PlanDraft {
        revision: 1,
        markdown: "# Draft".to_string(),
    });

    states.clear();

    assert!(states.plan_draft.bind("conv-1").latest_draft().is_none());
}
