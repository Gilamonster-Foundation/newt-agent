use super::*;

/// #1951: `startup_drift_notice` used to resolve its own config (a
/// SECOND `Config::resolve()` beyond the one the CLI dispatch preamble
/// already made for the shell-engine and splash checks — three
/// independent resolutions, and three repeats of any warning a bad
/// config file produced, on every single command). It now takes the
/// preamble's own result. `None` is exactly what that preamble passes
/// through when resolution failed — must return cleanly, never panic,
/// and never attempt a probe (there is nothing to interrogate a node
/// about with no config).
#[tokio::test]
async fn startup_drift_notice_skips_cleanly_when_config_did_not_resolve() {
    startup_drift_notice(None).await;
}

/// The other shape a resolved config commonly takes: no `[dgx]` section
/// at all. Must still be a clean no-op — dgx is only probed when
/// actually configured.
#[tokio::test]
async fn startup_drift_notice_skips_cleanly_when_dgx_is_unconfigured() {
    let cfg = Config::default();
    assert!(cfg.dgx.is_none());
    startup_drift_notice(Some(&cfg)).await;
}
use wiremock::matchers::{method, path as wm_path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Scoped guard that removes an env var for the duration of a test and
/// restores its prior value on drop. Keeps tests hermetic against ambient
/// process env (e.g. a sandbox exporting `NEWT_DGX_MODEL`).
#[allow(dead_code)]
struct EnvVarGuard {
    key: String,
    prev: Option<String>,
}

#[allow(dead_code)]
impl EnvVarGuard {
    fn unset(key: &str) -> Self {
        let prev = std::env::var(key).ok();
        std::env::remove_var(key);
        Self {
            key: key.to_string(),
            prev,
        }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.prev {
            Some(v) => std::env::set_var(&self.key, v),
            None => std::env::remove_var(&self.key),
        }
    }
}

/// A recorded SSH call: `(user, host, port, command)`.
type SshCall = (String, String, Option<u16>, String);

/// Recording fake SSH executor: captures the command instead of running it.
struct RecordingSsh {
    calls: std::cell::RefCell<Vec<SshCall>>,
}

impl RecordingSsh {
    fn new() -> Self {
        Self {
            calls: std::cell::RefCell::new(Vec::new()),
        }
    }
}

impl SshExec for RecordingSsh {
    fn run(&self, user: &str, host: &str, port: Option<u16>, command: &str) -> anyhow::Result<()> {
        self.calls.borrow_mut().push((
            user.to_string(),
            host.to_string(),
            port,
            command.to_string(),
        ));
        Ok(())
    }
}

fn classify(task: &str) -> Classification {
    Router::new().classify_detailed(task)
}

// --- route / recommend ---------------------------------------------

#[test]
fn complex_task_picks_coding_formation() {
    let cfg = DgxConfig::home_template();
    let rec = recommend(Some(&cfg), &classify("refactor the entire auth module"));
    assert_eq!(rec.tier, Tier::Complex);
    assert_eq!(rec.formation.as_deref(), Some("coding"));
    assert_eq!(rec.model.as_deref(), Some("qwen2.5-coder:32b"));
    assert_eq!(rec.endpoint, EndpointKind::Ollama);
}

#[test]
fn review_task_picks_review_formation() {
    let cfg = DgxConfig::home_template();
    let rec = recommend(Some(&cfg), &classify("review this PR for security issues"));
    assert_eq!(rec.tier, Tier::Review);
    assert_eq!(rec.formation.as_deref(), Some("review"));
    assert_eq!(rec.endpoint, EndpointKind::InCluster);
}

#[test]
fn no_config_falls_back_to_tier_endpoint() {
    let rec = recommend(None, &classify("fix a typo"));
    assert_eq!(rec.tier, Tier::Fast);
    assert_eq!(rec.formation, None);
    assert_eq!(rec.model, None);
    assert_eq!(rec.endpoint, EndpointKind::Ollama);
}

#[test]
fn config_without_formation_uses_active_model() {
    let cfg = DgxConfig {
        active_model: Some("llama3.1:8b".into()),
        ..DgxConfig::default()
    };
    let rec = recommend(Some(&cfg), &classify("refactor everything"));
    assert_eq!(rec.tier, Tier::Complex);
    assert_eq!(rec.formation, None);
    assert_eq!(rec.model.as_deref(), Some("llama3.1:8b"));
    assert_eq!(rec.endpoint, EndpointKind::InCluster);
}

#[test]
fn standard_tier_endpoint_is_lb() {
    let long = "a".repeat(250);
    let c = classify(&long);
    assert_eq!(c.tier, Tier::Standard);
    assert_eq!(recommend(None, &c).endpoint, EndpointKind::OllamaLb);
}

#[test]
fn tier_labels_are_lowercase() {
    assert_eq!(tier_label(Tier::Fast), "fast");
    assert_eq!(tier_label(Tier::Standard), "standard");
    assert_eq!(tier_label(Tier::Complex), "complex");
    assert_eq!(tier_label(Tier::Review), "review");
}

#[test]
fn why_is_populated_from_reasons() {
    let rec = recommend(None, &classify("review this"));
    assert!(rec.why.contains("review"), "why was: {}", rec.why);
}

// --- probes (wiremock) ---------------------------------------------

#[test]
fn extract_names_handles_shapes() {
    assert!(extract_names(&serde_json::json!(null)).is_empty());
    assert_eq!(
        extract_names(&serde_json::json!([{"name":"a"},{"x":1},{"name":"b"}])),
        vec!["a", "b"]
    );
}

#[tokio::test]
async fn fetch_ollama_models_parses_names() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(wm_path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [{"name":"qwen2.5-coder:32b"},{"name":"llama3.1:8b"}]
        })))
        .mount(&server)
        .await;
    let names = fetch_ollama_models(&http_client(), &server.uri())
        .await
        .unwrap();
    assert_eq!(names, vec!["qwen2.5-coder:32b", "llama3.1:8b"]);
}

#[tokio::test]
async fn fetch_ollama_running_empty_ok() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(wm_path("/api/ps"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"models":[]})))
        .mount(&server)
        .await;
    let names = fetch_ollama_running(&http_client(), &server.uri())
        .await
        .unwrap();
    assert!(names.is_empty());
}

