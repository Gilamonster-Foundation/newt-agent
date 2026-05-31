//! `newt dgx` — NVIDIA DGX endpoint management.
//!
//! - `route`  — classify a task and recommend a (model, endpoint) formation.
//! - `status` — active-endpoint health + models currently loaded on the DGX.
//! - `models` — list models available on the active endpoint.
//! - `doctor` — probe every configured endpoint flavor + DNS guidance.
//!
//! Later Phase 14 steps add `setup`/`use`/`endpoint`/`formation`/`node`,
//! Ollama lifecycle (`pull`/`rm`/`ps`), SSH ops (`run`/`push`/`watch`), and
//! `nim`.

use std::path::Path;

use clap::Subcommand;
use newt_core::dgx::{DgxConfig, EndpointKind};
use newt_core::router::Classification;
use newt_core::{Config, Router, Tier};
use newt_inference::local::LocalVllmBackend;

/// `newt dgx <cmd>` subcommands.
#[derive(Subcommand, Debug)]
pub enum DgxCmd {
    /// Classify a task and recommend a model + endpoint formation.
    Route {
        /// Task description to classify (quote multi-word tasks).
        task: String,
    },
    /// Show active-endpoint health and the models loaded on the DGX.
    Status,
    /// List the models available on the active DGX endpoint.
    Models,
    /// Probe every configured DGX endpoint flavor and report reachability.
    Doctor,
    /// Pre-load a model into VRAM on the active endpoint so the first real
    /// request doesn't pay the cold-load latency (which can blow past tight
    /// per-task timeouts, e.g. in `newt-eval`). Uses Ollama's load-only
    /// request — no tokens generated — and pins it resident via `keep_alive`.
    Warm {
        /// Model to warm. Defaults to the active model
        /// (`[dgx].active_model` / `NEWT_DGX_MODEL`).
        model: Option<String>,
        /// How long Ollama keeps the model resident after warming.
        #[arg(long, default_value = "30m")]
        keep_alive: String,
    },
}

/// Dispatch a `newt dgx` subcommand.
pub async fn run(cmd: DgxCmd, config_path: Option<&Path>) -> anyhow::Result<()> {
    match cmd {
        DgxCmd::Route { task } => route(&task, config_path),
        DgxCmd::Status => status(config_path).await,
        DgxCmd::Models => models(config_path).await,
        DgxCmd::Doctor => doctor(config_path).await,
        DgxCmd::Warm { model, keep_alive } => warm(config_path, model, &keep_alive).await,
    }
}

// ---------------------------------------------------------------------------
// route
// ---------------------------------------------------------------------------

fn route(task: &str, config_path: Option<&Path>) -> anyhow::Result<()> {
    let config = load_config(config_path)?;
    let classification = Router::new().classify_detailed(task);
    let rec = recommend(config.dgx.as_ref(), &classification);

    println!(
        "  Complexity:  {}  (confidence {:.2})",
        tier_label(rec.tier),
        rec.confidence
    );
    match &rec.formation {
        Some(name) => println!("  Formation:   {name}"),
        None => println!("  Formation:   (none configured — run `newt dgx setup`)"),
    }
    match &rec.model {
        Some(model) => println!("  Model:       {model}"),
        None => println!("  Model:       (unset — set [dgx].active_model or NEWT_DGX_MODEL)"),
    }
    println!("  Endpoint:    {}", rec.endpoint);
    println!("  Why:         {}", rec.why);
    Ok(())
}

/// A routing recommendation derived from a [`Classification`].
struct Recommendation {
    tier: Tier,
    confidence: f64,
    formation: Option<String>,
    model: Option<String>,
    endpoint: EndpointKind,
    why: String,
}

/// Recommend a formation for `c`: prefer a configured formation whose name
/// matches the tier convention, else fall back to the active model and a
/// tier-appropriate endpoint.
fn recommend(dgx: Option<&DgxConfig>, c: &Classification) -> Recommendation {
    let why = c
        .reasons
        .first()
        .cloned()
        .unwrap_or_else(|| "no routing signal".to_string());

    if let Some(formation) = dgx.and_then(|d| d.formation(preferred_formation(c.tier))) {
        return Recommendation {
            tier: c.tier,
            confidence: c.confidence,
            formation: Some(formation.name.clone()),
            model: Some(formation.model.clone()),
            endpoint: formation.endpoint,
            why,
        };
    }

    Recommendation {
        tier: c.tier,
        confidence: c.confidence,
        formation: None,
        model: dgx.and_then(|d| d.active_model.clone()),
        endpoint: default_endpoint_for(c.tier),
        why,
    }
}

