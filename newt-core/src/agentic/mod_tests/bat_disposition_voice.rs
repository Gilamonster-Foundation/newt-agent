//! BAT (#2051): the greeting turn that narrated its own harness plumbing.
//!
//! Observed on `v0.8.0 (dc49a077c971-dirty)` against `llama3.1:8b` (ollama,
//! local): the operator typed `hello?`, and the model answered and then added
//! *"(Also, please note that this is an "explain" turn, so I won't be making
//! any changes to the workspace.)"*
//!
//! **What this tier can and cannot prove.** A scripted backend cannot prove a
//! 9b model will stop narrating — only a real model can, and that check belongs
//! to the weekly / release tier (see `Out of scope` in the PR). What it CAN
//! prove, deterministically and on every PR, is the harness's half of the
//! contract: that the sentences which invited the narration are gone from the
//! wire, and that the two clauses which replace them actually reach the model.
//! That is the mocked belief; the weekly real-ollama run is what grounds it.
//!
//! Replayed through the same simulated environment as the #1257 BAT
//! (`bat_largest_files::run_scenario_for`) rather than a second copy of it.

use super::bat_largest_files_tests::run_scenario_for;
use super::{PromptDisposition, PromptIntake};

/// The operator's prompt, verbatim.
const PROMPT: &str = "hello?";

/// The classification half of the observed defect, pinned so the rest of the
/// BAT is known to be replaying the turn that actually happened.
///
/// `hello?` matches no action, research, or explain needle; it reaches
/// `Explain` purely through the trailing-`?` fallback. Asserted here because
/// the wire assertions below are only meaningful for an Explain turn.
#[test]
fn the_greeting_reaches_explain_through_the_question_mark_fallback() {
    assert_eq!(
        PromptIntake::analyze(PROMPT).disposition(),
        PromptDisposition::Explain
    );
    // Not the `?`: the same greeting without one matches nothing at all and
    // falls through to the terminal `Act` arm. Pinned so a later attempt to
    // "fix the `?` cliff" cannot silently hand a greeting execution authority.
    assert_eq!(
        PromptIntake::analyze("hello").disposition(),
        PromptDisposition::Act,
        "documenting today's behaviour, NOT endorsing it — see #2051 (3)"
    );
}

#[tokio::test]
async fn the_greeting_turn_never_ships_a_sentence_to_read_aloud() {
    let ws = tempfile::tempdir().expect("tempdir");
    let (_reply, _hallucinations, _end_reason, wire) = run_scenario_for(
        PROMPT,
        PromptDisposition::Explain,
        ws.path(),
        vec![serde_json::json!({ "content": "Hello! How can I help?" })],
    )
    .await;

    // The card must say WHOSE decision this was. Silence here is what let a
    // small model read the action line as an operator-imposed rule.
    assert!(
        wire.contains("disposition_source:"),
        "the card must name the classification as the harness's own inference"
    );
    // …and that it is not part of the conversation.
    assert!(
        wire.contains("disposition_privacy:"),
        "the card must mark itself as plumbing"
    );
    // The privacy clause bounds the MECHANISM, never the substance: a model
    // that genuinely cannot do the work still owes the operator a plain answer.
    assert!(
        wire.contains("say plainly what you cannot do"),
        "the privacy clause must not become a gag on honest limits"
    );
    // The phrase the evidenced model produced almost verbatim. It lived in the
    // dispatcher's refusal string, not the card — which is why rewording the
    // card alone would have left it in place.
    assert!(
        !wire.contains("This is an Explain turn"),
        "the refusal phrasing the model read aloud is back on the wire"
    );
    // The harness used to tell the model, twice, that it had no move. Under
    // that framing narrating the cage is close to the only response left.
    assert!(
        !wire.contains("cannot widen an already accepted turn"),
        "the you-have-no-move sentence is back on the wire"
    );
}
