//! Interactive first-run setup wizard (`newt setup`).
//!
//! Unlike [`crate::wizard`] (the silent auto-prober behind `newt init`), this
//! is a *human-driven* wizard: it asks where the model runs (local Ollama or a
//! remote DGX endpoint), probes the endpoint for installed models, lets the
//! user pick one, previews the resulting config, and writes
//! `~/.newt/config.toml` only after confirmation.
//!
//! The console I/O is abstracted behind the [`Console`] trait so the whole
//! flow can be driven by scripted answers in tests (against a `wiremock`
//! endpoint) without a real TTY. The pure config-building and URL-normalising
//! helpers are unit-tested directly.
//!
//! ## What it writes (and why)
//!
//! The runtime backend resolution (`resolve_backend_choice` in [`crate`]) reads
//! the config two different ways depending on protocol, so the wizard writes
//! whatever that resolver actually honours:
//!
//! - **Ollama-protocol** endpoints (local, or DGX `ollama` / `ollama_lb` /
//!   `in_cluster`) → a `[dgx]` block whose first node carries the URL in its
//!   `ollama` field plus an `active_model`. `resolve_backend_config` reads
//!   `dgx.nodes[0].ollama` + `dgx.active_model`, so the URL is mirrored into
//!   `ollama` even when the user picked an `ollama_lb` / `in_cluster` flavour
//!   (all three speak the native Ollama wire protocol).
//! - **vLLM / OpenAI-compatible** endpoints → a `[[backends]]` entry with
//!   `kind = "openai"`. `resolve_backend_choice` prefers the first such backend.

use newt_core::{BackendConfig, BackendKind, Config, DgxConfig, DgxNode, EndpointKind, Tier};
use std::io::{self, Write};
use std::path::Path;

// ---------------------------------------------------------------------------
// Console abstraction (real stdin/stdout vs. scripted answers in tests)
// ---------------------------------------------------------------------------

/// Console I/O for the wizard. The real impl talks to stdin/stdout; tests feed
/// a queue of answers and capture emitted lines.
pub trait Console {
    /// Print `prompt` (no trailing newline) and read one trimmed line of input.
    fn ask(&mut self, prompt: &str) -> io::Result<String>;
    /// Emit an informational line.
    fn say(&mut self, line: &str);
}

/// Real console: prompts on stdout, reads a line from stdin.
struct StdinConsole;

impl Console for StdinConsole {
    fn ask(&mut self, prompt: &str) -> io::Result<String> {
        print!("{prompt}");
        io::stdout().flush()?;
        let mut buf = String::new();
        let n = io::stdin().read_line(&mut buf)?;
        if n == 0 {
            // EOF (e.g. piped empty input): behave like an empty answer so the
            // caller's default kicks in rather than looping forever.
            return Ok(String::new());
        }
        Ok(buf.trim().to_string())
    }

    fn say(&mut self, line: &str) {
        println!("{line}");
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Run the interactive setup wizard, writing to `~/.newt/config.toml`.
pub fn run(_color: bool) -> anyhow::Result<()> {
    let config_path =
        Config::user_config_path().unwrap_or_else(|| std::path::PathBuf::from("newt.toml"));
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(4))
        .build()
        .unwrap_or_default();
    let mut console = StdinConsole;
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(run_with(&mut console, &client, &config_path))
    })
}

// ---------------------------------------------------------------------------
// Driver (fully testable: scripted Console + wiremock client)
// ---------------------------------------------------------------------------