#[tokio::test]
async fn fetch_names_http_error_is_err() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(wm_path("/api/tags"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    assert!(fetch_ollama_models(&http_client(), &server.uri())
        .await
        .is_err());
}

#[tokio::test]
async fn probe_reports_ok_and_status() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(wm_path("/api/tags"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    assert_eq!(
        probe(&http_client(), &server.uri(), "/api/tags").await,
        "OK"
    );
    let other = probe(&http_client(), &server.uri(), "/nope").await;
    assert!(other.starts_with("HTTP"), "got: {other}");
}

#[tokio::test]
async fn probe_unreachable_host() {
    // Port 1 is reserved/closed — connection fails fast.
    let s = probe(&http_client(), "http://127.0.0.1:1", "/api/tags").await;
    assert!(s.starts_with("unreachable"), "got: {s}");
}

// --- warm ----------------------------------------------------------

#[test]
fn warm_body_is_load_only() {
    let b = warm_body("qwen2.5-coder:7b", "30m");
    assert_eq!(b["model"], "qwen2.5-coder:7b");
    assert_eq!(b["keep_alive"], "30m");
    assert_eq!(b["stream"], false);
    // No prompt => Ollama loads without generating.
    assert!(b.get("prompt").is_none());
}

#[tokio::test]
async fn warm_model_reports_load_seconds() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wm_path("/api/generate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "model": "m",
            "done": true,
            "load_duration": 13_000_000_000u64
        })))
        .mount(&server)
        .await;
    let secs = warm_model(&http_client(), &server.uri(), "m", "30m")
        .await
        .unwrap();
    assert_eq!(secs, Some(13.0));
}

#[tokio::test]
async fn warm_model_already_resident_is_none() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wm_path("/api/generate"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "model": "m", "done": true })),
        )
        .mount(&server)
        .await;
    let secs = warm_model(&http_client(), &server.uri(), "m", "30m")
        .await
        .unwrap();
    assert_eq!(secs, None);
}

#[tokio::test]
async fn warm_model_http_error_is_err() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wm_path("/api/generate"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    assert!(warm_model(&http_client(), &server.uri(), "m", "30m")
        .await
        .is_err());
}

// --- setup ---------------------------------------------------------

#[test]
fn setup_template_prints_toml_does_not_write() {
    // --template should succeed and not touch any file.
    setup(None, None, "dgx", None, true, true).unwrap();
}

#[test]
fn setup_no_args_prints_usage() {
    // No host + no template: prints guidance, still succeeds.
    setup(None, None, "dgx", None, false, true).unwrap();
}

#[test]
fn setup_writes_config_with_host() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.toml");

    setup(
        Some(&cfg_path),
        Some("REDACTED-IP"),
        "dgx",
        Some("qwen2.5-coder:32b"),
        false,
        true, // yes — skip prompt
    )
    .unwrap();

    let text = std::fs::read_to_string(&cfg_path).unwrap();
    assert!(text.contains("REDACTED-IP"), "host not in config: {text}");
    assert!(
        text.contains("qwen2.5-coder:32b"),
        "model not in config: {text}"
    );
    assert!(text.contains(":11434"), "ollama port not in config: {text}");
    assert!(text.contains(":8000"), "vllm port not in config: {text}");
}

#[test]
fn setup_preserves_existing_config_fields() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.toml");

    // Write a seed config with a custom backend.
    std::fs::write(
        &cfg_path,
        r#"[[backends]]
name = "existing"
endpoint = "http://localhost:11434"
model = "llama3.1:8b"
tiers = ["FAST", "STANDARD"]
"#,
    )
    .unwrap();

    setup(
        Some(&cfg_path),
        Some("REDACTED-HOST"),
        "home",
        None,
        false,
        true,
    )
    .unwrap();

    let text = std::fs::read_to_string(&cfg_path).unwrap();
    assert!(
        text.contains("existing"),
        "pre-existing backend lost: {text}"
    );
    assert!(
        text.contains("REDACTED-HOST"),
        "new dgx host not written: {text}"
    );
}

#[test]
fn setup_node_name_propagates() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.toml");

    setup(
        Some(&cfg_path),
        Some("REDACTED-IP"),
        "lab",
        None,
        false,
        true,
    )
    .unwrap();

    let text = std::fs::read_to_string(&cfg_path).unwrap();
    assert!(
        text.contains("\"lab\"") || text.contains("'lab'") || text.contains("lab"),
        "node name not in config: {text}"
    );
    assert!(text.contains("active_node"), "active_node not set: {text}");
}

// --- pull: HF siblings fetch (wiremock) ----------------------------

#[tokio::test]
async fn fetch_hf_siblings_parses_gguf() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(wm_path("/api/models/unsloth/Repo-GGUF"))
        .and(query_param("blobs", "true"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "siblings": [
                {"rfilename": "README.md"},
                {"rfilename": "Repo-Q8_0-00001-of-00002.gguf", "size": 100u64},
                {"rfilename": "Repo-Q8_0-00002-of-00002.gguf", "size": 200u64}
            ]
        })))
        .mount(&server)
        .await;
    let files = fetch_hf_siblings(&http_client(), &server.uri(), "unsloth", "Repo-GGUF")
        .await
        .unwrap();
    assert_eq!(files.len(), 2);
}

