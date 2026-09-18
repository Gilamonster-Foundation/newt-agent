use super::*;
use newt_core::PlanDraftSink as _;
use std::sync::Mutex;

fn objective(text: &str) -> newt_core::TurnPromptContext {
    newt_core::TurnPromptContext::ephemeral_operator("conv-1", text, text)
}

fn root(ctx: &newt_core::TurnPromptContext) -> newt_core::PromptId {
    ctx.active().root_prompt_id()
}

/// In-memory stand-in for the one fs write the seam performs: records every
/// (path, markdown) pair. The real function is grounded by
/// `persist_plan_to_disk_writes_the_file` in the real-resource tier.
fn recording_persist(
    log: &Mutex<Vec<(std::path::PathBuf, String)>>,
) -> impl Fn(&std::path::Path, &str) -> std::io::Result<()> + Sync + '_ {
    move |path, markdown| {
        log.lock()
            .unwrap()
            .push((path.to_path_buf(), markdown.to_string()));
        Ok(())
    }
}

fn failing_persist(_: &std::path::Path, _: &str) -> std::io::Result<()> {
    Err(std::io::Error::other("disk full"))
}

/// #2424: revisions count per objective, starting at 1, and restart at 1
/// when the objective changes — the slot belongs to one objective.
#[test]
fn revision_counts_per_objective() {
    let a = root(&objective("A"));
    let b = root(&objective("B"));
    assert_eq!(next_plan_draft_revision(None, a), 1);
    let held = ObjectiveDraft {
        objective: a,
        draft: newt_core::agentic::PlanDraft {
            revision: 3,
            markdown: String::new(),
        },
    };
    assert_eq!(next_plan_draft_revision(Some(&held), a), 4);
    assert_eq!(
        next_plan_draft_revision(Some(&held), b),
        1,
        "a new objective starts its own slot"
    );
}

/// `save_draft` persists through the injected writer (exact path and bytes
/// observed), `latest_draft` reads back only under the same objective, and
/// `clear()` (the `/new` / restore / persona-rotation boundary) drops it.
#[test]
fn save_persists_through_the_seam_and_is_scoped_to_its_objective() {
    let log = Mutex::new(Vec::new());
    let persist = recording_persist(&log);
    let state = PlanDraftState::default();
    let a = objective("A");
    let sink_a = state.bind("conv-1", Some(root(&a)), &persist);
    assert!(
        sink_a.latest_draft().is_none(),
        "a fresh state has no draft"
    );

    assert_eq!(sink_a.save_draft("# v1".into()), Ok(1));
    assert_eq!(sink_a.save_draft("# v2".into()), Ok(2));
    let writes = log.lock().unwrap().clone();
    assert_eq!(writes.len(), 2);
    assert_eq!(writes[1].0, newt_core::session_plan_path("conv-1"));
    assert_eq!(writes[1].1, "# v2");
    assert_eq!(
        sink_a.latest_draft().map(|d| d.markdown),
        Some("# v2".to_string())
    );

    let b = objective("B");
    let sink_b = state.bind("conv-1", Some(root(&b)), &persist);
    assert!(
        sink_b.latest_draft().is_none(),
        "objective B must not see A's draft"
    );

    state.clear();
    assert!(sink_a.latest_draft().is_none());
}

/// A write failure is reported to the model as the tool error and the slot
/// is left as it was — a draft the disk never received is not "saved".
#[test]
fn a_persist_failure_is_an_error_and_leaves_the_slot_untouched() {
    let state = PlanDraftState::default();
    let a = objective("A");
    let sink = state.bind("conv-1", Some(root(&a)), &failing_persist);
    let err = sink.save_draft("# v1".into()).unwrap_err();
    assert!(err.contains("disk full"), "{err}");
    assert!(sink.latest_draft().is_none());
    assert!(state.take_for_presentation(root(&a)).is_none());
}

/// A turn with no receipt has nothing to bind a draft to; the sink refuses
/// rather than binding it to nothing (which a later objective could inherit).
#[test]
fn no_objective_refuses_to_save() {
    let log = Mutex::new(Vec::new());
    let persist = recording_persist(&log);
    let state = PlanDraftState::default();
    let sink = state.bind("conv-1", None, &persist);
    assert!(sink.save_draft("# v1".into()).is_err());
    assert!(log.lock().unwrap().is_empty(), "nothing was written");
}

