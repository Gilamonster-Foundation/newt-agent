use super::*;
use std::ffi::{OsStr, OsString};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct EnvVarGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        let guard = Self {
            key,
            previous: std::env::var_os(key),
        };
        // SAFETY: every caller is in the `serial(real_fs)` lane.
        unsafe { std::env::set_var(key, value) };
        guard
    }

    fn remove(key: &'static str) -> Self {
        let guard = Self {
            key,
            previous: std::env::var_os(key),
        };
        // SAFETY: every caller is in the `serial(real_fs)` lane.
        unsafe { std::env::remove_var(key) };
        guard
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        // SAFETY: every caller is in the `serial(real_fs)` lane. Drop
        // restores state even when an assertion panics or `?` returns.
        unsafe {
            match self.previous.as_ref() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

// D1b-3 (#1913): the scripted console went with the `Console` trait.
// `setup::operator::Script` replaces it — same answers-in-order, same
// recorded output — and it hands out an `Operator` instead of
// implementing a trait, so a test cannot supply a reader of its own.
use super::operator::Script as ScriptedConsole;

/// Read the backend drop-in `<config dir>/backends/<name>.toml` the new
/// writer (#1140) produces beside the config file.
fn read_dropin(config_path: &std::path::Path, name: &str) -> BackendConfig {
    let p = config_path
        .with_file_name("backends")
        .join(format!("{name}.toml"));
    toml::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap()
}

async fn mount_openai_chat(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": "OK"}}]
        })))
        .mount(server)
        .await;
}

async fn mount_authenticated_openai_chat(server: &MockServer, token: &str) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(wiremock::matchers::header(
            "authorization",
            format!("Bearer {token}").as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": "OK"}}]
        })))
        .mount(server)
        .await;
}

async fn mount_ollama_chat(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message": {"content": "OK"},
            "done": true
        })))
        .mount(server)
        .await;
}

// --- pure helpers -----------------------------------------------------

#[test]
fn normalize_url_bare_host_gets_scheme_and_port() {
    assert_eq!(
        normalize_url("REDACTED-HOST", "http", 11434),
        "http://REDACTED-HOST:11434"
    );
}

#[test]
fn normalize_url_keeps_explicit_port_and_full_url() {
    assert_eq!(
        normalize_url("REDACTED-HOST:8000", "http", 11434),
        "http://REDACTED-HOST:8000"
    );
    assert_eq!(
        normalize_url("https://REDACTED-HOST/", "http", 11434),
        "https://REDACTED-HOST"
    );
}

#[test]
fn build_ollama_config_writes_dropin_pair_no_dgx() {
    // #1140: the wizard's chimera is dead — the result is ONE backend
    // drop-in + a minimal config pointing at it. No [dgx], no inline
    // [[backends]].
    let (cfg, backend) = build_ollama_config(
        Config::default(),
        "default",
        EndpointKind::Ollama,
        "http://127.0.0.1:11434",
        "qwen2.5-coder:7b",
    );
    assert!(cfg.dgx.is_none(), "no legacy [dgx] block ever again");
    assert!(cfg.backends.is_empty(), "the drop-in IS the backend list");
    assert_eq!(cfg.default_backend.as_deref(), Some("default"));
    assert_eq!(backend.endpoint, "http://127.0.0.1:11434");
    assert_eq!(backend.effective_model(), Some("qwen2.5-coder:7b"));
    assert_eq!(backend.kind, Some(BackendKind::Ollama));
    assert_eq!(backend.serving, Some(newt_core::Serving::Multiplexer));
    assert!(
        backend.provenance.is_some(),
        "generated files self-describe"
    );
}

#[test]
fn target_candidates_expand_a_bare_host_and_keep_an_explicit_url_single() {
    let discovery = newt_core::config::Discovery {
        hosts: vec![],
        ollama_ports: vec![11434],
        vllm_ports: vec![8000, 8080],
    };
    assert_eq!(
        candidate_endpoints("dgx1.home.arpa", &discovery).unwrap(),
        vec![
            "http://dgx1.home.arpa:11434",
            "http://dgx1.home.arpa:8000",
            "http://dgx1.home.arpa:8080",
        ]
    );
    assert_eq!(
        candidate_endpoints("http://dgx1.home.arpa:8080/v1", &discovery).unwrap(),
        vec!["http://dgx1.home.arpa:8080"]
    );
}

#[test]
fn target_candidates_deduplicate_ports_and_reject_credentials() {
    let discovery = newt_core::config::Discovery {
        hosts: vec![],
        ollama_ports: vec![8000],
        vllm_ports: vec![8000, 8080, 8080],
    };
    assert_eq!(
        candidate_endpoints("dgx1.home.arpa", &discovery).unwrap(),
        vec!["http://dgx1.home.arpa:8000", "http://dgx1.home.arpa:8080",]
    );
    assert!(
        candidate_endpoints("http://user:secret@dgx1.home.arpa:8000", &discovery)
            .unwrap_err()
            .to_string()
            .contains("credentials")
    );
}

#[test]
fn authenticated_targets_require_an_explicit_secure_transport() {
    assert!(validate_authenticated_target("dgx1.home.arpa:8000").is_err());
    assert!(validate_authenticated_target("http://dgx1.home.arpa:8000").is_err());
    assert!(validate_authenticated_target("https://dgx1.home.arpa:8000").is_ok());
    assert!(validate_authenticated_target("http://127.0.0.1:8000").is_ok());
    assert!(validate_authenticated_target("http://[::1]:8000").is_ok());
}

#[tokio::test]
async fn preset_retry_path_revalidates_transport_before_sending_a_key() {
    let preset = ProviderPreset {
        name: "remote-plaintext".into(),
        base_url: "http://192.0.2.10/v1".into(),
        env_vars: vec!["UNUSED_TEST_KEY".into()],
        ..Default::default()
    };
    let cred = WizardCred {
        api_key_env: None,
        api_key_file: None,
        probe_key: Some(Secret::new("replacement-secret")),
        pending_token: None,
    };
    let console = ScriptedConsole::new(&[]);
    let result = verify_key_with_retries(
        &console.operator(),
        &reqwest::Client::new(),
        &preset,
        Path::new("config.toml"),
        "model",
        cred,
    )
    .await;
    let error = match result {
        Ok(_) => panic!("remote plaintext credential should be rejected"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("refusing to send a bearer token"), "{error}");
}

#[tokio::test]
async fn tool_free_chat_verification_does_not_pin_chat_completions() {
    let server = MockServer::start().await;
    mount_openai_chat(&server).await;
    let hit = EndpointProbeResult {
        endpoint: server.uri(),
        kind: BackendKind::Openai,
        models: vec!["model".into()],
        serving: newt_core::Serving::Instance,
        engine: None,
        warm: vec![],
    };
    let console = ScriptedConsole::new(&[]);
    let (_, api) = verify_custom_chat_with_retries(
        &console.operator(),
        &reqwest::Client::new(),
        &hit,
        "model",
        None,
    )
    .await
    .unwrap();
    assert_eq!(api, None, "runtime tool-capability probe must choose Chat");
}

#[tokio::test]
async fn setup_never_renders_provider_error_refusal_or_bearer_material() {
    const BEARER_SENTINEL: &str = "setup-secret-must-not-escape";
    const BODY_SENTINEL: &str = "setup-provider-body-must-not-escape";
    let escape = char::from(27);
    let bell = char::from(7);
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(wiremock::matchers::header(
            "authorization",
            format!("Bearer {BEARER_SENTINEL}").as_str(),
        ))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(wiremock::matchers::header(
            "authorization",
            format!("Bearer {BEARER_SENTINEL}").as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "completed",
            "error": {"message": format!(
                "{BEARER_SENTINEL} {BODY_SENTINEL} {escape}[31mred{bell}"
            )},
            "output": [{
                "type": "message",
                "content": [{
                    "type": "refusal",
                    "refusal": format!("{BODY_SENTINEL} {escape}[2J")
                }]
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let hit = EndpointProbeResult {
        endpoint: server.uri(),
        kind: BackendKind::Openai,
        models: vec!["model".into()],
        serving: newt_core::Serving::Instance,
        engine: None,
        warm: vec![],
    };
    let console = ScriptedConsole::new(&[]);

    let error = verify_custom_chat_with_retries(
        &console.operator(),
        &reqwest::Client::new(),
        &hit,
        "model",
        Some(Secret::new(BEARER_SENTINEL)),
    )
    .await
    .unwrap_err();
    let transcript = console.transcript();
    let rendered_error = error.to_string();

    assert!(transcript.contains("Responses generation payload was unusable"));
    for rendered in [&transcript, &rendered_error] {
        assert!(!rendered.contains(BEARER_SENTINEL));
        assert!(!rendered.contains(BODY_SENTINEL));
        assert!(!rendered.contains(escape));
        assert!(!rendered.contains(bell));
    }
    assert!(console
        .output
        .borrow()
        .iter()
        .all(|line| !line.chars().any(char::is_control)));
    assert!(!rendered_error.chars().any(char::is_control));
}

#[tokio::test]
async fn authentication_retry_does_not_collect_an_untested_final_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(401))
        .expect(GENERATION_CHECK_ATTEMPTS as u64)
        .mount(&server)
        .await;
    let hit = EndpointProbeResult {
        endpoint: server.uri(),
        kind: BackendKind::Openai,
        models: vec!["model".into()],
        serving: newt_core::Serving::Instance,
        engine: None,
        warm: vec![],
    };
    let console = ScriptedConsole::new(&["first-key", "second-key", "must-remain"]);

    let error = verify_custom_chat_with_retries(
        &console.operator(),
        &reqwest::Client::new(),
        &hit,
        "model",
        None,
    )
    .await
    .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("authentication was rejected 3 times"),
        "{error:#}"
    );
    assert_eq!(
        console.next_answer().as_deref(),
        Some("must-remain"),
        "the final rejection must not prompt for a key setup cannot test"
    );
}

#[tokio::test]
async fn preset_authentication_retry_does_not_collect_an_untested_final_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(401))
        .expect(GENERATION_CHECK_ATTEMPTS as u64)
        .mount(&server)
        .await;
    let preset = ProviderPreset {
        name: "test-provider".into(),
        base_url: server.uri(),
        env_vars: vec!["UNUSED_TEST_KEY".into()],
        ..Default::default()
    };
    let cred = WizardCred {
        api_key_env: None,
        api_key_file: None,
        probe_key: Some(Secret::new("initial-key")),
        pending_token: None,
    };
    let console =
        ScriptedConsole::new(&["Y", "first-key", "", "Y", "second-key", "", "must-remain"]);

    let result = verify_key_with_retries(
        &console.operator(),
        &reqwest::Client::new(),
        &preset,
        Path::new("/unused/config.toml"),
        "model",
        cred,
    )
    .await;
    let error = match result {
        Ok(_) => panic!("the provider should reject every attempted key"),
        Err(error) => error,
    };

    assert!(
        error
            .to_string()
            .contains("authentication was rejected 3 times"),
        "{error:#}"
    );
    assert_eq!(
        console.next_answer().as_deref(),
        Some("must-remain"),
        "the preset flow must not collect a final untested key"
    );
}

