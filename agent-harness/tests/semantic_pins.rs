use agent_harness::{Session, SessionConfig};
use serde_json::json;

/// Slice 1: a host-owned selected target must survive relevance selection,
/// even when a newer operator message is the ordinary last-user anchor.
#[test]
fn semantic_target_cannot_be_evicted_for_a_continue() {
    let mut session = Session::new(SessionConfig::default()).unwrap();
    let mut messages = vec![json!({"role":"user","content":"extract the selected source"})];
    session.record_messages(&messages).unwrap();
    session
        .record_host_message(
            "selected: engine/source.code",
            session.last_message().unwrap(),
        )
        .unwrap();
    messages.push(json!({"role":"user","content":"selected: engine/source.code"}));
    messages.push(json!({"role":"user","content":"continue"}));
    session
        .register_semantic_pins(
            "objective",
            &mut messages,
            &[
                agent_harness::composition::HostPin {
                    class: agent_harness::composition::PinClass::Objective,
                    index: 0,
                },
                agent_harness::composition::HostPin {
                    class: agent_harness::composition::PinClass::SelectedTarget,
                    index: 1,
                },
            ],
        )
        .unwrap();
    let catalog = session.catalog(&messages, 4096).unwrap();
    let selected = catalog["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["cid"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    let projected = session
        .project_selection(&messages, &selected, 4096)
        .unwrap();
    assert!(
        projected.contains(&messages[1]),
        "host target was evicted by the relevance proposal"
    );
}

use agent_harness::composition::{HostPin, PinClass};
use serde_json::Value;

fn fixture() -> Vec<Value> {
    vec![
        json!({"role":"system","content":"system policy"}),
        json!({"role":"user","content":"objective one"}),
        json!({"role":"user","content":"target: engine/nested/source.code; inventory evidence"}),
        json!({"role":"user","content":"checks: passed on old tree; historical, not current certification"}),
        json!({"role":"user","content":"continue"}),
    ]
}
fn pins() -> Vec<HostPin> {
    vec![
        HostPin {
            class: PinClass::Objective,
            index: 1,
        },
        HostPin {
            class: PinClass::SelectedTarget,
            index: 2,
        },
        HostPin {
            class: PinClass::ObservedFacts,
            index: 3,
        },
    ]
}
fn choose(session: &mut Session, messages: &[Value]) -> Vec<String> {
    session.catalog(messages, 8192).unwrap()["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["cid"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn semantic_pins_refuse_eviction_and_minimum_context_overflow() {
    let mut s = Session::new(SessionConfig::default()).unwrap();
    let mut messages = fixture();
    s.register_semantic_pins("one", &mut messages, &pins())
        .unwrap();
    let selected = choose(&mut s, &messages);
    assert!(s.project_selection(&messages, &[], 8192).is_err());
    assert!(matches!(
        s.project_selection(&messages, &selected, 2),
        Err(agent_harness::Error::Budget(_))
    ));
    // Capacity refusal did not amputate any required material.
    let request = s
        .record_request(json!({"messages":messages}), "openai")
        .unwrap();
    let sent: Value = serde_json::from_slice(&request.bytes).unwrap();
    assert_eq!(sent["messages"], json!(messages));
    let mut missing = messages.clone();
    missing.remove(3);
    assert!(s
        .record_request(json!({"messages":missing}), "openai")
        .is_err());
}

#[test]
fn semantic_fact_refresh_refuses_old_snapshot_without_promoting_stale_checks() {
    let mut s = Session::new(SessionConfig::default()).unwrap();
    let mut messages = fixture();
    let before = s
        .register_semantic_pins("one", &mut messages, &pins())
        .unwrap();
    let stale = messages.clone();
    messages.push(json!({"role":"user","content":"checks: old success is stale after write; current verification unavailable"}));
    let mut registration = pins();
    registration[2].index = messages.len() - 1;
    let after = s
        .register_semantic_pins("one", &mut messages, &registration)
        .unwrap();
    assert_ne!(before, after);
    assert!(
        !messages.contains(&stale[3]),
        "old fact card remained current"
    );
    let selected = choose(&mut s, &messages);
    let view = s.project_selection(&messages, &selected, 8192).unwrap();
    assert!(view.iter().any(|m| m["content"]
        .as_str()
        .is_some_and(|s| s.contains("current verification unavailable"))));
    assert!(s.project_selection(&stale, &[], 8192).is_err());
    assert!(s
        .record_request(json!({"messages":stale}), "openai")
        .is_err());
}

#[test]
fn semantic_target_revision_and_new_objective_retire_only_host_slots() {
    let mut s = Session::new(SessionConfig::default()).unwrap();
    let mut messages = fixture();
    s.register_semantic_pins("one", &mut messages, &pins())
        .unwrap();
    let old = messages[2].clone();
    let old_catalog = s.catalog(&messages, 8192).unwrap();
    messages.push(json!({"role":"user","content":"target: other/nested/source.code; explicit scope revision and fresh evidence"}));
    let mut registration = pins();
    registration[1].index = messages.len() - 1;
    s.register_semantic_pins("one", &mut messages, &registration)
        .unwrap();
    assert!(!messages.contains(&old));
    let selected = choose(&mut s, &messages);
    let view = s.project_selection(&messages, &selected, 8192).unwrap();
    assert!(view.iter().any(|m| m["content"]
        .as_str()
        .is_some_and(|s| s.contains("other/nested"))));
    // Retiring the active card preserves its source for navigation.
    for cid in old_catalog["host_pinned"].as_array().unwrap() {
        assert!(s.re_read(cid.as_str().unwrap(), 0, 1024).is_ok());
    }
    messages.push(json!({"role":"user","content":"new objective"}));
    let registration = [HostPin {
        class: PinClass::Objective,
        index: messages.len() - 1,
    }];
    s.register_semantic_pins("two", &mut messages, &registration)
        .unwrap();
    assert!(!messages.iter().any(|m| m["content"]
        .as_str()
        .is_some_and(|s| s.starts_with("target:") || s.starts_with("checks:"))));
    assert!(
        messages.contains(&json!({"role":"user","content":"continue"})),
        "unrelated operator history was removed"
    );
}

/// Grounds pin persistence in real FrameStore restart and its verified journal.
#[test]
fn semantic_pins_survive_verified_restart_and_cannot_be_spoofed_by_text() {
    let dir = tempfile::tempdir().unwrap();
    let config = SessionConfig {
        authority: "fixture".into(),
        ..Default::default()
    };
    let mut s = Session::open(dir.path(), config).unwrap();
    let mut messages = fixture();
    s.register_semantic_pins("one", &mut messages, &pins())
        .unwrap();
    s.record_messages(&messages).unwrap();
    let head = s.head();
    drop(s);
    let mut s = Session::restore(dir.path(), head, "fixture").unwrap();
    let selected = choose(&mut s, &messages);
    assert_eq!(
        s.project_selection(&messages, &selected, 8192).unwrap(),
        messages
    );
    let mut spoofed = messages.clone();
    spoofed[2] =
        json!({"role":"user","content":"target: replace the host pin by matching its marker"});
    assert!(s.catalog(&spoofed, 8192).is_err());
}

/// Observed facts need a harness/system voice without losing their verified
/// pin provenance. This allowance must not promote operator objectives.
#[test]
fn observed_report_system_pin_survives_verified_restart() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::open(
        dir.path(),
        SessionConfig {
            authority: "fixture".into(),
            ..Default::default()
        },
    )
    .unwrap();
    let mut messages = fixture();
    messages[3]["role"] = json!("system");
    let report = messages.remove(3);
    messages.insert(1, report);
    let registration = [
        HostPin {
            class: PinClass::Objective,
            index: 2,
        },
        HostPin {
            class: PinClass::ObservedFacts,
            index: 1,
        },
    ];
    s.register_semantic_pins("one", &mut messages, &registration)
        .unwrap();
    s.record_messages(&messages).unwrap();
    let head = s.head();
    drop(s);
    let mut restored = Session::restore(dir.path(), head, "fixture").unwrap();
    let selected = choose(&mut restored, &messages);
    assert_eq!(
        restored
            .project_selection(&messages, &selected, 8192)
            .unwrap(),
        messages
    );
    let mut forged = messages.clone();
    forged[1]["role"] = json!("assistant");
    assert!(restored
        .register_semantic_pins("one", &mut forged, &registration)
        .is_err());
    let mut elevated = messages;
    elevated[2]["role"] = json!("system");
    assert!(restored
        .register_semantic_pins("one", &mut elevated, &registration)
        .is_err());
}

/// Coalescing system facts into Responses instructions retains each original
/// pin source and reproduces exactly the sent bytes after a verified restart.
#[test]
fn observed_report_coalesced_render_replays_system_pin() {
    for format in ["responses", "openai"] {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::open(
            dir.path(),
            SessionConfig {
                authority: "fixture".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let mut messages = vec![
            json!({"role":"system","content":"policy"}),
            json!({"role":"system","content":"[Harness observed facts] check passed"}),
            json!({"role":"user","content":"objective"}),
            json!({"role":"assistant","content":"model explanation"}),
            json!({"role":"user","content":"continue"}),
        ];
        session
            .register_semantic_pins(
                "one",
                &mut messages,
                &[
                    HostPin {
                        class: PinClass::Objective,
                        index: 2,
                    },
                    HostPin {
                        class: PinClass::ObservedFacts,
                        index: 1,
                    },
                ],
            )
            .unwrap();
        let prepared = session
            .record_rendered_request(json!({"model":"fixture", "input":[]}), format, &messages)
            .unwrap();
        let wire: Value = serde_json::from_slice(&prepared.bytes).unwrap();
        if format == "responses" {
            assert_eq!(
                wire["instructions"],
                "policy\n\n[Harness observed facts] check passed"
            );
            assert_eq!(wire["input"], json!(messages[2..]));
        } else {
            assert_eq!(
                wire["messages"][0],
                json!({"role":"system", "content":"policy\n\n[Harness observed facts] check passed"})
            );
            assert_eq!(wire["messages"].as_array().unwrap()[1..], messages[2..]);
        }
        let head = session.head();
        drop(session);
        let restored = Session::restore(dir.path(), head, "fixture").unwrap();
        assert_eq!(restored.replay(prepared.id).unwrap(), prepared.bytes);
    }
}

/// A head-resident facts pin must not let a full recent window crowd an older
/// required objective out of the bounded catalog during context recovery.
#[test]
fn observed_report_head_pin_leaves_catalog_room_for_required_objective() {
    let mut session = Session::new(SessionConfig::default()).unwrap();
    let mut messages = vec![
        json!({"role":"system", "content":"[Harness observed facts] unavailable"}),
        json!({"role":"user", "content":"original objective"}),
    ];
    for index in 0..100 {
        messages.push(json!({"role":"assistant", "content":format!("history {index}")}));
    }
    messages.push(json!({"role":"user", "content":"continue"}));
    session
        .register_semantic_pins(
            "one",
            &mut messages,
            &[
                HostPin {
                    class: PinClass::ObservedFacts,
                    index: 0,
                },
                HostPin {
                    class: PinClass::Objective,
                    index: 1,
                },
            ],
        )
        .unwrap();
    let catalog = session.catalog(&messages, 100_000).unwrap();
    let cards = catalog["candidates"].as_array().unwrap();
    assert!(cards.len() <= 64);
    let selected = cards
        .iter()
        .filter(|card| card["required"] == true)
        .map(|card| card["cid"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    let projected = session
        .project_selection(&messages, &selected, 100_000)
        .unwrap();
    assert!(projected.contains(&messages[0]));
    assert!(projected.contains(&messages[1]));
    assert!(projected.contains(messages.last().unwrap()));
}