#[tokio::test]
async fn fetch_hf_siblings_http_error_is_err() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(wm_path("/api/models/o/r"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    assert!(fetch_hf_siblings(&http_client(), &server.uri(), "o", "r")
        .await
        .is_err());
}

// --- pull: fit pre-flight reporting --------------------------------

#[test]
fn report_fit_fits_ok() {
    assert!(report_fit(
        FitVerdict::Fits {
            model_bytes: 10,
            mem_bytes: 100
        },
        false
    )
    .is_ok());
}

#[test]
fn report_fit_undetectable_proceeds() {
    assert!(report_fit(FitVerdict::Undetectable { model_bytes: 10 }, false).is_ok());
}

#[test]
fn report_fit_exceeds_refuses_without_force() {
    let err = report_fit(
        FitVerdict::Exceeds {
            model_bytes: 200,
            mem_bytes: 100,
        },
        false,
    )
    .unwrap_err();
    assert!(err.to_string().contains("--force"), "{err}");
}

#[test]
fn report_fit_exceeds_proceeds_with_force() {
    assert!(report_fit(
        FitVerdict::Exceeds {
            model_bytes: 200,
            mem_bytes: 100,
        },
        true,
    )
    .is_ok());
}

// --- pull: plan execution via recording SSH ------------------------

#[test]
fn execute_native_plan_runs_ollama_pull() {
    let ssh = RecordingSsh::new();
    let plan = PullPlan::OllamaNative {
        tag: "qwen2.5-coder:32b".into(),
    };
    execute_hf_plan(&ssh, "bob", "dgx", &plan, false, false).unwrap();
    let calls = ssh.calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "bob");
    assert!(calls[0].3.contains("ollama pull 'qwen2.5-coder:32b'"));
}

#[test]
fn execute_single_file_plan_runs_hf_pull() {
    let ssh = RecordingSsh::new();
    let plan = PullPlan::SingleFileHf {
        org: "unsloth".into(),
        repo: "Repo-GGUF".into(),
        quant: "Q8_0".into(),
    };
    execute_hf_plan(&ssh, "bob", "dgx", &plan, false, false).unwrap();
    let calls = ssh.calls.borrow();
    assert!(calls[0]
        .3
        .contains("ollama pull 'hf.co/unsloth/Repo-GGUF:Q8_0'"));
}

#[test]
fn execute_sharded_plan_runs_script() {
    let ssh = RecordingSsh::new();
    let plan = PullPlan::ShardedHf {
        org: "unsloth".into(),
        repo: "Repo-GGUF".into(),
        quant: "Q8_0".into(),
        parts: vec![
            "Repo-Q8_0-00001-of-00002.gguf".into(),
            "Repo-Q8_0-00002-of-00002.gguf".into(),
        ],
        modelfile: "FROM ./Repo-Q8_0-00001-of-00002.gguf\n".into(),
        name: "repo-gguf-q8_0".into(),
    };
    execute_hf_plan(&ssh, "bob", "dgx", &plan, true, false).unwrap();
    let calls = ssh.calls.borrow();
    let cmd = &calls[0].3;
    assert!(cmd.contains("ollama create 'repo-gguf-q8_0'"));
    assert_eq!(cmd.matches("curl -L --fail -C -").count(), 2);
    assert!(cmd.contains("Authorization: Bearer $HF_TOKEN"));
}

#[test]
fn execute_dry_run_does_not_ssh() {
    let ssh = RecordingSsh::new();
    let plan = PullPlan::OllamaNative { tag: "m:1".into() };
    execute_hf_plan(&ssh, "bob", "dgx", &plan, false, true).unwrap();
    assert!(ssh.calls.borrow().is_empty(), "dry-run must not SSH");
}

// --- rm / ps (wiremock) --------------------------------------------

#[tokio::test]
async fn delete_ollama_model_ok() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(wm_path("/api/delete"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    delete_ollama_model(&http_client(), &server.uri(), "m:1")
        .await
        .unwrap();
}

#[tokio::test]
async fn delete_ollama_model_error_is_err() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(wm_path("/api/delete"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    assert!(delete_ollama_model(&http_client(), &server.uri(), "m:1")
        .await
        .is_err());
}

// extract_ps moved to newt_core::backend_probe::parse_ollama_ps (the ONE
// /api/ps home) together with its unit tests.

#[tokio::test]
async fn fetch_ollama_ps_parses_loaded() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(wm_path("/api/ps"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [{"name": "qwen2.5-coder:32b", "size": 21474836480u64}]
        })))
        .mount(&server)
        .await;
    let loaded = fetch_ollama_ps(&http_client(), &server.uri())
        .await
        .unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].0, "qwen2.5-coder:32b");
}

#[tokio::test]
async fn fetch_ollama_ps_http_error_is_err() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(wm_path("/api/ps"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    assert!(fetch_ollama_ps(&http_client(), &server.uri())
        .await
        .is_err());
}

// --- vllm wiring (Step 14.11) -----------------------------------------

fn sample_vllm_plan() -> dgx_vllm::VllmPlan {
    dgx_vllm::resolve_plan(dgx_vllm::PlanInputs {
        model: "nvidia/Qwen3.6-35B-A3B-NVFP4",
        served_name: Some("qwen3.6-35b"),
        dtype: Some(dgx_vllm::Dtype::Nvfp4),
        tensor_parallel: 1,
        max_model_len: Some(262144),
        gpu_mem_util: 0.90,
        port: 8000,
        runtime: dgx_vllm::VllmRuntime::Native,
        extra: vec![],
    })
}

fn plan_args() -> VllmPlanArgs {
    VllmPlanArgs {
        served_name: None,
        dtype: None,
        tensor_parallel: 1,
        max_model_len: None,
        gpu_mem_util: 0.90,
        port: 8000,
        docker: false,
        extra: vec![],
    }
}

#[test]
fn vllm_up_dry_run_does_not_ssh() {
    let ssh = RecordingSsh::new();
    execute_vllm_plan(&ssh, "bob", "dgx", &sample_vllm_plan(), true).unwrap();
    assert!(ssh.calls.borrow().is_empty(), "dry-run must not SSH");
}

#[test]
fn vllm_up_records_nohup_serve_command() {
    let ssh = RecordingSsh::new();
    execute_vllm_plan(&ssh, "bob", "dgx", &sample_vllm_plan(), false).unwrap();
    let calls = ssh.calls.borrow();
    assert_eq!(calls.len(), 1);
    let cmd = &calls[0].3;
    assert!(cmd.contains("nohup"));
    assert!(cmd.contains("vllm") && cmd.contains("serve"));
    // Model id shell-quoted; port + pidfile present.
    assert!(cmd.contains("'nvidia/Qwen3.6-35B-A3B-NVFP4'"));
    assert!(cmd.contains("--port"));
    assert!(cmd.contains("echo $! >") && cmd.contains(".pid"));
}

#[test]
fn vllm_down_records_kill_pidfile() {
    let ssh = RecordingSsh::new();
    let cmd = dgx_vllm::vllm_stop_command("qwen3.6-35b");
    run_or_dryrun(&ssh, "bob", "dgx", None, &cmd, false, "down").unwrap();
    let calls = ssh.calls.borrow();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].3.contains("qwen3.6-35b.pid"));
    assert!(calls[0].3.contains("kill"));
}