/// Real-resource grounding for a late config-selection failure: once the
/// immutable token/backend tuple is published it remains coherent for
/// lock-free concurrent readers and an idempotent setup retry.
#[ignore = "real-resource: weekly/release tier; touches the filesystem"]
#[serial_test::serial(real_fs)]
#[test]
fn late_setup_write_failure_retains_a_coherent_backend_tuple() {
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("backends/example.token.age");
    let backend_path = dir.path().join("backends/example.toml");
    let config = dir.path().join("config.toml");
    std::fs::create_dir_all(token.parent().unwrap()).unwrap();
    std::fs::write(&token, b"old-token").unwrap();
    std::fs::write(&backend_path, b"old-backend").unwrap();
    std::fs::write(&config, b"old-config").unwrap();

    let versioned_token = dir.path().join("backends/example.token.version.age");
    let versioned_reference = collapse_home(&versioned_token);
    let backend = BackendConfig {
        name: "example".into(),
        endpoint: "https://inference.example.test".into(),
        model: Some("model".into()),
        kind: Some(BackendKind::Openai),
        api_key_file: Some(versioned_reference.clone()),
        ..Default::default()
    };
    let cfg = Config {
        default_backend: Some("example".into()),
        ..Default::default()
    };
    let pending = PendingWizardToken {
        token: Secret::new("new-secret"),
        passphrase: Some(newt_core::secrets::SecretString::from("test-passphrase")),
        path: versioned_token.clone(),
        reference: credentials::SealedSecret::new(&versioned_reference).unwrap(),
    };
    let console = ScriptedConsole::new(&[]);
    let result = persist_interactive_backend_with(
        &console.operator(),
        &config,
        &cfg,
        &backend,
        Some(&pending),
        |staged, destination| destination.durable_replace(staged),
        |_cfg, _path| anyhow::bail!("simulated late config failure"),
    );

    assert!(result.is_err());
    assert_eq!(std::fs::read(&token).unwrap(), b"old-token");
    assert_eq!(std::fs::read(&config).unwrap(), b"old-config");
    let committed: BackendConfig =
        toml::from_str(&std::fs::read_to_string(&backend_path).unwrap()).unwrap();
    assert_eq!(committed.api_key_file, backend.api_key_file);
    assert!(versioned_token.exists());
    assert_eq!(
        newt_core::secrets::resolve_token_file(&versioned_token)
            .unwrap()
            .as_deref(),
        Some("new-secret")
    );
    for directory in [dir.path(), token.parent().unwrap()] {
        let leftovers = std::fs::read_dir(directory)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".newt-"))
            .collect::<Vec<_>>();
        assert!(leftovers.is_empty(), "staging leftovers: {leftovers:?}");
    }
}

/// Real-filesystem grounding for the backend-publication failpoint: when
/// rename commits but parent sync fails, setup must retain the credential
/// prerequisite referenced by the now-visible backend.
#[ignore = "real-resource: weekly/release tier; touches the filesystem"]
#[serial_test::serial(real_fs)]
#[test]
fn backend_post_commit_sync_failure_retains_its_credential() {
    let dir = tempfile::tempdir().unwrap();
    let backend_dir = dir.path().join("backends");
    std::fs::create_dir_all(&backend_dir).unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(&config, "default_backend = \"old\"\n").unwrap();
    let versioned_token = backend_dir.join("example.token.version.age");
    let token_reference = collapse_home(&versioned_token);
    let backend = BackendConfig {
        name: "example".into(),
        endpoint: "https://inference.example.test".into(),
        model: Some("model".into()),
        kind: Some(BackendKind::Openai),
        api_key_file: Some(token_reference.clone()),
        ..Default::default()
    };
    let cfg = Config {
        default_backend: Some("example".into()),
        ..Default::default()
    };
    let pending = PendingWizardToken {
        token: Secret::new("new-secret"),
        passphrase: Some(newt_core::secrets::SecretString::from("test-passphrase")),
        path: versioned_token.clone(),
        reference: credentials::SealedSecret::new(&token_reference).unwrap(),
    };

    let error = persist_interactive_backend_with(
        &ScriptedConsole::new(&[]).operator(),
        &config,
        &cfg,
        &backend,
        Some(&pending),
        |staged, destination| {
            std::fs::rename(staged, destination.as_path()).unwrap();
            Err(newt_core::atomic_fs::DurableReplaceError::after_commit(
                destination.as_path(),
                io::Error::other("injected parent sync failure"),
            ))
        },
        |_, _| unreachable!("config selection must not follow publication failure"),
    )
    .unwrap_err();

    assert!(error.to_string().contains("published coherently"));
    assert_eq!(
        std::fs::read_to_string(&config).unwrap(),
        "default_backend = \"old\"\n"
    );
    let committed: BackendConfig =
        toml::from_str(&std::fs::read_to_string(backend_dir.join("example.toml")).unwrap())
            .unwrap();
    assert_eq!(committed.api_key_file, backend.api_key_file);
    assert_eq!(
        newt_core::secrets::resolve_token_file(&versioned_token)
            .unwrap()
            .as_deref(),
        Some("new-secret")
    );
}

/// A process may load the old drop-in before rotation and resolve its key
/// later. Setup therefore never synchronously garbage-collects immutable
/// credential versions when it publishes a replacement.
#[ignore = "real-resource: weekly/release tier; touches the filesystem"]
#[serial_test::serial(real_fs)]
#[test]
fn successful_rotation_retains_the_previous_credential_for_live_readers() {
    let dir = tempfile::tempdir().unwrap();
    let backend_dir = dir.path().join("backends");
    std::fs::create_dir_all(&backend_dir).unwrap();
    let config = dir.path().join("config.toml");
    let backend_path = backend_dir.join("example.toml");
    let old_token = backend_dir.join("example.token.1-1-1.age");
    std::fs::write(&old_token, "old-secret\n").unwrap();
    let old_reader = BackendConfig {
        name: "example".into(),
        endpoint: "https://old.example.test".into(),
        api_key_file: Some(old_token.display().to_string()),
        ..Default::default()
    };
    std::fs::write(&backend_path, toml::to_string(&old_reader).unwrap()).unwrap();
    std::fs::write(&config, "default_backend = \"example\"\n").unwrap();

    let new_token = backend_dir.join("example.token.2-2-2.age");
    let new_reference = new_token.display().to_string();
    let replacement = BackendConfig {
        name: "example".into(),
        endpoint: "https://new.example.test".into(),
        api_key_file: Some(new_reference.clone()),
        ..Default::default()
    };
    let pending = PendingWizardToken {
        token: Secret::new("new-secret"),
        passphrase: Some(newt_core::secrets::SecretString::from("test-passphrase")),
        path: new_token,
        reference: credentials::SealedSecret::new(&new_reference).unwrap(),
    };
    let cfg = Config {
        default_backend: Some("example".into()),
        ..Default::default()
    };
    persist_interactive_backend(
        &ScriptedConsole::new(&[]).operator(),
        &config,
        &cfg,
        &replacement,
        Some(&pending),
    )
    .unwrap();

    assert_eq!(old_reader.resolve_api_key().as_deref(), Some("old-secret"));
    assert!(old_token.exists());
}

/// Real-resource grounding for the shared setup lock: an interactive
/// writer must fail before it stages or commits any file.
#[ignore = "real-resource: weekly/release tier; touches the filesystem"]
#[serial_test::serial(real_fs)]
#[test]
fn interactive_setup_lock_rejects_a_concurrent_writer_before_staging() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    let held = acquire_setup_lock(&config).unwrap();
    let backend = BackendConfig {
        name: "example".into(),
        endpoint: "https://inference.example.test".into(),
        model: Some("model".into()),
        kind: Some(BackendKind::Openai),
        ..Default::default()
    };
    let cfg = Config {
        default_backend: Some("example".into()),
        ..Default::default()
    };
    let console = ScriptedConsole::new(&[]);

    let error = persist_interactive_backend_with(
        &console.operator(),
        &config,
        &cfg,
        &backend,
        None,
        |_, _| unreachable!("lock failure must precede backend publication"),
        |_, _| Ok(()),
    )
    .unwrap_err();

    assert!(error.to_string().contains("another live process"));
    assert!(!config.exists());
    assert!(!dir.path().join("backends").exists());
    drop(held);
}

#[test]
fn detected_backend_name_is_stable_and_filesystem_safe() {
    assert_eq!(
        backend_name("http://dgx1.home.arpa:8000").unwrap(),
        "dgx1-home-arpa-8000"
    );
    assert_eq!(
        backend_name("https://[2001:db8::1]:8080").unwrap(),
        "2001-db8-1-8080"
    );
}

fn openai_hit(endpoint: &str, models: &[&str]) -> newt_core::backend_probe::EndpointProbeResult {
    newt_core::backend_probe::EndpointProbeResult {
        endpoint: endpoint.to_string(),
        kind: BackendKind::Openai,
        models: models.iter().map(|m| (*m).to_string()).collect(),
        serving: newt_core::backend_probe::api_for(BackendKind::Openai).serving(models.len()),
        engine: None,
        warm: Vec::new(),
    }
}

#[test]
fn chat_setup_reuses_a_runtime_responses_writeback_but_not_the_reverse() {
    let probe = openai_hit("https://inference.example.test", &["model"]);
    let mut existing = ExistingSetupBackend {
        name: "example".into(),
        path: PathBuf::from("backends/example.toml"),
        endpoint: Some(probe.endpoint.clone()),
        api_key_env: None,
        api_key_file: None,
        kind: Some(BackendKind::Openai),
        api: Some(OpenAiApi::Responses),
        serving: Some(probe.serving),
        model: Some("model".into()),
        generated_by_setup: false,
        probe_owned: false,
    };

    assert!(existing.matches_probe(&VerifiedTargetHit {
        probe: probe.clone(),
        api: Some(OpenAiApi::ChatCompletions),
    }));
    assert!(existing.matches_probe(&VerifiedTargetHit {
        probe: probe.clone(),
        api: Some(OpenAiApi::Responses),
    }));
    existing.api = Some(OpenAiApi::ChatCompletions);
    assert!(!existing.matches_probe(&VerifiedTargetHit {
        probe,
        api: Some(OpenAiApi::Responses),
    }));
}

#[test]
fn every_injected_commit_failure_leaves_a_coherent_retryable_tuple() {
    #[derive(Clone, Default)]
    struct State {
        new_token_exists: bool,
        backend_token: &'static str,
        selected: bool,
    }

    fn coherent(state: &State) -> bool {
        state.backend_token == "old"
            || (state.backend_token == "versioned" && state.new_token_exists)
    }

    for fail_at in SETUP_COMMIT_STEPS {
        let mut state = State {
            backend_token: "old",
            ..Default::default()
        };
        let result = run_setup_commit(|step| {
            if step == fail_at {
                anyhow::bail!("injected {step:?} failure");
            }
            match step {
                SetupCommitStep::PersistVersionedToken => state.new_token_exists = true,
                SetupCommitStep::PublishBackendTuple => state.backend_token = "versioned",
                SetupCommitStep::SelectBackend => state.selected = true,
            }
            Ok(())
        });
        assert!(result.is_err());
        assert!(
            coherent(&state),
            "failure at {fail_at:?} exposed a mixed tuple"
        );

        // Replaying all idempotent phases models setup recovery after a
        // killed process and must converge on the selected new tuple.
        run_setup_commit(|step| {
            match step {
                SetupCommitStep::PersistVersionedToken => state.new_token_exists = true,
                SetupCommitStep::PublishBackendTuple => state.backend_token = "versioned",
                SetupCommitStep::SelectBackend => state.selected = true,
            }
            Ok(())
        })
        .unwrap();
        assert!(coherent(&state));
        assert!(state.selected);
    }
}

#[test]
fn detected_backend_carries_served_truth_and_secret_references_only() {
    let token_file = std::path::Path::new("~/.newt/tokens/dgx1");
    let backend = backend_from_probe(
        &openai_hit("http://dgx1.home.arpa:8080", &["qwen3-coder", "gpt-oss"]),
        Some("DGX_TOKEN"),
        Some(token_file),
    )
    .unwrap();
    assert_eq!(backend.name, "dgx1-home-arpa-8080");
    assert_eq!(backend.host.as_deref(), Some("dgx1.home.arpa"));
    assert_eq!(backend.effective_model(), Some("qwen3-coder"));
    assert_eq!(backend.serving, Some(newt_core::Serving::Multiplexer));
    assert_eq!(backend.api_key_env.as_deref(), Some("DGX_TOKEN"));
    assert_eq!(backend.api_key_file.as_deref(), Some("~/.newt/tokens/dgx1"));
    let rendered = toml::to_string(&backend).unwrap();
    assert!(!rendered.contains("secret-value"));
}

