//! `newt dgx` — NVIDIA DGX endpoint management.
//!
//! - `setup`  — first-run DGX configuration wizard; writes `[dgx]` to config.
//! - `route`  — classify a task and recommend a (model, endpoint) formation.
//! - `status` — active-endpoint health + models currently loaded on the DGX.
//! - `models` — list models available on the active endpoint.
//! - `doctor` — probe every configured endpoint flavor + DNS guidance.
//! - `pull`   — pull an Ollama model, OR a HuggingFace GGUF (with automated
//!   sharded-GGUF workaround for ollama/ollama#5245) onto the node.
//! - `rm`     — delete a model from the active Ollama endpoint (`/api/delete`).
//! - `ps`     — list models currently loaded on the active endpoint (`/api/ps`).
//!
//! Later Phase 14 steps add `endpoint`/`formation`/`node`, SSH ops
//! (`run`/`push`/`watch`), and `nim`.
//!
//! ## `pull` and the sharded-GGUF workaround
//!
//! `ollama pull hf.co/<org>/<repo>:<quant>` fails on multi-part (sharded) GGUF
//! repos with `400: The specified tag is a sharded GGUF. Ollama does not
//! support this yet` (ollama/ollama#5245). For those, `pull` downloads each
//! shard onto the node with resumable `curl` and runs `ollama create` from a
//! generated `Modelfile`. Before any of that it runs a **fit pre-flight**
//! (the GLM-5.2 lesson): if the model's on-disk size exceeds the node's RAM it
//! refuses unless `--force`, because such a model is effectively unrunnable
//! (heavy disk paging). The pure logic lives in [`crate::dgx_pull`]; SSH/HF
//! execution is injectable so tests never touch the network or a real node.

use std::path::Path;

use std::io::Write as _;

use clap::Subcommand;
use newt_core::dgx::{DgxConfig, DgxNode, EndpointKind};
use newt_core::router::Classification;
use newt_core::{Config, Router, Tier};
use newt_inference::local::LocalVllmBackend;

