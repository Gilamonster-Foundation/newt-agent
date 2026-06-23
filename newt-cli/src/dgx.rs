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
use newt_core::retry::{with_backoff, RetryPolicy};
use newt_core::router::Classification;
use newt_core::{Config, Router, Tier};
use newt_inference::local::LocalVllmBackend;

use crate::dgx_vllm;

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
        /// DGX hostname or IP (e.g. 192.168.86.40 or dgx.home.lab).
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
    /// Manage a vLLM OpenAI-compatible server on the DGX node.
    ///
    /// Stands up (`up`) / tears down (`down`) a `vllm serve` process over SSH,
    /// reports the served models (`ps`), prints the resolved launch plan
    /// (`config`), or tails the server log (`logs`). Pure planning lives in
    /// [`crate::dgx_vllm`]; SSH/HTTP execution is injectable so tests never
    /// touch the network or a real node.
    Vllm {
        #[command(subcommand)]
        cmd: VllmCmd,
    },
    /// Cross-engine GPU residency snapshot: what Ollama and vLLM each hold on
    /// the node right now, plus available memory.
    ///
    /// On the unified-memory GB10 both engines draw from one ~117 GiB pool and
    /// neither negotiates, so this is the operator's window into contention.
    /// Live-queried (Ollama `/api/ps` + vLLM `/v1/models` + `MemAvailable`) —
    /// ground truth, not a cached lease file.
    Gpu,
}

/// Planning knobs shared by `dgx vllm up` and `dgx vllm config`.
#[derive(clap::Args, Debug)]
pub struct VllmPlanArgs {
    /// Override the OpenAI `served-model-name` (default: sanitized model name).
    #[arg(long)]
    pub served_name: Option<String>,
    /// Quant/dtype (auto|nvfp4|fp8|bf16|awq|gptq); inferred from the model name
    /// when omitted.
    #[arg(long)]
    pub dtype: Option<String>,
    /// `--tensor-parallel-size` (number of GPUs). Default 1; >1 is meaningless
    /// on a single unified-memory GB10.
    #[arg(long, default_value_t = 1)]
    pub tensor_parallel: u8,
    /// Cap the context window; otherwise the planner default is used.
    #[arg(long)]
    pub max_model_len: Option<u32>,
    /// `--gpu-memory-utilization` fraction (0.0..=1.0). Default 0.90.
    #[arg(long, default_value_t = 0.90)]
    pub gpu_mem_util: f64,
    /// Listen port on the node. Default 8000.
    #[arg(long, default_value_t = 8000)]
    pub port: u16,
    /// Render the `docker run vllm/vllm-openai` argv instead of native
    /// `vllm serve` (native is the only launcher executed in this step).
    #[arg(long)]
    pub docker: bool,
    /// Extra args appended verbatim to the vLLM command line.
    #[arg(long)]
    pub extra: Vec<String>,
}

