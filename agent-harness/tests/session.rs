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

/// Two accounted tool exchanges, a large older one and the current one. A
/// result is delivered as a generated preview, as the real harness does for
/// large output, unless `verbatim_older` makes the older one a source entry.
fn two_exchanges(
    max_catalog_entries: usize,
    verbatim_older: bool,
) -> (Session, Vec<serde_json::Value>) {
    let mut session = Session::new(SessionConfig {
        max_elapsed_ms: 300_000,
        max_catalog_entries,
        ..config()
    })
    .unwrap();
    let mut messages = vec![json!({"role":"user","content":"older context ".repeat(400)})];
    for (id, output) in [
        ("older", "retained old tool output\n".repeat(120)),
        ("current", "current tool output".to_owned()),
    ] {
        let request = session
            .record_request(json!({"messages":messages}), "openai")
            .unwrap();
        let call = json!({"id":id,"function":{"name":"read_file","arguments":{}}});
        let assistant = json!({"role":"assistant","content":"","tool_calls":[call]});
        let reply = session
            .record_reply(
                request.id,
                &serde_json::to_vec(&json!({"choices":[{"message":assistant}]})).unwrap(),
            )
            .unwrap();
        messages.push(assistant);
        let calls = session.begin_tool_batch(reply, &[call], &messages).unwrap();
        session.start_tool_call(calls[0]).unwrap();
        session
            .record_tool_return(
                calls[0],
                agent_harness::ToolReturn::Observed {
                    bytes: output.as_bytes(),
                    retained_sources: &[],
                },
            )
            .unwrap();
        // A presentation that differs from the observed bytes is generated.
        let content = if verbatim_older && id == "older" {
            output.clone()
        } else {
            format!("[preview] {output}")
        };
        let delivery = json!({"role":"tool","tool_call_id":id,"content":content});
        session.record_tool_delivery(calls[0], &delivery).unwrap();
        messages.push(delivery);
    }
    messages.push(json!({"role":"user","content":"current operator request"}));
    (session, messages)
}

fn call(catalog: &serde_json::Value, id: &str) -> serde_json::Value {
    catalog["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|card| card["role"] == "assistant" && card["pairs"] == json!([id]))
        .unwrap()
        .clone()
}

/// The catalog is the only place a relevance selector learns prices, and a
/// refused selection is a failed turn. So the catalog must be honest: every
/// card whose omission always fails says `required`, and a selection of cards
/// whose `bytes` sum to at most the catalog's `max_bytes` is never refused for
/// size, although the host adds generated companions, pinned entries and its
/// re-read pointer after the selector has chosen (#2463).
#[test]
fn a_selection_that_obeys_the_catalog_is_admitted_at_every_budget() {
    let (mut probe, messages) = two_exchanges(64, false);
    let whole = serde_json::to_vec(&messages).unwrap().len();
    let catalog = probe.catalog(&messages, whole).unwrap();
    assert_eq!(
        call(&catalog, "current")["required"],
        true,
        "its pinned result makes this call mandatory, so the card must say so"
    );
    assert_eq!(call(&catalog, "older")["required"], false);
    let older_delivery = &messages[2];

    // Prices are linear in the budget, so sweep coarsely and then probe one
    // byte either side of every budget at which the selection changes.
    let cards = catalog["candidates"].as_array().unwrap();
    let price = |required: bool| {
        cards
            .iter()
            .filter(move |card| card["required"] == required)
            .map(|card| card["bytes"].as_u64().unwrap() as usize)
    };
    let overhead = whole - catalog["max_bytes"].as_u64().unwrap() as usize;
    let floor = overhead + price(true).sum::<usize>();
    let optional = price(false).collect::<Vec<_>>();
    let mut budgets = (400..whole)
        .step_by(131)
        .collect::<std::collections::BTreeSet<_>>();
    for subset in 0..1usize << optional.len() {
        let extra = (0..optional.len())
            .filter(|bit| subset & (1 << bit) != 0)
            .map(|bit| optional[bit])
            .sum::<usize>();
        budgets.extend([floor + extra - 1, floor + extra, floor + extra + 1]);
    }

    let (mut kept_older, mut dropped_older) = (0, 0);
    for max in budgets {
        let (mut session, messages) = two_exchanges(64, false);
        let catalog = session.catalog(&messages, max).unwrap();
        let budget = catalog["max_bytes"].as_u64().unwrap();
        let cards = catalog["candidates"].as_array().unwrap();
        let mut spent = 0;
        let mut selected = Vec::new();
        // Required first, then optional cards newest-first while they fit.
        for card in cards
            .iter()
            .filter(|card| card["required"] == true)
            .chain(cards.iter().rev().filter(|card| card["required"] == false))
        {
            let bytes = card["bytes"].as_u64().unwrap();
            if card["required"] == true || spent + bytes <= budget {
                spent += bytes;
                selected.push(card["cid"].as_str().unwrap().to_owned());
            }
        }
        if spent > budget {
            continue; // the catalog itself says the required set cannot fit
        }
        let projected = session
            .project_selection(&messages, &selected, max)
            .unwrap_or_else(|error| panic!("catalog-legal selection refused at {max}: {error}"));
        let used = serde_json::to_vec(&projected).unwrap().len();
        // With something elided the prices are exact, not merely safe: what
        // the selector left unspent is what the projection leaves unused.
        // (With nothing elided the unused pointer reserve is left over.)
        let elided = projected != messages;
        if elided {
            assert_eq!(max - used, (budget - spent) as usize, "at {max}");
        }
        if selected.contains(&call(&catalog, "older")["cid"].as_str().unwrap().to_owned()) {
            assert!(projected.contains(older_delivery));
            kept_older += usize::from(elided);
        } else {
            dropped_older += 1;
        }
    }
    assert!(
        kept_older > 0 && dropped_older > 0,
        "the sweep must price the companion exactly on both sides: kept {kept_older}, dropped {dropped_older}"
    );
}

/// The catalog window must not break the same promise: a card a pinned entry
/// forces stays listed when it falls outside the window, and a source result
/// whose call fell outside it is not offered, because selecting it alone is
/// always refused as a split exchange.
#[test]
fn the_catalog_window_never_offers_half_a_tool_exchange() {
    let offered = |window, verbatim_older| {
        let (mut session, messages) = two_exchanges(window, verbatim_older);
        let max = serde_json::to_vec(&messages).unwrap().len() - 1;
        let catalog = session.catalog(&messages, max).unwrap();
        let selected = catalog["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|card| card["cid"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        session
            .project_selection(&messages, &selected, max)
            .expect("everything the catalog offers can be selected together");
        catalog
    };
    // Window of 2 = the pinned current result and the operator request.
    let catalog = offered(2, false);
    assert_eq!(call(&catalog, "current")["required"], true);
    // Window of 4 starts at the older result; its call is outside.
    let catalog = offered(4, true);
    assert!(catalog["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .all(|card| card["pairs"] != json!(["older"])));
}