#[serial_test::serial(real_fs)]
#[test]
fn detected_setup_writes_all_backends_and_preserves_existing_config() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        "# keep this comment\ndefault_backend = \"old\"\n\n[tui]\nno_splash = true\n",
    )
    .unwrap();
    let hits = vec![
        openai_hit("http://dgx1.home.arpa:8000", &["ornith"]),
        openai_hit("http://dgx1.home.arpa:8080", &["qwen3-coder", "gpt-oss"]),
    ];

    let written = persist_detected_setup(&path, &hits, None, None).unwrap();
    assert_eq!(written.len(), 2);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("# keep this comment"));
    assert!(text.contains("[tui]\nno_splash = true"));
    assert_eq!(
        std::fs::read_to_string(path.with_file_name("config.toml.bak")).unwrap(),
        "# keep this comment\ndefault_backend = \"old\"\n\n[tui]\nno_splash = true\n"
    );
    let config = Config::load(&path).unwrap();
    assert_eq!(
        config.default_backend.as_deref(),
        Some("dgx1-home-arpa-8000")
    );
    let vllm = read_dropin(&path, "dgx1-home-arpa-8000");
    let router = read_dropin(&path, "dgx1-home-arpa-8080");
    assert_eq!(vllm.serving, Some(newt_core::Serving::Instance));
    assert_eq!(router.serving, Some(newt_core::Serving::Multiplexer));

    let config_before = text;
    let vllm_before = std::fs::read_to_string(&written[0]).unwrap();
    persist_detected_setup(&path, &hits, None, None).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), config_before);
    assert_eq!(std::fs::read_to_string(&written[0]).unwrap(), vllm_before);
}

#[serial_test::serial(real_fs)]
#[test]
fn detected_setup_suffixes_a_colliding_name_without_overwriting() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    let backend_dir = dir.path().join("backends");
    std::fs::create_dir_all(&backend_dir).unwrap();
    let occupied = backend_dir.join("dgx1-home-arpa-8000.toml");
    let hand_authored = concat!(
        "# operator-owned backend\n",
        "name = \"ignored-by-filename\"\n",
        "endpoint = \"http://dgx1-home-arpa:8000\"\n",
        "model = \"hand-model\"\n",
        "tiers = [\"FAST\"]\n",
        "kind = \"openai\"\n",
    );
    std::fs::write(&occupied, hand_authored).unwrap();
    let hits = vec![openai_hit(
        "http://dgx1.home.arpa:8000",
        &["detected-model"],
    )];

    let written = persist_detected_setup(&config_path, &hits, None, None).unwrap();

    assert_eq!(std::fs::read_to_string(&occupied).unwrap(), hand_authored);
    assert_eq!(written.len(), 1);
    assert_eq!(
        written[0].file_name().and_then(|name| name.to_str()),
        Some("dgx1-home-arpa-8000-2.toml")
    );
    assert_eq!(
        Config::load(&config_path)
            .unwrap()
            .default_backend
            .as_deref(),
        Some("dgx1-home-arpa-8000-2")
    );

    let first_bytes = std::fs::read(&written[0]).unwrap();
    let rerun = persist_detected_setup(&config_path, &hits, None, None).unwrap();
    assert!(rerun.is_empty(), "the collision alias should be reused");
    assert_eq!(std::fs::read(&written[0]).unwrap(), first_bytes);
    assert!(!backend_dir.join("dgx1-home-arpa-8000-3.toml").exists());
}

#[serial_test::serial(real_fs)]
#[test]
fn detected_setup_reuses_a_matching_dropin_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    let backend_dir = dir.path().join("backends");
    std::fs::create_dir_all(&backend_dir).unwrap();
    let existing = backend_dir.join("operator-dgx.toml");
    let hand_authored = concat!(
        "# retain this comment and operator choices\n",
        "name = \"ignored-by-filename\"\n",
        "endpoint = \"http://dgx1.home.arpa:8080/\"\n",
        "model = \"operator-model\"\n",
        "tiers = [\"STANDARD\", \"REVIEW\"]\n",
        "kind = \"openai\"\n",
        "num_ctx = 32768\n",
    );
    std::fs::write(&existing, hand_authored).unwrap();
    let hits = vec![openai_hit(
        "http://dgx1.home.arpa:8080",
        &["detected-model", "operator-model"],
    )];

    let written = persist_detected_setup(&config_path, &hits, None, None).unwrap();

    assert!(written.is_empty());
    assert_eq!(std::fs::read_to_string(&existing).unwrap(), hand_authored);
    assert!(!backend_dir.join("dgx1-home-arpa-8080.toml").exists());
    assert_eq!(
        Config::load(&config_path)
            .unwrap()
            .default_backend
            .as_deref(),
        Some("operator-dgx")
    );
}

/// Real-resource grounding for setup × runtime-probe ownership: a
/// runtime `probe_v1` overlay at the setup slot's filename is
/// MACHINE-owned cache — it reserves the filename but is never REUSED
/// as the backend definition (pre-#1819 the reuse silently adopted the
/// probe cache as the definition, and the real declaration vanished).
/// A setup re-run writes a real definition under the next name and
/// leaves the probe cache's bytes untouched.
#[ignore = "real-resource: weekly/release tier; touches the filesystem"]
#[serial_test::serial(real_fs)]
#[test]
fn detected_setup_never_reuses_a_runtime_probe_overlay_as_a_definition() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    let _config_env = EnvVarGuard::set(newt_core::config::NEWT_CONFIG_DIR_ENV, dir.path());
    let verified = vec![VerifiedTargetHit {
        probe: openai_hit("https://inference.example.test", &["model"]),
        api: Some(OpenAiApi::ChatCompletions),
    }];

    persist_verified_setup(&config_path, &verified, None, None).unwrap();
    let backend_dir = dir.path().join("backends");
    let name = backend_name("https://inference.example.test").unwrap();
    let backend_path = backend_dir.join(format!("{name}.toml"));
    // The setup definition exists; replace it with a runtime probe
    // overlay at the same path (delete first: the typed writeback
    // rightly refuses to overwrite an operator-owned file).
    std::fs::remove_file(&backend_path).unwrap();
    let outcome = newt_core::persist_probe_observation(&newt_core::ProbeObservation {
        name,
        endpoint: "https://inference.example.test".into(),
        kind: Some(BackendKind::Openai),
        api: Some(OpenAiApi::Responses),
        serving: newt_core::ProbedServing::Instance {
            model: Some("model".into()),
        },
    })
    .unwrap();
    assert!(matches!(outcome, newt_core::ProbeWriteback::Written(_)));
    let runtime_bytes = std::fs::read(&backend_path).unwrap();

    let written = persist_verified_setup(&config_path, &verified, None, None).unwrap();

    assert_eq!(
        written.len(),
        1,
        "a probe overlay is not a definition — setup writes a real one"
    );
    assert_eq!(
        std::fs::read(&backend_path).unwrap(),
        runtime_bytes,
        "the machine-owned cache's bytes stay untouched"
    );
    assert_eq!(std::fs::read_dir(&backend_dir).unwrap().count(), 2);
}

#[serial_test::serial(real_fs)]
#[test]
fn detected_setup_preserves_but_does_not_select_a_stale_operator_dropin() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    let backend_dir = dir.path().join("backends");
    std::fs::create_dir_all(&backend_dir).unwrap();
    let existing = backend_dir.join("dgx1-home-arpa-8080.toml");
    let hand_authored = concat!(
        "# preserve even when stale\n",
        "name = \"dgx1-home-arpa-8080\"\n",
        "endpoint = \"http://dgx1.home.arpa:8080\"\n",
        "model = \"retired-model\"\n",
        "tiers = [\"STANDARD\"]\n",
        "kind = \"openai\"\n",
    );
    std::fs::write(&existing, hand_authored).unwrap();
    let hits = vec![openai_hit("http://dgx1.home.arpa:8080", &["current-model"])];

    let written = persist_detected_setup(&config_path, &hits, None, None).unwrap();

    assert_eq!(std::fs::read_to_string(existing).unwrap(), hand_authored);
    assert_eq!(written.len(), 1);
    assert_eq!(
        Config::load(&config_path)
            .unwrap()
            .default_backend
            .as_deref(),
        Some("dgx1-home-arpa-8080-2")
    );
}

#[serial_test::serial(real_fs)]
#[test]
fn detected_setup_does_not_reuse_a_different_auth_reference() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    let backend_dir = dir.path().join("backends");
    std::fs::create_dir_all(&backend_dir).unwrap();
    let existing = backend_dir.join("dgx1-home-arpa-8000.toml");
    let body = concat!(
        "name = \"dgx1-home-arpa-8000\"\n",
        "endpoint = \"http://dgx1.home.arpa:8000\"\n",
        "model = \"model\"\n",
        "tiers = [\"FAST\"]\n",
        "kind = \"openai\"\n",
        "serving = \"instance\"\n",
        "api_key_env = \"UNRELATED_TOKEN\"\n",
    );
    std::fs::write(&existing, body).unwrap();
    let hits = vec![openai_hit("http://dgx1.home.arpa:8000", &["model"])];

    let written = persist_detected_setup(&config_path, &hits, None, None).unwrap();

    assert_eq!(std::fs::read_to_string(existing).unwrap(), body);
    assert_eq!(written.len(), 1);
    assert_eq!(
        written[0].file_name().and_then(|name| name.to_str()),
        Some("dgx1-home-arpa-8000-2.toml")
    );
}

#[serial_test::serial(real_fs)]
#[test]
fn detected_setup_does_not_reuse_stale_generated_served_truth() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    let backend_dir = dir.path().join("backends");
    std::fs::create_dir_all(&backend_dir).unwrap();
    let existing = backend_dir.join("dgx1-home-arpa-8000.toml");
    let body = concat!(
        "name = \"dgx1-home-arpa-8000\"\n",
        "endpoint = \"http://dgx1.home.arpa:8000\"\n",
        "model = \"old-model\"\n",
        "tiers = [\"FAST\"]\n",
        "kind = \"openai\"\n",
        "serving = \"instance\"\n",
        "\n[provenance]\n",
        "source = \"newt setup v0.7.2 (auto-detected Openai)\"\n",
    );
    std::fs::write(&existing, body).unwrap();
    let hits = vec![openai_hit("http://dgx1.home.arpa:8000", &["new-model"])];

    let written = persist_detected_setup(&config_path, &hits, None, None).unwrap();

    assert_eq!(std::fs::read_to_string(existing).unwrap(), body);
    assert_eq!(written.len(), 1);
    assert_eq!(
        read_dropin(&config_path, "dgx1-home-arpa-8000-2")
            .model
            .as_deref(),
        Some("new-model")
    );
}