/// `newt dgx vllm <cmd>` subcommands.
#[derive(Subcommand, Debug)]
pub enum VllmCmd {
    /// Launch a vLLM server on the node for `model` (HF id or local path).
    ///
    /// Runs a fit pre-flight against the node's *available* memory, renders the
    /// `vllm serve` argv, launches it detached with `nohup`, polls
    /// `/v1/models` until ready, then persists the endpoint + active model.
    Up {
        /// Model to serve (HuggingFace `<org>/<repo>` or an on-node path).
        model: String,
        /// SSH node name (defaults to the active node).
        #[arg(long)]
        node: Option<String>,
        #[command(flatten)]
        plan: VllmPlanArgs,
        /// Proceed even when the model exceeds the memory budget.
        #[arg(long)]
        force: bool,
        /// Before launching, unload any models resident on the active Ollama
        /// endpoint to free the shared unified-memory pool (the eval-loop swap).
        #[arg(long)]
        evict_ollama: bool,
        /// Print the resolved plan + remote script without SSHing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Stop the vLLM server for `served_name` (default: the active model).
    Down {
        /// Served model name whose pidfile to kill (default: active model).
        served_name: Option<String>,
        /// SSH node name (defaults to the active node).
        #[arg(long)]
        node: Option<String>,
        /// Print the kill command without SSHing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Probe the configured vLLM endpoint and list its models (`/v1/models`).
    Ps,
    /// Print the resolved launch plan (argv) for `model`. Pure: no SSH/network.
    Config {
        /// Model to plan for.
        model: String,
        #[command(flatten)]
        plan: VllmPlanArgs,
    },
    /// Tail the vLLM server log on the node (`tail -f` over SSH).
    Logs {
        /// Served model name whose log to tail (default: active model).
        served_name: Option<String>,
        /// SSH node name (defaults to the active node).
        #[arg(long)]
        node: Option<String>,
        /// Number of trailing lines before following. Default 50.
        #[arg(long, default_value_t = 50)]
        lines: u32,
        /// Print the remote command without SSHing.
        #[arg(long)]
        dry_run: bool,
    },
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
        DgxCmd::Vllm { cmd } => match cmd {
            VllmCmd::Up {
                model,
                node,
                plan,
                force,
                evict_ollama,
                dry_run,
            } => {
                vllm_up(
                    config_path,
                    &model,
                    node.as_deref(),
                    &plan,
                    force,
                    evict_ollama,
                    dry_run,
                )
                .await
            }
            VllmCmd::Down {
                served_name,
                node,
                dry_run,
            } => {
                vllm_down(
                    config_path,
                    served_name.as_deref(),
                    node.as_deref(),
                    dry_run,
                )
                .await
            }
            VllmCmd::Ps => vllm_ps(config_path).await,
            VllmCmd::Config { model, plan } => vllm_config(&model, &plan),
            VllmCmd::Logs {
                served_name,
                node,
                lines,
                dry_run,
            } => {
                vllm_logs(
                    config_path,
                    served_name.as_deref(),
                    node.as_deref(),
                    lines,
                    dry_run,
                )
                .await
            }
        },
        DgxCmd::Gpu => gpu(config_path).await,
    }
}

// ---------------------------------------------------------------------------
// setup
// ---------------------------------------------------------------------------

/// First-run DGX configuration.
///
/// Synthesizes Ollama / vLLM endpoint URLs from a bare `host` (e.g.
/// `192.168.86.40` or `dgx.home.lab`) and writes the resulting `[dgx]`
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
        eprintln!("  newt dgx setup --host 192.168.86.40 --model qwen2.5-coder:32b --yes");
        eprintln!("  newt dgx setup --host dgx.home.lab --name home");
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
            "    NEWT_DGX_OLLAMA_URL=<url>     (direct URL, e.g. https://dgx-ollama.home.lab)"
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

/// `free -b` awk column for total RAM (`MemTotal`).
const MEM_TOTAL_AWK: &str = "$2";
/// `free -b` awk column for available RAM (`MemAvailable`).
const MEM_AVAILABLE_AWK: &str = "$7";

/// The remote command that prints one memory figure from `free -b` (pure).
fn node_mem_probe(awk_field: &str) -> String {
    format!("free -b | awk '/Mem:/{{print {awk_field}}}'")
}

/// Detect node RAM (bytes) via `free -b`, selecting the awk column. Best-effort:
/// `None` if SSH or parsing fails.
fn detect_node_mem_col(user: &str, host: &str, port: Option<u16>, awk_field: &str) -> Option<u64> {
    let argv = dgx_pull::ssh_argv(user, host, port, &node_mem_probe(awk_field));
    let (prog, rest) = argv.split_first()?;
    let out = std::process::Command::new(prog).args(rest).output().ok()?;
    if !out.status.success() {
        return None;
    }
    dgx_pull::parse_free_bytes(&String::from_utf8_lossy(&out.stdout))
}

/// Detect total node RAM (`MemTotal`). Used by the Ollama `pull` fit pre-flight,
/// whose semantics deliberately stay unchanged in this step.
fn detect_node_mem(user: &str, host: &str, port: Option<u16>) -> Option<u64> {
    detect_node_mem_col(user, host, port, MEM_TOTAL_AWK)
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

// ---------------------------------------------------------------------------
// vllm — stand up / tear down a vLLM server on the node (Step 14.11)
// ---------------------------------------------------------------------------

/// Available node RAM (`MemAvailable`, column 7). vLLM's fit budget must net out
/// memory the *other* engine (Ollama) already holds resident — see
/// [`crate::dgx_vllm::vllm_fit_check`] — so it sizes against available, not
/// total RAM.
fn detect_node_mem_available(user: &str, host: &str, port: Option<u16>) -> Option<u64> {
    detect_node_mem_col(user, host, port, MEM_AVAILABLE_AWK)
}

/// GET `<hf_base>/api/models/<org>/<repo>?blobs=true` → raw JSON (for sizing
/// safetensors weights; same endpoint as the GGUF sibling fetch).
async fn fetch_hf_model_json(
    client: &reqwest::Client,
    hf_base: &str,
    org: &str,
    repo: &str,
) -> anyhow::Result<serde_json::Value> {
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
    Ok(resp.json().await?)
}

/// Best-effort vLLM weight size: sum the repo's weight files (`.safetensors` /
/// `.bin`) when `model` is an HF id; `None` for a local path, a fetch/parse
/// failure, or a repo with no sized weights.
async fn fetch_vllm_weight_bytes(model: &str) -> Option<u64> {
    let (org, repo) = dgx_vllm::hf_repo_parts(model)?;
    let client = http_client_long();
    let json = fetch_hf_model_json(&client, &hf_api_base(), &org, &repo)
        .await
        .ok()?;
    let bytes = dgx_vllm::sum_weight_bytes(&json);
    (bytes > 0).then_some(bytes)
}

/// Docker execution isn't wired yet (the remote script wraps native
/// `vllm serve` only), so `up --docker` must refuse clearly rather than silently
/// launch native. `dgx vllm config --docker` still previews the docker argv.
fn ensure_executable_runtime(runtime: dgx_vllm::VllmRuntime) -> anyhow::Result<()> {
    if matches!(runtime, dgx_vllm::VllmRuntime::Docker) {
        anyhow::bail!(
            "--docker is preview-only here: `dgx vllm config --docker` prints the docker \
             argv, but native `vllm serve` is the only launcher `up` executes in this step. \
             Omit --docker to launch (docker execution is a follow-up)."
        );
    }
    Ok(())
}

/// Build a resolved [`dgx_vllm::VllmPlan`] from CLI args (shared by `up`/`config`).
fn build_plan_from_args(model: &str, a: &VllmPlanArgs) -> anyhow::Result<dgx_vllm::VllmPlan> {
    let dtype = match a.dtype.as_deref() {
        Some(s) => Some(dgx_vllm::Dtype::parse(s).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown --dtype {s:?}; expected one of: auto, nvfp4, fp8, bf16, awq, gptq"
            )
        })?),
        None => None,
    };
    let runtime = if a.docker {
        dgx_vllm::VllmRuntime::Docker
    } else {
        dgx_vllm::VllmRuntime::Native
    };
    Ok(dgx_vllm::resolve_plan(dgx_vllm::PlanInputs {
        model,
        served_name: a.served_name.as_deref(),
        dtype,
        tensor_parallel: a.tensor_parallel,
        max_model_len: a.max_model_len,
        gpu_mem_util: a.gpu_mem_util,
        port: a.port,
        runtime,
        extra: a.extra.clone(),
    }))
}

