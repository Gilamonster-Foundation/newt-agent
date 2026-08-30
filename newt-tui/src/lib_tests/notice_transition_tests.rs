use super::*;
use newt_core::model_card::CardApplicability as A;
use newt_core::BackendDestination;

fn active(card: &str) -> A {
    A::Active {
        card: card.to_string(),
    }
}
fn inactive_model(card: &str) -> A {
    A::InactiveModel {
        card: card.to_string(),
        bound_model: Some("bound".into()),
        active_model: "other".into(),
    }
}
fn inactive_destination(card: &str) -> A {
    A::InactiveDestination {
        card: card.to_string(),
        bound_destination: BackendDestination::new(Some("http://a:1".into()), None),
        active_destination: BackendDestination::new(Some("http://b:2".into()), None),
    }
}
/// Drive a sequence of (error, state) observations through the owner,
/// returning the lines each observation printed.
fn drive(seq: &[(Option<&str>, A)]) -> Vec<Vec<String>> {
    let mut notices = CardNotices::default();
    seq.iter()
        .map(|(error, state)| {
            let (next, lines) = notice_transition(&notices, *error, state);
            notices = next;
            lines
        })
        .collect()
}

/// The exhaustive typed transitions: replacement, removal, activation,
/// deactivation, re-activation — every change in WHICH card governs is
/// visible; unchanged identity is quiet (dedupe); startup Active is
/// deliberately quiet.
#[test]
fn every_governance_change_is_visible_and_dedupe_holds() {
    let out = drive(&[
        (None, active("a")),                       // startup Active: quiet
        (None, active("a")),                       // unchanged: dedupe, quiet
        (None, active("b")),                       // replacement a → b
        (None, A::None),                           // removal
        (None, A::None),                           // dedupe
        (None, active("a")),                       // activation from None
        (None, inactive_model("a")),               // deactivation: typed prose
        (None, inactive_model("a")),               // dedupe
        (None, active("a")),                       // re-activation
        (None, inactive_destination("a")),         // destination retarget prose
        (None, A::Undecided { card: "a".into() }), // undecided prose
    ]);
    assert!(
        out[0].is_empty(),
        "startup Active stays quiet: {:?}",
        out[0]
    );
    assert!(out[1].is_empty(), "dedupe: {:?}", out[1]);
    assert!(
        out[2][0].contains("`a`") && out[2][0].contains("`b`"),
        "replacement names both: {:?}",
        out[2]
    );
    assert!(out[3][0].contains("card removed"), "{:?}", out[3]);
    assert!(out[4].is_empty(), "dedupe after removal: {:?}", out[4]);
    assert!(out[5][0].contains("now applies"), "{:?}", out[5]);
    assert!(
        out[6][0].contains("card-derived behavior"),
        "family-neutral wording: {:?}",
        out[6]
    );
    assert!(out[7].is_empty(), "dedupe inactive: {:?}", out[7]);
    assert!(out[8][0].contains("applies again"), "{:?}", out[8]);
    assert!(out[9][0].contains("routed to"), "{:?}", out[9]);
    assert!(out[10][0].contains("not established"), "{:?}", out[10]);
}

/// A resolution error prints once per distinct message, and RECOVERY is
/// itself a visible transition — a fixed card never just silently
/// starts applying.
#[test]
fn error_lines_dedupe_and_recovery_is_visible() {
    let out = drive(&[
        (Some("card `x` — no such card"), A::None),
        (Some("card `x` — no such card"), A::None), // same error: quiet
        (None, active("x")),                        // fixed: recovery + activation
    ]);
    assert!(out[0][0].contains("no such card"), "{:?}", out[0]);
    assert!(out[1].is_empty(), "same error dedupes: {:?}", out[1]);
    assert!(
        out[2].iter().any(|l| l.contains("recovered")),
        "recovery visible: {:?}",
        out[2]
    );
    assert!(
        out[2].iter().any(|l| l.contains("now applies")),
        "activation visible: {:?}",
        out[2]
    );
}

/// The family-only inactive→active round trip renders through the SAME
/// owner — a capability-less family card's transitions are exactly as
/// visible (J): its states are the same typed identities.
#[test]
fn family_only_cards_share_the_same_visible_transitions() {
    let out = drive(&[
        (None, inactive_model("nano-team")),
        (None, active("nano-team")),
    ]);
    assert!(
        out[0][0].contains("family policy"),
        "the prose names the family contribution: {:?}",
        out[0]
    );
    assert!(out[1][0].contains("applies again"), "{:?}", out[1]);
}
