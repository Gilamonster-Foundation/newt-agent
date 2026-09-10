use super::*;

fn observed_session() -> (Session, ContentId) {
    let mut session = Session::new(SessionConfig::default()).unwrap();
    let request = session
        .record_request(
            json!({"messages":[{"role":"user","content":"task"}]}),
            "openai",
        )
        .unwrap();
    let reply = session.record_reply(request.id, b"answer").unwrap();
    (session, reply)
}

#[test]
fn operator_text_matching_auxiliary_output_stays_operator_input() {
    let (mut session, reply) = observed_session();
    let request = session
        .record_adjudication_request(reply, "classify")
        .unwrap();
    session
        .record_adjudication_reply(request, "answer")
        .unwrap();
    let messages = [
        json!({"role":"user","content":"task"}),
        json!({"role":"assistant","content":"old reply"}),
        json!({"role":"user","content":"answer"}),
    ];
    let catalog = session.catalog(&messages, 4096).unwrap();
    assert!(catalog["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .any(|card| card["excerpt"] == "answer" && card["required"] == true));
}

#[test]
fn observed_answer_remains_relevance_evidence_after_outcome() {
    let (mut session, reply) = observed_session();
    session
        .record_model_message(reply, "actual model answer")
        .unwrap();
    session.record_verdict(reply, Verdict::Answer).unwrap();
    session
        .record_outcome(reply, "deliver", "actual model answer plus host footer")
        .unwrap();
    let mut messages = session.restored_messages().unwrap();
    assert_eq!(messages.last().unwrap()["content"], "actual model answer");
    messages.push(json!({"role":"user","content":"explain the answer"}));
    let catalog = session.catalog(&messages, 4096).unwrap();
    assert!(catalog["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .any(|card| card["excerpt"] == "actual model answer"));
}

#[test]
fn one_session_cannot_switch_provider_formats_or_repeat_model_extraction() {
    let (mut session, reply) = observed_session();
    session.record_model_message(reply, "answer").unwrap();
    assert!(session.record_model_message(reply, "answer").is_err());
    assert!(session
        .record_request(
            json!({"input":[{"role":"user","content":"task"}]}),
            "responses"
        )
        .is_err());
}

#[test]
fn unsent_host_message_cannot_claim_next_turns_operator_text() {
    let (mut session, reply) = observed_session();
    session.record_host_message("continue", reply).unwrap();
    session.start_turn();
    let catalog = session
        .catalog(&[json!({"role":"user","content":"continue"})], 4096)
        .unwrap();
    assert_eq!(catalog["candidates"][0]["required"], true);
}

#[test]
fn selection_keeps_operator_occurrence_when_omitted_host_text_is_identical() {
    let (mut session, reply) = observed_session();
    session.record_host_message("continue", reply).unwrap();
    let messages = [
        json!({"role":"user","content":"task"}),
        json!({"role":"user","content":"continue"}),
        json!({"role":"user","content":"continue"}),
    ];
    let catalog = session.catalog(&messages, 4096).unwrap();
    let operator = catalog["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|card| card["required"] == true)
        .unwrap()["cid"]
        .as_str()
        .unwrap()
        .to_owned();
    let projected = session
        .project_selection(&messages, std::slice::from_ref(&operator), 4096)
        .unwrap();
    let request = session
        .record_request(json!({"messages":projected}), "openai")
        .unwrap();
    let projection: Projection = session.store.get(&request.projection).unwrap();
    assert!(projection
        .entries
        .iter()
        .any(|entry| entry.event.to_string() == operator));
}

/// Grounds genesis contract validation in a real restart with correctly hashed
/// but semantically substituted root metadata, not a broken content digest.
#[test]
fn restored_run_root_must_commit_its_actual_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = FrameStore::open(dir.path()).unwrap();
    let bytes = b"unrelated root";
    store.put_source(bytes).unwrap();
    let root = store
        .put(&RootEvent::new(RootKind::HarnessEvent, bytes, 0))
        .unwrap();
    let head = store
        .put(&MerkleNode::genesis(JournalEntry::Run {
            schema: 1,
            config: SessionConfig::default(),
            root,
        }))
        .unwrap();
    assert!(Session::restore(dir.path(), head, "local-session").is_err());
}

#[test]
fn decoded_journal_cannot_relabel_a_verdict_or_replay_an_occurrence() {
    let (mut session, reply) = observed_session();
    session.record_verdict(reply, Verdict::Answer).unwrap();
    let verdict = session
        .events
        .iter()
        .find(|(_, event)| matches!(event.body().kind, EventKind::Verdict { .. }))
        .map(|(id, _)| *id)
        .unwrap();
    assert!(session
        .apply(&JournalEntry::Observation { event: verdict })
        .is_err());
    assert!(session
        .apply(&JournalEntry::Reply {
            event: reply,
            request: *session.requests.iter().next().unwrap()
        })
        .is_err());
}

/// Grounds append failure behavior in a real failed atomic head publication.
/// Repairing the filesystem cannot make the partially mutated session reusable.
#[test]
fn failed_checkpoint_publication_aborts_the_live_session() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::open(dir.path(), SessionConfig::default()).unwrap();
    let locator = session.checkpoint_path().unwrap();
    std::fs::remove_file(&locator).unwrap();
    std::fs::create_dir(&locator).unwrap();
    let messages = [json!({"role":"user","content":"task"})];
    assert!(session.record_messages(&messages).is_err());
    std::fs::remove_dir(&locator).unwrap();
    assert!(session.record_messages(&messages).is_err());
}

#[test]
fn packet_slots_preserve_occurrences_of_identical_units() {
    let mut session = Session::new(SessionConfig::default()).unwrap();
    let messages = vec![
        json!({"role":"user","content":"task"}),
        json!({"role":"assistant","content":"same observation"}),
        json!({"role":"assistant","content":"same observation"}),
    ];
    let catalog = session.catalog(&messages, 4096).unwrap();
    let selected = vec![catalog["candidates"][0]["cid"].as_str().unwrap().to_owned()];
    session
        .project_selection(&messages, &selected, 4096)
        .unwrap();
    let mut elisions = session
        .events
        .iter()
        .filter(|(_, event)| matches!(event.body().kind, EventKind::Elision { .. }))
        .collect::<Vec<_>>();
    elisions.sort_by_key(|(_, event)| event.body().seq);
    assert_eq!(elisions.len(), 2);
    let first = *elisions[0].0;
    let pointer = session.packet_head.unwrap();
    let slice = session.re_read(&pointer.to_string(), 1, 4).unwrap();
    let receipt: MerkleNode<RetrievalBody> = session
        .store
        .get(&slice["retrieval"].as_str().unwrap().parse().unwrap())
        .unwrap();
    assert_eq!(receipt.payload().parts[0].source, first);
}

/// Grounds navigation's clock accounting in a real elapsed interval: primary
/// inference and operator idle time must not spend the navigation work budget.
#[test]
fn idle_time_does_not_exhaust_navigation_work_budget() {
    let mut session = Session::new(SessionConfig {
        max_elapsed_ms: 20,
        ..Default::default()
    })
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(25));
    assert!(session
        .project(&[json!({"role":"user","content":"task"})], 4096)
        .is_ok());
    session.account_navigation_elapsed(std::time::Duration::from_millis(21));
    assert!(matches!(
        session.project(&[json!({"role":"user","content":"task"})], 4096),
        Err(Error::Budget(_))
    ));
}

#[test]
fn native_provider_tool_receipts_never_become_operator_or_source_evidence() {
    for format in ["anthropic", "responses"] {
        let mut session = Session::new(SessionConfig::default()).unwrap();
        let request = session
            .record_request(
                json!({"messages":[{"role":"user","content":"task"}]}),
                "openai",
            )
            .unwrap();
        let reply = session.record_reply(request.id, b"I will proceed").unwrap();
        let intervention = session
            .record_intervention("harness generated instructions", reply)
            .unwrap();
        let slice = session
            .re_read(&intervention.to_string(), 0, 4096)
            .unwrap()
            .to_string();
        let message = if format == "anthropic" {
            json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"call-1","content":slice}]})
        } else {
            json!({"type":"function_call_output","call_id":"call-1","output":slice})
        };
        let entries = session.ingest(&[message]).unwrap();
        let event = &session.events[&entries[0].event];
        assert_eq!(event.body().origin, EventOrigin::Harness, "{format}");
        assert_eq!(event.depth(), 1);
    }
}
