//! Real CLI grounding for migration diagnostic coalescing and stream routing.

mod common;

/// The lock failure and its in-memory translation are one read outcome, not
/// two warnings. This grounds the injected-filesystem reporter's coalescing
/// against Config::save's real lock and the CLI's real redirected streams.
#[test]
fn migration_lock_failure_is_one_plain_diagnostic_with_parseable_stdout() {
    let mut command = common::newt();
    let home = command.home().to_path_buf();
    let dir = home.join("config-root");
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("config.toml");
    let old = "[tenacity]\ndefault = \"standard\"\n";
    std::fs::write(&path, old).unwrap();
    let destination = newt_core::atomic_fs::ResolvedPath::resolve(&path).unwrap();
    let _lock = newt_core::atomic_fs::acquire_lock(&destination.lock_path()).unwrap();
    command
        .arg("--config-dir")
        .arg(&dir)
        .arg("--config")
        .arg(&path)
        .arg("config")
        .env("TERM", "dumb")
        .env("NO_COLOR", "1")
        .env("RUST_LOG", "off");
    let output = command.output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let _: toml::Value = toml::from_str(&stdout).expect("stdout remains a config document");
    let stderr = String::from_utf8(output.stderr).unwrap();
    let reports: Vec<_> = stderr
        .lines()
        .filter(|line| line.contains("migration") || line.contains("old psyche labels"))
        .collect();
    assert_eq!(
        reports.len(),
        1,
        "one attempt must produce one report: {stderr}"
    );
    assert!(stderr.contains("cannot lock"), "{stderr}");
    assert!(
        stderr.contains("standard"),
        "changes must survive: {stderr}"
    );
    assert!(
        stderr.contains("memory"),
        "loaded source must be clear: {stderr}"
    );
    assert!(!stderr.contains('\u{1b}'), "plain stderr must remain plain");
    assert_eq!(std::fs::read_to_string(path).unwrap(), old);
}

/// Rendering policy must not control whether a report exists. A real redirected
/// stderr stays redirected; forced color cannot contaminate the TOML stdout.
#[test]
fn migration_redirected_stderr_survives_plain_and_forced_color_pipes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.toml");
    let old = "[tenacity]\ndefault = \"standard\"\n";
    std::fs::write(&path, old).unwrap();
    for color in ["--mono", "--color=always"] {
        let log = dir.path().join(format!("{color}.log"));
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_newt"));
        common::isolate(&mut command, dir.path());
        let output = command
            .arg(color)
            .arg("--config")
            .arg(&path)
            .arg("config")
            .env("TERM", "dumb")
            .env("NO_COLOR", "1")
            .env("RUST_LOG", "off")
            .stderr(std::fs::File::create(&log).unwrap())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{output:?}; redirected stderr: {}",
            std::fs::read_to_string(&log).unwrap()
        );
        assert!(output.stderr.is_empty(), "stderr must remain redirected");
        let stdout = String::from_utf8(output.stdout).unwrap();
        let _: toml::Value = toml::from_str(&stdout).unwrap();
        assert!(!stdout.contains("old psyche labels"));
        let stderr = std::fs::read_to_string(log).unwrap();
        assert_eq!(stderr.matches("old psyche labels").count(), 1, "{stderr}");
        assert!(stderr.contains("original text in memory"), "{stderr}");
        assert!(!stderr.contains('\u{1b}'), "{stderr}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), old);
    }
}

/// Protocol mode must preserve migration stderr even though Notice::emit alone
/// is deliberately silent there. The worker replies remain valid JSON-RPC.
#[test]
fn migration_worker_protocol_keeps_json_stdout_and_plain_stderr() {
    let mut command = common::newt();
    let home = command.home().to_path_buf();
    let path = home.join("legacy.toml");
    std::fs::write(&path, "default_backend = \"fixture\"\n[[backends]]\nname = \"fixture\"\nkind = \"openai\"\nendpoint = \"http://127.0.0.1:1\"\nmodel = \"fixture\"\n[tenacity]\ndefault = \"standard\"\n").unwrap();
    let output = command
        .arg("--config")
        .arg(&path)
        .arg("worker")
        .arg("--operator-key-path")
        .arg(home.join("operator.key"))
        .env("RUST_LOG", "off")
        .env("TERM", "dumb")
        .env("NO_COLOR", "1")
        .write_stdin("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
        .timeout(std::time::Duration::from_secs(20))
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let records: Vec<serde_json::Value> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        records
            .iter()
            .any(|record| record["jsonrpc"] == "2.0" && record["id"] == 1),
        "{stdout}"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("old psyche labels"), "{stderr}");
    assert!(!stderr.contains('\u{1b}'), "{stderr}");
}

