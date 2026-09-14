use agent_harness::{Session, SessionConfig, Verdict};
use serde_json::json;

fn config() -> SessionConfig {
    SessionConfig {
        authority: "test-workspace".into(),
        ..Default::default()
    }
}

#[test]
fn observation_precedes_verdict_and_request_replays() {
    let mut session = Session::new(config()).unwrap();
    let request = session
        .record_request(
            json!({"model":"test","messages":[{"role":"user","content":"calculate 1+2"}]}),
            "openai",
        )
        .unwrap();
    assert_eq!(request.bytes, session.replay(request.id).unwrap());
    let reply = session
        .record_reply(request.id, b"The answer is three.")
        .unwrap();
    assert_eq!(session.pending_replies(), vec![reply]);
    session.record_verdict(reply, Verdict::Answer).unwrap();
    assert!(session.pending_replies().is_empty());
    assert!(session.record_verdict(reply, Verdict::Answer).is_err());
    assert!(session.record_verdict(request.id, Verdict::Answer).is_err());
}

/// Grounds the in-memory pending observation and authorization checks in a
/// closed/reopened filesystem store, including a reply with no verdict.
#[test]
fn restart_verifies_closure_and_keeps_unadjudicated_observation() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::open(dir.path(), config()).unwrap();
    let request = session
        .record_request(
            json!({"messages":[{"role":"user","content":"question"}]}),
            "openai",
        )
        .unwrap();
    let reply = session.record_reply(request.id, b"Which option?").unwrap();
    let head = session.head();
    drop(session);
    assert!(Session::restore(dir.path(), head, "foreign-workspace").is_err());
    let mut restored = Session::restore(dir.path(), head, "test-workspace").unwrap();
    assert_eq!(restored.pending_replies(), vec![reply]);
    restored.record_verdict(reply, Verdict::Question).unwrap();
    assert!(restored.pending_replies().is_empty());
    assert_eq!(restored.replay(request.id).unwrap(), request.bytes);
    let head = restored.head();
    drop(restored);
    std::fs::remove_file(dir.path().join(format!("{}.cbor", request.projection))).unwrap();
    let error = Session::restore(dir.path(), head, "test-workspace")
        .err()
        .unwrap();
    assert!(matches!(error, agent_harness::Error::Storage(_)), "{error}");
}

#[test]
fn restored_transcript_includes_final_tool_observations_and_delivered_outcome() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::open(dir.path(), config()).unwrap();
    let messages = vec![
        json!({"role":"user","content":"task"}),
        json!({"role":"assistant","tool_calls":[{"id":"call-1"}]}),
        json!({"role":"tool","tool_call_id":"call-1","content":"result"}),
    ];
    session.record_messages(&messages).unwrap();
    let request = session
        .record_request(json!({"messages":messages}), "openai")
        .unwrap();
    let reply = session.record_reply(request.id, b"answer").unwrap();
    session.record_model_message(reply, "answer").unwrap();
    session.record_verdict(reply, Verdict::Answer).unwrap();
    session
        .record_outcome(reply, "deliver", "answer (checked)")
        .unwrap();
    let head = session.head();
    drop(session);
    let restored = Session::restore(dir.path(), head, "test-workspace").unwrap();
    let messages = restored.restored_messages().unwrap();
    assert_eq!(messages.last().unwrap()["content"], "answer");
    assert_eq!(messages[2]["content"], "result");
}