/// Conventional formation name for a tier (matched against configured
/// formations in [`recommend`]).
fn preferred_formation(tier: Tier) -> &'static str {
    match tier {
        Tier::Review => "review",
        Tier::Complex => "coding",
        Tier::Standard => "standard",
        Tier::Fast => "fast",
    }
}

/// Advisory endpoint when no formation matches: heavier tiers prefer the
/// model-aware in-cluster proxy, lighter tiers the direct / LB endpoints.
fn default_endpoint_for(tier: Tier) -> EndpointKind {
    match tier {
        Tier::Review | Tier::Complex => EndpointKind::InCluster,
        Tier::Standard => EndpointKind::OllamaLb,
        Tier::Fast => EndpointKind::Ollama,
    }
}

fn tier_label(tier: Tier) -> &'static str {
    match tier {
        Tier::Fast => "fast",
        Tier::Standard => "standard",
        Tier::Complex => "complex",
        Tier::Review => "review",
    }
}

// ---------------------------------------------------------------------------
// status / models / doctor
// ---------------------------------------------------------------------------

async fn models(config_path: Option<&Path>) -> anyhow::Result<()> {
    let dgx = dgx_config(config_path)?;
    let kind = dgx.active_endpoint;
    let base = dgx.resolve_endpoint()?;
    println!("Models on {kind} endpoint ({base}):");

    let names = if kind.is_openai_compatible() {
        LocalVllmBackend::new(base.as_str(), "")
            .list_models()
            .await?
            .into_iter()
            .map(|m| m.id)
            .collect::<Vec<_>>()
    } else {
        fetch_ollama_models(&http_client(), &base).await?
    };

    if names.is_empty() {
        println!("  (none)");
    }
    for name in &names {
        println!("  {name}");
    }
    Ok(())
}

async fn status(config_path: Option<&Path>) -> anyhow::Result<()> {
    let dgx = dgx_config(config_path)?;
    let kind = dgx.active_endpoint;
    let base = dgx.resolve_endpoint()?;
    let client = http_client();
    let health_path = if kind.is_openai_compatible() {
        "/v1/models"
    } else {
        "/api/tags"
    };

    println!("DGX status — {kind} endpoint ({base})");
    println!("  Health:   {}", probe(&client, &base, health_path).await);

    if kind.is_openai_compatible() {
        println!("  Running:  (vLLM exposes no running-models endpoint)");
    } else {
        match fetch_ollama_running(&client, &base).await {
            Ok(names) if names.is_empty() => println!("  Running:  (no models loaded)"),
            Ok(names) => println!("  Running:  {}", names.join(", ")),
            Err(e) => println!("  Running:  (unavailable: {e})"),
        }
    }
    println!("  GPU mem:  (run `newt dgx run nvidia-smi` once SSH lands in Step 14.6)");
    Ok(())
}

