//! Call occurrence admission and recovery through the durable session boundary.
use agent_frame::{EventOrigin, RawEvent};
use agent_harness::{store::FrameStore, Session, SessionConfig, ToolCallState, ToolReturn};
use content_addressable::ContentId;
use serde_json::{json, Value};

fn begin(session: &mut Session, format: &str, count: usize) -> (Vec<ContentId>, Vec<Value>) {
    begin_named(session, format, count, "test_tool")
}

fn begin_named(
    session: &mut Session,
    format: &str,
    count: usize,
    name: &str,
) -> (Vec<ContentId>, Vec<Value>) {
    let user = json!({"role":"user","content":"run the calls"});
    let request_body = if format == "responses" {
        json!({"input":[user],"instructions":"retain exact call IDs"})
    } else {
        json!({"messages":[user]})
    };
    let request = session.record_request(request_body, format).unwrap();
    let reply = session
        .record_reply(request.id, b"accepted tool batch")
        .unwrap();
    let calls = (0..count)
        .map(|ordinal| {
            json!({"id": if format == "ollama" { String::new() } else { format!("call-{ordinal}") },
            "function":{"name":name,"arguments":{}}})
        })
        .collect::<Vec<_>>();
    let mut messages = vec![user];
    if format == "responses" {
        messages.insert(
            0,
            json!({"role":"system","content":"retain exact call IDs"}),
        );
        messages.push(json!({"type":"reasoning","id":"reasoning-original","summary":[]}));
        for (ordinal, call) in calls.iter().enumerate() {
            messages.push(json!({"type":"function_call","id":format!("fc-{ordinal}"),
                "call_id":call["id"],"name":name,"arguments":"{}"}));
        }
    } else {
        messages.push(json!({"role":"assistant","tool_calls":calls}));
    }
    let ids = session.begin_tool_batch(reply, &calls, &messages).unwrap();
    (ids, messages)
}

fn envelope(format: &str, ordinal: usize, text: &str) -> Value {
    match format {
        "responses" => {
            json!({"type":"function_call_output","call_id":format!("call-{ordinal}"),"output":text})
        }
        "ollama" => json!({"role":"tool","content":text}),
        _ => json!({"role":"tool","tool_call_id":format!("call-{ordinal}"),"content":text}),
    }
}

fn content(message: &Value) -> Option<&str> {
    message
        .get("content")
        .or_else(|| message.get("output"))
        .and_then(Value::as_str)
}

#[test]
fn identical_idless_calls_have_distinct_occurrences_and_exact_returns() {
    let mut session = Session::new(SessionConfig::default()).unwrap();
    let (calls, _) = begin(&mut session, "ollama", 2);
    assert_ne!(calls[0], calls[1]);
    for (ordinal, text) in ["Error: actual tool output", ""].into_iter().enumerate() {
        session.start_tool_call(calls[ordinal]).unwrap();
        let returned = session
            .record_tool_return(
                calls[ordinal],
                ToolReturn::Observed {
                    bytes: text.as_bytes(),
                    retained_sources: &[],
                },
            )
            .unwrap();
        session
            .record_tool_delivery(calls[ordinal], &envelope("ollama", ordinal, text))
            .unwrap();
        let status = session.tool_call(calls[ordinal]).unwrap();
        assert_eq!(status.state, ToolCallState::Returned);
        assert_eq!(status.returned, Some(returned));
    }
    let restored = session.restored_messages().unwrap();
    assert_eq!(content(&restored[2]), Some("Error: actual tool output"));
    assert_eq!(content(&restored[3]), Some(""));
}

