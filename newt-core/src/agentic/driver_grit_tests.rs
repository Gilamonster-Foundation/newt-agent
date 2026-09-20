//! #2449: worker startup cannot retune a captured external turn's retry budget.
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use wiremock::{matchers::method, Mock, MockServer, Request, ResponseTemplate};

#[tokio::test]
async fn grit_2449_driver_captures_numeric_budget_before_worker_start() {
    let _settings = crate::test_guard::GlobalSettingsGuard::acquire();
    crate::tenacity::set_cli_tenacity(crate::Tenacity::Grit);
    crate::tenacity::set_tenacity_config(crate::tenacity::TenacityConfig {
        budgets: crate::tenacity::TenacityBudgets { grit_retries: 1 },
        ..Default::default()
    });
    let workspace = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let count = Arc::new(AtomicUsize::new(0));
    let responder_count = count.clone();
    Mock::given(method("POST")).respond_with(move |_: &Request| {
        let step = responder_count.fetch_add(1, Ordering::SeqCst);
        let body = if step == 0 {
            serde_json::json!({"message":{"role":"assistant","content":"", "tool_calls":[
                {"function":{"name":"read_file","arguments":{"path":"missing.txt"}}}]}, "done":true})
        } else {
            serde_json::json!({"message":{"role":"assistant","content":"The missing file is expected; no change is needed."}, "done":true})
        };
        ResponseTemplate::new(200).set_body_json(body)
    }).mount(&server).await;
    let mut config = TurnDriverConfig::new(
        server.uri(),
        "fixture",
        BackendKind::Ollama,
        workspace.path().to_string_lossy(),
    );
    config.max_tool_rounds = 8;
    config.workflow_grace_rounds = 0;
    let mut driver = TurnDriver::new(config);
    // Publication after driver capture but before its worker is started must
    // govern the next capture, not this already-captured driver posture.
    crate::tenacity::set_tenacity_config(crate::tenacity::TenacityConfig {
        budgets: crate::tenacity::TenacityBudgets { grit_retries: 0 },
        ..Default::default()
    });
    driver
        .submit("Read missing.txt and explain whether its absence is expected.")
        .unwrap();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            match driver.poll() {
                TurnStatus::Completed(outcome) => break outcome,
                TurnStatus::Failed(error) => panic!("driver failed: {error}"),
                _ => tokio::time::sleep(std::time::Duration::from_millis(5)).await,
            }
        }
    })
    .await
    .unwrap();
    assert!(!workspace.path().join("missing.txt").exists());
    assert_eq!(
        count.load(Ordering::SeqCst),
        3,
        "tool request + initial conclusion + one originally captured corrective continuation"
    );
    assert_eq!(outcome.end_reason, Some(crate::TurnEndReason::Completed));
}