#[cfg(unix)]
#[serial_test::serial(real_fs)]
#[test]
fn detected_setup_preserves_private_config_permissions() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "# private config\n").unwrap();
    std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let hits = vec![openai_hit("http://dgx1.home.arpa:8000", &["model"])];

    persist_detected_setup(&config_path, &hits, None, None).unwrap();

    assert_eq!(
        std::fs::metadata(&config_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(config_path.with_file_name("config.toml.bak"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[cfg(unix)]
#[serial_test::serial(real_fs)]
#[test]
fn detected_setup_updates_a_symlink_target_without_replacing_the_link() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let real_config = dir.path().join("dotfiles/newt.toml");
    std::fs::create_dir_all(real_config.parent().unwrap()).unwrap();
    std::fs::write(&real_config, "# linked config\n").unwrap();
    let config_path = dir.path().join("config.toml");
    symlink(&real_config, &config_path).unwrap();
    let hits = vec![openai_hit("http://dgx1.home.arpa:8000", &["model"])];

    persist_detected_setup(&config_path, &hits, None, None).unwrap();

    assert!(std::fs::symlink_metadata(&config_path)
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(std::fs::read_to_string(&real_config)
        .unwrap()
        .contains("default_backend"));
}

/// Real-filesystem grounding for the bound setup destination: retargeting
/// the operator's config symlink after staging cannot move the commit away
/// from the file whose lock setup acquired.
#[cfg(unix)]
#[ignore = "real-resource: weekly/release tier; retargets a filesystem symlink"]
#[serial_test::serial(real_fs)]
#[test]
fn setup_symlink_retarget_cannot_escape_the_locked_destination() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("first/config.toml");
    let second = dir.path().join("second/config.toml");
    std::fs::create_dir_all(first.parent().unwrap()).unwrap();
    std::fs::create_dir_all(second.parent().unwrap()).unwrap();
    std::fs::write(&first, "# first\n").unwrap();
    std::fs::write(&second, "# second\n").unwrap();
    let config = dir.path().join("config.toml");
    symlink(&first, &config).unwrap();
    let backend = BackendConfig {
        name: "example".into(),
        endpoint: "https://inference.example.test".into(),
        model: Some("model".into()),
        kind: Some(BackendKind::Openai),
        ..Default::default()
    };
    let cfg = Config {
        default_backend: Some("example".into()),
        ..Default::default()
    };

    persist_interactive_backend_with(
        &ScriptedConsole::new(&[]).operator(),
        &config,
        &cfg,
        &backend,
        None,
        |staged, destination| destination.durable_replace(staged),
        |staged, destination| {
            std::fs::remove_file(&config)?;
            symlink(&second, &config)?;
            destination
                .durable_replace(staged)
                .map_err(anyhow::Error::from)
        },
    )
    .unwrap();

    assert!(std::fs::read_to_string(&first)
        .unwrap()
        .contains("default_backend"));
    assert_eq!(std::fs::read_to_string(&second).unwrap(), "# second\n");
    assert_eq!(
        std::fs::canonicalize(&config).unwrap(),
        std::fs::canonicalize(&second).unwrap()
    );
}

#[serial_test::serial(real_fs)]
#[test]
fn failed_setup_staging_cleans_earlier_temporary_files() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    let backend_dir = dir.path().join("backends");
    let blocked_parent = dir.path().join("not-a-directory");
    std::fs::write(&blocked_parent, "occupied").unwrap();
    let planned = vec![
        PlannedSetupBackend {
            name: "first".into(),
            endpoint: "http://first:8000".into(),
            path: backend_dir.join("first.toml"),
            body: Some(b"name = \"first\"\n".to_vec()),
            replace: false,
        },
        PlannedSetupBackend {
            name: "second".into(),
            endpoint: "http://second:8000".into(),
            path: blocked_parent.join("second.toml"),
            body: Some(b"name = \"second\"\n".to_vec()),
            replace: false,
        },
    ];

    let destination = setup_config_destination(&config_path).unwrap();
    assert!(commit_setup_plan(
        &config_path,
        &destination,
        "",
        "default_backend = \"first\"\n",
        &planned,
        &mut Vec::new(),
    )
    .is_err());
    let leftovers = std::fs::read_dir(&backend_dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    assert!(leftovers.is_empty(), "leftover staged files: {leftovers:?}");
    assert!(!config_path.exists());
}

/// #1667: the backend panel's ADD persists through the SAME setup-lock plan
/// commit as the wizard (#1660) — a fresh drop-in appears, config.toml is
/// never rewritten, a duplicate add is refused, and the lock is released.
#[cfg(feature = "rich-tui")]
#[serial_test::serial(real_fs)]
#[test]
fn panel_backend_add_creates_a_dropin_and_never_touches_config() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "# operator config\n").unwrap();
    let edit = crate::backend_panel::BackendEdit {
        name: "dgx1".into(),
        kind: Some(BackendKind::Openai),
        endpoint: "http://dgx1:8000".into(),
        model: Some("gpt-oss-120b".into()),
        api_key_env: Some("DGX_KEY".into()),
        api_key_file: None,
        dirty: crate::backend_panel::DirtyFields::default(),
        replace: false,
    };
    let saved = persist_panel_backend(&config_path, &edit).unwrap();
    let path = saved.path;
    assert!(
        saved.warnings.is_empty(),
        "a clean write warns about nothing"
    );
    assert_eq!(path, dir.path().join("backends/dgx1.toml"));
    let written: BackendConfig = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(written.name, "dgx1");
    assert_eq!(written.endpoint, "http://dgx1:8000");
    assert_eq!(written.kind, Some(BackendKind::Openai));
    assert_eq!(written.model.as_deref(), Some("gpt-oss-120b"));
    assert_eq!(written.api_key_env.as_deref(), Some("DGX_KEY"));
    assert_eq!(
        std::fs::read_to_string(&config_path).unwrap(),
        "# operator config\n",
        "the editor never rewrites config.toml"
    );
    // Adding the same name again is refused (no clobber)…
    let error = persist_panel_backend(&config_path, &edit).unwrap_err();
    assert!(error.to_string().contains("already exists"), "{error:#}");
    // …the drop-in list names it for the chooser's editability marker…
    assert_eq!(
        panel_backend_file_names(&config_path),
        vec!["dgx1".to_string()]
    );
    // …and the setup lock was released (a fresh acquire succeeds).
    drop(acquire_setup_lock(&config_path).unwrap());
}

/// #1667: the panel's EDIT overlays ONLY the fields the operator actually
/// changed — wizard/probe-written fields the form does not show (tiers,
/// serving), operator comments, keys `BackendConfig` does not model, and a
/// `kind` the operator never dialed all round-trip untouched
/// (review §1/§6/§8).
#[cfg(feature = "rich-tui")]
#[serial_test::serial(real_fs)]
#[test]
fn panel_backend_edit_overlays_only_dirty_fields_and_preserves_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    let backend_dir = dir.path().join("backends");
    std::fs::create_dir_all(&backend_dir).unwrap();
    std::fs::write(
            backend_dir.join("gpu-runner.toml"),
            "# operator notes for the lab box\n\
             name = \"gpu-runner\"\nendpoint = \"http://gpu-runner:11434\" # LAN\nmodel = \"qwen3:30b\"\n\
             tiers = [\"FAST\"]\nkind = \"anthropic\"\nserving = \"multiplexer\"\n\
             operator_hint = \"keep me\"\n",
        )
        .unwrap();
    // The operator changed ONLY the model — the kind dial was never
    // touched, so `kind = "anthropic"` (outside the form's ladder) must
    // survive verbatim.
    let edit = crate::backend_panel::BackendEdit {
        name: "gpu-runner".into(),
        kind: Some(BackendKind::Anthropic),
        endpoint: "http://gpu-runner:11434".into(),
        model: Some("llama3.1:8b".into()),
        api_key_env: None,
        api_key_file: None,
        dirty: crate::backend_panel::DirtyFields {
            model: true,
            ..crate::backend_panel::DirtyFields::default()
        },
        replace: true,
    };
    let saved = persist_panel_backend(&config_path, &edit).unwrap();
    let body = std::fs::read_to_string(&saved.path).unwrap();
    let written: BackendConfig = toml::from_str(&body).unwrap();
    assert_eq!(
        written.model.as_deref(),
        Some("llama3.1:8b"),
        "the form field applied"
    );
    assert_eq!(
        written.kind,
        Some(BackendKind::Anthropic),
        "an out-of-ladder kind survived an edit that never touched it (§1)"
    );
    assert_eq!(
        written.serving,
        Some(newt_core::Serving::Multiplexer),
        "an unmanaged field survived the edit"
    );
    assert_eq!(
        written.tiers,
        vec![Tier::Fast],
        "an unmanaged field survived the edit"
    );
    assert!(body.contains("# operator notes"), "comment lost: {body}");
    assert!(body.contains("# LAN"), "inline comment lost: {body}");
    assert!(
        body.contains("operator_hint = \"keep me\""),
        "unmodelled key lost: {body}"
    );
    // Clearing an auth field IS written (a dirty None removes the key).
    let clear = crate::backend_panel::BackendEdit {
        api_key_env: None,
        dirty: crate::backend_panel::DirtyFields {
            api_key_env: true,
            ..crate::backend_panel::DirtyFields::default()
        },
        ..edit.clone()
    };
    std::fs::write(
        backend_dir.join("gpu-runner.toml"),
        format!("{body}api_key_env = \"OLD\"\n"),
    )
    .unwrap();
    let saved = persist_panel_backend(&config_path, &clear).unwrap();
    let written: BackendConfig =
        toml::from_str(&std::fs::read_to_string(&saved.path).unwrap()).unwrap();
    assert_eq!(written.api_key_env, None, "the cleared key is gone");
    // Editing a drop-in that vanished is a visible error, not a create.
    let ghost = crate::backend_panel::BackendEdit {
        name: "ghost".into(),
        replace: true,
        ..edit
    };
    assert!(persist_panel_backend(&config_path, &ghost).is_err());
    assert!(!backend_dir.join("ghost.toml").exists());
}

/// #1667: `:d <name>` deletes exactly one drop-in under the setup lock;
/// a missing name and a path-traversal shape are refused visibly.
#[cfg(feature = "rich-tui")]
#[serial_test::serial(real_fs)]
#[test]
fn panel_backend_remove_deletes_the_dropin_under_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    let backend_dir = dir.path().join("backends");
    std::fs::create_dir_all(&backend_dir).unwrap();
    std::fs::write(
        backend_dir.join("old.toml"),
        "name = \"old\"\nendpoint = \"http://old:1\"\n",
    )
    .unwrap();
    assert!(remove_panel_backend(&config_path, "old", None)
        .unwrap()
        .is_empty());
    assert!(!backend_dir.join("old.toml").exists());
    let error = remove_panel_backend(&config_path, "old", None).unwrap_err();
    assert!(
        error.to_string().contains("no backend drop-in"),
        "{error:#}"
    );
    let error = remove_panel_backend(&config_path, "../evil", None).unwrap_err();
    assert!(
        error.to_string().contains("invalid backend name"),
        "{error:#}"
    );
    drop(acquire_setup_lock(&config_path).unwrap());
}