/// The wizard flow, parameterised over its console and HTTP client so it can be
/// exercised end-to-end in tests.
async fn run_with(
    console: &mut dyn Console,
    client: &reqwest::Client,
    config_path: &Path,
) -> anyhow::Result<()> {
    console.say(&format!(
        "newt v{} — interactive setup",
        env!("CARGO_PKG_VERSION")
    ));

    if config_path.exists() {
        let ans = console.ask(&format!(
            "A config already exists at {}. Overwrite? [y/N] ",
            config_path.display()
        ))?;
        if !is_yes(&ans, false) {
            console.say("Keeping the existing config. Nothing written.");
            return Ok(());
        }
    }

    let cfg = match choose_backend(console)? {
        BackendChoice::Ollama => configure_ollama(console, client).await?,
        BackendChoice::Dgx => configure_dgx(console, client).await?,
    };

    // Preview before committing anything to disk.
    let preview = toml::to_string_pretty(&cfg)
        .unwrap_or_else(|e| format!("# (could not render preview: {e})"));
    console.say("\nResulting config:\n");
    console.say(&preview);

    let ans = console.ask(&format!("Write to {}? [Y/n] ", config_path.display()))?;
    if !is_yes(&ans, true) {
        console.say("Aborted. Nothing written.");
        return Ok(());
    }
    cfg.save(config_path)?;
    console.say(&format!("Wrote {}.", config_path.display()));
    console.say("Edit that file (or re-run `newt setup`) to change anything.");
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendChoice {
    Ollama,
    Dgx,
}

fn choose_backend(console: &mut dyn Console) -> anyhow::Result<BackendChoice> {
    console.say("\nWhere does your model run?");
    console.say("  1) Ollama  (local, or a plain self-hosted Ollama host)");
    console.say("  2) DGX     (remote NVIDIA endpoint: Ollama or vLLM)");
    let ans = console.ask("Choose [1]: ")?;
    match parse_choice(&ans, 2).unwrap_or(1) {
        2 => Ok(BackendChoice::Dgx),
        _ => Ok(BackendChoice::Ollama),
    }
}

// ---------------------------------------------------------------------------
// Ollama path
// ---------------------------------------------------------------------------

async fn configure_ollama(
    console: &mut dyn Console,
    client: &reqwest::Client,
) -> anyhow::Result<Config> {
    let default_url = "http://127.0.0.1:11434";
    let raw = console.ask(&format!("Ollama host [{default_url}]: "))?;
    let url = normalize_url(
        if raw.is_empty() { default_url } else { &raw },
        "http",
        11434,
    );

    let model = pick_model(console, client, &url, Protocol::Ollama).await?;
    Ok(build_ollama_config(
        Config::default(),
        "default",
        EndpointKind::Ollama,
        &url,
        &model,
    ))
}

// ---------------------------------------------------------------------------
// DGX path
// ---------------------------------------------------------------------------

async fn configure_dgx(
    console: &mut dyn Console,
    client: &reqwest::Client,
) -> anyhow::Result<Config> {
    // Host is required for DGX — keep asking until we get something.
    let host = loop {
        let raw = console.ask("DGX host (e.g. dgx.home.lab or http://10.0.0.5:8000): ")?;
        if !raw.is_empty() {
            break raw;
        }
        console.say("  A host is required for a DGX endpoint.");
    };

    console.say("\nEndpoint flavour:");
    console.say("  1) ollama       (direct DGX Ollama,        /api/chat)");
    console.say("  2) ollama_lb    (round-robin Ollama LB,    /api/chat)");
    console.say("  3) in_cluster   (in-cluster Ollama proxy,  /api/chat)");
    console.say("  4) vllm         (vLLM OpenAI-compatible,   /v1/chat/completions)");
    let ans = console.ask("Choose [1]: ")?;
    let kind = match parse_choice(&ans, 4).unwrap_or(1) {
        2 => EndpointKind::OllamaLb,
        3 => EndpointKind::InCluster,
        4 => EndpointKind::Vllm,
        _ => EndpointKind::Ollama,
    };

    let (default_port, protocol) = match kind {
        EndpointKind::Vllm => (8000, Protocol::OpenAi),
        _ => (11434, Protocol::Ollama),
    };
    let url = normalize_url(&host, "http", default_port);

    let model = pick_model(console, client, &url, protocol).await?;

    let cfg = match kind {
        EndpointKind::Vllm => {
            let key_env = console.ask("API-key env var (optional, e.g. DGX_API_KEY) [none]: ")?;
            let key_env = if key_env.is_empty() {
                None
            } else {
                Some(key_env)
            };
            build_openai_config(Config::default(), "dgx-vllm", &url, &model, key_env)
        }
        _ => build_ollama_config(Config::default(), "dgx", kind, &url, &model),
    };
    Ok(cfg)
}

// ---------------------------------------------------------------------------
// Model selection (probe → numbered list → pick, with manual fallback)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Protocol {
    Ollama,
    OpenAi,
}

