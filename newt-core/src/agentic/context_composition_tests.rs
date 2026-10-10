use super::*;
use serde_json::json;

fn messages() -> Vec<Value> {
    vec![
        json!({"role":"system","content":"rules"}),
        json!({"role":"user","content":"old detail".repeat(150)}),
        json!({"role":"assistant","content":"prior answer"}),
        json!({"role":"user","content":"current task"}),
    ]
}
fn harness(enabled: bool) -> SmartHarness {
    SmartHarness::new(Session::new(crate::test_guard::unbudgeted_session_config()).unwrap(), Arc::new(|prompt| {
        let pages:Value=serde_json::from_str(prompt.lines().last().unwrap()).unwrap();
        let page=&pages[0];
        let cid=page["cards"].as_array().unwrap().iter().find(|c| c["preview"].as_str().unwrap().contains("old detail")).unwrap()["cid"].clone();
        let reply=json!({"catalog":page["catalog"],"expected_head":page["expected_head"],"changes":[{"cid":cid,"action":"park","reason":"older detail"}]}).to_string();
        Box::pin(async move {Ok((reply,None))})
    }),AdjudicationSettings{composition_enabled:enabled,..Default::default()}).unwrap()
}

#[test]
fn composition_off_by_default_and_not_advertised() {
    assert!(!AdjudicationSettings::default().composition_enabled);
    let h = harness(false);
    assert!(h.propose_context(&json!({})).is_err());
    assert!(!advertise(json!([]), Some(&h))
        .to_string()
        .contains("propose_context"));
    assert!(advertise(json!([]), Some(&harness(true)))
        .to_string()
        .contains("propose_context"));
}

#[test]
fn primary_composition_binds_all_four_provider_wires() {
    for wire in ["openai", "ollama", "anthropic", "responses"] {
        let h = harness(true);
        let m = messages();
        let field = if wire == "responses" {
            "input"
        } else {
            "messages"
        };
        let mut body = json!({"model":"fixture","messages":m});
        if field == "input" {
            body = json!({"model":"fixture","input":m});
        }
        if wire == "anthropic" {
            h.prepare_with_messages(&body, wire, &m).unwrap();
        } else {
            h.request(&body, wire).unwrap();
        }
        let page: Value = serde_json::from_str(&h.propose_context(&json!({})).unwrap()).unwrap();
        h.propose_context(&json!({"catalog":page["catalog"],"expected_head":page["expected_head"],"changes":[{"cid":page["cards"][1]["cid"],"action":"park","reason":"irrelevant older detail"}]})).unwrap();
        let bytes = if wire == "anthropic" {
            h.prepare_with_messages(&body, wire, &m).unwrap()
        } else {
            h.request(&body, wire).unwrap()
        };
        assert!(
            !String::from_utf8_lossy(&bytes).contains("old detail"),
            "{wire}"
        );
        assert_eq!(h.replay_last_request().unwrap(), bytes, "{wire}");
        let mut state = h.state().unwrap();
        let decision = state.session.composition_head().unwrap().unwrap();
        assert_eq!(
            state
                .session
                .composition_decision(decision)
                .unwrap()
                .outcome,
            agent_harness::composition::Outcome::Accepted
        );
        let bound = state.session.record_composed_request().unwrap();
        assert_eq!(bound.composition, Some(decision));
        assert_eq!(bound.bytes, bytes);
    }
}

#[tokio::test]
async fn auxiliary_uses_same_proposal_validator_then_binds_real_template() {
    let h = harness(true);
    let m = messages();
    let projected = h.project(&m, 400).await.unwrap();
    assert_eq!(projected.len(), 3);
    assert!(h.state().unwrap().session.has_pending_composition());
    let body = json!({"model":"fixture","messages":projected,"temperature":0});
    let sent = h.request(&body, "openai").unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&sent).unwrap(), body);
    assert!(!h.state().unwrap().session.has_pending_composition());
    assert_eq!(h.replay_last_request().unwrap(), sent);
}

#[tokio::test]
async fn malformed_auxiliary_stops_without_legacy_fallback() {
    let h = SmartHarness::new(
        Session::new(crate::test_guard::unbudgeted_session_config()).unwrap(),
        Arc::new(|_| Box::pin(async { Ok(("[]".into(), None)) })),
        AdjudicationSettings {
            composition_enabled: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(h.project(&messages(), 400).await.is_err());
    assert!(h
        .state()
        .unwrap()
        .session
        .composition_head()
        .unwrap()
        .is_none());
}

#[tokio::test(start_paused = true)]
async fn composition_auxiliary_cancellation_is_recorded_and_bounded() {
    let h = SmartHarness::new(
        Session::new(crate::test_guard::unbudgeted_session_config()).unwrap(),
        Arc::new(|_| Box::pin(std::future::pending())),
        AdjudicationSettings {
            composition_enabled: true,
            ..Default::default()
        },
    )
    .unwrap();
    let before = h.head().unwrap();
    let m = messages();
    tokio::select! {
        result=h.project(&m,400)=>panic!("composition completed before cancellation: {result:?}"),
        _=tokio::time::sleep(Duration::from_millis(10))=>{},
    }
    assert_ne!(h.head().unwrap(), before);
    assert!(!h.state().unwrap().session.has_pending_composition());
    assert!(h
        .state()
        .unwrap()
        .session
        .composition_head()
        .unwrap()
        .is_none());
    assert!(h.state().unwrap().auxiliary_elapsed >= Duration::from_millis(10));
}