/// Execute (or dry-run) a vLLM launch over SSH. Injection seam: `ssh` is
/// `&RealSsh` in prod, `&RecordingSsh` in tests.
fn execute_vllm_plan(
    ssh: &dyn SshExec,
    user: &str,
    host: &str,
    plan: &dgx_vllm::VllmPlan,
    dry_run: bool,
) -> anyhow::Result<()> {
    let script = dgx_vllm::vllm_remote_script(plan);
    let summary = format!(
        "VllmPlan: serve {:?} as {:?} ({:?}, tp={}, max_len={}, util={:.2}, port={})",
        plan.model,
        plan.served_name,
        plan.dtype,
        plan.tensor_parallel,
        plan.max_model_len,
        plan.gpu_mem_util(),
        plan.port,
    );
    run_or_dryrun(ssh, user, host, None, &script, dry_run, &summary)
}

/// Poll the freshly-launched server's `/v1/models` with bounded backoff. Cold
/// model loads can take minutes, hence the generous `for_local_inference`
/// policy in production; tests inject `RetryPolicy::immediate`.
async fn poll_vllm_ready(endpoint: &str, policy: &RetryPolicy) -> anyhow::Result<()> {
    let backend = LocalVllmBackend::new(endpoint, "");
    with_backoff(policy, || async { backend.list_models().await.map(|_| ()) })
        .await
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("vLLM did not become ready at {endpoint}: {e}"))
}