/// Grounds explicit typed failure admission in a cold filesystem restore.
/// Identical bytes from an untyped return and a typed tool error retain their
/// different facts; neither is confused with a call whose return is unknown.
#[test]
fn typed_failure_survives_restore_distinct_from_error_text_and_uncertainty() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::open(dir.path(), SessionConfig::default()).unwrap();
    let (calls, _) = begin(&mut session, "openai", 3);
    let bytes = b"Error: same observed bytes";
    session.start_tool_call(calls[0]).unwrap();
    let ordinary = session
        .record_tool_return(
            calls[0],
            ToolReturn::Observed {
                bytes,
                retained_sources: &[],
            },
        )
        .unwrap();
    session
        .record_tool_delivery(
            calls[0],
            &envelope("openai", 0, std::str::from_utf8(bytes).unwrap()),
        )
        .unwrap();
    session.start_tool_call(calls[1]).unwrap();
    let failed = session
        .record_tool_return(
            calls[1],
            ToolReturn::Failed {
                bytes,
                retained_sources: &[],
            },
        )
        .unwrap();
    // No bounded delivery was recorded for the typed failure. Recovery must
    // retain the observed failure instead of downgrading it to uncertainty.
    session.start_tool_call(calls[2]).unwrap();
    let head = session.head();
    drop(session);
    let restored = Session::restore(dir.path(), head, "local-session").unwrap();
    assert_eq!(
        restored.tool_call(calls[0]).unwrap().state,
        ToolCallState::Returned
    );
    assert_eq!(
        restored.tool_call(calls[1]).unwrap().state,
        ToolCallState::Failed
    );
    assert_eq!(
        restored.tool_call(calls[2]).unwrap().state,
        ToolCallState::Uncertain
    );
    assert_eq!(restored.tool_call(calls[1]).unwrap().returned, Some(failed));
    assert!(restored.tool_call(calls[2]).unwrap().returned.is_none());
    let store = FrameStore::open(dir.path()).unwrap();
    for returned in [ordinary, failed] {
        let event: RawEvent = store.get(&returned).unwrap();
        assert_eq!(store.source(&event.payload().payload).unwrap(), bytes);
        assert_eq!(event.payload().origin, EventOrigin::Tool);
    }
}

#[test]
fn call_transitions_refuse_unknown_duplicate_and_out_of_order_facts() {
    let mut session = Session::new(SessionConfig::default()).unwrap();
    let (calls, _) = begin(&mut session, "openai", 1);
    let missing = session.run_id();
    assert!(session.start_tool_call(missing).is_err());
    assert!(session
        .record_tool_return(
            calls[0],
            ToolReturn::Observed {
                bytes: b"never started",
                retained_sources: &[],
            }
        )
        .is_err());
    session.start_tool_call(calls[0]).unwrap();
    assert!(session.start_tool_call(calls[0]).is_err());
    assert!(session
        .record_tool_delivery(calls[0], &envelope("openai", 0, "not observed"))
        .is_err());
    session
        .record_tool_return(
            calls[0],
            ToolReturn::Observed {
                bytes: b"observed",
                retained_sources: &[],
            },
        )
        .unwrap();
    assert!(session
        .record_tool_return(
            calls[0],
            ToolReturn::Observed {
                bytes: b"duplicate",
                retained_sources: &[],
            }
        )
        .is_err());
    assert!(session
        .record_tool_delivery(calls[0], &envelope("openai", 99, "observed"))
        .is_err());
    session
        .record_tool_delivery(calls[0], &envelope("openai", 0, "observed"))
        .unwrap();
    assert!(session
        .record_tool_delivery(calls[0], &envelope("openai", 0, "observed"))
        .is_err());
}

/// Grounds in-memory lifecycle state and transcript admission in closed and
/// reopened filesystem stores. Every wire keeps the completed return and
/// receives truthful host closures before a later operator can be appended.
#[test]
fn cold_restore_closes_each_wire_without_losing_a_completed_return() {
    for format in ["ollama", "openai", "anthropic", "responses"] {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::open(dir.path(), SessionConfig::default()).unwrap();
        let (calls, original) = begin(&mut session, format, 3);
        session.start_tool_call(calls[0]).unwrap();
        session
            .record_tool_return(
                calls[0],
                ToolReturn::Observed {
                    bytes: b"A returned exactly",
                    retained_sources: &[],
                },
            )
            .unwrap();
        let delivered = envelope(format, 0, "A returned exactly");
        session.record_tool_delivery(calls[0], &delivered).unwrap();
        session.start_tool_call(calls[1]).unwrap();
        let head = session.head();
        drop(session);
        let restored = Session::restore(dir.path(), head, "local-session").unwrap();
        assert_eq!(
            restored.tool_call(calls[0]).unwrap().state,
            ToolCallState::Returned
        );
        assert_eq!(
            restored.tool_call(calls[1]).unwrap().state,
            ToolCallState::Uncertain
        );
        assert_eq!(
            restored.tool_call(calls[2]).unwrap().state,
            ToolCallState::NotStarted
        );
        let messages = restored.restored_messages().unwrap();
        assert_eq!(&messages[..original.len()], original);
        assert_eq!(messages[original.len()], delivered);
        assert_eq!(messages.len(), original.len() + 3);
        for ordinal in [1, 2] {
            let status = restored.tool_call(calls[ordinal]).unwrap();
            assert!(status.returned.is_none());
            let store = FrameStore::open(dir.path()).unwrap();
            let event: RawEvent = store.get(&status.delivery.unwrap()).unwrap();
            assert_eq!(event.payload().origin, EventOrigin::Harness);
            if format != "ollama" {
                let key = if format == "responses" {
                    "call_id"
                } else {
                    "tool_call_id"
                };
                assert_eq!(
                    messages[original.len() + ordinal][key],
                    format!("call-{ordinal}")
                );
            }
        }
    }
}