async fn pick_model(
    console: &mut dyn Console,
    client: &reqwest::Client,
    url: &str,
    protocol: Protocol,
) -> anyhow::Result<String> {
    console.say(&format!("Probing {url} for installed models…"));
    let models = match protocol {
        Protocol::Ollama => fetch_ollama_models(client, url).await,
        Protocol::OpenAi => fetch_openai_models(client, url).await,
    };

    let models = match models {
        Ok(m) if !m.is_empty() => m,
        Ok(_) => {
            console.say("  Endpoint answered but listed no models.");
            return ask_model_name(console);
        }
        Err(e) => {
            console.say(&format!("  Could not reach the endpoint ({e})."));
            return ask_model_name(console);
        }
    };

    console.say("\nAvailable models:");
    for (i, m) in models.iter().enumerate() {
        console.say(&format!("  {}) {m}", i + 1));
    }
    let ans = console.ask("Choose [1]: ")?;
    let idx = parse_choice(&ans, models.len()).map(|n| n - 1).unwrap_or(0);
    Ok(models[idx].clone())
}

fn ask_model_name(console: &mut dyn Console) -> anyhow::Result<String> {
    let default = "llama3.1:8b";
    let raw = console.ask(&format!("Model name [{default}]: "))?;
    Ok(if raw.is_empty() {
        default.to_string()
    } else {
        raw
    })
}

// ---------------------------------------------------------------------------
// HTTP probes
// ---------------------------------------------------------------------------