/// #1667 review §2/§7/§11 REGRESSION: removing the backend config.toml's
/// `default_backend` names must never leave a dangling pointer — which
/// `Config::select_backend` reports as a hard `UnknownNamed` error to
/// `newt solve` / the ACP worker (no settings.toml mask exists there). It is
/// refused outright, and accepted only as one transaction that repoints the
/// default at the backend the caller just applied.
#[cfg(feature = "rich-tui")]
#[serial_test::serial(real_fs)]
#[test]
fn panel_backend_remove_never_orphans_the_config_default() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    let backend_dir = dir.path().join("backends");
    std::fs::create_dir_all(&backend_dir).unwrap();
    // The #1140 wizard shape: the backends live ONLY as drop-ins.
    let original = "# hand-authored\ndefault_backend = \"dgx1\" # keep this note\n";
    std::fs::write(&config_path, original).unwrap();
    for name in ["dgx1", "gpu-runner"] {
        std::fs::write(
            backend_dir.join(format!("{name}.toml")),
            format!("endpoint = \"http://{name}:8000\"\n"),
        )
        .unwrap();
    }

    // Refused without a replacement…
    let error = remove_panel_backend(&config_path, "dgx1", None).unwrap_err();
    assert!(error.to_string().contains("default_backend"), "{error:#}");
    assert!(
        backend_dir.join("dgx1.toml").exists(),
        "a refused remove deletes nothing"
    );
    assert_eq!(std::fs::read_to_string(&config_path).unwrap(), original);
    // …and refused when the replacement is not a real backend.
    let error = remove_panel_backend(&config_path, "dgx1", Some("ghost")).unwrap_err();
    assert!(error.to_string().contains("unknown backend"), "{error:#}");
    assert!(backend_dir.join("dgx1.toml").exists());

    // Accepted as ONE transaction: the pointer moves first, then the file.
    let notes = remove_panel_backend(&config_path, "dgx1", Some("gpu-runner")).unwrap();
    assert!(!backend_dir.join("dgx1.toml").exists());
    let config = std::fs::read_to_string(&config_path).unwrap();
    assert!(
        config.contains("default_backend = \"gpu-runner\""),
        "the durable pointer followed the switch: {config}"
    );
    assert!(
        config.contains("# keep this note") && config.contains("# hand-authored"),
        "the repoint preserved operator content: {config}"
    );
    assert!(
        notes
            .iter()
            .any(|n| n.contains("default_backend now points")),
        "the repoint is reported: {notes:?}"
    );
    // A non-default backend still removes with no config rewrite at all.
    std::fs::write(
        backend_dir.join("spare.toml"),
        "endpoint = \"http://spare:1\"\n",
    )
    .unwrap();
    assert!(remove_panel_backend(&config_path, "spare", None)
        .unwrap()
        .is_empty());
    assert_eq!(std::fs::read_to_string(&config_path).unwrap(), config);
    drop(acquire_setup_lock(&config_path).unwrap());
}

/// #1667 review §10: a post-rename parent-sync failure is a WARNING on a
/// successful write (the bytes are the file), never a "save failed" that
/// would leave the panel reporting a visible edit as lost. A before-commit
/// failure is still a failure.
#[test]
fn an_after_commit_sync_failure_is_a_warning_not_a_failure() {
    let path = Path::new("/tmp/newt-test/backends/dgx1.toml");
    assert_eq!(replace_warning(Ok(())).unwrap(), None);
    let warning = replace_warning(Err(
        newt_core::atomic_fs::DurableReplaceError::after_commit(
            path,
            io::Error::other("injected parent sync failure"),
        ),
    ))
    .unwrap()
    .expect("an after-commit failure is a warning");
    assert!(
        warning.contains("could not durably sync") && warning.contains("dgx1.toml"),
        "{warning}"
    );
}

/// #1667 review §4: the inline `[[backends]]` names are what the panel uses
/// to warn that a same-named drop-in does not fully own its fields.
#[cfg(feature = "rich-tui")]
#[test]
fn inline_backend_names_reads_the_declared_entries() {
    let text = "default_backend = \"dgx1\"\n\
                    [[backends]]\nname = \"dgx1\"\nendpoint = \"http://dgx1:8000\"\n\
                    [[backends]]\nname = \"relic\"\nendpoint = \"http://relic:1\"\n";
    assert_eq!(inline_backend_names_in(text), vec!["dgx1", "relic"]);
    assert_eq!(default_backend_in(text).as_deref(), Some("dgx1"));
    assert!(inline_backend_names_in("# nothing here\n").is_empty());
    assert_eq!(default_backend_in("# nothing here\n"), None);
}

/// Real-filesystem grounding for the detected-setup config failpoint: a
/// post-rename sync failure must not delete drop-ins already selected by
/// the visible replacement config.
#[ignore = "real-resource: weekly/release tier; touches the filesystem"]
#[serial_test::serial(real_fs)]
#[test]
fn detected_config_post_commit_sync_failure_retains_selected_backends() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    let old_config = "default_backend = \"old\"\n";
    let updated_config = "default_backend = \"example\"\n";
    std::fs::write(&config_path, old_config).unwrap();
    let backend_path = dir.path().join("backends/example.toml");
    let planned = vec![PlannedSetupBackend {
        name: "example".into(),
        endpoint: "https://inference.example.test".into(),
        path: backend_path.clone(),
        body: Some(b"name = \"example\"\nendpoint = \"https://inference.example.test\"\n".to_vec()),
        replace: false,
    }];
    let destination = setup_config_destination(&config_path).unwrap();

    let error = commit_setup_plan_with(
        &config_path,
        &destination,
        old_config,
        updated_config,
        &planned,
        &mut Vec::new(),
        |staged, destination| {
            destination.durable_replace(staged).unwrap();
            Err(newt_core::atomic_fs::DurableReplaceError::after_commit(
                destination.as_path(),
                io::Error::other("injected parent sync failure"),
            ))
        },
    )
    .unwrap_err();

    assert!(error.to_string().contains("could not durably sync"));
    assert_eq!(
        std::fs::read_to_string(&config_path).unwrap(),
        updated_config
    );
    assert_eq!(
        std::fs::read_to_string(config_path.with_file_name("config.toml.bak")).unwrap(),
        old_config
    );
    assert!(backend_path.exists());
    assert!(std::fs::read_to_string(backend_path)
        .unwrap()
        .contains("https://inference.example.test"));
}

#[serial_test::serial(real_fs)]
#[test]
fn setup_lock_blocks_a_second_writer_and_can_be_reacquired() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");

    let first = acquire_setup_lock(&config_path).unwrap();
    let error = acquire_setup_lock(&config_path).unwrap_err();
    assert!(error.to_string().contains("another live process"));
    drop(first);

    let reacquired = acquire_setup_lock(&config_path).unwrap();
    drop(reacquired);
    assert!(!dir.path().join("config.toml.lock").exists());
}

/// Per-PR mocked BAT for the regression where a public model catalog was
/// mistaken for authentication success and setup persisted an unusable
/// backend. No real filesystem or credential is involved in this lane.
#[tokio::test]
async fn bat_public_catalog_auth_rejection_never_calls_persistence() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "publicly-listed-model"}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;
    let persistence_called = std::cell::Cell::new(false);
    let console = ScriptedConsole::new(&[]);

    let error = run_target_with_persist(
        &console.operator(),
        &reqwest::Client::new(),
        Path::new("unused/config.toml"),
        TargetSetupRequest {
            target: &server.uri(),
            token_env: None,
            token_file: None,
            model: None,
            yes: true,
        },
        &Discovery::default(),
        |_, _, _, _| {
            persistence_called.set(true);
            Ok(Vec::new())
        },
    )
    .await
    .unwrap_err();

    assert!(error
        .to_string()
        .contains("no inference backend passed a minimal generation check"));
    assert!(console.transcript().contains("requires authentication"));
    assert!(!persistence_called.get());
}

/// Real-resource grounding for the mocked multi-port target flow;
/// weekly/release only because it writes config files.
#[ignore = "real-resource: weekly/release tier; touches the filesystem"]
#[serial_test::serial(real_fs)]
#[tokio::test]
async fn target_flow_probes_multiple_ports_and_writes_each_live_endpoint() {
    let vllm = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "ornith"}]
        })))
        .mount(&vllm)
        .await;
    mount_openai_chat(&vllm).await;
    let router = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "qwen"}, {"id": "gpt-oss"}]
        })))
        .mount(&router)
        .await;
    mount_openai_chat(&router).await;
    let discovery = newt_core::config::Discovery {
        hosts: vec![],
        ollama_ports: vec![],
        vllm_ports: vec![vllm.address().port(), router.address().port()],
    };
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    let client = reqwest::Client::new();
    let console = ScriptedConsole::new(&[]);

    run_target_with(
        &console.operator(),
        &client,
        &config_path,
        TargetSetupRequest {
            target: "127.0.0.1",
            token_env: None,
            token_file: None,
            model: None,
            yes: true,
        },
        &discovery,
    )
    .await
    .unwrap();

    let backend_dir = dir.path().join("backends");
    assert_eq!(
        std::fs::read_dir(&backend_dir).unwrap().count(),
        2,
        "one drop-in per live endpoint"
    );
    let config = Config::load(&config_path).unwrap();
    assert_eq!(
        config.default_backend.as_deref(),
        Some(format!("127-0-0-1-{}", vllm.address().port()).as_str())
    );
    assert!(console
        .transcript()
        .contains("Detected 2 inference backends"));
}

#[serial_test::serial(real_fs)]
#[tokio::test]
async fn target_flow_reports_auth_failure_alongside_a_successful_probe() {
    let open = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "open-model"}]
        })))
        .mount(&open)
        .await;
    mount_openai_chat(&open).await;
    let secured = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&secured)
        .await;
    let discovery = newt_core::config::Discovery {
        hosts: vec![],
        ollama_ports: vec![],
        vllm_ports: vec![open.address().port(), secured.address().port()],
    };
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    let console = ScriptedConsole::new(&[]);

    run_target_with(
        &console.operator(),
        &reqwest::Client::new(),
        &config_path,
        TargetSetupRequest {
            target: "127.0.0.1",
            token_env: None,
            token_file: None,
            model: None,
            yes: true,
        },
        &discovery,
    )
    .await
    .unwrap();

    let transcript = console.transcript();
    assert!(transcript.contains("Detected 1 inference backend"));
    assert!(transcript.contains("authentication required"));
    assert!(transcript.contains(&secured.address().port().to_string()));
}

#[serial_test::serial(real_fs)]
#[tokio::test]
async fn target_flow_decline_writes_nothing() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "served-model"}]
        })))
        .mount(&server)
        .await;
    mount_openai_chat(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    let console = ScriptedConsole::new(&["n"]);

    run_target_with(
        &console.operator(),
        &reqwest::Client::new(),
        &config_path,
        TargetSetupRequest {
            target: &server.uri(),
            token_env: None,
            token_file: None,
            model: None,
            yes: false,
        },
        &newt_core::config::Discovery::default(),
    )
    .await
    .unwrap();

    assert!(console.transcript().contains("Aborted. Nothing written."));
    assert!(!config_path.exists());
    assert!(!dir.path().join("backends").exists());
}

#[serial_test::serial(real_fs)]
#[tokio::test]
async fn target_flow_requires_an_explicit_endpoint_before_sending_a_token() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "secured-model"}]
        })))
        .expect(0)
        .mount(&server)
        .await;
    let discovery = newt_core::config::Discovery {
        hosts: vec![],
        ollama_ports: vec![],
        vllm_ports: vec![server.address().port()],
    };
    let dir = tempfile::tempdir().unwrap();
    let token_path = dir.path().join("token");
    std::fs::write(&token_path, "secret-value\n").unwrap();
    let console = ScriptedConsole::new(&[]);

    let error = run_target_with(
        &console.operator(),
        &reqwest::Client::new(),
        &dir.path().join("config.toml"),
        TargetSetupRequest {
            target: "127.0.0.1",
            token_env: None,
            token_file: Some(&token_path),
            model: None,
            yes: true,
        },
        &discovery,
    )
    .await
    .unwrap_err();

    assert!(error.to_string().contains("explicit URL"));
    assert!(!dir.path().join("config.toml").exists());
}

#[serial_test::serial(real_fs)]
#[tokio::test]
async fn target_flow_uses_token_file_for_probe_without_echoing_it() {
    use wiremock::matchers::header;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer secret-value"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "secured-model"}]
        })))
        .mount(&server)
        .await;
    mount_authenticated_openai_chat(&server, "secret-value").await;
    let dir = tempfile::tempdir().unwrap();
    let token_path = dir.path().join("token");
    std::fs::write(&token_path, "secret-value\n").unwrap();
    let config_path = dir.path().join("config.toml");
    let client = reqwest::Client::new();
    let console = ScriptedConsole::new(&[]);

    run_target_with(
        &console.operator(),
        &client,
        &config_path,
        TargetSetupRequest {
            target: &server.uri(),
            token_env: None,
            token_file: Some(&token_path),
            model: None,
            yes: true,
        },
        &newt_core::config::Discovery::default(),
    )
    .await
    .unwrap();

    let name = backend_name(&server.uri()).unwrap();
    let backend = read_dropin(&config_path, &name);
    assert_eq!(
        backend.api_key_file.as_deref(),
        std::fs::canonicalize(&token_path).unwrap().to_str(),
        "persist the reference, never the token"
    );
    assert!(!console.transcript().contains("secret-value"));
    assert!(!std::fs::read_to_string(&config_path)
        .unwrap()
        .contains("secret-value"));
}