/// Pure in-memory mutation for persist-on-`up`: point the active endpoint at the
/// freshly-launched vLLM server and write its URL onto the target node. No IO —
/// the disk write lives in [`persist_vllm_endpoint`].
///
/// Returns `true` when the URL was written onto a matching node. `false` means
/// no node matched (the target name isn't in `nodes[]`) — the active endpoint is
/// still flipped to vLLM, but the URL wasn't recorded, so the caller warns
/// rather than leaving the user with a silently incomplete config.
#[must_use]
fn apply_vllm_persist(
    config: &mut Config,
    node_name: Option<&str>,
    endpoint_url: &str,
    served_name: &str,
) -> bool {
    let dgx = config.dgx.get_or_insert_with(Default::default);
    dgx.active_endpoint = EndpointKind::Vllm;
    dgx.active_model = Some(served_name.to_string());
    let target = node_name
        .map(str::to_string)
        .or_else(|| dgx.active_node.clone());
    if let Some(name) = target {
        if let Some(node) = dgx.nodes.iter_mut().find(|n| n.name == name) {
            node.vllm = Some(endpoint_url.to_string());
            return true;
        }
    }
    false
}

/// Persist the vLLM endpoint to config (load → [`apply_vllm_persist`] → save).
fn persist_vllm_endpoint(
    config_path: Option<&Path>,
    node_name: Option<&str>,
    endpoint_url: &str,
    served_name: &str,
) -> anyhow::Result<()> {
    let mut config = load_config(config_path)?;
    let node_recorded = apply_vllm_persist(&mut config, node_name, endpoint_url, served_name);
    let save_path = config_path
        .map(std::path::PathBuf::from)
        .or_else(newt_core::Config::user_config_path)
        .ok_or_else(|| anyhow::anyhow!("cannot determine config file path"))?;
    config.save(&save_path)?;
    println!("vLLM endpoint {endpoint_url} active; model {served_name}");
    println!("Saved → {}", save_path.display());
    if !node_recorded {
        eprintln!(
            "  WARNING: active endpoint set to vLLM, but no matching node was found to \
             record {endpoint_url} on — configure the node (`dgx setup` / `dgx node`) so \
             the URL resolves without env fallback."
        );
    }
    Ok(())
}

/// The served name to act on: an explicit arg, else the active model.
fn resolve_served_name(dgx: &DgxConfig, served_name: Option<&str>) -> anyhow::Result<String> {
    match served_name {
        Some(s) => Ok(s.to_string()),
        None => Ok(dgx.resolve_active_model()?),
    }
}

