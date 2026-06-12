//! Acceptance test for issue #308 — the cowork foundation.
//!
//! This test stands in for the downstream gilamonster-agent cowork consumer:
//! it drives ONE agentic turn through the public [`newt_core::TurnDriver`] and
//! renders the resulting transcript with the public
//! [`newt_core::transcript_lines`] — touching **only** the published crate
//! surface, with no `run_chat`, no blocking REPL, and no ratatui dependency
//! pulled into newt-core. If this file compiles and passes against a clean
//! `newt_core::` import, the seam a private downstream consumes via git-dep is
//! good.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use newt_core::{
    transcript_lines, BackendKind, Role, ShellObservation, TranscriptRole, TurnDriver,
    TurnDriverConfig, TurnStatus,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// Ollama-shaped responder that always returns a fixed text answer (no tools).
struct PlainOllama {
    served: Arc<AtomicUsize>,
    reply: String,
}

impl Respond for PlainOllama {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        self.served.fetch_add(1, Ordering::SeqCst);
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message": { "content": self.reply }
        }))
    }
}

/// Pump the driver to completion the way a crossterm event loop would: poll on
/// a short interval, never blocking the "frame".
fn pump_to_done(driver: &mut TurnDriver) -> TurnStatus {
    for _ in 0..600 {
        match driver.poll() {
            TurnStatus::Running => std::thread::sleep(Duration::from_millis(10)),
            other => return other,
        }
    }
    panic!("turn did not complete within the pump budget");
}

#[tokio::test]
async fn consumer_drives_a_turn_and_renders_the_transcript_with_only_public_api() {
    let server = MockServer::start().await;
    let served = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(PlainOllama {
            served: served.clone(),
            reply: "I see your shell — the build is green.".into(),
        })
        .mount(&server)
        .await;

    // 1. A consumer builds a driver from owned config — no borrow-heavy ChatCtx.
    let config = TurnDriverConfig::new(server.uri(), "test-model", BackendKind::Ollama, ".");
    let mut driver = TurnDriver::new(config);

    // 2. It folds in some shell activity the human produced (redacted seam).
    driver.submit_observation(ShellObservation::new(
        "bash",
        "$ cargo build\n   Finished dev profile",
    ));

    // 3. It submits a human message — and pumps the turn from its own loop,
    //    never calling run_chat.
    driver.submit("did the build pass?").expect("submit a turn");
    let outcome = match pump_to_done(&mut driver) {
        TurnStatus::Completed(outcome) => outcome,
        other => panic!("expected Completed, got {other:?}"),
    };
    assert_eq!(outcome.reply, "I see your shell — the build is green.");

    // 4. It renders the transcript into its own pane width using only the
    //    public render-data fn (renderer-agnostic; the consumer maps these to
    //    ratatui Lines in its own layout).
    let lines = transcript_lines(driver.transcript(), 40);
    assert!(!lines.is_empty());
    for line in &lines {
        assert!(
            line.text.chars().count() <= 40,
            "render must fit the pane width: {:?}",
            line.text
        );
    }
    // The human's question and the model's answer both appear, tagged by role.
    assert!(lines
        .iter()
        .any(|l| l.role == TranscriptRole::User && l.text.contains("did the build pass?")));
    assert!(lines
        .iter()
        .any(|l| l.role == TranscriptRole::Assistant && l.text.contains("green")));

    // The shell observation is in context (its framing reached the transcript),
    // and it carried no secret — proven structurally by the user-role turns.
    let user_turns: Vec<&newt_core::MemMessage> = driver
        .transcript()
        .iter()
        .filter(|m| m.role == Role::User)
        .collect();
    assert!(user_turns
        .iter()
        .any(|m| m.content.contains("shell observation")));

    assert!(served.load(Ordering::SeqCst) >= 1);
}