/// The turn-end presentation hook shows a revision exactly once per
/// objective: a second call with no new save returns `None`, a new revision
/// is offered again, and the snapshot carries a content id.
#[test]
fn take_for_presentation_shows_each_revision_exactly_once() {
    let log = Mutex::new(Vec::new());
    let persist = recording_persist(&log);
    let state = PlanDraftState::default();
    let a = objective("A");
    let sink = state.bind("conv-1", Some(root(&a)), &persist);
    assert!(state.take_for_presentation(root(&a)).is_none());

    sink.save_draft("# v1".into()).unwrap();
    let shown = state
        .take_for_presentation(root(&a))
        .expect("a fresh draft to show");
    assert_eq!(shown.draft.revision, 1);
    assert_eq!(
        shown,
        newt_core::agentic::PresentedPlan::new(shown.draft.clone()),
        "the snapshot id is content-addressed over the shown bytes"
    );
    assert!(
        state.take_for_presentation(root(&a)).is_none(),
        "the same revision must never be presented twice"
    );

    sink.save_draft("# v2".into()).unwrap();
    let again = state
        .take_for_presentation(root(&a))
        .expect("a genuinely new revision is offered again");
    assert_eq!(again.draft.revision, 2);
}

/// The steer's binding regression: A is drafted and presented; an unrelated
/// objective B runs and ends without a B draft. Nothing is presented under
/// B, and an approval under B finds NO plan — A's draft cannot become B's
/// implementation input. A's snapshot is still there for A itself.
#[test]
fn a_draft_presented_under_a_is_neither_shown_nor_approved_under_b() {
    let log = Mutex::new(Vec::new());
    let persist = recording_persist(&log);
    let state = PlanDraftState::default();
    let a = objective("A");
    state
        .bind("conv-1", Some(root(&a)), &persist)
        .save_draft("# plan A".into())
        .unwrap();
    assert!(state.take_for_presentation(root(&a)).is_some());

    let b = objective("B");
    assert!(state.take_for_presentation(root(&b)).is_none());
    assert!(
        state.take_approved(root(&b)).is_none(),
        "approval under B must not seed A's plan"
    );
    assert_eq!(
        state.take_approved(root(&a)).map(|p| p.draft.markdown),
        Some("# plan A".to_string()),
        "A's own approval still sees A's snapshot"
    );
}

/// A discussion continuation shares the objective root, so the presented
/// snapshot survives it: a later approval (no new draft) seeds the plan the
/// operator actually saw. And approval consumes it — a second approval
/// finds nothing, so one "yes" seeds at most one implementation turn.
#[test]
fn a_continuation_keeps_the_snapshot_and_approval_consumes_it_once() {
    let log = Mutex::new(Vec::new());
    let persist = recording_persist(&log);
    let state = PlanDraftState::default();
    let a = objective("A");
    state
        .bind("conv-1", Some(root(&a)), &persist)
        .save_draft("# plan A".into())
        .unwrap();
    let shown = state.take_for_presentation(root(&a)).unwrap();

    let discuss =
        newt_core::TurnPromptContext::ephemeral_operator_continuation("conv-1", "why?", "why?", &a)
            .unwrap();
    assert_eq!(root(&discuss), root(&a), "continuation keeps the lineage");
    assert!(
        state.take_for_presentation(root(&discuss)).is_none(),
        "no new revision: nothing re-presented"
    );
    let approved = state
        .take_approved(root(&discuss))
        .expect("the shown snapshot");
    assert_eq!(
        approved, shown,
        "exactly the presented snapshot is approved"
    );
    assert!(
        state.take_approved(root(&discuss)).is_none(),
        "a second approval cannot consume it again"
    );
}

/// `ConversationModeStates::clear()` must clear the draft AND the presented
/// snapshot alongside the Plan-mode flag — the whole point of bundling them
/// is that no boundary handler (`/new`, restore, persona) can clear one and
/// forget the others, leaving a snapshot for a later objective to approve.
#[test]
fn conversation_mode_states_clear_drops_the_plan_draft_and_snapshot_too() {
    let log = Mutex::new(Vec::new());
    let persist = recording_persist(&log);
    let states = ConversationModeStates::default();
    let a = objective("A");
    states
        .plan_draft
        .bind("conv-1", Some(root(&a)), &persist)
        .save_draft("# Draft".into())
        .unwrap();
    assert!(states.plan_draft.take_for_presentation(root(&a)).is_some());

    states.clear();

    assert!(states
        .plan_draft
        .bind("conv-1", Some(root(&a)), &persist)
        .latest_draft()
        .is_none());
    assert!(states.plan_draft.take_approved(root(&a)).is_none());
}

/// Real-resource tier: grounds the injected writer above by running the
/// production `persist_plan_to_disk` against a real directory — the parent
/// is created and the bytes land. Weekly/release lane, single-threaded, per
/// the repo's real-resource convention.
#[test]
#[ignore = "real-fs tier; weekly/release lanes run with --ignored --test-threads=1"]
#[serial_test::serial(real_fs)]
fn persist_plan_to_disk_writes_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sessions").join("s1").join("plan.md");
    persist_plan_to_disk(&path, "# real").unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "# real");
    persist_plan_to_disk(&path, "# real v2").unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "# real v2");
}