/// Grounds the raw-return checkpoint independently of later bounded delivery:
/// a restart must expose a retained source pointer, not downgrade observed work
/// to uncertainty because presentation never completed.
#[test]
fn returned_without_delivery_stays_returned_and_retains_its_source_closure() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::open(dir.path(), SessionConfig::default()).unwrap();
    let (calls, _) = begin(&mut session, "openai", 1);
    session.start_tool_call(calls[0]).unwrap();
    let returned = session
        .record_tool_return(
            calls[0],
            ToolReturn::Observed {
                bytes: b"already disclosed wrapper",
                retained_sources: &[],
            },
        )
        .unwrap();
    let retained = session
        .retain_tool_output("test_tool", b"original spill tail")
        .unwrap();
    session.record_tool_sources(calls[0], &[retained]).unwrap();
    let head = session.head();
    drop(session);
    let mut restored = Session::restore(dir.path(), head, "local-session").unwrap();
    let status = restored.tool_call(calls[0]).unwrap();
    assert_eq!(status.state, ToolCallState::Returned);
    assert_eq!(status.returned, Some(returned));
    assert_eq!(status.retained_sources, vec![retained]);
    let messages = restored.restored_messages().unwrap();
    let notice = content(messages.last().unwrap()).unwrap();
    assert!(notice.contains(&returned.to_string()));
    assert!(notice.contains(&retained.to_string()));
    let store = FrameStore::open(dir.path()).unwrap();
    let event: RawEvent = store.get(&returned).unwrap();
    assert_eq!(
        store.source(&event.payload().payload).unwrap(),
        b"already disclosed wrapper"
    );
    assert_eq!(event.payload().origin, EventOrigin::Tool);
    let slice = restored.re_read(&retained.to_string(), 0, 64).unwrap();
    assert_eq!(slice["text"], "original spill tail");
}

#[test]
fn source_attachments_validate_state_membership_origin_and_duplicates() {
    let mut session = Session::new(SessionConfig::default()).unwrap();
    let old = session
        .retain_tool_output("test_tool", b"before this batch")
        .unwrap();
    let (calls, _) = begin(&mut session, "openai", 2);
    let source = session
        .retain_tool_output("test_tool", b"same batch")
        .unwrap();
    assert!(session
        .record_tool_sources(session.run_id(), &[source])
        .is_err());
    assert!(session.record_tool_sources(calls[0], &[source]).is_err());
    session.start_tool_call(calls[0]).unwrap();
    assert!(session.record_tool_sources(calls[0], &[source]).is_err());
    let returned = session
        .record_tool_return(
            calls[0],
            ToolReturn::Failed {
                bytes: b"typed failure with retained details",
                retained_sources: &[],
            },
        )
        .unwrap();
    let head = session.head();
    session.record_tool_sources(calls[0], &[]).unwrap();
    assert_eq!(session.head(), head);
    for invalid in [
        vec![session.run_id()],
        vec![old],
        vec![calls[0]],
        vec![source, source],
    ] {
        assert!(session.record_tool_sources(calls[0], &invalid).is_err());
        assert_eq!(session.head(), head);
    }
    session.record_tool_sources(calls[0], &[source]).unwrap();
    assert_eq!(
        session.tool_call(calls[0]).unwrap().returned,
        Some(returned)
    );
    assert_eq!(
        session.tool_call(calls[0]).unwrap().state,
        ToolCallState::Failed
    );
    assert!(session.record_tool_sources(calls[0], &[source]).is_err());
    session
        .record_tool_delivery(
            calls[0],
            &envelope("openai", 0, "typed failure with retained details"),
        )
        .unwrap();
    assert!(session.record_tool_sources(calls[0], &[]).is_err());
    session.start_tool_call(calls[1]).unwrap();
    session
        .record_tool_return(calls[1], ToolReturn::Host("host refusal"))
        .unwrap();
    assert!(session.record_tool_sources(calls[1], &[source]).is_err());
}

