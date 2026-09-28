//! F34 (NOTES-2483): a reasoning overflow (`finish_reason=length`, no content,
//! no tool call) re-dispatches once with thinking off (same budget) and a concise
//! nudge. Another retry requires intervening progress; consecutive empty
//! responses end the turn with one visible reason line.

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
    let _settings = default_loop_settings();
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

/// A successful recovery can perform useful work before a later, independent
/// output overflow. The retry outcome must describe its own request, and a
/// fresh observation must not be confused with consecutive empty retries.
async fn separated_overflow_episodes(
    max_rounds: usize,
    later_read: Option<&'static str>,
    supports_thinking_off: bool,
    cancel_later_overflow: bool,
) -> Run {
    let _settings = default_loop_settings();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("observation.txt"),
        "fresh observation\n",
    )
    .unwrap();
    let server = MockServer::start().await;
    let round = Arc::new(AtomicUsize::new(0));
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let response_cancel = Arc::clone(&cancel);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |_: &Request| {
            let index = round.fetch_add(1, Ordering::SeqCst);
            if index == 2 && cancel_later_overflow {
                response_cancel.store(true, Ordering::SeqCst);
            }
            let (finish, message) = match index {
                0 | 2 => ("length", serde_json::json!({
                    "role": "assistant", "content": null,
                    "reasoning_content": "unfinished reasoning"
                })),
                4 if later_read.is_some() => ("length", serde_json::json!({
                    "role": "assistant", "content": null,
                    "reasoning_content": "unfinished reasoning"
                })),
                1 => ("tool_calls", serde_json::json!({
                    "role": "assistant", "content": "",
                    "tool_calls": [{"id":"read_observation", "type":"function",
                        "function":{"name":"read_file", "arguments":"{\"path\":\"observation.txt\"}"}}]
                })),
                3 if later_read.is_some() => ("tool_calls", serde_json::json!({
                    "role": "assistant", "content": "",
                    "tool_calls": [{"id":"read_again", "type":"function",
                        "function":{"name":"read_file", "arguments":serde_json::json!({
                            "path":later_read.unwrap()
                        }).to_string()}}]
                })),
                _ => ("stop", serde_json::json!({
                    "role":"assistant", "content":"concise answer"
                })),
            };
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices":[{"finish_reason":finish, "message":message}],
                "usage":{"prompt_tokens":24, "completion_tokens":8}
            }))
        })
        .mount(&server)
        .await;
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut observation = crate::agentic::observability::SolveObservation::default();
    let mut events = Vec::new();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.workspace = workspace.path().to_str().unwrap();
    c.cognition = Some(Cognition::Thoughtful);
    c.chat_completions_capability.cognition = Some(true);
    c.chat_completions_capability.chat_template_kwargs = supports_thinking_off.then_some(true);
    c.max_tool_rounds = max_rounds;
    c.workflow_grace_rounds = 0;
    c.action_nudges = false;
    c.solve_obs = Some(&mut observation);
    c.tool_events = Some(&mut events);
    c.cancel = Some(&cancel);
    let (reply, _, _, _) = chat_complete(c, &mut NoMcp).await.expect("turn ends");
    if supports_thinking_off {
        assert_eq!(events[0].tool, "read_file");
        assert!(events[0].ok, "the recovered observation is successful");
        if events.len() == 2 {
            assert_eq!(events[1].tool, "read_file");
            assert_eq!(events[1].ok, later_read == Some("observation.txt"));
        }
    }
    let bodies = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(body_json)
        .collect();
    Run {
        replies: vec![reply],
        bodies,
        signals: observation.behavior_signals,
    }
}

#[tokio::test]
async fn a_later_overflow_does_not_misreport_a_successful_retry_as_empty() {
    let r = separated_overflow_episodes(3, None, true, false).await;
    assert_eq!(
        r.bodies.len(),
        3,
        "the explicit zero-grace cap remains hard"
    );
    assert_eq!(
        r.bodies[1]["chat_template_kwargs"]["enable_thinking"],
        false
    );
    assert_eq!(r.bodies[2]["chat_template_kwargs"]["enable_thinking"], true);
    assert!(
        !r.replies[0].contains("retry without thinking was also empty"),
        "the off-policy request succeeded with a read; this is a later overflow: {}",
        r.replies[0]
    );
}

#[tokio::test]
async fn fresh_tool_progress_allows_a_new_bounded_overflow_recovery() {
    let r = separated_overflow_episodes(5, None, true, false).await;
    assert_eq!(
        r.bodies.len(),
        4,
        "one retry for each separated overflow episode"
    );
    assert_eq!(r.replies, ["concise answer"]);
    for (body, thinking) in r.bodies.iter().zip([true, false, true, false]) {
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], thinking);
        assert_eq!(
            max_tokens(body),
            10_000,
            "every attempt keeps the original allowance"
        );
    }
    assert_eq!(drops(&r).len(), 2);
}

#[tokio::test]
async fn repeated_or_failed_reads_do_not_rearm_overflow_recovery() {
    for later_read in ["observation.txt", "missing.txt"] {
        let r = separated_overflow_episodes(8, Some(later_read), true, false).await;
        assert_eq!(r.bodies.len(), 5, "no third retry for {later_read}");
        assert_eq!(drops(&r).len(), 2);
        assert!(!r.replies[0].contains("retry without thinking was also empty"));
    }
}

#[tokio::test]
async fn cancellation_prevents_a_new_overflow_retry() {
    let r = separated_overflow_episodes(5, None, true, true).await;
    assert_eq!(r.bodies.len(), 3, "cancellation prevents the next request");
    assert!(!r.replies[0].contains("retry without thinking was also empty"));
}

#[tokio::test]
async fn unsupported_thinking_switch_does_not_attempt_a_noop_retry() {
    let r = separated_overflow_episodes(5, None, false, false).await;
    assert_eq!(
        r.bodies.len(),
        1,
        "no supported wire switch to disable thinking"
    );
    assert!(drops(&r).is_empty());
    assert!(!r.replies[0].contains("retry without thinking was also empty"));
}