#[serial_test::serial(real_fs)]
#[tokio::test]
async fn target_flow_failure_is_actionable_and_writes_nothing() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    let client = reqwest::Client::new();
    let console = ScriptedConsole::new(&[]);

    let err = run_target_with(
        &console.operator(),
        &client,
        &config_path,
        TargetSetupRequest {
            target: &server.uri(),
            token_env: None,
            token_file: None,
            model: None,
            yes: true,
        },
        &newt_core::config::Discovery::default(),
    )
    .await
    .unwrap_err();

    assert!(err.to_string().contains("no supported inference API"));
    assert!(!config_path.exists());
    assert!(!dir.path().join("backends").exists());
}

#[serial_test::serial(real_fs)]
#[tokio::test]
async fn target_flow_rejects_an_endpoint_with_no_served_models() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": []
        })))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    let client = reqwest::Client::new();
    let console = ScriptedConsole::new(&[]);

    let error = run_target_with(
        &console.operator(),
        &client,
        &config_path,
        TargetSetupRequest {
            target: &server.uri(),
            token_env: None,
            token_file: None,
            model: None,
            yes: true,
        },
        &newt_core::config::Discovery::default(),
    )
    .await
    .unwrap_err();

    assert!(error.to_string().contains("no supported inference API"));
    assert!(console.transcript().contains("listed no models"));
    assert!(!config_path.exists());
    assert!(!dir.path().join("backends").exists());
}

// --- HTTP probes ------------------------------------------------------

#[tokio::test]
async fn fetch_ollama_models_parses_tags() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [{"name": "llama3.1:8b"}, {"name": "qwen2.5-coder:7b"}]
        })))
        .mount(&server)
        .await;
    let client = reqwest::Client::new();
    let models = fetch_ollama_models(&client, &server.uri()).await.unwrap();
    assert_eq!(models, vec!["llama3.1:8b", "qwen2.5-coder:7b"]);
}

#[tokio::test]
async fn fetch_openai_models_auth_sends_bearer() {
    // Regression: the session-start adopt probe hit authenticated
    // gateways WITHOUT the backend's bearer token -> 401 -> a spurious
    // "unreachable" banner every launch and no adoption.
    use wiremock::matchers::header;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer sekrit"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "gated-model"}]
        })))
        .mount(&server)
        .await;
    let client = reqwest::Client::new();
    let models =
        newt_core::backend_probe::fetch_openai_models_auth(&client, &server.uri(), Some("sekrit"))
            .await
            .unwrap();
    assert_eq!(models, vec!["gated-model".to_string()]);
    // Without the token the mock does not match -> error, never a silent [].
    assert!(
        newt_core::backend_probe::fetch_openai_models(&client, &server.uri())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn fetch_openai_models_parses_data() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "meta/llama-3.1-8b-instruct"}]
        })))
        .mount(&server)
        .await;
    let client = reqwest::Client::new();
    let models = newt_core::backend_probe::fetch_openai_models(&client, &server.uri())
        .await
        .unwrap();
    assert_eq!(models, vec!["meta/llama-3.1-8b-instruct"]);
}

#[tokio::test]
async fn fetch_ollama_models_errors_on_500() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let client = reqwest::Client::new();
    assert!(fetch_ollama_models(&client, &server.uri()).await.is_err());
}

// --- full driver flows ------------------------------------------------

#[serial_test::serial(real_fs)]
#[tokio::test]
async fn ollama_flow_writes_config() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [{"name": "llama3.1:8b"}, {"name": "qwen2.5-coder:7b"}]
        })))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let client = reqwest::Client::new();
    // backend=1 (Ollama), host=<mock>, model=2 (qwen), write=Y
    let console = ScriptedConsole::new(&["1", &server.uri(), "2", "y"]);
    run_with(&console.operator(), &client, &path).await.unwrap();

    let cfg = Config::load(&path).unwrap();
    assert!(cfg.dgx.is_none(), "no legacy [dgx] block (#1140)");
    assert_eq!(cfg.default_backend.as_deref(), Some("default"));
    let b = read_dropin(&path, "default");
    assert_eq!(b.effective_model(), Some("qwen2.5-coder:7b"));
    assert_eq!(b.endpoint, server.uri());
}

#[serial_test::serial(real_fs)]
#[tokio::test]
async fn bad_pasted_key_is_caught_by_the_live_test_and_reentered() {
    // Field regression: ollama.com serves the model catalog to anyone, so
    // a mistyped key sailed through setup and 401'd on the first message.
    // The wizard now live-tests the key (1-token chat on the ollama wire)
    // and offers re-entry.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [{"name": "big-cloud-model"}]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .and(wiremock::matchers::header(
            "authorization",
            "Bearer good-key",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message": {"content": "hi"}, "done": true
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(serde_json::json!({"error": "Unauthorized"})),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.toml");
    // Pin the config dir: the machine identity for blank-passphrase
    // encryption lives under it.
    let _config_env = EnvVarGuard::set(newt_core::config::NEWT_CONFIG_DIR_ENV, dir.path());
    newt_core::secrets::session().reset_for_test();
    let preset = ProviderPreset {
        name: "cloudish".into(),
        base_url: server.uri(),
        api_mode: newt_core::provider_preset::ApiMode::Ollama,
        env_vars: vec!["NEWT_TEST_NO_SUCH_VAR_EXISTS".into()],
        ..Default::default()
    };
    // paste bad key → blank passphrase → model 1 → re-enter? Y →
    // paste good key → blank passphrase.
    let console = ScriptedConsole::new(&["bad-key", "", "1", "Y", "good-key", ""]);
    let result = configure_preset(
        &console.operator(),
        &reqwest::Client::new(),
        &preset,
        &cfg_path,
    )
    .await;
    let (_cfg, backend, _pending) = result.unwrap();
    let t = console.transcript();
    assert!(t.contains("✗ authentication rejected (HTTP 401)"), "{t}");
    assert!(t.contains("✓ generation accepted"), "{t}");
    assert!(backend.api_key_file.is_some(), "re-entered key is stored");
}

#[serial_test::serial(real_fs)]
#[tokio::test]
async fn two_backends_in_one_sitting_with_default_pick() {
    // The multi-backend loop: local ollama, then a custom host, then the
    // default-backend pick — all in one wizard pass.
    let s1 = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [{"name": "llama3.1:8b"}]
        })))
        .mount(&s1)
        .await;
    let s2 = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [{"name": "qwen3:30b"}]
        })))
        .mount(&s2)
        .await;
    mount_ollama_chat(&s2).await;
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.toml");
    let client = reqwest::Client::new();
    let host2_name = format!("127-0-0-1-{}", s2.address().port());
    // ollama door → write → add another → custom host door → write →
    // stop → pick backend 2 as the default.
    let console = ScriptedConsole::new(&[
        "1",
        &s1.uri(),
        "1",
        "y",
        "y",
        "2",
        &s2.uri(),
        "1",
        "1",
        "y",
        "n",
        "2",
    ]);
    run_with(&console.operator(), &client, &cfg_path)
        .await
        .unwrap();

    assert!(cfg_path
        .with_file_name("backends")
        .join("default.toml")
        .exists());
    assert!(cfg_path
        .with_file_name("backends")
        .join(format!("{host2_name}.toml"))
        .exists());
    let cfg = Config::load(&cfg_path).unwrap();
    assert_eq!(
        cfg.default_backend.as_deref(),
        Some(host2_name.as_str()),
        "the end-of-loop pick wins over last-written"
    );
}

#[serial_test::serial(real_fs)]
#[tokio::test]
async fn custom_host_flow_detects_openai_backend() {
    // The custom-host door subsumes the old DGX flavour menu: the probe
    // detects the wire (here: OpenAI-compatible, one model = instance).
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "meta/llama-3.1-8b-instruct"}]
        })))
        .mount(&server)
        .await;
    mount_openai_chat(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let client = reqwest::Client::new();
    // custom host=2, host=<mock url>, endpoint=1, model=1, write=Y
    let console = ScriptedConsole::new(&["2", &server.uri(), "1", "1", "y"]);
    run_with(&console.operator(), &client, &path).await.unwrap();

    let name = format!("127-0-0-1-{}", server.address().port());
    let cfg = Config::load(&path).unwrap();
    assert_eq!(cfg.default_backend.as_deref(), Some(name.as_str()));
    let b = read_dropin(&path, &name);
    assert_eq!(b.kind, Some(BackendKind::Openai));
    assert_eq!(b.serving, Some(newt_core::Serving::Instance));
    assert_eq!(b.effective_model(), Some("meta/llama-3.1-8b-instruct"));
    assert_eq!(b.endpoint, server.uri());
}

#[serial_test::serial(real_fs)]
#[tokio::test]
async fn custom_host_adopts_the_warm_model_as_the_enter_default() {
    // /api/tags lists install order; /api/ps says what's LOADED. The menu
    // must put the warm model first so a blank Enter adopts it.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [
                {"name": "cold-a:7b"}, {"name": "cold-b:13b"}, {"name": "warm:32b"}
            ]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/ps"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [{"name": "warm:32b"}]
        })))
        .mount(&server)
        .await;
    mount_ollama_chat(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let client = reqwest::Client::new();
    // custom host=2, host, endpoint=1, model=<Enter> (default = warm), write=Y
    let console = ScriptedConsole::new(&["2", &server.uri(), "1", "", "y"]);
    run_with(&console.operator(), &client, &path).await.unwrap();

    let name = format!("127-0-0-1-{}", server.address().port());
    assert_eq!(
        read_dropin(&path, &name).effective_model(),
        Some("warm:32b"),
        "a blank Enter adopts the WARM model, not install order"
    );
    let seen = console.transcript();
    assert!(seen.contains("warm: warm:32b"), "row shows warmth: {seen}");
}

#[serial_test::serial(real_fs)]
#[tokio::test]
async fn manual_model_when_endpoint_unreachable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(200))
        .build()
        .unwrap();
    // Ollama, an unroutable host → probe fails → manual model name → write.
    let console = ScriptedConsole::new(&[
        "1",
        "http://127.0.0.1:1", // connection refused
        "phi3:mini",
        "y",
    ]);
    run_with(&console.operator(), &client, &path).await.unwrap();
    let cfg = Config::load(&path).unwrap();
    assert_eq!(cfg.default_backend.as_deref(), Some("default"));
    assert_eq!(
        read_dropin(&path, "default").effective_model(),
        Some("phi3:mini")
    );
}

#[serial_test::serial(real_fs)]
#[tokio::test]
async fn decline_overwrite_keeps_existing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "# sentinel\n").unwrap();
    let client = reqwest::Client::new();
    // Overwrite? → N (default).
    let console = ScriptedConsole::new(&["n"]);
    run_with(&console.operator(), &client, &path).await.unwrap();
    // Untouched.
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "# sentinel\n");
}

#[serial_test::serial(real_fs)]
#[tokio::test]
async fn decline_final_write_leaves_no_file() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [{"name": "llama3.1:8b"}]
        })))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let client = reqwest::Client::new();
    // Ollama, host, model=1, write=n → nothing written.
    let console = ScriptedConsole::new(&["1", &server.uri(), "1", "n"]);
    run_with(&console.operator(), &client, &path).await.unwrap();
    assert!(!path.exists());
}

// --- custom-host / preset pure helpers ----------------------------------

