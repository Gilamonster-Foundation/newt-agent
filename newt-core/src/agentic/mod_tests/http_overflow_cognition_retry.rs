//! F34 (NOTES-2483): a reasoning overflow (`finish_reason=length`, no content,
//! no tool call) re-dispatches ONCE with thinking off (same budget) and a concise
//! nudge; a second overflow ends the turn with one visible reason line.

use super::*;
use crate::role_profile::Cognition;

/// Replies overflow for the first `overflow_rounds` requests, then answers.
/// Captures every request body.
struct Responder {
    round: AtomicUsize,
    overflow_rounds: usize,
    bodies: Arc<Mutex<Vec<serde_json::Value>>>,
    /// Content on the overflowing rounds (case (c): length WITH content).
    overflow_content: Option<&'static str>,
    /// Answer this request index with a tool call instead of the final reply.
    tool_call_on: Option<usize>,
}

impl Respond for Responder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        self.bodies.lock().expect("bodies").push(body_json(req));
        let round = self.round.fetch_add(1, Ordering::SeqCst);
        if round < self.overflow_rounds {
            return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{
                    "finish_reason": "length",
                    "message": {
                        "role": "assistant",
                        "content": self.overflow_content,
                        "reasoning_content": format!("unfinished plan {round}")
                    }
                }],
                "usage": {"prompt_tokens": 20, "completion_tokens": 8}
            }));
        }
        if self.tool_call_on == Some(round) {
            return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{
                    "finish_reason": "tool_calls",
                    "message": {
                        "role": "assistant",
                        "content": "",
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": {"name": "list_files", "arguments": "{}"}
                        }]
                    }
                }],
                "usage": {"prompt_tokens": 22, "completion_tokens": 3}
            }));
        }
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {"role": "assistant", "content": "concise answer"}
            }],
            "usage": {"prompt_tokens": 24, "completion_tokens": 5}
        }))
    }
}

struct Run {
    replies: Vec<String>,
    bodies: Vec<serde_json::Value>,
    signals: Vec<crate::agentic::observability::BehaviorSignal>,
}

async fn run(
    level: Option<Cognition>,
    overflow_rounds: usize,
    overflow_content: Option<&'static str>,
    turns: usize,
) -> Run {
    run_with(
        level,
        overflow_rounds,
        overflow_content,
        turns,
        None,
        Default::default(),
    )
    .await
}

async fn run_with(
    level: Option<Cognition>,
    overflow_rounds: usize,
    overflow_content: Option<&'static str>,
    turns: usize,
    tool_call_on: Option<usize>,
    overflow_retry: crate::config::OverflowRetry,
) -> Run {
    let server = MockServer::start().await;
    let bodies = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(Responder {
            round: AtomicUsize::new(0),
            overflow_rounds,
            bodies: bodies.clone(),
            overflow_content,
            tool_call_on,
        })
        .mount(&server)
        .await;
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut replies = Vec::new();
    let mut observation = crate::agentic::observability::SolveObservation::default();
    for _ in 0..turns {
        let mut c = ctx(&uri, &messages, &caveats);
        c.kind = BackendKind::Openai;
        c.cognition = level;
        c.overflow_retry = overflow_retry;
        c.chat_completions_capability.cognition = Some(true);
        c.chat_completions_capability.chat_template_kwargs = Some(true);
        c.solve_obs = Some(&mut observation);
        let (reply, _, _, _) = chat_complete(c, &mut NoMcp).await.expect("turn ends");
        replies.push(reply);
    }
    let bodies = bodies.lock().expect("bodies").clone();
    Run {
        replies,
        bodies,
        signals: observation.behavior_signals,
    }
}

fn max_tokens(body: &serde_json::Value) -> u64 {
    body["max_tokens"].as_u64().expect("max_tokens projected")
}