/// List models from an Ollama endpoint via `GET /api/tags`.
async fn fetch_ollama_models(client: &reqwest::Client, url: &str) -> anyhow::Result<Vec<String>> {
    let tags_url = format!("{}/api/tags", url.trim_end_matches('/'));
    let resp = client.get(&tags_url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("HTTP {}", resp.status());
    }
    let json: serde_json::Value = resp.json().await?;
    Ok(json["models"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default())
}

/// List models from an OpenAI-compatible endpoint via `GET /v1/models`.
async fn fetch_openai_models(client: &reqwest::Client, url: &str) -> anyhow::Result<Vec<String>> {
    let models_url = format!("{}/v1/models", url.trim_end_matches('/'));
    let resp = client.get(&models_url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("HTTP {}", resp.status());
    }
    let json: serde_json::Value = resp.json().await?;
    Ok(json["data"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m["id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default())
}

// ---------------------------------------------------------------------------
// Pure helpers (unit-tested directly)
// ---------------------------------------------------------------------------

/// Parse a 1-based menu answer into its 1-based number when it's a valid digit
/// in `1..=max`. Empty or out-of-range input returns `None` so the caller can
/// apply its own default.
fn parse_choice(input: &str, max: usize) -> Option<usize> {
    let n: usize = input.trim().parse().ok()?;
    if (1..=max).contains(&n) {
        Some(n)
    } else {
        None
    }
}

/// Interpret a yes/no answer. `default` is returned for empty input.
fn is_yes(input: &str, default: bool) -> bool {
    match input.trim().to_ascii_lowercase().as_str() {
        "" => default,
        "y" | "yes" => true,
        _ => false,
    }
}

/// Normalise a user-typed endpoint into a full `scheme://host:port` URL.
///
/// Accepts a bare host (`dgx.home.lab`), `host:port`, or a full URL
/// (`https://dgx.home.lab`). A bare host gets `default_scheme` and
/// `default_port`; a `host:port` keeps its port; a full URL is passed through
/// (trailing slash trimmed).
fn normalize_url(raw: &str, default_scheme: &str, default_port: u16) -> String {
    let raw = raw.trim().trim_end_matches('/');
    if raw.contains("://") {
        return raw.to_string();
    }
    if raw.contains(':') {
        // host:port — assume the default scheme.
        return format!("{default_scheme}://{raw}");
    }
    format!("{default_scheme}://{raw}:{default_port}")
}

/// Build a config for an Ollama-protocol endpoint (local or DGX flavour).
///
/// The URL is written into the node's `ollama` field regardless of `kind`
/// (all three Ollama flavours speak the native wire protocol, and the runtime
/// resolver reads `dgx.nodes[0].ollama`); the kind-specific field is set too
/// for fidelity, and `active_endpoint` records the user's choice.
fn build_ollama_config(
    mut base: Config,
    node_name: &str,
    kind: EndpointKind,
    url: &str,
    model: &str,
) -> Config {
    let mut node = DgxNode {
        name: node_name.to_string(),
        ollama: Some(url.to_string()),
        ..Default::default()
    };
    match kind {
        EndpointKind::OllamaLb => node.ollama_lb = Some(url.to_string()),
        EndpointKind::InCluster => node.in_cluster = Some(url.to_string()),
        // Ollama (already in `ollama`) and Vllm (handled elsewhere) need nothing extra.
        _ => {}
    }
    base.dgx = Some(DgxConfig {
        active_node: Some(node_name.to_string()),
        active_endpoint: kind,
        active_model: Some(model.to_string()),
        nodes: vec![node],
        ..Default::default()
    });
    base
}

/// Build a config for a vLLM / OpenAI-compatible endpoint: a single
/// `kind = "openai"` backend, which `resolve_backend_choice` prefers.
fn build_openai_config(
    mut base: Config,
    name: &str,
    endpoint: &str,
    model: &str,
    api_key_env: Option<String>,
) -> Config {
    base.backends = vec![BackendConfig {
        name: name.to_string(),
        endpoint: endpoint.to_string(),
        model: model.to_string(),
        tiers: vec![Tier::Fast, Tier::Standard, Tier::Complex, Tier::Review],
        kind: BackendKind::Openai,
        api: Default::default(),
        api_key_file: None,
        api_key_env,
    }];
    base
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Scripted console: pops answers in order, records what was said.
    struct ScriptedConsole {
        answers: VecDeque<String>,
        output: Vec<String>,
    }

    impl ScriptedConsole {
        fn new(answers: &[&str]) -> Self {
            Self {
                answers: answers.iter().map(|s| s.to_string()).collect(),
                output: Vec::new(),
            }
        }
        fn transcript(&self) -> String {
            self.output.join("\n")
        }
    }

    impl Console for ScriptedConsole {
        fn ask(&mut self, _prompt: &str) -> io::Result<String> {
            Ok(self.answers.pop_front().unwrap_or_default())
        }
        fn say(&mut self, line: &str) {
            self.output.push(line.to_string());
        }
    }

    // --- pure helpers -----------------------------------------------------

    #[test]
    fn parse_choice_valid_and_out_of_range() {
        assert_eq!(parse_choice("1", 2), Some(1));
        assert_eq!(parse_choice("2", 2), Some(2));
        assert_eq!(parse_choice("3", 2), None); // out of range
        assert_eq!(parse_choice("", 2), None); // empty
        assert_eq!(parse_choice("abc", 2), None); // non-numeric
        assert_eq!(parse_choice("0", 2), None); // zero is not 1-based
    }

    #[test]
    fn is_yes_respects_default() {
        assert!(is_yes("", true));
        assert!(!is_yes("", false));
        assert!(is_yes("y", false));
        assert!(is_yes("YES", false));
        assert!(!is_yes("n", true));
        assert!(!is_yes("nope", true));
    }

    #[test]
    fn normalize_url_bare_host_gets_scheme_and_port() {
        assert_eq!(
            normalize_url("dgx.home.lab", "http", 11434),
            "http://dgx.home.lab:11434"
        );
    }

    #[test]
    fn normalize_url_keeps_explicit_port_and_full_url() {
        assert_eq!(
            normalize_url("dgx.home.lab:8000", "http", 11434),
            "http://dgx.home.lab:8000"
        );
        assert_eq!(
            normalize_url("https://dgx.home.lab/", "http", 11434),
            "https://dgx.home.lab"
        );
    }

    #[test]
    fn build_ollama_config_writes_resolvable_node() {
        let cfg = build_ollama_config(
            Config::default(),
            "default",
            EndpointKind::Ollama,
            "http://127.0.0.1:11434",
            "qwen2.5-coder:7b",
        );
        let dgx = cfg.dgx.unwrap();
        assert_eq!(dgx.active_model.as_deref(), Some("qwen2.5-coder:7b"));
        // The URL must land in `ollama` so resolve_backend_config picks it up.
        assert_eq!(
            dgx.nodes[0].ollama.as_deref(),
            Some("http://127.0.0.1:11434")
        );
    }

    #[test]
    fn build_ollama_config_mirrors_lb_url_into_ollama_field() {
        let cfg = build_ollama_config(
            Config::default(),
            "dgx",
            EndpointKind::OllamaLb,
            "https://ollama.home.lab",
            "llama3.1:70b",
        );
        let node = &cfg.dgx.unwrap().nodes[0];
        // Both the flavour-specific field and the resolver-read `ollama` field.
        assert_eq!(node.ollama_lb.as_deref(), Some("https://ollama.home.lab"));
        assert_eq!(node.ollama.as_deref(), Some("https://ollama.home.lab"));
    }

    #[test]
    fn build_openai_config_sets_openai_kind() {
        let cfg = build_openai_config(
            Config::default(),
            "dgx-vllm",
            "http://dgx.home.lab:8000",
            "llama3.1:8b",
            Some("DGX_API_KEY".to_string()),
        );
        let b = &cfg.backends[0];
        assert_eq!(b.kind, BackendKind::Openai);
        assert_eq!(b.endpoint, "http://dgx.home.lab:8000");
        assert_eq!(b.api_key_env.as_deref(), Some("DGX_API_KEY"));
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
        let models = fetch_openai_models(&client, &server.uri()).await.unwrap();
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
        let mut console = ScriptedConsole::new(&["1", &server.uri(), "2", "y"]);
        run_with(&mut console, &client, &path).await.unwrap();

        let cfg = Config::load(&path).unwrap();
        let dgx = cfg.dgx.unwrap();
        assert_eq!(dgx.active_model.as_deref(), Some("qwen2.5-coder:7b"));
        assert_eq!(dgx.nodes[0].ollama.as_deref(), Some(server.uri().as_str()));
    }

    #[serial_test::serial(real_fs)]
    #[tokio::test]
    async fn dgx_vllm_flow_writes_openai_backend() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": "meta/llama-3.1-8b-instruct"}]
            })))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let client = reqwest::Client::new();
        // backend=2 (DGX), host=<mock url>, flavour=4 (vllm), model=1, key-env=(none), write=Y
        let mut console = ScriptedConsole::new(&["2", &server.uri(), "4", "1", "", "y"]);
        run_with(&mut console, &client, &path).await.unwrap();

        let cfg = Config::load(&path).unwrap();
        let b = &cfg.backends[0];
        assert_eq!(b.kind, BackendKind::Openai);
        assert_eq!(b.model, "meta/llama-3.1-8b-instruct");
        assert_eq!(b.endpoint, server.uri());
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
        let mut console = ScriptedConsole::new(&[
            "1",
            "http://127.0.0.1:1", // connection refused
            "phi3:mini",
            "y",
        ]);
        run_with(&mut console, &client, &path).await.unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.dgx.unwrap().active_model.as_deref(), Some("phi3:mini"));
    }

    #[serial_test::serial(real_fs)]
    #[tokio::test]
    async fn decline_overwrite_keeps_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "# sentinel\n").unwrap();
        let client = reqwest::Client::new();
        // Overwrite? → N (default).
        let mut console = ScriptedConsole::new(&["n"]);
        run_with(&mut console, &client, &path).await.unwrap();
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
        let mut console = ScriptedConsole::new(&["1", &server.uri(), "1", "n"]);
        run_with(&mut console, &client, &path).await.unwrap();
        assert!(!path.exists());
    }

    #[serial_test::serial(real_fs)]
    #[tokio::test]
    async fn dgx_requires_a_host() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [{"name": "qwen2.5-coder:32b"}]
            })))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let client = reqwest::Client::new();
        // DGX, empty host (reprompt), then real host, flavour=1 (ollama), model=1, write=Y
        let mut console = ScriptedConsole::new(&["2", "", &server.uri(), "1", "1", "y"]);
        run_with(&mut console, &client, &path).await.unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(
            cfg.dgx.unwrap().active_model.as_deref(),
            Some("qwen2.5-coder:32b")
        );
        // The reprompt message was shown.
        assert!(console.transcript().contains("host is required"));
    }
}