/// `dgx vllm up <model>` — fit pre-flight, launch, wait for readiness, persist.
async fn vllm_up(
    config_path: Option<&Path>,
    model: &str,
    node: Option<&str>,
    plan_args: &VllmPlanArgs,
    force: bool,
    evict_ollama: bool,
    dry_run: bool,
) -> anyhow::Result<()> {
    let mut dgx = dgx_config(config_path)?;
    if let Some(n) = node {
        dgx.active_node = Some(n.to_string());
    }
    let user = dgx.ssh_user();
    let host = dgx.ssh_host()?;

    let plan = build_plan_from_args(model, plan_args)?;
    ensure_executable_runtime(plan.runtime)?;

    // Fit pre-flight (the GLM-5.2 lesson, ported). Skipped on dry-run — no
    // network: dry-run only shows the plan + remote script.
    if !dry_run {
        // The eval-loop swap: free the shared pool first so the fit probe below
        // sees the reclaimed memory.
        if evict_ollama {
            evict_ollama_models(&dgx).await?;
        }
        let mem = detect_node_mem_available(&user, &host, None);
        match fetch_vllm_weight_bytes(model).await {
            Some(weight) => report_fit(
                dgx_vllm::vllm_fit_check(weight, mem, plan_args.gpu_mem_util),
                force,
            )?,
            None => eprintln!(
                "  WARNING: could not size weights for {model:?} (local path, HF lookup \
                 failed, or no sized .safetensors/.bin) — skipping the fit pre-flight; the \
                 model may exceed node memory."
            ),
        }
    }

    execute_vllm_plan(&RealSsh, &user, &host, &plan, dry_run)?;

    if !dry_run {
        let endpoint = format!("http://{host}:{}", plan.port);
        println!("Waiting for vLLM at {endpoint}/v1/models (cold load can take minutes) …");
        poll_vllm_ready(&endpoint, &RetryPolicy::for_local_inference()).await?;
        persist_vllm_endpoint(config_path, node, &endpoint, &plan.served_name)?;
    }
    Ok(())
}

/// `dgx vllm down [served_name]` — kill the recorded server PID on the node.
async fn vllm_down(
    config_path: Option<&Path>,
    served_name: Option<&str>,
    node: Option<&str>,
    dry_run: bool,
) -> anyhow::Result<()> {
    let mut dgx = dgx_config(config_path)?;
    if let Some(n) = node {
        dgx.active_node = Some(n.to_string());
    }
    let user = dgx.ssh_user();
    let host = dgx.ssh_host()?;
    let served = resolve_served_name(&dgx, served_name)?;
    let command = dgx_vllm::vllm_stop_command(&served);
    run_or_dryrun(
        &RealSsh,
        &user,
        &host,
        None,
        &command,
        dry_run,
        &format!("vLLM down: {served:?}"),
    )
}

/// `dgx vllm logs [served_name]` — tail and follow the server log on the node.
async fn vllm_logs(
    config_path: Option<&Path>,
    served_name: Option<&str>,
    node: Option<&str>,
    lines: u32,
    dry_run: bool,
) -> anyhow::Result<()> {
    let mut dgx = dgx_config(config_path)?;
    if let Some(n) = node {
        dgx.active_node = Some(n.to_string());
    }
    let user = dgx.ssh_user();
    let host = dgx.ssh_host()?;
    let served = resolve_served_name(&dgx, served_name)?;
    let command = dgx_vllm::vllm_logs_command(&served, lines);
    run_or_dryrun(
        &RealSsh,
        &user,
        &host,
        None,
        &command,
        dry_run,
        &format!("vLLM logs: {served:?} (tail -f)"),
    )
}