/// Grounds the headless config reader with the real binary and a loopback model;
/// migration diagnostics stay outside both the answer and JSONL receipt stream.
#[tokio::test(flavor = "multi_thread")]
async fn migration_headless_keeps_answer_and_receipt_clean() {
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "migration fixture answer"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 4}
        }))).mount(&server).await;
    let mut command = common::newt();
    let home = command.home().to_path_buf();
    common::isolate_loopback_chat(&mut *command, &home);
    let config = home.join("legacy.toml");
    std::fs::write(&config, "[tenacity]\ndefault = \"standard\"\n").unwrap();
    let instruction = home.join("task.md");
    std::fs::write(&instruction, "Say migration fixture answer.\n").unwrap();
    let events = home.join("events.jsonl");
    let output = command
        .arg("--config")
        .arg(&config)
        .args([
            "--backend-endpoint",
            &server.uri(),
            "--backend-model",
            "fixture",
            "--backend-kind",
            "openai",
        ])
        .args(["headless", "--cwd"])
        .arg(&home)
        .arg("--instruction-file")
        .arg(&instruction)
        .arg("--events")
        .arg(&events)
        .args(["--max-rounds", "1"])
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .env("RUST_LOG", "off")
        .timeout(std::time::Duration::from_secs(30))
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("migration fixture answer"), "{stdout}");
    assert!(!stdout.contains("old psyche labels"), "{stdout}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("old psyche labels"), "{stderr}");
    let receipts = std::fs::read_to_string(events).unwrap();
    assert!(!receipts.contains("old psyche labels"));
    for line in receipts.lines() {
        let _: serde_json::Value = serde_json::from_str(line).unwrap();
    }
}

/// Grounds MCP's own host collector, independently of the ACP worker. Persona
/// migration occurs inside connect_persona after CLI setup; both successful
/// service startup and later persona decode failure must preserve its notice.
#[test]
fn migration_mcp_host_reports_persona_before_decode_and_keeps_wire_clean() {
    for invalid in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let config_root = dir.path().join("config-root");
        let personas = config_root.join("personas");
        std::fs::create_dir_all(&personas).unwrap();
        let config = config_root.join("config.toml");
        // Configured providers short-circuit ambient Ollama discovery. No goal
        // request is sent, so this inert provider is never launched.
        std::fs::write(&config, "[[providers]]\nname = \"fixture\"\ncommand = \"unused-provider-fixture\"\nmodel = \"fixture\"\ntiers = [\"complex\"]\n").unwrap();
        let persona = personas.join("migration-wire.md");
        let old = format!(
            "+++\ncognition = \"pondering\"\n{}+++\nMigration persona body.\n",
            if invalid { "tools = 7\n" } else { "" }
        );
        std::fs::write(&persona, &old).unwrap();
        for first_read in [true, false] {
            let mut command = common::newt();
            common::isolate(&mut *command, dir.path());
            let output = command
                .arg("--config-dir")
                .arg(&config_root)
                .arg("--config")
                .arg(&config)
                .args(["--persona", "migration-wire", "mcp", "serve"])
                .env("TERM", "dumb")
                .env("NO_COLOR", "1")
                .env("RUST_LOG", "off")
                .write_stdin(
                    "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
                )
                .timeout(std::time::Duration::from_secs(20))
                .output()
                .unwrap();
            assert_eq!(output.status.success(), !invalid, "{output:?}");
            let stderr = String::from_utf8(output.stderr).unwrap();
            assert_eq!(
                stderr.matches("newt: migrated persona").count(),
                usize::from(first_read),
                "{stderr}"
            );
            assert!(!stderr.contains('\u{1b}'), "{stderr:?}");
            if first_read {
                assert!(stderr.contains(persona.to_str().unwrap()), "{stderr}");
            }
            let stdout = String::from_utf8(output.stdout).unwrap();
            if invalid {
                assert!(
                    stdout.trim().is_empty(),
                    "failed initialization cannot emit a reply: {stdout}"
                );
            } else {
                let records: Vec<serde_json::Value> = stdout
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
                assert!(
                    records
                        .iter()
                        .any(|record| record["jsonrpc"] == "2.0" && record["id"] == 1),
                    "{stdout}"
                );
            }
            assert_eq!(
                std::fs::read_to_string(&persona).unwrap(),
                newt_core::psyche_import::migrate_persona_text(&old)
                    .unwrap()
                    .text
            );
        }
    }
}