#[test]
fn queued_host_resolution_is_explicit_and_never_an_observed_return() {
    let mut session = Session::new(SessionConfig::default()).unwrap();
    let (calls, _) = begin(&mut session, "openai", 1);
    let host = envelope("openai", 0, "The harness skipped this repeated call.");
    session.resolve_tool_call(calls[0], &host).unwrap();
    let status = session.tool_call(calls[0]).unwrap();
    assert_eq!(status.state, ToolCallState::HostResolved);
    assert!(status.returned.is_none());
    assert!(session.start_tool_call(calls[0]).is_err());
    assert_eq!(session.restored_messages().unwrap().last(), Some(&host));
}

/// Grounds delivery provenance in the admitted on-disk Event rather than only
/// the derived status view: a changed presentation is a harness projection,
/// never a new external tool observation carrying rewritten content.
#[test]
fn transformed_delivery_has_harness_origin_and_the_real_return_as_source() {
    for changed in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::open(dir.path(), SessionConfig::default()).unwrap();
        let (calls, _) = begin(&mut session, "openai", 1);
        session.start_tool_call(calls[0]).unwrap();
        let returned = session
            .record_tool_return(
                calls[0],
                ToolReturn::Observed {
                    bytes: b"original observed text",
                    retained_sources: &[],
                },
            )
            .unwrap();
        let text = if changed {
            "bounded host presentation"
        } else {
            "original observed text"
        };
        session
            .record_tool_delivery(calls[0], &envelope("openai", 0, text))
            .unwrap();
        let delivery = session.tool_call(calls[0]).unwrap().delivery.unwrap();
        let store = FrameStore::open(dir.path()).unwrap();
        let event: RawEvent = store.get(&delivery).unwrap();
        assert!(event.parents().contains(&returned));
        if changed {
            assert_eq!(event.payload().origin, EventOrigin::Harness);
            assert!(event.payload().sources.contains(&returned));
            assert_eq!(event.payload().depth, 1);
        } else {
            assert_eq!(event.payload().origin, EventOrigin::Tool);
            assert_eq!(event.payload().depth, 0);
        }
    }
}

#[test]
fn native_output_cannot_use_a_shadow_content_field_to_claim_external_origin() {
    let mut session = Session::new(SessionConfig::default()).unwrap();
    let (calls, _) = begin(&mut session, "responses", 1);
    session.start_tool_call(calls[0]).unwrap();
    session
        .record_tool_return(
            calls[0],
            ToolReturn::Observed {
                bytes: b"actual observed bytes",
                retained_sources: &[],
            },
        )
        .unwrap();
    let ambiguous = json!({"type":"function_call_output", "call_id":"call-0",
        "output":"rewritten wire text", "content":"actual observed bytes"});
    assert!(session.record_tool_delivery(calls[0], &ambiguous).is_err());
}

#[test]
fn openai_proxy_flat_tool_use_keeps_its_original_input_envelope() {
    let mut session = Session::new(SessionConfig::default()).unwrap();
    let user = json!({"role":"user","content":"read the file"});
    let request = session
        .record_request(json!({"messages":[user]}), "openai")
        .unwrap();
    let assistant = json!({"role":"assistant","content":"", "tool_calls":[
        {"id":"native-call","type":"tool_use","name":"read_file","input":{"path":"src/lib.rs"}}
    ]});
    let reply = session
        .record_reply(request.id, &serde_json::to_vec(&assistant).unwrap())
        .unwrap();
    let messages = vec![user, assistant];
    let calls = [
        json!({"id":"native-call","function":{"name":"read_file","arguments":{"path":"src/lib.rs"}}}),
    ];
    let invocation = session.begin_tool_batch(reply, &calls, &messages).unwrap()[0];
    assert_eq!(session.restored_messages().unwrap(), messages);
    session.start_tool_call(invocation).unwrap();
    session
        .record_tool_return(
            invocation,
            ToolReturn::Observed {
                bytes: b"file content",
                retained_sources: &[],
            },
        )
        .unwrap();
    session
        .record_tool_delivery(
            invocation,
            &json!({"role":"tool","tool_call_id":"native-call","content":"file content"}),
        )
        .unwrap();
    let history = session.restored_messages().unwrap();
    assert_eq!(history[1], messages[1]);
    let next = session
        .record_request(json!({"messages":history}), "openai")
        .unwrap();
    assert_eq!(next.bytes, session.replay(next.id).unwrap());
}

