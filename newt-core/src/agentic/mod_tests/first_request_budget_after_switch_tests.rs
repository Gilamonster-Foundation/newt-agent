use super::*;

fn est() -> crate::tokens::TokenEstimation {
    crate::tokens::TokenEstimation::default() // chars / 4
}
fn system(content: &str) -> serde_json::Value {
    serde_json::json!({"role": "system", "content": content})
}
fn user(content: &str) -> serde_json::Value {
    serde_json::json!({"role": "user", "content": content})
}

/// ISSUE 3 — large → small. After a switch to a small-context model, the
/// FIRST request is gated by the SMALL budget BEFORE it leaves the process:
/// a history sized for a 128K model is REFUSED (not dispatched, so there is
/// no "send oversized → 400 → learn → retry") against a 32K-class budget,
/// and the same turn compacted to the small budget then passes. Exercises
/// `preflight_full_message_request` — the Chat-side gate immediately before
/// dispatch.
#[test]
fn a_large_history_is_refused_before_dispatch_against_a_small_budget() {
    let big = "x ".repeat(50_000); // ~100k chars ≈ 25k tokens (fit for 128K)
    let messages = vec![system("sys"), user(&big)];
    let small_budget = Some(8_000usize); // a 32K model's input budget after reserves

    let refused =
        preflight_full_message_request(&messages, None, small_budget, 1.0, est(), "small-32k");
    assert!(
        refused.is_err(),
        "an oversized first request must be refused before dispatch, not sent"
    );
    assert!(refused
        .unwrap_err()
        .to_string()
        .contains("refusing before inference dispatch"));

    // The SAME turn compacted to fit the small budget dispatches cleanly.
    let compacted = vec![system("sys"), user("recent question")];
    assert!(
        preflight_full_message_request(&compacted, None, small_budget, 1.0, est(), "small-32k")
            .is_ok(),
        "a request within the small budget must pass"
    );
}

/// The fail-open hole the rebudget must close: with NO authoritative budget
/// the gate is a no-op (Ok). The large→small fix's job is precisely to make
/// the budget `Some` (from the target model's known/declared window) so the
/// gate above actually fires. This pins the contrast so a regression that
/// lets the budget collapse to `None` after a switch is caught.
#[test]
fn a_none_budget_fails_open_so_a_switch_must_supply_the_target_window() {
    let big = "x ".repeat(50_000);
    let messages = vec![system("sys"), user(&big)];
    assert!(
        preflight_full_message_request(&messages, None, None, 1.0, est(), "unknown").is_ok(),
        "None budget is fail-open — the switch MUST resolve the target window to Some"
    );
}

/// ISSUE 4 — impossible target. When the MANDATORY, non-compactable context
/// (the protected system head + newest live user + advertised tool schemas)
/// alone exceeds the target model's window, the transition is refused
/// CLEANLY before inference: a useful error, the operator prompt NOT
/// truncated, no doomed dispatch, and a SINGLE-SHOT `Err` (no compaction
/// loop, because non-compactable material cannot shrink).
#[test]
fn an_impossible_mandatory_context_is_refused_cleanly_before_inference() {
    let huge_system = "S ".repeat(60_000); // ~30k tokens of non-compactable head
    let messages = vec![system(&huge_system), user("hi")];
    let tiny_budget = Some(8_000usize);

    let refused =
        preflight_irreducible_request(&messages, None, tiny_budget, 1.0, est(), "tiny-16k");
    assert!(
        refused.is_err(),
        "mandatory context larger than the window must refuse"
    );
    let msg = refused.unwrap_err().to_string();
    assert!(
        msg.contains("cannot fit"),
        "must explain it cannot fit: {msg}"
    );
    assert!(msg.contains("refusing before inference dispatch"));
    assert!(
        msg.contains("was not truncated"),
        "must promise the operator prompt is intact: {msg}"
    );

    // The refusal is not blanket: a mandatory context that DOES fit passes.
    let ok_messages = vec![system("small system"), user("hi")];
    assert!(
        preflight_irreducible_request(&ok_messages, None, tiny_budget, 1.0, est(), "tiny-16k")
            .is_ok(),
        "a mandatory context within the window must pass"
    );
}
