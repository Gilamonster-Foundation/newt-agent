use super::*;

/// Suite #2449: the actual process must emit build/config identity after a real
/// scripted HTTP turn; pure contract fixtures alone cannot establish this boundary.
#[tokio::test(flavor = "multi_thread")]
async fn actual_producer_v3_trace_has_exact_build_and_config_identity() {
    let server = MockServer::start().await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(CaptureThenFinish {
            requests: requests.clone(),
        })
        .mount(&server)
        .await;
    let fixture = tempfile::tempdir().unwrap();
    let config = fixture.path().join("config.toml");
    let instruction = fixture.path().join("instruction.md");
    let events = fixture.path().join("events.jsonl");
    std::fs::write(&config, format!(
        "default_backend = \"fixture\"\n[[backends]]\nname = \"fixture\"\nendpoint = \"{}\"\nmodel = \"fixture-model\"\nkind = \"openai\"\napi = \"chat_completions\"\n", server.uri()
    )).unwrap();
    std::fs::write(&instruction, "Finish without calling a tool.\n").unwrap();
    let mut command = Command::cargo_bin("newt").unwrap();
    common::isolate_loopback_chat(&mut command, fixture.path());
    command
        .timeout(std::time::Duration::from_secs(30))
        .args([
            "--tenacity",
            "normal",
            "--cognition",
            "meticulous",
            "--config",
        ])
        .arg(&config)
        .args(["headless", "--cwd"])
        .arg(fixture.path())
        .arg("--instruction-file")
        .arg(&instruction)
        .arg("--events")
        .arg(&events)
        .args(["--max-rounds", "1", "--context-window", "32768"]);
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let sent = requests.lock().unwrap();
    assert_eq!(
        sent.len(),
        1,
        "the scripted endpoint actually served the turn"
    );
    assert!(sent[0].get("reasoning_effort").is_none());
    let trace = std::fs::read(&events).unwrap();
    // Optional local integration export copies ACTUAL emitted bytes, never a
    // synthetic record. The caller owns retaining these beside the bound row.
    if let Some(directory) = std::env::var_os("NEWT_TEST_V3_EXPORT_DIR") {
        let directory = std::path::PathBuf::from(directory);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("producer-events.jsonl"), &trace).unwrap();
        std::fs::write(directory.join("producer-stdout.txt"), &output.stdout).unwrap();
        let candidate = std::path::Path::new(env!("CARGO_BIN_EXE_newt"));
        let bytes = std::fs::read(candidate).unwrap();
        // Retain exact candidate bytes before another Cargo invocation can
        // replace the executable; the independent consumer hashes this copy.
        let retained_candidate = directory.join("producer-candidate.bin");
        std::fs::write(&retained_candidate, &bytes).unwrap();
        let identity = serde_json::json!({
            "path": retained_candidate,
            "source_path": candidate,
            "content_id": content_addressable::RawContentId::from_content(&bytes).to_string(),
        });
        std::fs::write(
            directory.join("producer-candidate.json"),
            identity.to_string(),
        )
        .unwrap();
    }
    let contract = contract_from(&events);
    assert_eq!(contract["contract_version"], "3", "{contract}");
    assert_eq!(
        contract["agent_version"],
        newt_core::build_info::VERSION_WITH_COMMIT
    );
    let expected = content_addressable::ContentId::from_canonical_bytes(
        &content_addressable::canonical::to_canonical_dagcbor(&contract["effective_config"])
            .unwrap(),
    );
    assert_eq!(contract["config_digest"], expected.to_string());
    assert_eq!(
        contract["effective_config"]["semantic_cognition"],
        "meticulous"
    );
    assert_eq!(contract["effective_config"]["cognition"], "default");
    assert_eq!(
        contract["effective_config"]["verification"],
        contract["receipt"]["verification"]
    );
}

/// Suite #2449: a runtime usize budget must not silently become max_rounds=0
/// in the u32 contract. Refuse it before any model request or event append.
#[tokio::test(flavor = "multi_thread")]
async fn policy_identity_refuses_unrepresentable_round_cap_before_dispatch() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}]
        })))
        .mount(&server)
        .await;
    let fixture = tempfile::tempdir().unwrap();
    let instruction = fixture.path().join("instruction.md");
    let events = fixture.path().join("events.jsonl");
    std::fs::write(&instruction, "Finish without calling a tool.\n").unwrap();
    let mut command = Command::cargo_bin("newt").unwrap();
    common::isolate_loopback_chat(&mut command, fixture.path());
    command
        .timeout(std::time::Duration::from_secs(30))
        .args(["--tenacity", "normal", "--backend-endpoint", &server.uri()])
        .args([
            "--backend-kind",
            "openai",
            "--backend-model",
            "fixture-model",
        ])
        .args(["headless", "--cwd"])
        .arg(fixture.path())
        .arg("--instruction-file")
        .arg(&instruction)
        .arg("--events")
        .arg(&events)
        .args(["--max-rounds", "4294967296"]);
    let output = command.output().unwrap();
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        (output.status.success(), requests.len()),
        (false, 0),
        "unrepresentable budget must fail before inference: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!events.exists(), "no misleading contract can be emitted");
    if usize::BITS > 32 {
        assert!(String::from_utf8_lossy(&output.stderr).contains("max-rounds exceeds"));
    }
}