fn drops(run: &Run) -> Vec<(String, String)> {
    run.signals
        .iter()
        .filter_map(|s| match s {
            crate::agentic::observability::BehaviorSignal::CognitionDropRetry {
                from, to, ..
            } => Some((from.clone(), to.clone())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn empty_length_retries_once_one_level_lower_then_completes() {
    let r = run(Some(Cognition::Thoughtful), 1, None, 1).await;
    assert_eq!(r.replies, ["concise answer"]);
    assert_eq!(r.bodies.len(), 2, "exactly one retry");
    assert_eq!(max_tokens(&r.bodies[0]), 10_000);
    assert_eq!(max_tokens(&r.bodies[1]), 10_000, "same budget");
    assert_eq!(
        r.bodies[1]["chat_template_kwargs"]["enable_thinking"],
        false
    );
    assert_eq!(r.bodies[0]["chat_template_kwargs"]["enable_thinking"], true);
    let sent = r.bodies[1]["messages"].as_array().expect("messages");
    let nudge = sent.last().expect("last message").clone();
    assert_eq!(sent[sent.len() - 2]["role"], "assistant", "alternation");
    assert_eq!(nudge["role"], "user");
    assert!(nudge["content"].as_str().expect("text").contains("concise"));
    assert_eq!(
        drops(&r),
        [("thoughtful".to_string(), "thinking-off".to_string())]
    );
}

#[tokio::test]
async fn second_empty_length_ends_with_one_visible_reason() {
    let r = run(Some(Cognition::Thoughtful), 2, None, 1).await;
    assert_eq!(r.bodies.len(), 2, "no second retry");
    let reply = &r.replies[0];
    assert!(!reply.trim().is_empty());
    assert!(
        reply.contains("retry without thinking was also empty"),
        "{reply}"
    );
    assert_eq!(reply.lines().count(), 1);
}

#[tokio::test]
async fn length_with_content_is_not_retried() {
    let r = run(Some(Cognition::Thoughtful), 1, Some("partial answer"), 1).await;
    assert_eq!(r.bodies.len(), 1);
    assert!(drops(&r).is_empty());
}

#[tokio::test]
async fn lowest_level_goes_straight_to_the_reason_line() {
    let r = run(Some(Cognition::Zen), 1, None, 1).await;
    assert_eq!(r.bodies.len(), 1, "nothing lower to try");
    assert!(drops(&r).is_empty());
    assert!(r.replies[0].contains("output budget"), "{}", r.replies[0]);
}

#[tokio::test]
async fn the_next_turn_uses_the_original_level() {
    let r = run(Some(Cognition::Thoughtful), 1, None, 2).await;
    assert_eq!(r.bodies.len(), 3);
    assert_eq!(
        max_tokens(&r.bodies[2]),
        10_000,
        "drop was for one re-dispatch"
    );
}

#[tokio::test]
async fn the_round_after_the_retry_runs_the_original_policy() {
    // Overflow, then the retry answers with a tool call, then a final reply.
    let r = run_with(
        Some(Cognition::Rational),
        1,
        None,
        1,
        Some(1),
        Default::default(),
    )
    .await;
    assert_eq!(r.bodies.len(), 3);
    assert_eq!(max_tokens(&r.bodies[1]), 4_096, "same budget on the retry");
    assert_eq!(
        r.bodies[1]["chat_template_kwargs"]["enable_thinking"],
        false
    );
    assert_eq!(max_tokens(&r.bodies[2]), 4_096, "original budget restored");
    assert_eq!(r.bodies[2]["chat_template_kwargs"]["enable_thinking"], true);
}

#[tokio::test]
async fn overflow_retry_off_takes_the_old_path_with_no_retry() {
    let r = run_with(
        Some(Cognition::Thoughtful),
        1,
        None,
        1,
        None,
        crate::config::OverflowRetry::Off,
    )
    .await;
    assert_eq!(r.bodies.len(), 1, "no re-dispatch");
    assert!(drops(&r).is_empty());
    assert!(r.replies[0].contains("empty response"), "{}", r.replies[0]);
}