/// `newt dgx <cmd>` subcommands.
#[derive(Subcommand, Debug)]
pub enum DgxCmd {
    /// First-run DGX setup: write a [dgx] block to ~/.newt/config.toml.
    ///
    /// With no arguments, prints setup instructions. With --host, synthesizes
    /// Ollama / vLLM endpoint URLs from the bare hostname or IP and writes the
    /// config (atomically via Config::save). Use --template to dump the
    /// home.lab reference template as TOML without writing anything.
    Setup {
        /// DGX hostname or IP (e.g. REDACTED-IP or REDACTED-HOST).
        #[arg(long)]
        host: Option<String>,

        /// Node name stored in config (default: "dgx").
        #[arg(long, default_value = "dgx")]
        name: String,

        /// Active model id (e.g. qwen2.5-coder:32b, llama3.1:8b).
        #[arg(long)]
        model: Option<String>,

        /// Print the home.lab reference template as TOML and exit without writing.
        #[arg(long)]
        template: bool,

        /// Skip the write-confirmation prompt (useful in scripts / tests).
        #[arg(long, short = 'y')]
        yes: bool,
    },
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
    /// Set the active DGX model and persist it to ~/.newt/config.toml.
    Use {
        /// Model id to activate (e.g. gemma4:e2b, qwen2.5-coder:32b).
        model: String,
    },
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
    /// Pull a model onto the DGX node.
    ///
    /// A plain Ollama name (`qwen2.5-coder:32b`) is pulled directly. A
    /// HuggingFace GGUF reference (`unsloth/Repo-GGUF:Q8_0` or
    /// `hf.co/unsloth/Repo-GGUF:Q8_0`) takes the smart path: it queries the HF
    /// API for the quant's GGUF files, runs a fit pre-flight against node RAM,
    /// and — for sharded (multi-part) GGUF — downloads each shard with `curl`
    /// and `ollama create`s from a generated `Modelfile` (the documented
    /// workaround for ollama/ollama#5245).
    Pull {
        /// Model to pull (Ollama name or HuggingFace `<org>/<repo>:<quant>`).
        model: String,
        /// SSH node name (defaults to the active node).
        #[arg(long)]
        node: Option<String>,
        /// Override the resulting Ollama model name (sharded path only;
        /// default is a sanitized `<repo>-<quant>`).
        #[arg(long)]
        name: Option<String>,
        /// Proceed even when the model is larger than node RAM (the fit
        /// pre-flight would otherwise refuse).
        #[arg(long)]
        force: bool,
        /// Print the resolved plan and the exact remote command/script without
        /// SSHing or executing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Delete a model from the active Ollama endpoint (`/api/delete`).
    Rm {
        /// Model id to delete (e.g. qwen2.5-coder:32b).
        model: String,
    },
    /// List models currently loaded on the active Ollama endpoint (`/api/ps`).
    Ps,
}

/// Dispatch a `newt dgx` subcommand.
pub async fn run(cmd: DgxCmd, config_path: Option<&Path>) -> anyhow::Result<()> {
    match cmd {
        DgxCmd::Setup {
            host,
            name,
            model,
            template,
            yes,
        } => setup(
            config_path,
            host.as_deref(),
            &name,
            model.as_deref(),
            template,
            yes,
        ),
        DgxCmd::Route { task } => route(&task, config_path),
        DgxCmd::Status => status(config_path).await,
        DgxCmd::Models => models(config_path).await,
        DgxCmd::Doctor => doctor(config_path).await,
        DgxCmd::Use { model } => use_model(config_path, &model),
        DgxCmd::Warm { model, keep_alive } => warm(config_path, model, &keep_alive).await,
        DgxCmd::Pull {
            model,
            node,
            name,
            force,
            dry_run,
        } => {
            pull(
                config_path,
                &model,
                node.as_deref(),
                name.as_deref(),
                force,
                dry_run,
            )
            .await
        }
        DgxCmd::Rm { model } => rm(config_path, &model).await,
        DgxCmd::Ps => ps(config_path).await,
    }
}

// ---------------------------------------------------------------------------
// setup
// ---------------------------------------------------------------------------

/// First-run DGX configuration.
///
/// Synthesizes Ollama / vLLM endpoint URLs from a bare `host` (e.g.
/// `REDACTED-IP` or `REDACTED-HOST`) and writes the resulting `[dgx]`
/// block into the resolved config file. Loads the existing config first so
/// non-DGX fields are preserved.
///
/// Confirmation is skipped when `yes = true` (non-interactive / test mode).
fn setup(
    config_path: Option<&Path>,
    host: Option<&str>,
    name: &str,
    model: Option<&str>,
    template: bool,
    yes: bool,
) -> anyhow::Result<()> {
    // --template: dump home_template as TOML then exit without writing.
    if template {
        let tmpl = DgxConfig::home_template();
        let text = toml::to_string_pretty(&tmpl)
            .map_err(|e| anyhow::anyhow!("TOML serialisation failed: {e}"))?;
        println!("# [dgx] home.lab reference template");
        println!("# Copy into ~/.newt/config.toml under [dgx]\n");
        print!("{text}");
        return Ok(());
    }

    // No --host: print guidance and exit without error.
    let Some(host) = host else {
        eprintln!("Usage: newt dgx setup --host <hostname-or-ip> [--model <model>] [--yes]");
        eprintln!("       newt dgx setup --template");
        eprintln!("\nExamples:");
        eprintln!("  newt dgx setup --host REDACTED-IP --model qwen2.5-coder:32b --yes");
        eprintln!("  newt dgx setup --host REDACTED-HOST --name home");
        return Ok(());
    };

    // Build the DGX node from the bare host. Ollama default port is 11434;
    // vLLM default port is 8000. The LB and in-cluster URLs require distinct
    // hostnames so they cannot be synthesised from a bare host.
    let node = DgxNode {
        name: name.to_string(),
        ollama: Some(format!("http://{host}:11434")),
        vllm: Some(format!("http://{host}:8000")),
        ssh_host: Some(host.to_string()),
        ..Default::default()
    };
    let dgx = DgxConfig {
        active_node: Some(name.to_string()),
        active_endpoint: EndpointKind::Ollama,
        active_model: model.map(str::to_string),
        nodes: vec![node],
        formations: vec![],
    };

    let text = toml::to_string_pretty(&dgx)
        .map_err(|e| anyhow::anyhow!("TOML serialisation failed: {e}"))?;

    let save_path = config_path
        .map(std::path::PathBuf::from)
        .or_else(newt_core::Config::user_config_path)
        .ok_or_else(|| anyhow::anyhow!("cannot determine config file path (HOME unset?)"))?;

    println!("# [dgx] config to write to {}:\n", save_path.display());
    print!("{text}");

    if !yes {
        print!("\nWrite? [y/N] ");
        std::io::stdout().flush()?;
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !matches!(input.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("Aborted.");
            return Ok(());
        }
    }

    // Preserve any non-DGX fields already in the config. If the target file
    // doesn't exist yet (first run), start from defaults rather than erroring.
    let mut config = match config_path {
        Some(p) if p.exists() => Config::load(p).map_err(anyhow::Error::from)?,
        Some(_) => Config::default(),
        None => Config::resolve().map_err(anyhow::Error::from)?,
    };
    config.dgx = Some(dgx);
    config.save(&save_path)?;
    println!("Saved → {}", save_path.display());
    Ok(())
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
        None => println!("  Formation:   (none configured — set NEWT_DGX_HOST=<host>)"),
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
    println!("  GPU mem:  (SSH to the host and run: nvidia-smi)");
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
        println!("\n  No DGX endpoints configured. Set:");
        println!("    NEWT_DGX_HOST=<host>          (synthesizes ollama + vllm URLs from a bare hostname)");
        println!(
            "    NEWT_DGX_OLLAMA_URL=<url>     (direct URL, e.g. https://REDACTED-HOST)"
        );
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
// use — persist active model to config
// ---------------------------------------------------------------------------

fn use_model(config_path: Option<&Path>, model: &str) -> anyhow::Result<()> {
    let mut config = load_config(config_path)?;

    // Update or create the [dgx] section with the chosen model.
    let dgx = config.dgx.get_or_insert_with(Default::default);
    dgx.active_model = Some(model.to_string());

    let save_path = config_path
        .map(std::path::PathBuf::from)
        .or_else(newt_core::Config::user_config_path)
        .ok_or_else(|| anyhow::anyhow!("cannot determine config file path"))?;

    config.save(&save_path)?;
    println!("Active model set to {model}");
    println!("Saved → {}", save_path.display());
    Ok(())
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

// ---------------------------------------------------------------------------
// pull / rm / ps
// ---------------------------------------------------------------------------

use crate::dgx_pull::{self, fit_check, FitVerdict, GgufFile, ModelRef, PullPlan};

/// How the pull touches the node: real SSH, or a captured dry-run.
///
/// The trait lets tests substitute a recording fake so no real SSH ever runs.
trait SshExec {
    /// Run `command` on `user@host` (optional `port`), streaming output to the
    /// child's inherited stderr. Returns Ok on a zero exit.
    fn run(&self, user: &str, host: &str, port: Option<u16>, command: &str) -> anyhow::Result<()>;
}

/// Default executor: spawns the real `ssh` binary.
struct RealSsh;

impl SshExec for RealSsh {
    fn run(&self, user: &str, host: &str, port: Option<u16>, command: &str) -> anyhow::Result<()> {
        let argv = dgx_pull::ssh_argv(user, host, port, command);
        let (prog, rest) = argv.split_first().expect("ssh argv non-empty");
        let status = std::process::Command::new(prog)
            .args(rest)
            .status()
            .map_err(|e| anyhow::anyhow!("failed to spawn ssh: {e}"))?;
        if !status.success() {
            anyhow::bail!("ssh command failed: {status}");
        }
        Ok(())
    }
}

/// Detect total node RAM in bytes via `free -b | awk '/Mem:/{print $2}'`.
/// Best-effort: returns `None` (rather than erroring) if SSH or parsing fails.
fn detect_node_mem(user: &str, host: &str, port: Option<u16>) -> Option<u64> {
    let argv = dgx_pull::ssh_argv(user, host, port, "free -b | awk '/Mem:/{print $2}'");
    let (prog, rest) = argv.split_first()?;
    let out = std::process::Command::new(prog).args(rest).output().ok()?;
    if !out.status.success() {
        return None;
    }
    dgx_pull::parse_free_bytes(&String::from_utf8_lossy(&out.stdout))
}

/// GET `<hf_base>/api/models/<org>/<repo>?blobs=true` → the `.gguf` siblings.
async fn fetch_hf_siblings(
    client: &reqwest::Client,
    hf_base: &str,
    org: &str,
    repo: &str,
) -> anyhow::Result<Vec<GgufFile>> {
    let url = format!(
        "{}/api/models/{org}/{repo}?blobs=true",
        hf_base.trim_end_matches('/')
    );
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("HF API request failed: {e}"))?;
    if !resp.status().is_success() {
        anyhow::bail!("HF API returned HTTP {}", resp.status());
    }
    let json: serde_json::Value = resp.json().await?;
    Ok(dgx_pull::parse_gguf_siblings(&json))
}

/// Top-level `pull` handler: resolve config, dispatch on the arg shape.
async fn pull(
    config_path: Option<&Path>,
    model: &str,
    node: Option<&str>,
    name: Option<&str>,
    force: bool,
    dry_run: bool,
) -> anyhow::Result<()> {
    let mut dgx = dgx_config(config_path)?;
    if let Some(node) = node {
        dgx.active_node = Some(node.to_string());
    }
    let user = dgx.ssh_user();
    let host = dgx.ssh_host()?;
    let has_token = std::env::var("HF_TOKEN").is_ok_and(|t| !t.trim().is_empty());

    match ModelRef::parse(model) {
        ModelRef::Ollama { name: tag } => {
            // A plain Ollama name needs no HF metadata or fit pre-flight.
            execute_hf_plan(
                &RealSsh,
                &user,
                &host,
                &PullPlan::OllamaNative { tag },
                has_token,
                dry_run,
            )
        }
        ModelRef::Hf { org, repo, quant } => {
            let client = http_client_long();
            let all = fetch_hf_siblings(&client, &hf_api_base(), &org, &repo).await?;
            let matched: Vec<GgufFile> = all
                .into_iter()
                .filter(|f| dgx_pull::file_matches_quant(&f.path, &quant))
                .collect();
            let plan = dgx_pull::plan_hf(&org, &repo, &quant, &matched, name)
                .map_err(|e| anyhow::anyhow!(e))?;

            // Fit pre-flight (the GLM-5.2 lesson).
            let model_bytes = dgx_pull::total_bytes(&matched);
            let mem = if dry_run {
                None
            } else {
                detect_node_mem(&user, &host, None)
            };
            report_fit(fit_check(model_bytes, mem), force)?;

            execute_hf_plan(&RealSsh, &user, &host, &plan, has_token, dry_run)
        }
    }
}

/// HF API base URL (overridable via `NEWT_HF_API_BASE` for tests).
fn hf_api_base() -> String {
    std::env::var("NEWT_HF_API_BASE").unwrap_or_else(|_| "https://huggingface.co".to_string())
}

/// A reqwest client with a generous timeout (HF downloads metadata can be slow).
fn http_client_long() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .expect("build reqwest client")
}

/// Print the fit verdict and either proceed or refuse (unless `--force`).
fn report_fit(verdict: FitVerdict, force: bool) -> anyhow::Result<()> {
    let refuse = verdict.should_refuse();
    match &verdict {
        FitVerdict::Fits {
            model_bytes,
            mem_bytes,
        } => println!(
            "  Fit: model {:.1} GB fits in node memory {:.1} GB",
            dgx_pull::bytes_to_gib(*model_bytes),
            dgx_pull::bytes_to_gib(*mem_bytes)
        ),
        FitVerdict::Undetectable { model_bytes } => eprintln!(
            "  WARNING: could not detect node memory; model is {:.1} GB. Proceeding best-effort.",
            dgx_pull::bytes_to_gib(*model_bytes)
        ),
        FitVerdict::Exceeds {
            model_bytes,
            mem_bytes,
        } => eprintln!(
            "  WARNING: model {:.1} GB exceeds node memory {:.1} GB — \
             will be unrunnable / heavy disk paging.",
            dgx_pull::bytes_to_gib(*model_bytes),
            dgx_pull::bytes_to_gib(*mem_bytes)
        ),
    }
    if refuse && !force {
        anyhow::bail!("refusing to pull a model larger than node memory; pass --force to override");
    }
    if refuse {
        eprintln!("  --force given: proceeding anyway.");
    }
    Ok(())
}

/// Execute (or dry-run) an HF [`PullPlan`].
fn execute_hf_plan(
    ssh: &dyn SshExec,
    user: &str,
    host: &str,
    plan: &PullPlan,
    has_token: bool,
    dry_run: bool,
) -> anyhow::Result<()> {
    match plan {
        PullPlan::OllamaNative { tag } => {
            let command = dgx_pull::ollama_native_remote_command(tag);
            run_or_dryrun(
                ssh,
                user,
                host,
                None,
                &command,
                dry_run,
                &format!("PullPlan: OllamaNative {{ tag: {tag:?} }}"),
            )
        }
        PullPlan::SingleFileHf { org, repo, quant } => {
            let command = dgx_pull::single_file_remote_command(org, repo, quant);
            run_or_dryrun(
                ssh,
                user,
                host,
                None,
                &command,
                dry_run,
                "PullPlan: SingleFileHf (ollama pull hf.co/...)",
            )
        }
        PullPlan::ShardedHf {
            org,
            repo,
            quant,
            parts,
            modelfile,
            name,
        } => {
            let script =
                dgx_pull::sharded_remote_script(org, repo, parts, modelfile, name, has_token);
            let summary = format!(
                "PullPlan: ShardedHf {{ quant: {quant:?}, parts: {}, name: {name:?} }}",
                parts.len()
            );
            run_or_dryrun(ssh, user, host, None, &script, dry_run, &summary)
        }
    }
}

/// Either print the plan + exact remote command (dry-run) or SSH-execute it.
fn run_or_dryrun(
    ssh: &dyn SshExec,
    user: &str,
    host: &str,
    port: Option<u16>,
    command: &str,
    dry_run: bool,
    summary: &str,
) -> anyhow::Result<()> {
    if dry_run {
        println!("{summary}");
        let argv = dgx_pull::ssh_argv(user, host, port, command);
        println!("Would run: {}", argv.join(" "));
        println!("--- remote command ---");
        println!("{command}");
        return Ok(());
    }
    println!("{summary}");
    eprintln!("→ ssh {user}@{host}: executing pull (output below)");
    ssh.run(user, host, port, command)
}

/// `rm <model>` — DELETE `/api/delete` on the active Ollama endpoint.
async fn rm(config_path: Option<&Path>, model: &str) -> anyhow::Result<()> {
    let dgx = dgx_config(config_path)?;
    if dgx.active_endpoint.is_openai_compatible() {
        anyhow::bail!("`newt dgx rm` targets Ollama endpoints; the active endpoint is vLLM");
    }
    let base = dgx.resolve_endpoint()?;
    delete_ollama_model(&http_client(), &base, model).await?;
    println!("Deleted {model}");
    Ok(())
}

/// DELETE `{base}/api/delete` with a `{ "model": <name> }` body.
async fn delete_ollama_model(
    client: &reqwest::Client,
    base: &str,
    model: &str,
) -> anyhow::Result<()> {
    let url = format!("{}/api/delete", base.trim_end_matches('/'));
    let resp = client
        .delete(&url)
        .json(&serde_json::json!({ "model": model }))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("request failed: {e}"))?;
    if !resp.status().is_success() {
        anyhow::bail!("HTTP {}", resp.status());
    }
    Ok(())
}