#[test]
fn vllm_logs_records_tail_command() {
    let ssh = RecordingSsh::new();
    let cmd = dgx_vllm::vllm_logs_command("qwen3.6-35b", 50);
    run_or_dryrun(&ssh, "bob", "dgx", None, &cmd, false, "logs").unwrap();
    assert!(ssh.calls.borrow()[0].3.contains("tail -n 50 -f"));
}

#[test]
fn vllm_config_renders_argv_without_ssh() {
    // Pure: builds + prints the plan, returns Ok, never SSHes.
    assert!(vllm_config("nvidia/Qwen3.6-35B-A3B-NVFP4", &plan_args()).is_ok());
    let mut docker = plan_args();
    docker.docker = true;
    assert!(vllm_config("org/model", &docker).is_ok());
}

#[test]
fn vllm_config_rejects_unknown_dtype() {
    let mut bad = plan_args();
    bad.dtype = Some("nonsense".to_string());
    let err = vllm_config("org/model", &bad).unwrap_err().to_string();
    // The error must name the valid set so the user can self-correct.
    assert!(
        err.contains("nvfp4") && err.contains("gptq"),
        "unhelpful error: {err}"
    );
}

#[test]
fn vllm_up_refuses_docker_execution() {
    // `up --docker` must refuse (preview-only); native is the only launcher.
    let err = ensure_executable_runtime(dgx_vllm::VllmRuntime::Docker)
        .unwrap_err()
        .to_string();
    assert!(err.contains("--docker") && err.contains("config"));
    assert!(ensure_executable_runtime(dgx_vllm::VllmRuntime::Native).is_ok());
}

#[tokio::test]
async fn vllm_ps_parses_v1_models() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(wm_path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{ "id": "m1" }, { "id": "m2" }]
        })))
        .mount(&server)
        .await;
    let models = fetch_vllm_models(&server.uri()).await.unwrap();
    assert_eq!(models, vec!["m1".to_string(), "m2".to_string()]);
}

#[tokio::test]
async fn poll_vllm_ready_succeeds_on_first_ok() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(wm_path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "data": [] })))
        .mount(&server)
        .await;
    assert!(poll_vllm_ready(&server.uri(), &RetryPolicy::immediate(0))
        .await
        .is_ok());
}

#[tokio::test]
async fn poll_vllm_ready_retries_then_succeeds() {
    let server = MockServer::start().await;
    // wiremock matches the FIRST-mounted mock of equal priority, so mount the
    // 503 (capped at 2 hits) FIRST and the success SECOND: requests 1-2 hit
    // the exhausting 503, request 3 falls through to the 200. (Asserting the
    // request count guards against a trivially-passing single-request test.)
    Mock::given(method("GET"))
        .and(wm_path("/v1/models"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(wm_path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "data": [] })))
        .mount(&server)
        .await;
    // 503 is a retryable 5xx; 3 retries cover the two failures + success.
    assert!(poll_vllm_ready(&server.uri(), &RetryPolicy::immediate(3))
        .await
        .is_ok());
    // The retry path was actually exercised: 2 failures + 1 success = 3.
    let received = server.received_requests().await.unwrap();
    assert_eq!(
        received.len(),
        3,
        "expected retry-then-succeed (2x503 + 1x200)"
    );
}

