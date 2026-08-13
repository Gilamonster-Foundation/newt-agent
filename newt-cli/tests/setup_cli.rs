//! Process-level coverage for `newt setup <host-or-url>`.

use assert_cmd::Command;
use predicates::prelude::*;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
#[ignore = "real-resource: weekly/release tier; spawns newt and touches the filesystem"]
async fn setup_url_probes_with_token_reference_and_writes_backend_dropin() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer secret-value"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "served-model"}]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer secret-value"))
        .and(body_json(serde_json::json!({
            "model": "served-model",
            "messages": [{"role": "user", "content": "Reply with OK."}],
            "max_tokens": 8,
            "stream": false
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "hi"}}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let config_dir = tempfile::tempdir().unwrap();

    Command::cargo_bin("newt")
        .unwrap()
        .env("DGX_SETUP_TEST_TOKEN", "secret-value")
        .arg("--config-dir")
        .arg(config_dir.path())
        .args([
            "setup",
            &server.uri(),
            "--token-env",
            "DGX_SETUP_TEST_TOKEN",
            "--yes",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Detected 1 inference backend"))
        .stdout(predicate::str::contains("secret-value").not())
        .stderr(predicate::str::contains("secret-value").not());

    let config_path = config_dir.path().join("config.toml");
    let config = newt_core::Config::load(&config_path).unwrap();
    let backend_name = format!("127-0-0-1-{}", server.address().port());
    assert_eq!(
        config.default_backend.as_deref(),
        Some(backend_name.as_str())
    );
    let backend_path = config_dir
        .path()
        .join("backends")
        .join(format!("{backend_name}.toml"));
    let text = std::fs::read_to_string(&backend_path).unwrap();
    let backend: newt_core::BackendConfig = toml::from_str(&text).unwrap();
    assert_eq!(backend.endpoint, server.uri());
    assert_eq!(backend.effective_model(), Some("served-model"));
    assert_eq!(backend.kind, Some(newt_core::BackendKind::Openai));
    assert_eq!(backend.api_key_env.as_deref(), Some("DGX_SETUP_TEST_TOKEN"));
    assert!(!text.contains("secret-value"));

    Command::cargo_bin("newt")
        .unwrap()
        .arg("--config-dir")
        .arg(config_dir.path())
        .arg("config")
        .assert()
        .success()
        .stdout(predicate::str::contains(server.uri()))
        .stdout(predicate::str::contains("served-model"));
}

// Model: GPT-5 | Harness: Codex | Operator: Shawn Hartsock | Time: 13:18 EDT | Date: 2026-08-12

#[tokio::test]
async fn setup_url_failure_writes_nothing() {
    let server = MockServer::start().await;
    let config_dir = tempfile::tempdir().unwrap();

    Command::cargo_bin("newt")
        .unwrap()
        .env("NEWT_CONFIG", "")
        .arg("--config-dir")
        .arg(config_dir.path())
        .args(["setup", &server.uri(), "--yes"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no supported inference API"));

    assert!(!config_dir.path().join("config.toml").exists());
    assert!(!config_dir.path().join("backends").exists());
}

#[test]
fn setup_target_rejects_explicit_config_before_writing() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("custom.toml");

    Command::cargo_bin("newt")
        .unwrap()
        .arg("--config")
        .arg(&config_path)
        .args(["setup", "127.0.0.1:9", "--yes"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("use --config-dir"));

    assert!(!config_path.exists());
    assert!(!dir.path().join("backends").exists());
}

#[test]
fn setup_target_rejects_newt_config_before_writing() {
    let dir = tempfile::tempdir().unwrap();
    let explicit_config = dir.path().join("explicit.toml");
    let config_dir = dir.path().join("managed");

    Command::cargo_bin("newt")
        .unwrap()
        .env("NEWT_CONFIG", &explicit_config)
        .arg("--config-dir")
        .arg(&config_dir)
        .args(["setup", "127.0.0.1:9", "--yes"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("NEWT_CONFIG"))
        .stderr(predicate::str::contains("use --config-dir"));

    assert!(!explicit_config.exists());
    assert!(!config_dir.join("config.toml").exists());
    assert!(!config_dir.join("backends").exists());
}

#[test]
fn setup_target_requires_yes_when_stdin_is_not_a_terminal() {
    let config_dir = tempfile::tempdir().unwrap();

    Command::cargo_bin("newt")
        .unwrap()
        .arg("--config-dir")
        .arg(config_dir.path())
        .args(["setup", "127.0.0.1:9"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("pass --yes"));

    assert!(!config_dir.path().join("config.toml").exists());
    assert!(!config_dir.path().join("backends").exists());
}

#[tokio::test]
#[ignore = "real-resource: weekly/release tier; spawns newt and touches the filesystem"]
async fn setup_v1_url_requires_generation_and_persists_canonical_token_reference() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer secret-value"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "secured-model"}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer secret-value"))
        .and(body_json(serde_json::json!({
            "model": "secured-model",
            "messages": [{"role": "user", "content": "Reply with OK."}],
            "max_tokens": 8,
            "stream": false
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "hi"}}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let root = tempfile::tempdir().unwrap();
    let token_path = root.path().join("dgx.token");
    std::fs::write(&token_path, "secret-value\n").unwrap();

    let target = format!("{}/v1/", server.uri());
    let assertion = Command::cargo_bin("newt")
        .unwrap()
        .current_dir(root.path())
        .args([
            "--config-dir",
            "config",
            "setup",
            &target,
            "--token-file",
            "dgx.token",
            "--model",
            "secured-model",
            "--yes",
        ])
        .assert()
        .success();
    assertion
        .stdout(predicate::str::contains("generation accepted"))
        .stdout(predicate::str::contains("secret-value").not())
        .stderr(predicate::str::contains("secret-value").not());

    let name = format!("127-0-0-1-{}", server.address().port());
    let config_body = std::fs::read_to_string(root.path().join("config/config.toml")).unwrap();
    let body = std::fs::read_to_string(
        root.path()
            .join("config/backends")
            .join(format!("{name}.toml")),
    )
    .unwrap();
    let backend: newt_core::BackendConfig = toml::from_str(&body).unwrap();
    assert_eq!(backend.endpoint, server.uri());
    assert_eq!(
        backend.api_key_file.as_deref(),
        std::fs::canonicalize(&token_path).unwrap().to_str()
    );
    assert!(!config_body.contains("secret-value"));
    assert!(!body.contains("secret-value"));
}

#[tokio::test]
#[ignore = "real-resource: weekly/release tier; spawns newt and touches the filesystem"]
async fn setup_public_models_but_unauthorized_generation_writes_nothing() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "catalog-model"}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer rejected-secret"))
        .and(body_json(serde_json::json!({
            "model": "catalog-model",
            "messages": [{"role": "user", "content": "Reply with OK."}],
            "max_tokens": 8,
            "stream": false
        })))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "error": {"message": "unauthorized"}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let root = tempfile::tempdir().unwrap();
    let token_path = root.path().join("token");
    std::fs::write(&token_path, "rejected-secret\n").unwrap();
    let config_dir = root.path().join("config");
    let target = format!("{}/v1/", server.uri());

    Command::cargo_bin("newt")
        .unwrap()
        .arg("--config-dir")
        .arg(&config_dir)
        .args(["setup", &target, "--token-file"])
        .arg(&token_path)
        .arg("--yes")
        .assert()
        .failure()
        .stdout(predicate::str::contains("HTTP 401"))
        .stdout(predicate::str::contains("rejected-secret").not())
        .stderr(predicate::str::contains("minimal generation check"))
        .stderr(predicate::str::contains("rejected-secret").not());

    assert!(!config_dir.join("config.toml").exists());
    assert!(!config_dir.join("backends").exists());
}

#[tokio::test]
#[ignore = "real-resource: weekly/release tier; spawns newt and touches the filesystem"]
async fn setup_bare_host_without_token_discovers_every_live_configured_port() {
    let first = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "vllm-model"}]
        })))
        .mount(&first)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "hi"}}]
        })))
        .mount(&first)
        .await;
    let second = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "router-a"}, {"id": "router-b"}]
        })))
        .mount(&second)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "hi"}}]
        })))
        .mount(&second)
        .await;
    let config_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        config_dir.path().join("config.toml"),
        format!(
            "[discovery]\nhosts = []\nollama_ports = []\nvllm_ports = [{}, {}]\n",
            first.address().port(),
            second.address().port()
        ),
    )
    .unwrap();

    Command::cargo_bin("newt")
        .unwrap()
        .arg("--config-dir")
        .arg(config_dir.path())
        .args(["setup", "127.0.0.1", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Detected 2 inference backends"));

    assert_eq!(
        std::fs::read_dir(config_dir.path().join("backends"))
            .unwrap()
            .count(),
        2
    );
    Command::cargo_bin("newt")
        .unwrap()
        .arg("--config-dir")
        .arg(config_dir.path())
        .arg("config")
        .assert()
        .success()
        .stdout(predicate::str::contains("vllm-model"))
        .stdout(predicate::str::contains("router-a"));
}

#[test]
fn setup_help_explains_host_expansion_and_authenticated_target_scope() {
    Command::cargo_bin("newt")
        .unwrap()
        .args(["setup", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Bare hosts expand"))
        .stdout(predicate::str::contains("Requires an explicit HTTPS URL"));
}