/// `ps` — GET `/api/ps`, print each loaded model with its size.
async fn ps(config_path: Option<&Path>) -> anyhow::Result<()> {
    let dgx = dgx_config(config_path)?;
    if dgx.active_endpoint.is_openai_compatible() {
        anyhow::bail!("`newt dgx ps` targets Ollama endpoints; the active endpoint is vLLM");
    }
    let base = dgx.resolve_endpoint()?;
    let loaded = fetch_ollama_ps(&http_client(), &base).await?;
    println!("Loaded models on {} ({base}):", dgx.active_endpoint);
    if loaded.is_empty() {
        println!("  (none)");
    }
    for (name, size) in &loaded {
        match size {
            Some(bytes) => println!("  {name}  ({:.1} GB)", dgx_pull::bytes_to_gib(*bytes)),
            None => println!("  {name}"),
        }
    }
    Ok(())
}

/// GET `{base}/api/ps` → `(name, size_bytes)` for each loaded model.
async fn fetch_ollama_ps(
    client: &reqwest::Client,
    base: &str,
) -> anyhow::Result<Vec<(String, Option<u64>)>> {
    let url = format!("{}/api/ps", base.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("request failed: {e}"))?;
    if !resp.status().is_success() {
        anyhow::bail!("HTTP {}", resp.status());
    }
    let json: serde_json::Value = resp.json().await?;
    Ok(extract_ps(&json["models"]))
}

/// Pull `(name, size)` pairs out of an `/api/ps` `models` array.
fn extract_ps(models: &serde_json::Value) -> Vec<(String, Option<u64>)> {
    models
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    let name = m["name"].as_str()?.to_string();
                    let size = m["size"].as_u64();
                    Some((name, size))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path as wm_path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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
        fn run(
            &self,
            user: &str,
            host: &str,
            port: Option<u16>,
            command: &str,
        ) -> anyhow::Result<()> {
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

        setup(Some(&cfg_path), Some("REDACTED-IP"), "lab", None, false, true).unwrap();

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

    #[test]
    fn extract_ps_reads_names_and_sizes() {
        let v = serde_json::json!([
            {"name": "a", "size": 1024u64},
            {"x": 1},
            {"name": "b"}
        ]);
        let out = extract_ps(&v);
        assert_eq!(out, vec![("a".into(), Some(1024)), ("b".into(), None)]);
        assert!(extract_ps(&serde_json::json!(null)).is_empty());
    }

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
}