async fn doctor(config_path: Option<&Path>) -> anyhow::Result<()> {
    let dgx = dgx_config(config_path)?;
    let client = http_client();
    println!("newt dgx doctor — probing configured endpoints\n");

    let mut any = false;
    for kind in EndpointKind::ALL {
        let label = kind.as_str();
        match dgx.resolve_endpoint_for(kind) {
            Ok(base) => {
                any = true;
                let path = if kind.is_openai_compatible() {
                    "/v1/models"
                } else {
                    "/api/tags"
                };
                let health = probe(&client, &base, path).await;
                println!("  {label:<10} {base}  —  {health}");
            }
            Err(_) => println!("  {label:<10} (not set)"),
        }
    }

    if !any {
        println!("\n  No DGX endpoints configured. Run `newt dgx setup` (Step 14.4) or set");
        println!("  NEWT_DGX_OLLAMA_URL=https://dgx-ollama.home.lab (or NEWT_DGX_HOST=<host>).");
    }
    println!("\n  DNS note: on the Google-WiFi mesh, .home.lab resolves but .home.lan does");
    println!("  not (the pucks intercept the .lan TLD). Use .home.lab from a laptop; inside");
    println!(
        "  k3s use the in_cluster proxy (http://ollama-proxy.inference.svc.cluster.local:11434)."
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Load the `[dgx]` sub-table, defaulting to an empty config so that
/// `NEWT_DGX_*` env overrides still work without a config file.
fn dgx_config(config_path: Option<&Path>) -> anyhow::Result<DgxConfig> {
    Ok(load_config(config_path)?.dgx.unwrap_or_default())
}

fn load_config(config_path: Option<&Path>) -> anyhow::Result<Config> {
    let config = match config_path {
        Some(p) => Config::load(p)?,
        None => Config::resolve()?,
    };
    Ok(config)
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .expect("build reqwest client")
}

/// GET `{base}/api/tags` → installed Ollama model names.
async fn fetch_ollama_models(client: &reqwest::Client, base: &str) -> anyhow::Result<Vec<String>> {
    fetch_names(client, base, "/api/tags").await
}

/// GET `{base}/api/ps` → running Ollama model names.
async fn fetch_ollama_running(client: &reqwest::Client, base: &str) -> anyhow::Result<Vec<String>> {
    fetch_names(client, base, "/api/ps").await
}

async fn fetch_names(
    client: &reqwest::Client,
    base: &str,
    path: &str,
) -> anyhow::Result<Vec<String>> {
    let url = format!("{}{path}", base.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("request failed: {e}"))?;
    if !resp.status().is_success() {
        anyhow::bail!("HTTP {}", resp.status());
    }
    let json: serde_json::Value = resp.json().await?;
    Ok(extract_names(&json["models"]))
}

/// Pull `name` fields out of a JSON array (`[{ "name": "..." }, ...]`).
fn extract_names(models: &serde_json::Value) -> Vec<String> {
    models
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Reachability probe: GET `{base}{path}`, summarized as a status string.
async fn probe(client: &reqwest::Client, base: &str, path: &str) -> String {
    let url = format!("{}{path}", base.trim_end_matches('/'));
    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => "OK".to_string(),
        Ok(resp) => format!("HTTP {}", resp.status()),
        Err(e) => format!("unreachable: {e}"),
    }
}

// ---------------------------------------------------------------------------
// warm
// ---------------------------------------------------------------------------

/// Ollama load-only request body: a `/api/generate` call with no `prompt`
/// and a `keep_alive` window loads the model into VRAM (and pins it resident)
/// without generating any tokens.
fn warm_body(model: &str, keep_alive: &str) -> serde_json::Value {
    serde_json::json!({ "model": model, "keep_alive": keep_alive, "stream": false })
}

/// POST the load-only request and return the load time in seconds when Ollama
/// actually had to load the model (absent / `None` when it was already
/// resident — a warm hit returns near-instantly with no `load_duration`).
async fn warm_model(
    client: &reqwest::Client,
    base: &str,
    model: &str,
    keep_alive: &str,
) -> anyhow::Result<Option<f64>> {
    let url = format!("{}/api/generate", base.trim_end_matches('/'));
    let resp = client
        .post(&url)
        .json(&warm_body(model, keep_alive))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("request failed: {e}"))?;
    if !resp.status().is_success() {
        anyhow::bail!("HTTP {}", resp.status());
    }
    let json: serde_json::Value = resp.json().await?;
    Ok(json["load_duration"].as_u64().map(|ns| ns as f64 / 1e9))
}

async fn warm(
    config_path: Option<&Path>,
    model: Option<String>,
    keep_alive: &str,
) -> anyhow::Result<()> {
    let dgx = dgx_config(config_path)?;
    let kind = dgx.active_endpoint;
    if kind.is_openai_compatible() {
        anyhow::bail!(
            "`newt dgx warm` targets Ollama endpoints; the active endpoint is vLLM \
             (vLLM keeps its served model resident already)"
        );
    }
    let base = dgx.resolve_endpoint()?;
    let model = match model {
        Some(m) => m,
        None => dgx.resolve_active_model()?,
    };
    println!("Warming {model} on {kind} endpoint ({base}) — keep_alive={keep_alive}");

    // Cold loads of large models can take tens of seconds; give warm its own
    // generous timeout rather than the short probe client.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .expect("build reqwest client");
    match warm_model(&client, &base, &model, keep_alive).await? {
        Some(secs) => println!("  loaded in {secs:.1}s — now resident"),
        None => println!("  ready (already resident)"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"models":[]})),
            )
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
}
