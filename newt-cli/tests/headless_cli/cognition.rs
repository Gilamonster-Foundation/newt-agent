use super::*;

async fn assert_semantic_chat(allowance: Option<u32>, capable: bool) {
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
    let declaration = if capable {
        "[backends.capability.chat_completions]\ncognition = true\nchat_template_kwargs = true\n"
    } else {
        ""
    };
    std::fs::write(&config, format!(
        "default_backend = \"strict\"\n[[backends]]\nname = \"strict\"\nendpoint = \"{}\"\nmodel = \"arbitrary-served-model\"\nkind = \"openai\"\napi = \"chat_completions\"\n{declaration}", server.uri()
    )).unwrap();
    std::fs::write(&instruction, "Finish without calling a tool.\n").unwrap();
    let mut command = Command::cargo_bin("newt").unwrap();
    command
        .env_remove("NEWT_TEAM")
        .args(["--cognition", "meticulous", "--config"])
        .arg(&config)
        .args(["headless", "--cwd"])
        .arg(fixture.path())
        .arg("--instruction-file")
        .arg(&instruction)
        .arg("--events")
        .arg(&events)
        .args(["--max-rounds", "1", "--context-window", "32768"]);
    if let Some(tokens) = allowance {
        command.args(["--output-allowance", &tokens.to_string()]);
    }
    command.assert().success();
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    for key in ["reasoning_effort", "reasoning"] {
        assert!(requests[0].get(key).is_none(), "Responses-only field {key}");
    }
    let contract = contract_from(&events);
    if capable {
        assert_eq!(requests[0]["max_tokens"], allowance.unwrap_or(16_000));
        assert_eq!(requests[0]["chat_template_kwargs"]["enable_thinking"], true);
        assert_eq!(contract["effective_config"]["cognition"], "meticulous");
    } else {
        for key in ["max_tokens", "chat_template_kwargs", "temperature", "top_p"] {
            assert!(
                requests[0].get(key).is_none(),
                "strict request acquired {key}: {}",
                requests[0]
            );
        }
        assert_eq!(contract["effective_config"]["cognition"], "default");
    }
    let expected_allowance = allowance.or(capable.then_some(16_000));
    match expected_allowance {
        None => assert!(contract["effective_config"]
            .get("output_allowance")
            .is_none()),
        Some(tokens) => assert_eq!(
            contract["effective_config"]["output_allowance"],
            serde_json::json!({
                "tokens": tokens, "enforced": if capable { "server" } else { "local" }
            })
        ),
    }
    assert_eq!(
        contract["effective_config"]["semantic_cognition"], "meticulous",
        "captured intent stays separate from accepted wire projection"
    );
}

/// Suite #2449: the actual process and HTTP request ground the projection
/// tests. Unknown Chat retains no cognition-derived fields or reservation.
#[tokio::test(flavor = "multi_thread")]
async fn semantic_cognition_survives_unknown_chat_without_changing_admission() {
    assert_semantic_chat(None, false).await;
}

/// Suite #2449: independent explicit-allowance case, not hidden behind the
/// no-override case's first failing assertion.
#[tokio::test(flavor = "multi_thread")]
async fn semantic_cognition_preserves_explicit_output_allowance_on_strict_chat() {
    assert_semantic_chat(Some(3_000), false).await;
}

/// Suite #2449: supported twin control keeps existing Meticulous projection
/// and lets explicit operator output allowance override the cognition table.
#[tokio::test(flavor = "multi_thread")]
async fn semantic_cognition_preserves_supported_chat_projection() {
    assert_semantic_chat(None, true).await;
    assert_semantic_chat(Some(3_000), true).await;
}