#[test]
fn standalone_request_refuses_unclosed_calls_until_durable_protocol_repair() {
    let mut session = Session::new(SessionConfig::default()).unwrap();
    let (_, messages) = begin(&mut session, "openai", 1);
    let error = session
        .record_request(json!({"messages":messages}), "openai")
        .unwrap_err();
    assert!(
        matches!(error, agent_harness::Error::Integrity(_)),
        "{error}"
    );
    let messages = session
        .interrupt_tool_batch("operator interrupted")
        .unwrap();
    let request = session
        .record_request(json!({"messages":messages}), "openai")
        .unwrap();
    assert_eq!(request.bytes, session.replay(request.id).unwrap());
}

#[test]
fn unresolved_batches_refuse_transcript_advancement_but_allow_retained_facts() {
    let mut results = Vec::new();
    for operation in ["model", "catalog", "host_user", "host_tool"] {
        let mut session = Session::new(SessionConfig::default()).unwrap();
        let (calls, messages) = begin(&mut session, "openai", 1);
        let reply = session.tool_call(calls[0]).unwrap().reply;
        let result = match operation {
            "model" => session
                .record_model_message(reply, "another assistant message")
                .map(|_| ()),
            "catalog" => session.catalog(&messages, 4096).map(|_| ()),
            "host_user" => session
                .record_host_message("new host message", reply)
                .map(|_| ()),
            "host_tool" => session
                .record_host_envelope(&envelope("openai", 0, "untracked host result"), reply)
                .map(|_| ()),
            _ => unreachable!(),
        };
        results.push((operation, result.is_err()));
        let source = session
            .record_intervention("retained host observation", reply)
            .unwrap();
        assert_eq!(
            session.re_read(&source.to_string(), 0, 64).unwrap()["text"],
            "retained host observation"
        );
        session
            .retain_tool_output("test_tool", b"retained source before return")
            .unwrap();
        session.interrupt_tool_batch("closed explicitly").unwrap();
    }
    assert!(results.iter().all(|(_, refused)| *refused), "{results:?}");
}

#[test]
fn idless_delivery_cannot_advance_past_an_earlier_unresolved_slot() {
    let mut session = Session::new(SessionConfig::default()).unwrap();
    let (calls, _) = begin(&mut session, "ollama", 2);
    session.start_tool_call(calls[0]).unwrap();
    // The leaf records host facts independently of scheduling. Even when B
    // returns first, positional wire delivery must preserve the original order.
    session.start_tool_call(calls[1]).unwrap();
    session
        .record_tool_return(
            calls[1],
            ToolReturn::Observed {
                bytes: b"B",
                retained_sources: &[],
            },
        )
        .unwrap();
    assert!(session
        .record_tool_delivery(calls[1], &envelope("ollama", 1, "B"))
        .is_err());
    session
        .record_tool_return(
            calls[0],
            ToolReturn::Observed {
                bytes: b"A",
                retained_sources: &[],
            },
        )
        .unwrap();
    session
        .record_tool_delivery(calls[0], &envelope("ollama", 0, "A"))
        .unwrap();
    session
        .record_tool_delivery(calls[1], &envelope("ollama", 1, "B"))
        .unwrap();
    let messages = session.restored_messages().unwrap();
    assert_eq!(content(&messages[2]), Some("A"));
    assert_eq!(content(&messages[3]), Some("B"));
}