#[tokio::test]
async fn poll_vllm_ready_gives_up_when_never_ready() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(wm_path("/v1/models"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    assert!(poll_vllm_ready(&server.uri(), &RetryPolicy::immediate(1))
        .await
        .is_err());
}

fn config_with_nodes(active: &str, names: &[&str]) -> Config {
    Config {
        dgx: Some(DgxConfig {
            active_node: Some(active.to_string()),
            nodes: names
                .iter()
                .map(|n| DgxNode {
                    name: n.to_string(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[test]
fn apply_vllm_persist_falls_back_to_active_node() {
    let mut config = config_with_nodes("home", &["home"]);
    let recorded = apply_vllm_persist(&mut config, None, "http://dgx:8000", "qwen3.6-35b");
    assert!(recorded);
    let dgx = config.dgx.unwrap();
    assert_eq!(dgx.active_endpoint, EndpointKind::Vllm);
    assert_eq!(dgx.active_model.as_deref(), Some("qwen3.6-35b"));
    assert_eq!(dgx.nodes[0].vllm.as_deref(), Some("http://dgx:8000"));
}

#[test]
fn apply_vllm_persist_targets_explicit_node_over_active() {
    let mut config = config_with_nodes("home", &["home", "other"]);
    let recorded = apply_vllm_persist(&mut config, Some("other"), "http://other:8000", "m");
    assert!(recorded);
    let dgx = config.dgx.unwrap();
    // The named node gets the URL; the active node is left untouched.
    assert_eq!(
        dgx.node("other").unwrap().vllm.as_deref(),
        Some("http://other:8000")
    );
    assert_eq!(dgx.node("home").unwrap().vllm, None);
}

#[test]
fn apply_vllm_persist_reports_false_when_node_missing() {
    let mut config = config_with_nodes("home", &["home"]);
    let recorded = apply_vllm_persist(&mut config, Some("ghost"), "http://ghost:8000", "m");
    // No matching node: URL not recorded (caller warns), but endpoint flips.
    assert!(!recorded);
    let dgx = config.dgx.unwrap();
    assert_eq!(dgx.active_endpoint, EndpointKind::Vllm);
    assert_eq!(dgx.nodes[0].vllm, None);
}

#[test]
fn vllm_probe_uses_memavailable_pull_uses_memtotal() {
    // Regression: the vLLM fit probe must read MemAvailable (awk $7) so it
    // nets out a resident Ollama model, while the Ollama pull path stays on
    // MemTotal ($2) — unchanged behavior in this step.
    assert_eq!(MEM_AVAILABLE_AWK, "$7");
    assert_eq!(MEM_TOTAL_AWK, "$2");
    assert!(node_mem_probe(MEM_AVAILABLE_AWK).contains("print $7"));
    assert!(node_mem_probe(MEM_TOTAL_AWK).contains("print $2"));
}

// --- gpu residency + eviction (Step 14.12) ----------------------------

fn residency(ollama: &[(&str, Option<u64>)], vllm: &[&str], mem: Option<u64>) -> Residency {
    Residency {
        ollama: ollama.iter().map(|(n, s)| (n.to_string(), *s)).collect(),
        vllm: vllm.iter().map(|s| s.to_string()).collect(),
        mem_available: mem,
    }
}

#[test]
fn ollama_unload_body_uses_keep_alive_zero() {
    // keep_alive: 0 is the unload signal (the inverse of `warm`).
    let body = ollama_unload_body("qwen3-coder:30b");
    assert_eq!(body["model"], "qwen3-coder:30b");
    assert_eq!(body["keep_alive"], 0);
}

#[test]
fn ollama_evict_targets_lists_resident_names() {
    let res = residency(&[("a", Some(100)), ("b", None)], &[], None);
    assert_eq!(
        ollama_evict_targets(&res),
        vec!["a".to_string(), "b".to_string()]
    );
    assert!(ollama_evict_targets(&residency(&[], &[], None)).is_empty());
}

#[test]
fn residency_is_contended_only_when_both_resident() {
    assert!(residency(&[("a", None)], &["v"], None).is_contended());
    assert!(!residency(&[("a", None)], &[], None).is_contended());
    assert!(!residency(&[], &["v"], None).is_contended());
}

#[test]
fn render_residency_shows_both_engines_mem_and_contention() {
    let gib = 1024 * 1024 * 1024;
    let out = render_residency(&residency(
        &[("qwen3-coder:30b", Some(38 * gib))],
        &["qwen3.6-35b"],
        Some(105 * gib),
    ));
    assert!(out.contains("MemAvailable: 105.0 GB"));
    assert!(out.contains("qwen3-coder:30b") && out.contains("38.0 GB"));
    assert!(out.contains("qwen3.6-35b"));
    // Both resident → the contention warning fires.
    assert!(out.contains("--evict-ollama"));
}

#[test]
fn render_residency_empty_shows_none_and_no_warning() {
    let out = render_residency(&residency(&[], &[], None));
    assert!(out.contains("MemAvailable: (undetected)"));
    assert_eq!(out.matches("(none)").count(), 2); // both engines empty
    assert!(!out.contains("⚠"));
}

#[tokio::test]
async fn unload_ollama_model_posts_keep_alive_zero() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wm_path("/api/generate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&server)
        .await;
    unload_ollama_model(&http_client(), &server.uri(), "qwen3-coder:30b")
        .await
        .unwrap();
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    let body: serde_json::Value = reqs[0].body_json().unwrap();
    assert_eq!(body["keep_alive"], 0);
}

#[tokio::test]
async fn unload_ollama_model_http_error_is_err() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wm_path("/api/generate"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    assert!(unload_ollama_model(&http_client(), &server.uri(), "m")
        .await
        .is_err());
}

/// A DgxConfig whose active node's Ollama endpoint points at `uri`.
fn ollama_config_at(uri: &str) -> DgxConfig {
    DgxConfig {
        active_node: Some("home".to_string()),
        active_endpoint: EndpointKind::Ollama,
        nodes: vec![DgxNode {
            name: "home".to_string(),
            ollama: Some(uri.to_string()),
            ..Default::default()
        }],
        ..Default::default()
    }
}

#[tokio::test]
async fn evict_ollama_models_unloads_each_resident() {
    let server = MockServer::start().await;
    // Two resident models from /api/ps.
    Mock::given(method("GET"))
        .and(wm_path("/api/ps"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [{ "name": "a", "size": 100 }, { "name": "b", "size": 200 }]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(wm_path("/api/generate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&server)
        .await;
    evict_ollama_models(&ollama_config_at(&server.uri()))
        .await
        .unwrap();
    let reqs = server.received_requests().await.unwrap();
    // One /api/ps probe + one unload POST per resident model.
    assert_eq!(reqs.iter().filter(|r| r.url.path() == "/api/ps").count(), 1);
    assert_eq!(
        reqs.iter()
            .filter(|r| r.url.path() == "/api/generate")
            .count(),
        2
    );
}

#[tokio::test]
async fn evict_ollama_models_ok_when_none_resident() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(wm_path("/api/ps"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "models": [] })))
        .mount(&server)
        .await;
    evict_ollama_models(&ollama_config_at(&server.uri()))
        .await
        .unwrap();
    // No resident models → no unload POSTs issued.
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(
        reqs.iter()
            .filter(|r| r.url.path() == "/api/generate")
            .count(),
        0
    );
}

#[tokio::test]
async fn evict_ollama_models_ok_when_no_endpoint() {
    // No nodes / no Ollama endpoint → best-effort no-op, not an error.
    assert!(evict_ollama_models(&DgxConfig::default()).await.is_ok());
}

/// A DgxConfig whose active node serves vLLM at `uri` and is SSH-reachable.
fn vllm_config_at(uri: &str) -> DgxConfig {
    DgxConfig {
        active_node: Some("home".to_string()),
        active_endpoint: EndpointKind::Vllm,
        nodes: vec![DgxNode {
            name: "home".to_string(),
            vllm: Some(uri.to_string()),
            ssh_host: Some("dgx.test".to_string()),
            ..Default::default()
        }],
        ..Default::default()
    }
}

#[tokio::test]
async fn evict_vllm_server_stops_each_served_model() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(wm_path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{ "id": "qwen3.6-35b" }]
        })))
        .mount(&server)
        .await;
    let ssh = RecordingSsh::new();
    evict_vllm_server(&ssh, &vllm_config_at(&server.uri()))
        .await
        .unwrap();
    let calls = ssh.calls.borrow();
    assert_eq!(calls.len(), 1, "one stop command per served model");
    // The kill targets the served model's pidfile, over the node's SSH host.
    assert_eq!(calls[0].1, "dgx.test");
    assert!(calls[0].3.contains("qwen3.6-35b.pid"));
    assert!(calls[0].3.contains("kill"));
}

#[tokio::test]
async fn evict_vllm_server_noop_when_no_server() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(wm_path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "data": [] })))
        .mount(&server)
        .await;
    let ssh = RecordingSsh::new();
    evict_vllm_server(&ssh, &vllm_config_at(&server.uri()))
        .await
        .unwrap();
    assert!(ssh.calls.borrow().is_empty(), "no server → no SSH kill");
}

