//! #2449: exhausted non-check recovery is a scored attempt, not a harness error.
use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn grit_2449_headless_recovery_failure_remains_a_completed_attempt() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let served = calls.clone();
    Mock::given(method("POST")).and(path("/api/chat"))
        .respond_with(move |_: &Request| {
            let first = served.fetch_add(1, Ordering::SeqCst) == 0;
            let message = if first {
                serde_json::json!({"role":"assistant", "content":"", "tool_calls":[{
                    "function":{"name":"read_file", "arguments":{"path":"missing.txt"}}}]})
            } else {
                serde_json::json!({"role":"assistant", "content":"The requested file is missing; the task remains unresolved."})
            };
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"message":message,"done":true}))
        }).mount(&server).await;
    let mut command = common::newt();
    command.timeout(std::time::Duration::from_secs(30));
    let home = command.home().to_path_buf();
    common::isolate_loopback_chat(&mut *command, &home);
    let workspace = home.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let config = home.join("grit.toml");
    let instruction = home.join("instruction.md");
    let events = home.join("events.jsonl");
    std::fs::write(
        &config,
        "[tenacity]\nversion = 2\n[tenacity.budgets]\ngrit_retries = 0\n",
    )
    .unwrap();
    std::fs::write(
        &instruction,
        "Read missing.txt and finish the requested assessment.",
    )
    .unwrap();
    let output = command
        .env("NEWT_SELF_VERIFY", "0")
        .env("NEWT_VERIFY_OUTCOMES", "0")
        .env("NEWT_NUDGE", "off")
        .arg("--config")
        .arg(&config)
        .args(["--tenacity", "grit"])
        .args(["--backend-endpoint", &server.uri()])
        .args(["--backend-model", "fixture", "--backend-kind", "ollama"])
        .args(["headless", "--cwd"])
        .arg(&workspace)
        .arg("--instruction-file")
        .arg(&instruction)
        .arg("--events")
        .arg(&events)
        .args(["--max-rounds", "8"])
        .output()
        .unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "zero Grit refuses a corrective request"
    );
    assert!(!workspace.join("missing.txt").exists());
    let result = solve_result_from(&events);
    assert_eq!(result["end_reason"], "Some(Failed)");
    assert_eq!(
        result["status"], "incomplete",
        "recovery exhaustion is not an infrastructure failure"
    );
    let contract = contract_from(&events);
    assert_eq!(contract["outcome"], "completed");
    assert_eq!(contract["receipt"]["recovery"]["tenacity"], "grit");
    assert_eq!(contract["receipt"]["recovery"]["grit_retries"], 0);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Harness: recovery incomplete"));
}