/// `dgx vllm config <model>` — print the resolved launch argv. Pure (no SSH).
fn vllm_config(model: &str, plan_args: &VllmPlanArgs) -> anyhow::Result<()> {
    let plan = build_plan_from_args(model, plan_args)?;
    let argv = if matches!(plan.runtime, dgx_vllm::VllmRuntime::Docker) {
        dgx_vllm::vllm_docker_argv(&plan)
    } else {
        dgx_vllm::render_vllm_argv(&plan)
    };
    println!("Resolved vLLM launch plan for {model:?}:");
    println!("  served-model-name: {}", plan.served_name);
    println!(
        "  dtype={:?}  tensor-parallel={}  max-model-len={}  gpu-mem-util={:.2}  port={}",
        plan.dtype,
        plan.tensor_parallel,
        plan.max_model_len,
        plan.gpu_mem_util(),
        plan.port,
    );
    println!("  argv: {}", argv.join(" "));
    Ok(())
}

/// `dgx vllm ps` — GET the configured vLLM endpoint's `/v1/models`.
async fn vllm_ps(config_path: Option<&Path>) -> anyhow::Result<()> {
    let dgx = dgx_config(config_path)?;
    let base = dgx.resolve_endpoint_for(EndpointKind::Vllm)?;
    let models = fetch_vllm_models(&base).await?;
    println!("vLLM models on {base}:");
    if models.is_empty() {
        println!("  (none)");
    }
    for m in &models {
        println!("  {m}");
    }
    Ok(())
}

/// GET `<base>/v1/models` → served model ids (OpenAI-compatible). `base` is the
/// injection seam: tests pass a `wiremock` `MockServer::uri()`.
async fn fetch_vllm_models(base: &str) -> anyhow::Result<Vec<String>> {
    let backend = LocalVllmBackend::new(base, "");
    Ok(backend
        .list_models()
        .await?
        .into_iter()
        .map(|m| m.id)
        .collect())
}

// ---------------------------------------------------------------------------
// gpu — cross-engine residency view + eviction (Step 14.12)
// ---------------------------------------------------------------------------

/// A ground-truth snapshot of what each engine holds on the node, plus the
/// headroom. Assembled from live probes (`/api/ps`, `/v1/models`,
/// `MemAvailable`) rather than a cached lease file.
struct Residency {
    /// Ollama resident models: `(name, size_bytes)` from `/api/ps`.
    ollama: Vec<(String, Option<u64>)>,
    /// vLLM served model ids from `/v1/models`.
    vllm: Vec<String>,
    /// `MemAvailable` in bytes, or `None` when the probe failed.
    mem_available: Option<u64>,
}

impl Residency {
    /// Both engines hold something → they're sharing the one unified pool.
    fn is_contended(&self) -> bool {
        !self.ollama.is_empty() && !self.vllm.is_empty()
    }
}

/// The Ollama models to unload to free the GPU (every currently-resident one).
fn ollama_evict_targets(res: &Residency) -> Vec<String> {
    res.ollama.iter().map(|(name, _)| name.clone()).collect()
}

/// Ollama unload request body: `keep_alive: 0` unloads the model immediately
/// (the inverse of `warm`'s positive keep-alive). Pure.
fn ollama_unload_body(model: &str) -> serde_json::Value {
    serde_json::json!({ "model": model, "keep_alive": 0 })
}

/// Render the residency snapshot for `dgx gpu`. Pure → unit-tested directly.
fn render_residency(res: &Residency) -> String {
    let mut s = String::new();
    match res.mem_available {
        // bytes_to_gib divides by 1024^3 (gibibytes); the codebase labels these
        // "GB" throughout (fit verdict, `ps`), so match that convention here.
        Some(b) => s.push_str(&format!(
            "  MemAvailable: {:.1} GB\n",
            dgx_pull::bytes_to_gib(b)
        )),
        None => s.push_str("  MemAvailable: (undetected)\n"),
    }
    s.push_str("  Ollama resident:\n");
    if res.ollama.is_empty() {
        s.push_str("    (none)\n");
    }
    for (name, size) in &res.ollama {
        match size {
            Some(b) => s.push_str(&format!(
                "    {name}  ({:.1} GB)\n",
                dgx_pull::bytes_to_gib(*b)
            )),
            None => s.push_str(&format!("    {name}\n")),
        }
    }
    s.push_str("  vLLM served:\n");
    if res.vllm.is_empty() {
        s.push_str("    (none)\n");
    }
    for m in &res.vllm {
        s.push_str(&format!("    {m}\n"));
    }
    if res.is_contended() {
        s.push_str(
            "  ⚠ both engines are resident and share one unified pool — \
             use `dgx vllm up --evict-ollama` to free it before a large vLLM serve.\n",
        );
    }
    s
}