#[tokio::test]
async fn evict_vllm_server_ok_when_no_endpoint() {
    // No vLLM endpoint configured → best-effort no-op, no SSH.
    let ssh = RecordingSsh::new();
    assert!(evict_vllm_server(&ssh, &DgxConfig::default()).await.is_ok());
    assert!(ssh.calls.borrow().is_empty());
}

#[test]
fn resolve_warm_model_explicit_arg_wins() {
    let dgx = DgxConfig::default();
    assert_eq!(
        resolve_warm_model(Some("qwen2.5-coder:32b".to_string()), true, &dgx).unwrap(),
        "qwen2.5-coder:32b"
    );
}

#[test]
fn resolve_warm_model_evict_vllm_requires_explicit_model() {
    // Hermetic: inject an empty env so ambient NEWT_DGX_MODEL cannot leak
    // in via resolve_active_model() and override the config's active model.
    let no_env = |_: &str| None;
    // After `vllm up`, active_model is the vLLM served name; warming it on
    // Ollama would 404, so --evict-vllm with no arg must refuse helpfully.
    let mut dgx = DgxConfig {
        active_endpoint: EndpointKind::Vllm,
        active_model: Some("qwen3.6-35b".to_string()),
        ..Default::default()
    };
    let err = resolve_warm_model_with(None, true, &dgx, &no_env)
        .unwrap_err()
        .to_string();
    assert!(err.contains("--evict-vllm") && err.contains("qwen3.6-35b"));
    // Without --evict-vllm, the active model is used as before.
    dgx.active_endpoint = EndpointKind::Ollama;
    dgx.active_model = Some("llama3.1:8b".to_string());
    assert_eq!(
        resolve_warm_model_with(None, false, &dgx, &no_env).unwrap(),
        "llama3.1:8b"
    );
}

#[test]
fn resolve_warm_model_errors_when_no_active_and_no_arg() {
    // No arg, no --evict-vllm, no active model → the usual NoActiveModel.
    // Hermetic: inject an empty env so ambient NEWT_DGX_MODEL cannot leak
    // in via resolve_active_model() and make this branch resolve.
    let no_env = |_: &str| None;
    assert!(resolve_warm_model_with(None, false, &DgxConfig::default(), &no_env).is_err());
}

// --- switch (Step 14.14) ----------------------------------------------

#[test]
fn parse_switch_engine_accepts_ollama_and_vllm() {
    assert_eq!(parse_switch_engine("ollama").unwrap(), EndpointKind::Ollama);
    assert_eq!(parse_switch_engine("VLLM").unwrap(), EndpointKind::Vllm);
    // The non-node-local kinds (and junk) are refused.
    assert!(parse_switch_engine("ollama_lb").is_err());
    assert!(parse_switch_engine("openai").is_err());
}

#[test]
fn ollama_has_model_matches_exact_tag() {
    let installed = vec!["nemotron3:33b".to_string(), "qwen2.5-coder:32b".to_string()];
    assert!(ollama_has_model(&installed, "nemotron3:33b"));
    // No fuzzy / prefix match — a different tag is absent.
    assert!(!ollama_has_model(&installed, "nemotron3:8b"));
    assert!(!ollama_has_model(&installed, "nemotron3"));
}

#[test]
fn apply_active_sets_endpoint_and_model() {
    let mut config = Config::default();
    apply_active(&mut config, EndpointKind::Vllm, "qwen3.6-35b");
    let dgx = config.dgx.as_ref().unwrap();
    assert_eq!(dgx.active_endpoint, EndpointKind::Vllm);
    assert_eq!(dgx.active_model.as_deref(), Some("qwen3.6-35b"));
    // Flipping back to Ollama updates both in place.
    apply_active(&mut config, EndpointKind::Ollama, "nemotron3:33b");
    let dgx = config.dgx.as_ref().unwrap();
    assert_eq!(dgx.active_endpoint, EndpointKind::Ollama);
    assert_eq!(dgx.active_model.as_deref(), Some("nemotron3:33b"));
}

#[test]
fn default_vllm_plan_args_are_sane() {
    let a = default_vllm_plan_args();
    assert_eq!(a.tensor_parallel, 1);
    assert_eq!(a.port, 8000);
    assert!((a.gpu_mem_util - 0.90).abs() < f64::EPSILON);
    assert!(!a.docker && a.dtype.is_none() && a.max_model_len.is_none());
}

// --- adopt (Step 14.15) -----------------------------------------------

#[test]
fn derive_live_state_prefers_vllm_then_ollama() {
    // A running vLLM server wins (it holds the pool).
    let vllm = vec!["qwen3.6-35b".to_string()];
    let ollama = vec![("nemotron3:33b".to_string(), Some(38))];
    assert_eq!(
        derive_live_state(&vllm, &ollama),
        Some((EndpointKind::Vllm, "qwen3.6-35b".to_string()))
    );
    // No vLLM → the resident Ollama model.
    assert_eq!(
        derive_live_state(&[], &ollama),
        Some((EndpointKind::Ollama, "nemotron3:33b".to_string()))
    );
    // Nothing running → None.
    assert_eq!(derive_live_state(&[], &[]), None);
}

