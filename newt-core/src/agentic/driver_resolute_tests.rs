//! #2451: the headless boundary captures the policy that the real loop records.
use super::*;
use crate::agentic::tools::disable_ocap_tests::{env_lock, EnvVar};

#[tokio::test]
#[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
async fn resolute_2451_headless_policy_and_receipt_survive_republication() {
    let _env = env_lock().await;
    let _globals = crate::test_guard::GlobalSettingsGuard::acquire();
    let _verify = EnvVar::set("NEWT_SELF_VERIFY", "0");
    let _outcomes = EnvVar::set("NEWT_VERIFY_OUTCOMES", "0");
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"role":"assistant", "content":"done"}, "done":true
            })),
        )
        .mount(&server)
        .await;
    let workspace = tempfile::tempdir().unwrap();
    let mut config = TurnDriverConfig::new(
        server.uri(),
        "test-model",
        BackendKind::Ollama,
        workspace.path().to_string_lossy(),
    );
    config.max_tool_rounds = 4;
    crate::tenacity::set_cli_tenacity(crate::Tenacity::Resolute);
    let driver = TurnDriver::new(config);
    crate::tenacity::set_cli_tenacity(crate::Tenacity::Normal);
    crate::Config::default().publish_runtime_settings();
    let _changed = EnvVar::set("NEWT_SELF_VERIFY", "1");
    let outcome = run_one_turn(
        &driver.config,
        &driver.runtime,
        &[MemMessage::user("Complete the task.")],
        "Complete the task.",
    )
    .await
    .unwrap();
    assert_eq!(
        outcome.end_reason,
        Some(crate::TurnEndReason::VerificationIncomplete)
    );
    assert!(outcome.reply.contains("verification"));
    assert_eq!(
        outcome.verification,
        Some(serde_json::json!({
            "mode":"result_aware", "repair_allowance":3, "required_by_tenacity":true
        }))
    );
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "empty checks cannot spin"
    );
    assert_eq!(
        crate::tenacity::effective_tenacity(),
        crate::Tenacity::Normal
    );
}