/// `dgx gpu` — print the cross-engine residency snapshot. Each side is
/// best-effort: an unconfigured/unreachable engine simply shows nothing.
async fn gpu(config_path: Option<&Path>) -> anyhow::Result<()> {
    let dgx = dgx_config(config_path)?;
    let client = http_client();
    let ollama = match dgx.resolve_endpoint_for(EndpointKind::Ollama) {
        Ok(base) => fetch_ollama_ps(&client, &base).await.unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let vllm = match dgx.resolve_endpoint_for(EndpointKind::Vllm) {
        Ok(base) => fetch_vllm_models(&base).await.unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let mem_available = match dgx.ssh_host() {
        Ok(host) => detect_node_mem_available(&dgx.ssh_user(), &host, None),
        Err(_) => None,
    };
    let res = Residency {
        ollama,
        vllm,
        mem_available,
    };
    println!("GPU residency:");
    print!("{}", render_residency(&res));
    Ok(())
}

/// POST an `/api/generate` unload (`keep_alive: 0`) to the Ollama endpoint.
async fn unload_ollama_model(
    client: &reqwest::Client,
    base: &str,
    model: &str,
) -> anyhow::Result<()> {
    let url = format!("{}/api/generate", base.trim_end_matches('/'));
    let resp = client
        .post(&url)
        .json(&ollama_unload_body(model))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("request failed: {e}"))?;
    if !resp.status().is_success() {
        anyhow::bail!("HTTP {}", resp.status());
    }
    Ok(())
}

/// Unload every model resident on the active Ollama endpoint (the
/// `--evict-ollama` swap). Best-effort: a missing endpoint or a single failed
/// unload warns rather than aborting the launch.
async fn evict_ollama_models(dgx: &DgxConfig) -> anyhow::Result<()> {
    let base = match dgx.resolve_endpoint_for(EndpointKind::Ollama) {
        Ok(b) => b,
        Err(_) => {
            eprintln!("  --evict-ollama: no Ollama endpoint configured; nothing to evict");
            return Ok(());
        }
    };
    let client = http_client();
    let resident = fetch_ollama_ps(&client, &base).await.unwrap_or_default();
    let targets = ollama_evict_targets(&Residency {
        ollama: resident,
        vllm: Vec::new(),
        mem_available: None,
    });
    if targets.is_empty() {
        println!("  --evict-ollama: no resident Ollama models to free");
        return Ok(());
    }
    for m in &targets {
        match unload_ollama_model(&client, &base, m).await {
            Ok(()) => println!("  evicted Ollama model {m}"),
            Err(e) => eprintln!("  WARNING: failed to evict Ollama model {m}: {e}"),
        }
    }
    Ok(())
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
            Some("192.168.86.40"),
            "dgx",
            Some("qwen2.5-coder:32b"),
            false,
            true, // yes — skip prompt
        )
        .unwrap();

        let text = std::fs::read_to_string(&cfg_path).unwrap();
        assert!(text.contains("192.168.86.40"), "host not in config: {text}");
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
            Some("dgx.home.lab"),
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
            text.contains("dgx.home.lab"),
            "new dgx host not written: {text}"
        );
    }

    #[test]
    fn setup_node_name_propagates() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");

        setup(Some(&cfg_path), Some("10.0.0.1"), "lab", None, false, true).unwrap();

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
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "data": [] })),
            )
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
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "data": [] })),
            )
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
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "models": [] })),
            )
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
}