#[test]
fn reconcile_classifies_sync_mismatch_and_no_live() {
    let no_vllm: Vec<String> = vec![];
    let no_ollama: Vec<(String, Option<u64>)> = vec![];
    let ollama_nemo = vec![("nemotron3:33b".to_string(), Some(38))];
    let vllm_qwen = vec!["qwen3.6-35b".to_string()];

    // In sync: the configured Ollama model is resident.
    assert_eq!(
        reconcile(
            EndpointKind::Ollama,
            Some("nemotron3:33b"),
            &no_vllm,
            &ollama_nemo
        ),
        ReconcileVerdict::InSync {
            kind: EndpointKind::Ollama,
            model: "nemotron3:33b".to_string()
        }
    );
    // Coexistence is NOT a mismatch: config wants ollama:nemotron, it's
    // resident, and a vLLM server is ALSO up — the configured target is live.
    assert_eq!(
        reconcile(
            EndpointKind::Ollama,
            Some("nemotron3:33b"),
            &vllm_qwen,
            &ollama_nemo
        ),
        ReconcileVerdict::InSync {
            kind: EndpointKind::Ollama,
            model: "nemotron3:33b".to_string()
        }
    );
    // Mismatch: config says ollama:nemotron, it's NOT resident, vLLM is up.
    assert_eq!(
        reconcile(
            EndpointKind::Ollama,
            Some("nemotron3:33b"),
            &vllm_qwen,
            &no_ollama
        ),
        ReconcileVerdict::Mismatch {
            cfg_kind: EndpointKind::Ollama,
            cfg_model: Some("nemotron3:33b".to_string()),
            live_kind: EndpointKind::Vllm,
            live_model: "qwen3.6-35b".to_string(),
        }
    );
    // Same engine, configured model NOT among those served → mismatch.
    assert!(matches!(
        reconcile(
            EndpointKind::Vllm,
            Some("a"),
            &["b".to_string()],
            &no_ollama
        ),
        ReconcileVerdict::Mismatch { .. }
    ));
    // Nothing live.
    assert_eq!(
        reconcile(EndpointKind::Ollama, Some("x"), &no_vllm, &no_ollama),
        ReconcileVerdict::NoLiveState
    );
}

#[test]
fn reconcile_action_flags_win_then_tty_then_report() {
    // Explicit flags win regardless of TTY.
    assert_eq!(
        reconcile_action(true, false, false, None),
        ReconcileAction::Adopt
    );
    assert_eq!(
        reconcile_action(false, true, false, None),
        ReconcileAction::Enforce
    );
    // No flag + not a TTY → only report (never change state silently).
    assert_eq!(
        reconcile_action(false, false, false, None),
        ReconcileAction::Report
    );
    // TTY answers.
    assert_eq!(
        reconcile_action(false, false, true, Some("a")),
        ReconcileAction::Adopt
    );
    assert_eq!(
        reconcile_action(false, false, true, Some("e")),
        ReconcileAction::Enforce
    );
    assert_eq!(
        reconcile_action(false, false, true, Some("c")),
        ReconcileAction::Report
    );
    assert_eq!(
        reconcile_action(false, false, true, None),
        ReconcileAction::Report
    );
}

#[test]
fn drift_notice_line_only_on_mismatch() {
    // Mismatch → a one-line notice naming the node, the config, and the fix.
    let line = drift_notice_line(&ReconcileVerdict::Mismatch {
        cfg_kind: EndpointKind::Ollama,
        cfg_model: Some("nemotron3:33b".to_string()),
        live_kind: EndpointKind::Vllm,
        live_model: "qwen3.6-35b".to_string(),
    })
    .unwrap();
    assert!(line.contains("vllm:qwen3.6-35b"));
    assert!(line.contains("ollama:nemotron3:33b"));
    assert!(line.contains("dgx adopt"));
    // In-sync / no-live stay silent — no startup noise.
    assert!(drift_notice_line(&ReconcileVerdict::InSync {
        kind: EndpointKind::Vllm,
        model: "m".to_string()
    })
    .is_none());
    assert!(drift_notice_line(&ReconcileVerdict::NoLiveState).is_none());
}

// --- deploy / clear (issue #709, PR2) ---------------------------------

/// Fetch a registry variant by slug for tests (panics on a typo'd slug).
fn variant(slug: &str) -> &'static ModelVariant {
    dgx_registry::find_variant(slug).unwrap_or_else(|| panic!("no variant {slug}"))
}

#[test]
fn serve_port_maps_tool_to_convention_port() {
    assert_eq!(serve_port(InferenceTool::Vllm), 8000);
    assert_eq!(serve_port(InferenceTool::LlamaCpp), 8000);
    assert_eq!(serve_port(InferenceTool::Ollama), 11434);
}

#[test]
fn select_strongest_variant_picks_the_best_fit_for_the_budget() {
    // 96 GiB available, 1 node → 81.6 usable → 21/35/70 fit, 104 does not →
    // the strongest is the 35B FP16 (70 GiB). This WIRES PR1's selector.
    let v = select_deploy_variant(true, None, 96.0).expect("a model fits");
    assert_eq!(v.name, "Ornith-1.0-35B");
    assert_eq!(v.format, "FP16");
    // A huge budget reaches the strongest overall (the 397B FP8).
    let v = select_deploy_variant(true, None, 600.0).expect("everything fits");
    assert_eq!(v.name, "Ornith-1.0-397B");
    assert_eq!(v.format, "FP8");
}

#[test]
fn select_strongest_variant_errors_when_nothing_fits() {
    // 10 GiB → below the smallest model's headroom-adjusted need.
    let err = select_deploy_variant(true, None, 10.0)
        .unwrap_err()
        .to_string();
    assert!(err.contains("no registry model fits"), "{err}");
    assert!(err.contains("newt dgx clear"), "{err}");
}

#[test]
fn select_explicit_variant_looks_up_the_slug() {
    let v = select_deploy_variant(false, Some("ornith-397b-q2"), 0.0).expect("known slug");
    assert_eq!(v.name, "Ornith-1.0-397B");
    assert_eq!(v.format, "Q2_K_GGUF");
    assert_eq!(v.tool, InferenceTool::LlamaCpp);
}

#[test]
fn select_explicit_variant_rejects_unknown_and_missing() {
    let err = select_deploy_variant(false, Some("ornith-99b-fp4"), 0.0)
        .unwrap_err()
        .to_string();
    assert!(err.contains("unknown model"), "{err}");
    // No --strongest and no slug → a clear, actionable error.
    let err = select_deploy_variant(false, None, 0.0)
        .unwrap_err()
        .to_string();
    assert!(err.contains("--strongest"), "{err}");
}

