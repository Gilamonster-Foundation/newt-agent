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
    assert!(
        !wire.contains("unavailable under the current prompt disposition"),
        "the mechanism is named on the wire again"
    );
    // The harness used to tell the model, twice, that it had no move. Under
    // that framing narrating the cage is close to the only response left.
    assert!(
        !wire.contains("cannot widen an already accepted turn"),
        "the you-have-no-move sentence is back on the wire"
    );
}

/// The turn the greeting BAT above cannot exercise: a request the lexicon
/// misfiles as Explain, on which the model reaches for a write, is refused,
/// then goes looking for an execution tool. Both of those results land on the
/// model wire as tool results, which is the only place the refusal and the
/// discovery note exist — a script that answers in one round never puts either
/// there, and the negative assertions above would pass vacuously.
///
/// Observed on `llama3.1:8b` under the first version of this branch: the
/// refusal said "under the current prompt disposition" and "do not report this
/// refusal", and the model, told to stay quiet about a write it could not do,
/// reported the write as done. The wire must now carry a refusal that names no
/// mechanism and that separates what may not be quoted from what must still
/// be said.
#[tokio::test]
async fn a_refused_write_and_a_discovery_reach_the_wire_without_naming_the_mechanism() {
    let ws = tempfile::tempdir().expect("tempdir");
    let (_reply, _hallucinations, _end_reason, wire) = run_scenario_for(
        "could you add a line saying hello to README.md?",
        PromptDisposition::Explain,
        ws.path(),
        vec![
            serde_json::json!({
                "content": null,
                "tool_calls": [{
                    "id": "c1", "type": "function",
                    "function": { "name": "write_file",
                        "arguments": "{\"path\":\"README.md\",\"content\":\"hello\\n\"}" }
                }]
            }),
            serde_json::json!({
                "content": null,
                "tool_calls": [{
                    "id": "c2", "type": "function",
                    "function": { "name": "tool_search",
                        "arguments": "{\"query\":\"run command\"}" }
                }]
            }),
            serde_json::json!({ "content":
                "I can't change README.md from here, so the line was not added." }),
        ],
    )
    .await;

    // The refusal reached the model, names the tool, and says what to do
    // with the gap: not quote the notice, and not pretend the write happened.
    assert!(
        wire.contains("Tool `write_file` is not available for this request."),
        "the refusal must reach the model as a tool result: {wire}"
    );
    assert!(
        wire.contains("say plainly what remains undone; never claim it was done"),
        "the refusal must require the honest answer, not silence"
    );
    // The discovery note reached the model and still coaches the handoff.
    assert!(
        wire.contains("Catalog scope:") && wire.contains("direct action request"),
        "the discovery scope note must reach the model as a tool result"
    );
    // None of the sentences that named the mechanism, or told the model it
    // had no move, or gagged it, are on the wire in either result.
    for gone in [
        "unavailable under the current prompt disposition",
        "This is an Explain turn",
        "cannot widen an already accepted turn",
        "Do not report this refusal",
    ] {
        assert!(!wire.contains(gone), "{gone:?} is back on the wire");
    }
    // The workspace was never touched: the refusal was a refusal.
    assert!(
        !ws.path().join("README.md").exists(),
        "a refused write must not write"
    );
}