#[test]
fn format_endpoint_row_shows_engine_and_warmth() {
    let hit = newt_core::backend_probe::EndpointProbeResult {
        endpoint: "http://gpu-box:8080".into(),
        kind: BackendKind::Openai,
        models: vec!["a".into(), "b".into()],
        serving: newt_core::Serving::Multiplexer,
        engine: Some(newt_core::config::Engine::LlamaCpp),
        warm: vec!["b".into()],
    };
    let row = format_endpoint_row(&hit);
    assert!(row.contains("llama.cpp"), "{row}");
    assert!(row.contains("2 models"), "{row}");
    assert!(row.contains("warm: b"), "{row}");
    // Unknown engine degrades to the wire-kind label; no warmth shown.
    let bare = newt_core::backend_probe::EndpointProbeResult {
        engine: None,
        warm: vec![],
        ..hit
    };
    let row = format_endpoint_row(&bare);
    assert!(row.contains("openai"), "{row}");
    assert!(!row.contains("warm:"), "{row}");
}

#[test]
fn order_models_warm_first_promotes_only_served_warm_entries() {
    let models: Vec<String> = ["a", "b", "c"].map(String::from).into();
    let warm: Vec<String> = ["c", "ghost"].map(String::from).into();
    assert_eq!(
        order_models_warm_first(&models, &warm),
        ["c", "a", "b"].map(String::from).to_vec(),
        "warm first, stale warm entries ignored, order stable"
    );
    assert_eq!(order_models_warm_first(&models, &[]), models);
}

#[test]
fn parse_identity_line_accepts_name_email_and_rejects_malformed() {
    assert_eq!(
        parse_identity_line("Ada Lovelace <ada@example.com>"),
        Some(("Ada Lovelace".to_string(), "ada@example.com".to_string()))
    );
    assert_eq!(parse_identity_line("no brackets"), None);
    assert_eq!(parse_identity_line("<ada@example.com>"), None, "empty name");
    assert_eq!(parse_identity_line("Ada <not-an-email>"), None);
}

#[test]
fn select_hosted_provider_lists_available_and_notes_unavailable_rows() {
    // The picker over the core roster: supported rows are numbered;
    // an oauth-auth drop-in shows as an "(unavailable: …)" note with
    // the reason — visible, never silently dropped, never numbered.
    let mut presets = newt_core::provider_preset::builtin_presets();
    presets.push(ProviderPreset {
        name: "corp-sso".into(),
        display_name: Some("Corp SSO".into()),
        base_url: "https://llm.corp.example/v1".into(),
        auth_type: newt_core::provider_preset::AuthType::OauthDeviceCode,
        ..Default::default()
    });
    // 9 available rows == FILTER_THRESHOLD → straight numbered list
    // (no filter prompt); row 4 is OpenRouter in roster order. The
    // filter path itself is pinned by select_row_filter_maps_back….
    let console = ScriptedConsole::new(&["4"]);
    let picked = select_hosted_provider(&console.operator(), &presets).unwrap();
    assert!(matches!(
        picked,
        HostedProviderChoice::Preset(preset) if preset.name == "openrouter"
    ));
    assert!(
        !console.transcript().contains("Filter"),
        "at the threshold the list shows directly: {}",
        console.transcript()
    );
    let seen = console.transcript();
    assert!(
        seen.contains("(unavailable: Corp SSO — auth oauth_device_code"),
        "{seen}"
    );
    assert!(
        !seen.contains(") Corp SSO"),
        "unavailable rows are never numbered: {seen}"
    );
}

#[test]
fn select_hosted_provider_accepts_custom_endpoint() {
    let presets = newt_core::provider_preset::builtin_presets();
    let console = ScriptedConsole::new(&["0"]);

    let picked = select_hosted_provider(&console.operator(), &presets).unwrap();

    assert_eq!(picked, HostedProviderChoice::CustomEndpoint);
    // C0c renders options as `[0] label`, one per line, where the wizard
    // used to `say` a hand-numbered "  0) label".
    assert!(console
        .transcript()
        .contains("[0] I have a URL (custom endpoint)"));
}

#[test]
fn select_row_filter_maps_back_to_original_indices() {
    let rows: Vec<String> = (1..=12).map(|i| format!("row-{i}")).collect();
    // Filter to "row-1" matches row-1, row-10..12; pick 2 → "row-10"
    // (original index 9) — the picker must return ORIGINAL indices.
    let console = ScriptedConsole::new(&["row-1", "2"]);
    let idx = select_row(&console.operator(), &rows, "rows").unwrap();
    assert_eq!(idx, 9);
}

#[test]
fn zero_choice_is_available_before_filtering_a_large_roster() {
    let rows: Vec<String> = (1..=12).map(|i| format!("provider-{i}")).collect();
    let console = ScriptedConsole::new(&["0"]);

    let picked = select_row_with_zero(
        &console.operator(),
        &rows,
        "providers",
        "I have a URL (custom endpoint)",
    )
    .unwrap();

    assert_eq!(picked, None);
}

// --- custom-host / preset integration tests ------------------------------

/// Real-resource grounding for the mocked custom-endpoint generation and
/// credential checks; weekly/release only because it writes config files.
#[ignore = "real-resource: weekly/release tier; touches the filesystem"]
#[serial_test::serial(real_fs)]
#[tokio::test]
async fn hosted_provider_custom_endpoint_uses_supplied_base_url() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(wiremock::matchers::header(
            "Authorization",
            "Bearer test-remote-key",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "example/model-a"}]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    mount_authenticated_openai_chat(&server, "test-remote-key").await;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let _config_env = EnvVarGuard::set(newt_core::config::NEWT_CONFIG_DIR_ENV, dir.path());
    newt_core::secrets::session().reset_for_test();
    let server_with_v1 = format!("{}/v1/", server.uri());
    let console = ScriptedConsole::new(&[
        "3",
        "0",
        &server_with_v1,
        "test-remote-key",
        "1",
        "1",
        "",
        "y",
    ]);

    run_with_flow(
        &console.operator(),
        &reqwest::Client::new(),
        &path,
        Flow::FirstRun,
    )
    .await
    .unwrap();

    let name = format!("127-0-0-1-{}", server.address().port());
    let dropin = read_dropin(&path, &name);
    assert_eq!(dropin.endpoint, server.uri());
    assert_eq!(dropin.effective_model(), Some("example/model-a"));
    assert_eq!(dropin.kind, Some(BackendKind::Openai));
    assert!(dropin.api_key_file.is_some());
    assert!(console
        .transcript()
        .contains("0) I have a URL (custom endpoint)"));
    assert!(!console.transcript().contains("test-remote-key"));

    newt_core::secrets::session().reset_for_test();
    assert_eq!(dropin.resolve_api_key().as_deref(), Some("test-remote-key"));
    newt_core::secrets::session().reset_for_test();
}

#[serial_test::serial(real_fs)]
#[tokio::test]
async fn custom_host_auth_required_stores_the_token_encrypted() {
    // An authenticated endpoint 401s the bare probe; the wizard asks for
    // the key ONCE (hidden input), re-probes, and stores the pasted token
    // ENCRYPTED at rest — plaintext never lands on disk or in output.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(wiremock::matchers::header(
            "Authorization",
            "Bearer test-remote-key",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [
                {"id": "example/model-a"},
                {"id": "example/model-b"}
            ]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    mount_authenticated_openai_chat(&server, "test-remote-key").await;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    // Pin the config dir: the machine identity for blank-passphrase
    // encryption lives under it.
    let _config_env = EnvVarGuard::set(newt_core::config::NEWT_CONFIG_DIR_ENV, dir.path());
    newt_core::secrets::session().reset_for_test();
    let client = reqwest::Client::new();

    let server_with_v1 = format!("{}/v1", server.uri());
    // custom host=2, host (with /v1 — stripped), key (hidden), endpoint=1,
    // model=1, passphrase=<Enter: machine key>, write=Y
    let console =
        ScriptedConsole::new(&["2", &server_with_v1, "test-remote-key", "1", "1", "", "y"]);
    run_with(&console.operator(), &client, &path).await.unwrap();

    let name = format!("127-0-0-1-{}", server.address().port());
    let dropin = read_dropin(&path, &name);
    assert_eq!(dropin.effective_model(), Some("example/model-a"));
    assert!(!dropin.endpoint.ends_with("/v1"), "probe suffix stripped");
    let token_ref = dropin.api_key_file.as_deref().expect("key recorded");
    let token_path = PathBuf::from(token_ref);
    let token_name = token_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap();
    assert!(
        token_name.starts_with(&format!("{name}.token.")) && token_name.ends_with(".age"),
        "versioned encrypted ref: {token_ref}"
    );
    let body = std::fs::read_to_string(&token_path).unwrap();
    assert!(
        body.starts_with("-----BEGIN AGE ENCRYPTED FILE-----"),
        "ciphertext on disk"
    );
    assert!(!body.contains("test-remote-key"), "no plaintext token");
    assert!(
        !console.transcript().contains("test-remote-key"),
        "the token is never echoed"
    );
    // The freshly stored token resolves transparently (machine identity).
    newt_core::secrets::session().reset_for_test();
    assert_eq!(dropin.resolve_api_key().as_deref(), Some("test-remote-key"));

    newt_core::secrets::session().reset_for_test();
}

#[serial_test::serial(real_fs)]
#[tokio::test]
async fn preset_skip_key_records_the_env_reference() {
    // A preset with no pasted key writes the backend anyway, recording
    // the provider's canonical env var — nothing stored on disk.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "open-model"}]
        })))
        .mount(&server)
        .await;
    mount_openai_chat(&server).await;
    let _preset_env = EnvVarGuard::remove("NEWT_TEST_PRESET_KEY");
    let preset = ProviderPreset {
        name: "testcloud".into(),
        display_name: Some("Test Cloud".into()),
        base_url: format!("{}/v1", server.uri()),
        env_vars: vec!["NEWT_TEST_PRESET_KEY".into()],
        fallback_models: vec!["fallback-model".into()],
        signup_url: Some("https://example.invalid/keys".into()),
        ..Default::default()
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let client = reqwest::Client::new();
    // key=<Enter: skip>, model=1
    let console = ScriptedConsole::new(&["", "1"]);
    let (_cfg, backend, _pending) = configure_preset(&console.operator(), &client, &preset, &path)
        .await
        .unwrap();
    assert_eq!(backend.api_key_env.as_deref(), Some("NEWT_TEST_PRESET_KEY"));
    assert!(backend.api_key_file.is_none(), "nothing stored on skip");
    assert_eq!(backend.effective_model(), Some("open-model"));
    assert_eq!(backend.kind, Some(BackendKind::Openai));
    assert!(
        console
            .transcript()
            .contains("export $NEWT_TEST_PRESET_KEY"),
        "the skip warns how to supply the key: {}",
        console.transcript()
    );
}