#[test]
fn deploy_route_mode_a_when_script_present() {
    // The expected Mode-A script path for the selected variant.
    let v = variant("ornith-35b-fp8");
    assert_eq!(
        deploy_route(v, true),
        DeployRoute::Script {
            path: "~/ornith-35b-fp8.sh".to_string(),
            slug: "ornith-35b-fp8".to_string(),
            port: 8000,
        }
    );
    // An Ollama variant routes Mode A on the Ollama port.
    let v = variant("ornith-35b-q4");
    assert_eq!(
        deploy_route(v, true),
        DeployRoute::Script {
            path: "~/ornith-35b-q4.sh".to_string(),
            slug: "ornith-35b-q4".to_string(),
            port: 11434,
        }
    );
}

#[test]
fn deploy_route_mode_b_fallback_when_script_absent() {
    // No convention script → fall back to a generated vLLM serve of the
    // variant's model name.
    let v = variant("ornith-35b-fp8");
    assert_eq!(
        deploy_route(v, false),
        DeployRoute::VllmFallback {
            model: "Ornith-1.0-35B".to_string(),
            port: 8000,
        }
    );
}

#[test]
fn script_probe_command_and_parse() {
    let v = variant("ornith-35b-fp16");
    let path = dgx_registry::script_name(v);
    let cmd = script_probe_command(&path);
    // Always exits 0 (FOUND/MISSING) so SSH-up is distinguishable from a
    // missing script; the tilde is left unquoted so the node expands it.
    assert_eq!(
        cmd,
        "test -f ~/ornith-35b-fp16.sh && echo FOUND || echo MISSING"
    );
    assert!(script_is_present("FOUND\n"));
    assert!(script_is_present("  FOUND  "));
    assert!(!script_is_present("MISSING\n"));
    assert!(!script_is_present(""));
}

#[test]
fn deploy_launch_command_nohups_the_script_detached() {
    let cmd = deploy_launch_command("~/ornith-35b-fp8.sh", "ornith-35b-fp8");
    assert!(cmd.starts_with("set -eu\n"));
    // Tilde left unquoted (expands on the node), detached with nohup.
    assert!(cmd.contains("nohup ~/ornith-35b-fp8.sh"));
    assert!(cmd.contains("ornith-35b-fp8.log"));
    assert!(cmd.contains("ornith-35b-fp8.pid"));
    assert!(cmd.contains("echo $! >"));
    // The state dir is $HOME-rooted (no leaked absolute home path).
    assert!(cmd.contains("$HOME/.newt/dgx/deploy"));
}

#[test]
fn restart_ollama_command_is_host_free_and_stable() {
    let cmd = restart_ollama_command();
    assert!(cmd.contains("systemctl"));
    assert!(cmd.contains("restart ollama"));
    // Tries user scope then system scope — names no host.
    assert!(cmd.contains("--user"));
}

#[test]
fn restart_ollama_service_records_the_command() {
    let ssh = RecordingSsh::new();
    restart_ollama_service(&ssh, "bob", "dgx.test");
    let calls = ssh.calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "bob");
    assert_eq!(calls[0].1, "dgx.test");
    assert_eq!(calls[0].3, restart_ollama_command());
}

#[test]
fn freed_gib_reports_growth_and_saturates() {
    let mk = |avail_gib: u64| dgx_status::MemBudget {
        total_bytes: 128 * 1024 * 1024 * 1024,
        available_bytes: avail_gib * 1024 * 1024 * 1024,
        workloads: vec![],
    };
    // available grew 10 → 30 GiB → 20 GiB freed.
    assert!((freed_gib(&mk(10), &mk(30)) - 20.0).abs() < 1e-9);
    // A shrink (a workload started in between) saturates to 0, never < 0.
    assert!((freed_gib(&mk(30), &mk(10))).abs() < 1e-9);
}

/// A DgxConfig whose active node serves BOTH engines at `uri` and is
/// SSH-reachable — for exercising the `clear` eviction sequence.
fn both_engines_config_at(uri: &str) -> DgxConfig {
    DgxConfig {
        active_node: Some("home".to_string()),
        active_endpoint: EndpointKind::Vllm,
        nodes: vec![DgxNode {
            name: "home".to_string(),
            ollama: Some(uri.to_string()),
            vllm: Some(uri.to_string()),
            ssh_host: Some("dgx.test".to_string()),
            ..Default::default()
        }],
        ..Default::default()
    }
}

#[tokio::test]
async fn clear_sequence_evicts_both_engines_then_restarts() {
    // The exact SSH sequence `clear --restart-ollama` issues, over a mocked
    // SSH seam + wiremock'd engine endpoints (no live node/network).
    let server = MockServer::start().await;
    // A running vLLM server (so evict_vllm_server kills it).
    Mock::given(method("GET"))
        .and(wm_path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{ "id": "qwen3.6-35b" }]
        })))
        .mount(&server)
        .await;
    // A resident Ollama model (so evict_ollama_models unloads it).
    Mock::given(method("GET"))
        .and(wm_path("/api/ps"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [{ "name": "nemotron3:33b", "size": 100 }]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(wm_path("/api/generate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&server)
        .await;

    let cfg = both_engines_config_at(&server.uri());
    let ssh = RecordingSsh::new();
    // Mirror clear()'s mutating steps over the injectable seams.
    evict_vllm_server(&ssh, &cfg).await.unwrap();
    evict_ollama_models(&cfg).await.unwrap();
    restart_ollama_service(&ssh, &cfg.ssh_user(), &cfg.ssh_host().unwrap());

    // The Ollama unload went over HTTP (not SSH): one /api/generate POST.
    // (Done before borrowing `calls` so no RefCell ref is held across await.)
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(
        reqs.iter()
            .filter(|r| r.url.path() == "/api/generate")
            .count(),
        1
    );

    let calls = ssh.calls.borrow();
    assert_eq!(calls.len(), 2, "vLLM stop + ollama restart");
    // 1) vLLM server stopped by pidfile.
    assert_eq!(calls[0].1, "dgx.test");
    assert!(calls[0].3.contains("qwen3.6-35b.pid"));
    assert!(calls[0].3.contains("kill"));
    // 2) ollama service restarted.
    assert_eq!(calls[1].3, restart_ollama_command());
}
