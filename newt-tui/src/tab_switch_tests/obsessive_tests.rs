//! Real tab transitions ground the selector-only reversible-toggle tests.
use super::*;
use newt_core::cognition::{cli_cognition, CognitionOverride};
use newt_core::initiative::{cli_initiative, Initiative};
use newt_core::tenacity::{cli_tenacity, effective_tenacity, Tenacity};

fn selectors() -> (CognitionOverride, Option<Tenacity>, Option<Initiative>) {
    (cli_cognition(), cli_tenacity(), cli_initiative())
}

fn choose(cognition: &str, tenacity: &str, initiative: &str) {
    for (field, value) in [
        (crate::settings_form::Field::Cognition, cognition),
        (crate::settings_form::Field::Tenacity, tenacity),
        (crate::settings_form::Field::Initiative, initiative),
    ] {
        crate::settings_form::apply_and_record(field, value, "/settings").unwrap();
    }
}

fn toggle(value: &str) {
    crate::commands::settings::dispatch(
        "psyche",
        "obsessive",
        &format!("/psyche obsessive {value}"),
        ".",
        false,
        false,
    )
    .unwrap();
}

/// Regression: a rowless tab retains its original selectors without creating a ghost row.
/// The real store/switch path grounds text-toggle snapshot ownership before persistence.
#[test]
fn obsessive_rowless_tab_retains_restore_selectors() {
    let _guard = guard();
    let (mut h, mut tabs) = Harness::new(&["sol"]);
    create_fresh_tab(&mut h.ctx(), &mut tabs).unwrap();
    let b = h.active_conversation_id.clone();
    assert!(!h.store.exists(&b).unwrap());
    choose("off", "auto", "eager");
    let original = selectors();
    toggle("on");
    activate_tab(&mut h.ctx(), &mut tabs, 0).unwrap();
    activate_tab(&mut h.ctx(), &mut tabs, 1).unwrap();
    assert_eq!(h.active_conversation_id, b);
    assert!(
        !h.store.exists(&b).unwrap(),
        "a toggle must not materialize a conversation"
    );
    assert_eq!(effective_tenacity(), Tenacity::Relentless);
    toggle("off");
    assert_eq!(selectors(), original);
}

/// Regression: independently enabled tabs cannot overwrite each other's inverse.
/// Real durable preference writes ground the in-memory snapshot isolation contract.
#[test]
fn obsessive_two_tabs_restore_their_own_selectors() {
    let _guard = guard();
    let (mut h, mut tabs) = Harness::new(&["sol"]);
    let a = h.active_conversation_id.clone();
    h.store.create_with_id(&a, "A", None).unwrap();
    choose("off", "auto", "patient");
    let a_original = selectors();
    toggle("on");
    let b = h.durable("B");
    h.open_tab_on(&mut tabs, &b);
    choose("rational", "resolute", "eager");
    let b_original = selectors();
    toggle("on");
    activate_tab(&mut h.ctx(), &mut tabs, 0).unwrap();
    assert_eq!(effective_tenacity(), Tenacity::Relentless);
    toggle("off");
    assert_eq!(selectors(), a_original);
    activate_tab(&mut h.ctx(), &mut tabs, 1).unwrap();
    assert_eq!(effective_tenacity(), Tenacity::Relentless);
    toggle("off");
    assert_eq!(selectors(), b_original);
}

/// Regression: closing and reopening a conversation cannot lose its original selectors.
/// The real store restore is essential: retaining only a TabSidecar would pass switch tests.
#[test]
fn obsessive_closed_tab_restores_from_durable_evidence() {
    let _guard = guard();
    let (mut h, mut tabs) = Harness::new(&["sol"]);
    let a = h.active_conversation_id.clone();
    h.store.create_with_id(&a, "A", None).unwrap();
    let b = h.durable("B");
    h.open_tab_on(&mut tabs, &b);
    choose("off", "auto", "eager");
    let original = selectors();
    toggle("on");
    close_tab(&mut h.ctx(), &mut tabs, 1).unwrap();
    assert_eq!(tabs.len(), 1);
    assert_eq!(h.active_conversation_id, a);
    h.open_tab_on(&mut tabs, &b);
    assert_eq!(effective_tenacity(), Tenacity::Relentless);
    assert!(tabs.active().pin_degraded.is_none());
    toggle("off");
    assert_eq!(selectors(), original);
}

/// Regression: /resume within a tab must not retain the outgoing conversation's inverse.
/// This exercises the actual adopt seam rather than replacing a conversation id in a model.
#[test]
fn obsessive_adopt_other_conversation_drops_outgoing_overlay() {
    let _guard = guard();
    let (mut h, mut tabs) = Harness::new(&["sol"]);
    let a = h.active_conversation_id.clone();
    h.store.create_with_id(&a, "A", None).unwrap();
    let b = h.durable("B");
    choose("off", "auto", "patient");
    toggle("on");
    assert!(matches!(
        adopt_conversation(&mut h.ctx(), &mut tabs, &b).unwrap(),
        Adopted::ProceedInActiveTab
    ));
    // The adoption seam only decides whether an existing tab owns B. Follow
    // chat's ProceedInActiveTab branch through the actual resume and pin reset.
    h.ctx().deactivate(&mut tabs);
    assert!(matches!(
        h.store.claim(&b).unwrap(),
        newt_core::ClaimOutcome::Claimed
    ));
    h.store.release(&a).unwrap();
    let mut resume = crate::ConversationCommandContext {
        store: &h.store,
        persona_store: &h.persona_store,
        workspace: &h.workspace,
        memory: &mut h.memory,
        system: &mut h.system,
        active_persona: &mut h.active_persona,
        active_conversation_id: &mut h.active_conversation_id,
        compress_state: &mut h.compress_state,
        scratchpad: &h.scratchpad,
        step_ledger: &h.step_ledger,
        active_prompt_context: &mut h.active_prompt_context,
        mode_states: &h.mode_states,
    };
    crate::resume_session_conversation(&mut resume, &b).unwrap();
    let restored = h.ctx().reset_and_overlay();
    tabs.active_mut().hold_conversation(&b);
    tabs.active_mut().pin_degraded = restored.degraded;
    assert_eq!(h.active_conversation_id, b);
    choose("rational", "normal", "eager");
    let b_before = selectors();
    let _ = newt_core::runtime::drain_preference_actions();
    toggle("off");
    assert_eq!(selectors(), b_before, "B had no posture to turn off");
    assert!(newt_core::runtime::drain_preference_actions().is_empty());
}