#[serial_test::serial(real_fs)]
#[tokio::test]
async fn preset_pasted_token_is_stored_encrypted_with_a_passphrase() {
    // The pasted-key path: hidden input, optional passphrase, encrypted
    // .token.age reference — and the model probe runs WITH the key.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(wiremock::matchers::header(
            "Authorization",
            "Bearer sk-preset-secret",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "gated-model"}]
        })))
        .mount(&server)
        .await;
    mount_authenticated_openai_chat(&server, "sk-preset-secret").await;
    let _preset_env = EnvVarGuard::remove("NEWT_TEST_PRESET_KEY");
    let preset = ProviderPreset {
        name: "gatedcloud".into(),
        display_name: Some("Gated Cloud".into()),
        base_url: format!("{}/v1", server.uri()),
        env_vars: vec!["NEWT_TEST_PRESET_KEY".into()],
        fallback_models: vec!["fallback-model".into()],
        signup_url: Some("https://example.invalid/keys".into()),
        ..Default::default()
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let _config_env = EnvVarGuard::set(newt_core::config::NEWT_CONFIG_DIR_ENV, dir.path());
    newt_core::secrets::session().reset_for_test();
    let client = reqwest::Client::new();
    // key (hidden), passphrase, model=1
    let console = ScriptedConsole::new(&["sk-preset-secret", "open sesame", "1"]);
    let (_cfg, backend, pending) = configure_preset(&console.operator(), &client, &preset, &path)
        .await
        .unwrap();
    assert!(backend.api_key_env.is_none());
    let token_ref = backend.api_key_file.as_deref().expect("encrypted ref");
    assert!(token_ref.contains("gatedcloud.token."));
    assert!(token_ref.ends_with(".age"));
    assert_eq!(backend.effective_model(), Some("gated-model"));
    let pending = pending.expect("token is held until final write");
    assert_eq!(
        persist_wizard_token(&console.operator(), &path, "gatedcloud", &pending).unwrap(),
        token_ref
    );
    let body = std::fs::read_to_string(&pending.path).unwrap();
    assert!(body.starts_with("-----BEGIN AGE ENCRYPTED FILE-----"));
    assert!(!body.contains("sk-preset-secret"));
    assert!(!console.transcript().contains("sk-preset-secret"));

    newt_core::secrets::session().reset_for_test();
}

#[serial_test::serial(real_fs)]
#[tokio::test]
async fn preset_uses_an_exported_env_var_without_storing_anything() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "env-model"}]
        })))
        .mount(&server)
        .await;
    mount_authenticated_openai_chat(&server, "sk-from-env").await;
    let _preset_env = EnvVarGuard::set("NEWT_TEST_PRESET_KEY", "sk-from-env");
    let preset = ProviderPreset {
        name: "envcloud".into(),
        display_name: Some("Env Cloud".into()),
        base_url: format!("{}/v1", server.uri()),
        env_vars: vec!["NEWT_TEST_PRESET_KEY".into()],
        fallback_models: vec!["fallback-model".into()],
        signup_url: Some("https://example.invalid/keys".into()),
        ..Default::default()
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let client = reqwest::Client::new();
    // use exported?=y, model=1.
    //
    // D1b-2 (#1903): this said `""` — Enter — because `is_yes(&ans, true)`
    // read blank as YES. Adopting a credential from the environment is a
    // decision, and the wizard no longer makes it for the operator; see
    // `an_empty_answer_does_not_adopt_the_exported_key` below.
    let console = ScriptedConsole::new(&["y", "1"]);
    let (_cfg, backend, _pending) = configure_preset(&console.operator(), &client, &preset, &path)
        .await
        .unwrap();
    assert_eq!(backend.api_key_env.as_deref(), Some("NEWT_TEST_PRESET_KEY"));
    assert!(backend.api_key_file.is_none(), "env reference only");
    assert_eq!(backend.effective_model(), Some("env-model"));
}

/// **Regression (D1b-2, #1903): a blank answer does not adopt an exported
/// credential.**
///
/// Before this slice the prompt was `"${var} is set in this shell. Use it?
/// [Y/n] "` resolved by `is_yes(&ans, true)`, so BOTH an empty answer and
/// every unrecognised word meant yes. Adopting a key from the environment is
/// a decision — one with a security consequence, since the adopted key is
/// what the backend then authenticates with — and the wizard no longer makes
/// it for the operator. Blank re-asks, and an input with nothing left to give
/// fails rather than choosing.
// Mutates the process environment, so it shares the sibling's serial
// lane (issue #514 / the #1872 env-race class): two tests setting and
// restoring the same var in parallel see each other's writes.
#[serial_test::serial(real_fs)]
#[tokio::test]
async fn an_empty_answer_does_not_adopt_the_exported_key() {
    let _preset_env = EnvVarGuard::set("NEWT_TEST_PRESET_KEY", "sk-from-env");
    let preset = ProviderPreset {
        name: "envcloud".into(),
        base_url: "http://127.0.0.1:1/v1".into(),
        env_vars: vec!["NEWT_TEST_PRESET_KEY".into()],
        ..Default::default()
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    // An exhausted script answers "" forever — a short pipe.
    let console = ScriptedConsole::new(&[]);
    // `let Err(..) else` rather than `expect_err`: the success type contains
    // `PendingWizardToken`, which deliberately has no `Debug` (it holds a
    // key). Reaching for `expect_err` would mean deriving `Debug` on a
    // secret-carrying record to satisfy a test, which is backwards.
    let Err(err) =
        configure_preset(&console.operator(), &reqwest::Client::new(), &preset, &path).await
    else {
        panic!("a blank answer must not adopt the key");
    };
    assert!(
        err.to_string().contains("no usable answer"),
        "gave up rather than guessing: {err}"
    );
    assert!(
        console
            .output
            .borrow()
            .iter()
            .any(|l| l.contains("not one of the choices — enter y, n")),
        "and said why: {:?}",
        console.output
    );
}

/// **The anti-vacuous twin.** If `decide` refused everything — or if the
/// prompt never resolved at all — the test above would pass while the wizard
/// became unusable. An explicit `n` must still be heard, and must take the
/// paste branch rather than the env branch.
// Mutates the process environment, so it shares the sibling's serial
// lane (issue #514 / the #1872 env-race class): two tests setting and
// restoring the same var in parallel see each other's writes.
#[serial_test::serial(real_fs)]
#[tokio::test]
async fn an_explicit_no_declines_the_exported_key_and_asks_for_one() {
    let _preset_env = EnvVarGuard::set("NEWT_TEST_PRESET_KEY", "sk-from-env");
    let preset = ProviderPreset {
        name: "envcloud".into(),
        base_url: "http://127.0.0.1:1/v1".into(),
        env_vars: vec!["NEWT_TEST_PRESET_KEY".into()],
        ..Default::default()
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    // n = do not use the exported key; then Enter skips the paste prompt.
    let console = ScriptedConsole::new(&["n", ""]);
    let _ = configure_preset(&console.operator(), &reqwest::Client::new(), &preset, &path).await;
    assert!(
        console
            .output
            .borrow()
            .iter()
            .any(|l| l.contains("export $NEWT_TEST_PRESET_KEY")),
        "declining routed to the paste path, which skipped: {:?}",
        console.output
    );
}

/// Regression heir: the old plaintext writer used `var_os("HOME")?`, so
/// on Windows — where the variable is `USERPROFILE` — the `?` bailed and
/// the key went unrecorded. The encrypted writer must likewise record a
/// usable (absolute) reference even with no home to collapse against.
#[test]
#[serial_test::serial(real_fs)]
fn a_token_reference_is_recorded_even_when_home_is_unset() {
    let dir = tempfile::tempdir().unwrap();
    // The machine identity needs a config root even with HOME unset.
    let _config_env = EnvVarGuard::set(newt_core::config::NEWT_CONFIG_DIR_ENV, dir.path());
    newt_core::secrets::session().reset_for_test();
    let _home = EnvVarGuard::remove("HOME");
    let _userprofile = EnvVarGuard::remove("USERPROFILE");

    let path = dir.path().join("config.toml");
    // passphrase=<Enter: machine key>
    let console = ScriptedConsole::new(&[""]);
    let pending = collect_wizard_token(
        &console.operator(),
        &Secret::new("a-secret"),
        &path,
        "example",
    )
    .unwrap();
    let recorded = persist_wizard_token(&console.operator(), &path, "example", &pending)
        .expect("a supplied key must always be recorded, home dir or not");

    assert!(
        !recorded.starts_with('~'),
        "with no home to collapse against, the path stays absolute: {recorded}"
    );
    assert!(recorded.contains("example.token."));
    assert!(recorded.ends_with(".age"));
    let body = std::fs::read_to_string(&recorded).unwrap();
    assert!(body.starts_with("-----BEGIN AGE ENCRYPTED FILE-----"));
    assert!(!body.contains("a-secret"), "never plaintext on disk");

    newt_core::secrets::session().reset_for_test();
}

// --- model selector (#1452): a llama.cpp router serves 30+ models, so the
// operator must never have to type an id exactly. ---

#[test]
fn a_short_list_is_shown_directly_with_no_filter_prompt() {
    let models: Vec<String> = (1..=3).map(|i| format!("model-{i}")).collect();
    let console = ScriptedConsole::new(&["2"]);
    assert_eq!(
        select_model(&console.operator(), &models).unwrap(),
        "model-2"
    );
    // Asking to filter three items would be pure ceremony.
    assert!(
        !console.transcript().contains("Filter"),
        "no filter prompt below the threshold: {}",
        console.transcript()
    );
}

#[test]
fn a_long_list_filters_then_picks_by_number() {
    let mut models: Vec<String> = (1..=30).map(|i| format!("filler-{i}")).collect();
    models.push("qwen3.6_35b".into());
    models.push("qwen3-coder_30b".into());

    // Type a fragment, then choose from the two matches — the operator
    // never types the full id.
    let console = ScriptedConsole::new(&["qwen", "2"]);
    assert_eq!(
        select_model(&console.operator(), &models).unwrap(),
        "qwen3-coder_30b"
    );
    let seen = console.transcript();
    assert!(seen.contains("32 models available"), "{seen}");
    assert!(!seen.contains("filler-1)"), "filtered out: {seen}");
}

#[test]
fn the_filter_is_case_insensitive_and_matches_substrings() {
    let mut models: Vec<String> = (1..=20).map(|i| format!("filler-{i}")).collect();
    models.push("Qwen3-Coder".into());
    let console = ScriptedConsole::new(&["CODER", "1"]);
    assert_eq!(
        select_model(&console.operator(), &models).unwrap(),
        "Qwen3-Coder"
    );
}

/// A filter that matches nothing must not dead-end the operator in an empty
/// menu — it falls back to the whole list.
#[test]
fn a_filter_matching_nothing_falls_back_to_the_full_list() {
    let models: Vec<String> = (1..=20).map(|i| format!("model-{i}")).collect();
    let console = ScriptedConsole::new(&["zzz-no-such-model", "3"]);
    assert_eq!(
        select_model(&console.operator(), &models).unwrap(),
        "model-3"
    );
    assert!(console.transcript().contains("showing all"));
}

#[test]
fn a_blank_filter_shows_everything() {
    let models: Vec<String> = (1..=15).map(|i| format!("model-{i}")).collect();
    let console = ScriptedConsole::new(&["", "15"]);
    assert_eq!(
        select_model(&console.operator(), &models).unwrap(),
        "model-15"
    );
}

/// An out-of-range or unparseable choice takes the first entry rather than
/// erroring out mid-setup.
#[test]
fn an_invalid_choice_falls_back_to_the_first_entry() {
    let models: Vec<String> = vec!["a".into(), "b".into()];
    for answer in ["", "99", "nonsense", "0", "-1"] {
        let console = ScriptedConsole::new(&[answer]);
        assert_eq!(select_model(&console.operator(), &models).unwrap(), "a");
    }
}

#[serial_test::serial(real_fs)]
#[tokio::test]
async fn custom_host_requires_a_host() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [{"name": "qwen2.5-coder:32b"}]
        })))
        .mount(&server)
        .await;
    mount_ollama_chat(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let client = reqwest::Client::new();
    // custom host, empty host (reprompt), then real host, endpoint=1, model=1, write=Y
    let console = ScriptedConsole::new(&["2", "", &server.uri(), "1", "1", "y"]);
    run_with(&console.operator(), &client, &path).await.unwrap();
    let name = format!("127-0-0-1-{}", server.address().port());
    let cfg = Config::load(&path).unwrap();
    assert_eq!(cfg.default_backend.as_deref(), Some(name.as_str()));
    assert_eq!(
        read_dropin(&path, &name).effective_model(),
        Some("qwen2.5-coder:32b")
    );
    // The reprompt message was shown.
    assert!(console.transcript().contains("host is required"));
}