/// Grounds receipt-aware return admission in the actual retained event graph.
/// Reading generated material and serializing its tool envelope must preserve
/// its depth and host origin rather than laundering it into external evidence.
#[test]
fn reread_and_known_host_refusal_cannot_be_relabelled_as_external_returns() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::open(dir.path(), SessionConfig::default()).unwrap();
    let (calls, _) = begin_named(&mut session, "openai", 2, "re_read");
    let reply = session.tool_call(calls[0]).unwrap().reply;
    let original = session
        .record_intervention("already generated host material", reply)
        .unwrap();
    session.start_tool_call(calls[0]).unwrap();
    let receipt = session
        .re_read(&original.to_string(), 0, 64)
        .unwrap()
        .to_string();
    assert!(session
        .record_tool_return(
            calls[0],
            ToolReturn::Observed {
                bytes: receipt.as_bytes(),
                retained_sources: &[],
            }
        )
        .is_err());
    assert!(session
        .record_tool_return(calls[0], ToolReturn::Retrieval("not a receipt"))
        .is_err());
    let returned = session
        .record_tool_return(calls[0], ToolReturn::Retrieval(&receipt))
        .unwrap();
    session
        .record_tool_delivery(calls[0], &envelope("openai", 0, &receipt))
        .unwrap();
    session.start_tool_call(calls[1]).unwrap();
    let refused = session
        .record_tool_return(calls[1], ToolReturn::Host("refused by host authority"))
        .unwrap();
    session
        .record_tool_delivery(
            calls[1],
            &envelope("openai", 1, "refused by host authority"),
        )
        .unwrap();
    let delivery = session.tool_call(calls[0]).unwrap().delivery.unwrap();
    let head = session.head();
    drop(session);
    let restored = Session::restore(dir.path(), head, "local-session").unwrap();
    assert_eq!(
        content(&restored.restored_messages().unwrap()[2]),
        Some(receipt.as_str())
    );
    let store = FrameStore::open(dir.path()).unwrap();
    for id in [returned, refused, delivery] {
        let event: RawEvent = store.get(&id).unwrap();
        assert_eq!(event.payload().origin, EventOrigin::Harness);
        assert_eq!(event.payload().depth, 1);
    }
    let event: RawEvent = store.get(&returned).unwrap();
    let retained = event.parents().iter().find(|id| **id != calls[0]).unwrap();
    let source: RawEvent = store.get(retained).unwrap();
    assert!(source.payload().sources.contains(&original));
    assert_eq!(source.payload().depth, 1);
}

/// Grounds the lifecycle's writer-error propagation in a real broken head
/// locator. A failed start authorizes no work; a failed return publication
/// preserves uncertainty, and neither invents a typed tool failure.
#[test]
fn storage_failure_never_becomes_an_observed_tool_failure() {
    for started in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::open(dir.path(), SessionConfig::default()).unwrap();
        let (calls, _) = begin(&mut session, "openai", 1);
        if started {
            session.start_tool_call(calls[0]).unwrap();
        }
        let head = session.head();
        let locator = session.checkpoint_path().unwrap();
        let committed = std::fs::read(&locator).unwrap();
        std::fs::remove_file(&locator).unwrap();
        std::fs::create_dir(&locator).unwrap();
        let error = if started {
            session
                .record_tool_return(
                    calls[0],
                    ToolReturn::Observed {
                        bytes: b"actually observed but publication failed",
                        retained_sources: &[],
                    },
                )
                .unwrap_err()
        } else {
            session.start_tool_call(calls[0]).unwrap_err()
        };
        assert!(
            matches!(
                error,
                agent_harness::Error::Storage(_) | agent_harness::Error::Conflict(_)
            ),
            "{error}"
        );
        assert_eq!(session.head(), head);
        assert!(session.tool_call(calls[0]).is_err());
        // Repairing the test fault cannot revive the aborted writer. Only a
        // fresh owner can recover the actual last committed lifecycle fact.
        std::fs::remove_dir(&locator).unwrap();
        std::fs::write(&locator, committed).unwrap();
        assert!(session.start_tool_call(calls[0]).is_err());
        drop(session);
        let restored = Session::restore(dir.path(), head, "local-session").unwrap();
        let status = restored.tool_call(calls[0]).unwrap();
        assert_eq!(
            status.state,
            if started {
                ToolCallState::Uncertain
            } else {
                ToolCallState::NotStarted
            }
        );
        assert!(status.returned.is_none());
    }
}
