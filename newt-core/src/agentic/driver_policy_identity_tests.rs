/// Suite #2449: emitted numeric policy must describe the worker's pinned
/// threshold, even when a sibling publishes another config during inference.
/// Both successful and partial-error outcomes retain the captured value.
#[tokio::test]
async fn policy_identity_numeric_threshold_survives_mid_turn_publication() {
    use crate::initiative::{Initiative, InitiativeConfig, InitiativeRounds};
    let _settings = crate::test_guard::GlobalSettingsGuard::acquire();
    for succeeds in [true, false] {
        crate::initiative::set_initiative_config(InitiativeConfig {
            rounds: InitiativeRounds {
                measured: 7,
                ..Default::default()
            },
            ..Default::default()
        });
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(move |_: &Request| {
                crate::initiative::set_initiative_config(InitiativeConfig {
                    rounds: InitiativeRounds {
                        measured: 99,
                        ..Default::default()
                    },
                    ..Default::default()
                });
                if succeeds {
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "message":{"role":"assistant","content":"done"}, "done":true
                    }))
                } else {
                    // This tests captured error outcomes, not transport backoff.
                    ResponseTemplate::new(400).set_body_string("fixture inference failure")
                }
            })
            .mount(&server)
            .await;
        let mut driver = TurnDriver::new(cfg(&server.uri()))
            .with_tenacity(crate::Tenacity::Normal)
            .with_initiative(Initiative::Measured);
        driver.submit("finish without tools").unwrap();
        let TurnStatus::Completed(outcome) = pump_to_done(&mut driver).await else {
            panic!("worker must retain its success or partial-error outcome")
        };
        assert_eq!(outcome.error.is_none(), succeeds);
        assert!(!server.received_requests().await.unwrap().is_empty());
        assert_eq!(outcome.initiative_read_only_rounds, 7);
        assert_eq!(
            Initiative::Measured.read_only_nudge_after(),
            99,
            "the sibling publication occurred; capture must not leak back"
        );
    }
}