#[test]
fn actual_operator_stays_pinned_after_harness_nudge() {
    let mut session = Session::new(config()).unwrap();
    let request = session
        .record_request(
            json!({"messages":[{"role":"user","content":"actual task"}]}),
            "openai",
        )
        .unwrap();
    let reply = session.record_reply(request.id, b"planning").unwrap();
    session
        .record_host_message("continue working", reply)
        .unwrap();
    let messages = vec![
        json!({"role":"user","content":"actual task"}),
        json!({"role":"assistant","content":"planning"}),
        json!({"role":"user","content":"continue working"}),
    ];
    let catalog = session.catalog(&messages, 4096).unwrap();
    let cards = catalog["candidates"].as_array().unwrap();
    assert!(cards
        .iter()
        .any(|card| card["excerpt"] == "actual task" && card["required"] == true));
    assert!(!cards
        .iter()
        .any(|card| card["excerpt"] == "continue working"));
    let missing_operator = cards
        .iter()
        .filter(|card| card["required"] != true)
        .map(|card| card["cid"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert!(session
        .project_selection(&messages, &missing_operator, 4096)
        .is_err());
}

#[test]
fn newest_tool_result_stays_pinned_after_a_context_rejection_observes_the_request() {
    let mut session = Session::new(config()).unwrap();
    let messages = vec![
        json!({"role":"user","content":"old context"}),
        json!({"role":"user","content":"exact operator prompt"}),
        json!({"role":"assistant","tool_calls":[{"id":"call-1"},{"id":"call-2"}]}),
        json!({"role":"tool","tool_call_id":"call-1","content":"first result"}),
        json!({"role":"tool","tool_call_id":"call-2","content":"last result must survive"}),
    ];
    let request = session
        .record_request(json!({"messages":messages}), "openai")
        .unwrap();
    session
        .record_reply(request.id, b"Context size has been exceeded")
        .unwrap();
    let catalog = session.catalog(&messages, 4096).unwrap();
    let cards = catalog["candidates"].as_array().unwrap();
    let ids = cards
        .iter()
        .map(|card| card["cid"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        cards[4]["required"], true,
        "observed rejection does not unpin the last result"
    );
    assert!(
        session
            .project_selection(&messages, &ids[1..2], 4096)
            .is_err(),
        "the operator alone must not displace the newest tool result"
    );
    assert!(
        session
            .project_selection(&messages, &[ids[1].clone(), ids[4].clone()], 4096)
            .is_err(),
        "keeping the result requires the complete tool-call batch"
    );
    let projected = session
        .project_selection(&messages, &ids[1..], 4096)
        .unwrap();
    assert!(projected.iter().any(|message| message == &messages[1]));
    for original in &messages[2..] {
        assert!(
            projected.iter().any(|message| message == original),
            "the original tool-call/result group must remain verbatim"
        );
    }
}

#[test]
fn model_parroting_an_intervention_remains_a_model_observation() {
    let mut session = Session::new(config()).unwrap();
    let request = session
        .record_request(
            json!({"messages":[{"role":"user","content":"task"}]}),
            "openai",
        )
        .unwrap();
    let reply = session.record_reply(request.id, b"planning").unwrap();
    session
        .record_host_message("continue working", reply)
        .unwrap();
    let catalog = session
        .catalog(
            &[
                json!({"role":"user","content":"task"}),
                json!({"role":"assistant","content":"continue working"}),
            ],
            4096,
        )
        .unwrap();
    assert!(catalog["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .any(|card| card["role"] == "assistant" && card["excerpt"] == "continue working"));
}

/// Grounds in-memory elision/retrieval in a real filesystem and cold restore.
/// Allow build I/O contention here; clock accounting and auxiliary deadlines
/// have separate small-budget tests.
#[test]
fn elision_reread_is_recorded_bounded_and_restorable() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::open(
        dir.path(),
        SessionConfig {
            max_elapsed_ms: 300_000,
            ..config()
        },
    )
    .unwrap();
    let messages = vec![
        json!({"role":"user","content":"older request"}),
        json!({"role":"assistant","content":"x".repeat(2000)}),
        json!({"role":"user","content":"current request"}),
    ];
    let catalog = session.catalog(&messages, 900).unwrap();
    let selected = catalog["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["required"] == true)
        .map(|c| c["cid"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    let projected = session
        .project_selection(&messages, &selected, 900)
        .unwrap();
    assert_eq!(projected.last().unwrap()["content"], "current request");
    let pointer = projected[0]["content"]
        .as_str()
        .unwrap()
        .split_whitespace()
        .find(|word| word.starts_with("bafy"))
        .unwrap()
        .trim_end_matches('.');
    let before = session.head();
    let result = session.re_read(pointer, 0, 20).unwrap();
    assert_eq!(result["complete"], false);
    assert_eq!(result["next_offset"], 20);
    assert_ne!(session.head(), before);
    assert!(result["retrieval"].is_string());
    assert!(session.re_read(pointer, 0, 0).is_err());
    assert!(session.re_read(pointer, 0, usize::MAX).is_err());
    let head = session.head();
    drop(session);
    let restored = Session::restore(dir.path(), head, "test-workspace");
    assert!(restored.is_ok(), "{:?}", restored.err());
}
