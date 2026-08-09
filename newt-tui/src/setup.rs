//! Inference-backend setup (`newt setup`).
//!
//! With no target, this is the human-driven first-run wizard. With a hostname,
//! URL, or `host:port`, it probes Ollama and OpenAI-compatible model-list APIs,
//! records every live endpoint, and adopts the models reported by the server.
//! Bare hosts expand through `[discovery]`; authenticated probing requires an
//! explicit HTTPS URL (HTTP only for loopback) so a bearer token is never
//! broadcast across guessed ports or sent over implicit plaintext transport.
//!
//! The console I/O is abstracted behind the [`Console`] trait so the whole
//! flow can be driven by scripted answers in tests (against a `wiremock`
//! endpoint) without a real TTY. The pure config-building and URL-normalising
//! helpers are unit-tested directly.
//!
//! ## What it writes
//!
//! Each endpoint gets a `backends/<name>.toml` drop-in. The main `config.toml`
//! only selects the first detected endpoint as `default_backend`, preserving
//! unrelated keys and comments and backing up an existing file before change.
//! Existing matching drop-ins are reused byte-for-byte; name collisions receive
//! a numeric suffix. Token values are used only in memory, while configuration
//! stores an environment-variable or absolute file reference.
//!
//! ## Agent commit identity — not yet a setup step
//!
//! Harness attribution (`name` / `email`) lives in
//! `.newt/agent-identity.toml` (see [`newt_core::AgentIdentity`]), defaulting
//! to the GitHub User <https://github.com/newt-agent>. Operators set it today
//! with `newt identity set --name … --email …` (or by editing the file). A
//! future setup-dialog step should call [`newt_core::AgentIdentity::save`]
//! into that same path — do not open-code a second writer here.

use crate::line_console::{is_yes, Console, StdinConsole};
use newt_core::backend_probe::EndpointProbeResult;
use newt_core::config::Discovery;
use newt_core::provider_preset::{
    self, list_models_for_preset, preset_support, PresetSupport, ProviderPreset,
};
use newt_core::{BackendConfig, BackendKind, Config, EndpointKind, Tier};
use std::collections::HashSet;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Run the interactive setup wizard, writing to `~/.newt/config.toml`.
pub fn run(_color: bool) -> anyhow::Result<()> {
    wizard_entry(&mut StdinConsole, Flow::Setup)
}

/// The first-run variant ([`crate::wizard::maybe_run`]): driven through
/// [`FirstRunConsole`] so Esc/Ctrl-C surface as catchable aborts (the caller
/// falls back to probe-and-write defaults), and the overwrite guard is
/// skipped (the caller already proved no config exists).
pub(crate) fn run_first_run(_color: bool) -> anyhow::Result<()> {
    wizard_entry(&mut crate::line_console::FirstRunConsole, Flow::FirstRun)
}

fn wizard_entry(console: &mut dyn Console, flow: Flow) -> anyhow::Result<()> {
    let config_path =
        Config::user_config_path().unwrap_or_else(|| std::path::PathBuf::from("newt.toml"));
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(4))
        .build()
        .unwrap_or_default();
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(run_with_flow(
            console,
            &client,
            &config_path,
            flow,
        ))
    })
}

/// Probe a hostname or URL and write one backend drop-in per live inference
/// endpoint. A bare hostname expands through `[discovery]`; an explicit URL or
/// `host:port` is probed exactly once.
pub async fn run_target(
    target: &str,
    token_env: Option<&str>,
    token_file: Option<&Path>,
    yes: bool,
    explicit_config_path: Option<&Path>,
) -> anyhow::Result<()> {
    let has_env_config = std::env::var_os("NEWT_CONFIG").is_some_and(|value| !value.is_empty());
    if explicit_config_path.is_some() || has_env_config {
        anyhow::bail!(
            "targeted setup does not support --config or NEWT_CONFIG because backend drop-ins \
             need a config root; use --config-dir instead"
        );
    }
    if !yes && !io::stdin().is_terminal() {
        anyhow::bail!("setup needs confirmation on a terminal; pass --yes for non-interactive use");
    }
    let config_path = explicit_config_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| {
            Config::user_config_path().unwrap_or_else(|| PathBuf::from("newt.toml"))
        });
    let discovery = if config_path.is_file() {
        Config::load(&config_path)?.discovery
    } else {
        Discovery::default()
    };
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()?;
    let mut console = StdinConsole;
    run_target_with(
        &mut console,
        &client,
        &config_path,
        TargetSetupRequest {
            target,
            token_env,
            token_file,
            yes,
        },
        &discovery,
    )
    .await
}

#[derive(Clone, Copy)]
struct TargetSetupRequest<'a> {
    target: &'a str,
    token_env: Option<&'a str>,
    token_file: Option<&'a Path>,
    yes: bool,
}

async fn run_target_with(
    console: &mut dyn Console,
    client: &reqwest::Client,
    config_path: &Path,
    request: TargetSetupRequest<'_>,
    discovery: &Discovery,
) -> anyhow::Result<()> {
    let TargetSetupRequest {
        target,
        token_env,
        token_file,
        yes,
    } = request;
    if token_env.is_some() || token_file.is_some() {
        validate_authenticated_target(target)?;
    }
    let candidates = candidate_endpoints(target, discovery)?;
    let token_file = token_file.map(std::fs::canonicalize).transpose()?;
    let api_key = resolve_setup_token(token_env, token_file.as_deref())?;
    console.say(&format!(
        "Probing {} candidate endpoint{} for Ollama or OpenAI-compatible APIs...",
        candidates.len(),
        if candidates.len() == 1 { "" } else { "s" }
    ));

    let (hits, failures) =
        probe_candidates_concurrently(client, &candidates, api_key.as_deref()).await?;
    if hits.is_empty() {
        for failure in failures {
            console.say(&format!("  {failure}"));
        }
        anyhow::bail!(
            "no supported inference API found for `{target}`; tried {}",
            candidates.join(", ")
        );
    }

    if !failures.is_empty() {
        console.say(&format!(
            "Skipped {} candidate endpoint{}:",
            failures.len(),
            if failures.len() == 1 { "" } else { "s" }
        ));
        for failure in failures {
            console.say(&format!("  {failure}"));
        }
    }

    console.say(&format!(
        "Detected {} inference backend{}:",
        hits.len(),
        if hits.len() == 1 { "" } else { "s" }
    ));
    for hit in &hits {
        let backend = backend_from_probe(hit, token_env, token_file.as_deref())?;
        console.say(&format!(
            "  {} ({:?}, {}, {} model{})",
            backend.name,
            backend.kind,
            backend.endpoint,
            hit.models.len(),
            if hit.models.len() == 1 { "" } else { "s" }
        ));
    }

    if !yes {
        let answer = console.ask(&format!(
            "Write backend files and update {}? [Y/n] ",
            config_path.display()
        ))?;
        if !is_yes(&answer, true) {
            console.say("Aborted. Nothing written.");
            return Ok(());
        }
    }

    let written = persist_detected_setup(config_path, &hits, token_env, token_file.as_deref())?;
    for path in &written {
        console.say(&format!("Wrote {}.", path.display()));
    }
    console.say(&format!(
        "Configuration ready at {}.",
        config_path.display()
    ));
    Ok(())
}

// ---------------------------------------------------------------------------
// Driver (fully testable: scripted Console + wiremock client)
// ---------------------------------------------------------------------------

/// Which door the wizard was entered through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    /// `newt setup` — a deliberate re-run; guard against overwriting.
    Setup,
    /// First run (unboxing) — the caller already proved no config exists.
    FirstRun,
}

/// The wizard flow, parameterised over its console and HTTP client so it can be
/// exercised end-to-end in tests (production enters via [`run_with_flow`]).
#[cfg(test)]
async fn run_with(
    console: &mut dyn Console,
    client: &reqwest::Client,
    config_path: &Path,
) -> anyhow::Result<()> {
    run_with_flow(console, client, config_path, Flow::Setup).await
}

async fn run_with_flow(
    console: &mut dyn Console,
    client: &reqwest::Client,
    config_path: &Path,
    flow: Flow,
) -> anyhow::Result<()> {
    // First run already printed the branded crawl header; only a standalone
    // `newt setup` announces itself.
    if flow == Flow::Setup {
        console.say(&format!("newt v{} — interactive setup", crate::VERSION));
    }

    if flow == Flow::Setup && config_path.exists() {
        let ans = console.ask(&format!(
            "A config already exists at {}. Overwrite? [y/N] ",
            config_path.display()
        ))?;
        if !is_yes(&ans, false) {
            console.say("Keeping the existing config. Nothing written.");
            return Ok(());
        }
    }

    // Multi-backend loop: each pass configures + writes ONE backend, then
    // offers another round — so anthropic + ollama.com + a LAN box can all
    // land in one sitting. With several written, the default is picked at
    // the end (until then, last-written wins via each cfg.save).
    let mut written: Vec<String> = Vec::new();
    loop {
        let (cfg, backend) = match choose_backend(console)? {
            BackendChoice::LocalOllama => configure_ollama(console, client).await?,
            BackendChoice::CustomHost => {
                configure_custom_host(console, client, config_path).await?
            }
            BackendChoice::HostedProvider => configure_hosted(console, client, config_path).await?,
        };

        // Preview before committing anything to disk: the backend drop-in is
        // the interesting file; config.toml just points at it.
        let preview = toml::to_string_pretty(&backend)
            .unwrap_or_else(|e| format!("# (could not render preview: {e})"));
        console.say(&format!("\nbackends/{}.toml:\n", backend.name));
        console.say(&preview);

        let ans = console.ask(&format!("Write to {}? [Y/n] ", config_path.display()))?;
        if !is_yes(&ans, true) {
            if written.is_empty() {
                console.say("Aborted. Nothing written.");
                return Ok(());
            }
            console.say("Skipped this one.");
        } else {
            let dropin = newt_core::write_backend_dropin(config_path, &backend)
                .map_err(|e| anyhow::anyhow!(e))?;
            cfg.save(config_path)?;
            console.say(&format!(
                "Wrote {} and {}.",
                config_path.display(),
                dropin.display()
            ));
            written.push(backend.name.clone());
        }

        let more = console.ask("Add another backend? [y/N] ")?;
        if !is_yes(&more, false) {
            break;
        }
    }

    if written.len() > 1 {
        console.say("\nWhich backend should sessions start on?");
        let idx = select_row(console, &written, "backends")?;
        let chosen = &written[idx];
        // Rewrite ONLY default_backend, preserving the config's other keys
        // and comments (the same comment-preserving editor `newt setup
        // <target>` uses).
        let old_text = std::fs::read_to_string(config_path).unwrap_or_default();
        let new_text =
            Config::with_default_backend(&old_text, chosen).map_err(|e| anyhow::anyhow!(e))?;
        std::fs::write(config_path, new_text)?;
        console.say(&format!(
            "Default backend: {chosen} (/backends switches per session)."
        ));
    }

    console.say("Edit those files (or re-run `newt setup`) to change anything.");
    offer_identity(console);
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum BackendChoice {
    LocalOllama,
    CustomHost,
    HostedProvider,
}

fn choose_backend(console: &mut dyn Console) -> anyhow::Result<BackendChoice> {
    console.say("\nWhere does your model run?");
    console.say("  1) Ollama on this machine   (http://127.0.0.1:11434)");
    console.say(
        "  2) Another machine          (hostname or URL — newt probes for Ollama / llama.cpp / vLLM)",
    );
    console
        .say("  3) A hosted provider        (OpenAI, Anthropic, OpenRouter, NVIDIA, … — API key)");
    let ans = console.ask("Choose [1]: ")?;
    Ok(match parse_choice(&ans, 3).unwrap_or(1) {
        2 => BackendChoice::CustomHost,
        3 => BackendChoice::HostedProvider,
        _ => BackendChoice::LocalOllama,
    })
}

// ---------------------------------------------------------------------------
// Ollama path
// ---------------------------------------------------------------------------

async fn configure_ollama(
    console: &mut dyn Console,
    client: &reqwest::Client,
) -> anyhow::Result<(Config, BackendConfig)> {
    let default_url = "http://127.0.0.1:11434";
    let raw = console.ask(&format!("Ollama host [{default_url}]: "))?;
    let url = normalize_url(
        if raw.is_empty() { default_url } else { &raw },
        "http",
        11434,
    );

    let model = pick_model(console, client, &url).await?;
    Ok(build_ollama_config(
        Config::default(),
        "default",
        EndpointKind::Ollama,
        &url,
        &model,
    ))
}

// ---------------------------------------------------------------------------
// Custom-host path (auto-probe: Ollama / llama.cpp / vLLM)
// ---------------------------------------------------------------------------

/// One probe-result row for the endpoint menu: engine label (kind label when
/// the fingerprint came back empty), model count, and the first warm model
/// when one is reported. Pure.
fn format_endpoint_row(hit: &EndpointProbeResult) -> String {
    let engine = hit
        .engine
        .map(|e| e.label().to_string())
        .unwrap_or_else(|| hit.kind.label().to_string());
    let mut row = format!(
        "{}   {}   {} model{}",
        hit.endpoint,
        engine,
        hit.models.len(),
        if hit.models.len() == 1 { "" } else { "s" }
    );
    if let Some(warm) = hit.warm.first() {
        row.push_str(&format!("   warm: {warm}"));
    }
    row
}

/// Order `models` with the warm (loaded-in-memory) subset first, so the
/// numbered menu's Enter default IS the model that answers immediately.
/// Warm entries not present in `models` are ignored (stale-probe safety —
/// same rule as `backend_probe::adopt`). Pure.
fn order_models_warm_first(models: &[String], warm: &[String]) -> Vec<String> {
    let mut out: Vec<String> = warm
        .iter()
        .filter(|w| models.contains(w))
        .cloned()
        .collect();
    for m in models {
        if !out.contains(m) {
            out.push(m.clone());
        }
    }
    out
}

/// The wizard's "another machine" door: expand the host through
/// `[discovery]`, probe every candidate concurrently, present what answered
/// (engine + warm model), and adopt the warm model as the Enter default.
/// Auth-required endpoints get one hidden-input key prompt + re-probe;
/// pasted keys go through the encrypted store like every other wizard token.
async fn configure_custom_host(
    console: &mut dyn Console,
    client: &reqwest::Client,
    config_path: &Path,
) -> anyhow::Result<(Config, BackendConfig)> {
    loop {
        let raw = console
            .ask("Host name or URL (e.g. gpu-box, 10.0.0.5:8000, https://llm.example.net): ")?;
        let target = raw.trim().to_string();
        if target.is_empty() {
            console.say("  A host is required.");
            continue;
        }
        let discovery = if config_path.is_file() {
            Config::load(config_path)
                .map(|c| c.discovery)
                .unwrap_or_default()
        } else {
            Discovery::default()
        };
        let candidates = match candidate_endpoints(&target, &discovery) {
            Ok(c) => c,
            Err(e) => {
                console.say(&format!("  {e}"));
                continue;
            }
        };
        console.say(&format!("Probing {}…", candidates.join(", ")));
        let (mut hits, mut failures) =
            probe_candidates_concurrently(client, &candidates, None).await?;

        // Authentication-required endpoints (the typed probe error): offer one
        // hidden key prompt and re-probe. The https/loopback transport rule
        // applies before any token leaves this process.
        let mut api_key: Option<String> = None;
        if hits.is_empty()
            && failures
                .iter()
                .any(|f| f.contains("authentication required"))
        {
            let key = console.ask_secret("API key (echoes as *, Enter to skip): ")?;
            let key = key.trim().to_string();
            if !key.is_empty() {
                validate_authenticated_target(&target)?;
                let (h, f) = probe_candidates_concurrently(client, &candidates, Some(&key)).await?;
                hits = h;
                failures = f;
                api_key = Some(key);
            }
        }

        if hits.is_empty() {
            for failure in &failures {
                console.say(&format!("  {failure}"));
            }
            console.say(&format!(
                "\nNo inference API answered at {target} (tried {}).",
                candidates.join(", ")
            ));
            console.say("  1) Try a different host");
            console.say("  2) Enter endpoint and model by hand");
            console.say("  3) Cancel setup");
            let ans = console.ask("Choose [1]: ")?;
            match parse_choice(&ans, 3).unwrap_or(1) {
                2 => return manual_backend_entry(console),
                3 => {
                    // Interrupted, not a plain bail: first run maps this to
                    // its defaults fallback (`wizard::is_abort`).
                    return Err(anyhow::Error::from(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "setup cancelled",
                    )));
                }
                _ => continue,
            }
        }

        console.say(&format!(
            "\nFound {} endpoint{}:",
            hits.len(),
            if hits.len() == 1 { "" } else { "s" }
        ));
        for (i, hit) in hits.iter().enumerate() {
            console.say(&format!("  {}) {}", i + 1, format_endpoint_row(hit)));
        }
        let ans = console.ask("Endpoint [1]: ")?;
        let idx = parse_choice(&ans, hits.len()).map(|n| n - 1).unwrap_or(0);
        let hit = &hits[idx];

        let ordered = order_models_warm_first(&hit.models, &hit.warm);
        let model = if ordered.is_empty() {
            ask_model_name(console)?
        } else {
            select_model(console, &ordered)?
        };

        let mut backend = backend_from_probe(hit, None, None)?;
        backend.model = Some(model);
        if let Some(key) = api_key {
            let reference = persist_wizard_token(console, config_path, &backend.name, &key)?;
            backend.api_key_file = Some(reference);
        }
        let config = Config {
            backends: vec![], // the drop-in IS the backend list
            default_backend: Some(backend.name.clone()),
            ..Default::default()
        };
        return Ok((config, backend));
    }
}

/// Nothing answered but the operator knows the endpoint: take it verbatim
/// (name still host-derived, #1448 — never a fixed literal).
fn manual_backend_entry(console: &mut dyn Console) -> anyhow::Result<(Config, BackendConfig)> {
    let url = loop {
        let raw = console.ask("Endpoint URL: ")?;
        if !raw.trim().is_empty() {
            break normalize_url(raw.trim(), "http", 11434);
        }
        console.say("  An endpoint URL is required (e.g. http://host:8080).");
    };
    console.say("\nWire protocol:");
    console.say("  1) ollama            (POST /api/chat)");
    console.say("  2) openai-compatible (POST /v1/chat/completions — llama.cpp, vLLM, gateways)");
    let ans = console.ask("Choose [1]: ")?;
    let kind = match parse_choice(&ans, 2) {
        Some(2) => BackendKind::Openai,
        _ => BackendKind::Ollama,
    };
    let model = ask_model_name(console)?;
    let name = backend_name(&url)?;
    let serving = match kind {
        BackendKind::Openai => newt_core::Serving::Instance,
        _ => newt_core::Serving::Multiplexer,
    };
    Ok(build_backend_pair(
        &name, &url, &model, kind, serving, None, "manual",
    ))
}

// ---------------------------------------------------------------------------
// Hosted-provider presets (the newt_core::provider_preset roster)
// ---------------------------------------------------------------------------

/// The "hosted provider" door: resolve the roster (builtin + drop-ins,
/// incl. copied Hermes YAML), pick one, configure it.
async fn configure_hosted(
    console: &mut dyn Console,
    client: &reqwest::Client,
    config_path: &Path,
) -> anyhow::Result<(Config, BackendConfig)> {
    let presets = provider_preset::resolve_presets(None);
    let preset = select_preset(console, &presets)?;
    configure_preset(console, client, &preset, config_path).await
}

/// Filterable roster picker. Unsupported presets (oauth-auth drop-ins,
/// bedrock modes, unroutable base URLs) are listed as "(unavailable: …)"
/// notes with the reason — visible, never numbered, never silently dropped.
fn select_preset(
    console: &mut dyn Console,
    presets: &[ProviderPreset],
) -> anyhow::Result<ProviderPreset> {
    let mut available: Vec<(&ProviderPreset, String)> = Vec::new();
    let mut unavailable: Vec<(String, String)> = Vec::new();
    for p in presets {
        match preset_support(p) {
            PresetSupport::Supported { endpoint, .. } => available.push((p, endpoint)),
            PresetSupport::Unsupported { reason } => {
                unavailable.push((p.label().to_string(), reason));
            }
        }
    }
    for (label, reason) in &unavailable {
        console.say(&format!("  (unavailable: {label} — {reason})"));
    }
    if available.is_empty() {
        anyhow::bail!("no usable provider presets (see the unavailable notes above)");
    }
    let rows: Vec<String> = available
        .iter()
        .map(|(p, endpoint)| format!("{:<24}{}", p.label(), endpoint))
        .collect();
    let idx = select_row(console, &rows, "providers")?;
    Ok(available[idx].0.clone())
}

/// Configure a hosted provider from its preset: env-var reference first
/// (checked in `env_vars` priority order — the var that RESOLVES is the one
/// recorded), else a hidden-input paste stored ENCRYPTED at rest; then a
/// model pick probed through the preset's own catalog. An empty `env_vars`
/// (LM Studio) skips the credential step entirely.
async fn configure_preset(
    console: &mut dyn Console,
    client: &reqwest::Client,
    preset: &ProviderPreset,
    config_path: &Path,
) -> anyhow::Result<(Config, BackendConfig)> {
    console.say(&format!("\n{} — {}", preset.label(), preset.base_url));
    if let Some(description) = &preset.description {
        console.say(description);
    }
    if let Some(signup) = &preset.signup_url {
        console.say(&format!("Create an API key at {signup}"));
    }
    if !preset.default_headers.is_empty() {
        // Limitation L1 (docs/provider-presets.md): carried, not sent.
        console.say(&format!(
            "  Note: {} suggests extra request headers; newt does not send custom headers yet.",
            preset.label()
        ));
    }

    // (env reference, encrypted-file reference, key for the model probe)
    let (api_key_env, api_key_file, probe_key): (Option<String>, Option<String>, Option<String>) =
        if preset.env_vars.is_empty() {
            (None, None, None) // keyless provider — no credential step
        } else {
            let exported = preset.env_vars.iter().find_map(|var| {
                std::env::var(var)
                    .ok()
                    .filter(|v| !v.trim().is_empty())
                    .map(|v| (var.clone(), v))
            });
            if let Some((var, value)) = exported {
                let ans = console.ask(&format!("${var} is set in this shell. Use it? [Y/n] "))?;
                if is_yes(&ans, true) {
                    // Record the var that actually resolved, not [0].
                    (Some(var), None, Some(value))
                } else {
                    preset_pasted_key(console, preset, config_path)?
                }
            } else {
                preset_pasted_key(console, preset, config_path)?
            }
        };

    console.say(&format!(
        "Probing {} for available models…",
        preset.base_url
    ));
    let models = list_models_for_preset(client, preset, probe_key.as_deref()).await;
    let model = match models {
        Ok(m) if !m.is_empty() => select_model(console, &m)?,
        Ok(_) | Err(_) => {
            console.say("  Could not list models (endpoint unreachable or key not yet usable).");
            // Fallback ladder: curated list → single default → free entry.
            match preset.fallback_models.len() {
                0 => ask_model_name(console)?,
                1 => {
                    let default = &preset.fallback_models[0];
                    let raw = console.ask(&format!("Model name [{default}]: "))?;
                    if raw.trim().is_empty() {
                        default.clone()
                    } else {
                        raw.trim().to_string()
                    }
                }
                _ => select_model(console, &preset.fallback_models)?,
            }
        }
    };

    // Field note: a bad token used to sail through setup (list endpoints
    // aren't uniformly auth-gated) and only 401 on the first real message.
    // Test the credential NOW, with re-entry on rejection.
    let (api_key_env, api_key_file, _probe_key) = verify_key_with_retries(
        console,
        client,
        preset,
        config_path,
        &model,
        (api_key_env, api_key_file, probe_key),
    )
    .await?;

    let backend = provider_preset::backend_from_preset(
        preset,
        &model,
        api_key_env,
        api_key_file,
        crate::VERSION,
    )
    .map_err(|reason| anyhow::anyhow!("preset {} is not usable: {reason}", preset.name))?;
    let config = Config {
        backends: vec![], // the drop-in IS the backend list
        default_backend: Some(backend.name.clone()),
        ..Default::default()
    };
    Ok((config, backend))
}

/// Credential triple as the wizard threads it: (env reference, encrypted-file
/// reference, plaintext key for probes — never persisted).
type WizardCred = (Option<String>, Option<String>, Option<String>);

/// Live-test the pasted key before anything is written (wire-aware —
/// [`provider_preset::verify_key_for_preset`]), with up to two re-entries on
/// a 401/403. Env-only / keyless configurations (no probe key) pass through
/// untouched, and an unverifiable check (endpoint down) continues honestly
/// rather than blocking setup.
async fn verify_key_with_retries(
    console: &mut dyn Console,
    client: &reqwest::Client,
    preset: &ProviderPreset,
    config_path: &Path,
    model: &str,
    mut cred: WizardCred,
) -> anyhow::Result<WizardCred> {
    for _ in 0..3 {
        let Some(key) = cred.2.as_deref() else {
            return Ok(cred);
        };
        console.say(&format!("Testing the key against {}…", preset.base_url));
        match provider_preset::verify_key_for_preset(client, preset, key, model).await {
            provider_preset::KeyCheck::Accepted => {
                console.say("  ✓ key accepted");
                return Ok(cred);
            }
            provider_preset::KeyCheck::Rejected(code) => {
                console.say(&format!("  ✗ key rejected (HTTP {code})"));
                let ans = console.ask("Re-enter the key? [Y/n] ")?;
                if !is_yes(&ans, true) {
                    console.say("  Keeping it — fix later by re-running `newt setup`.");
                    return Ok(cred);
                }
                cred = preset_pasted_key(console, preset, config_path)?;
            }
            provider_preset::KeyCheck::Unverified(reason) => {
                console.say(&format!(
                    "  Could not verify the key ({reason}) — continuing."
                ));
                return Ok(cred);
            }
        }
    }
    console.say("  Still rejected — keeping the last key; fix later with `newt setup`.");
    Ok(cred)
}

/// The paste path shared by both preset branches: hidden input; Enter skips
/// (env reference recorded, nothing stored); a pasted token goes through the
/// encrypted store. Returns (api_key_env, api_key_file, probe key).
#[allow(clippy::type_complexity)]
fn preset_pasted_key(
    console: &mut dyn Console,
    preset: &ProviderPreset,
    config_path: &Path,
) -> anyhow::Result<(Option<String>, Option<String>, Option<String>)> {
    let key = console.ask_secret("API key (echoes as *, Enter to skip): ")?;
    let key = key.trim().to_string();
    if key.is_empty() {
        let var = preset
            .env_vars
            .first()
            .cloned()
            .unwrap_or_else(|| provider_preset::synthesized_env_var(&preset.name));
        console.say(&format!(
            "  No key — writing the backend anyway; export ${var} before use."
        ));
        return Ok((Some(var), None, None));
    }
    let reference = persist_wizard_token(console, config_path, &preset.name, &key)?;
    Ok((None, Some(reference), Some(key)))
}

/// Store a pasted token ENCRYPTED at rest (`newt_core::secrets`): one
/// optional-passphrase question (Enter = machine-local key), then the
/// `.token.age` write. Returns the tilde-collapsed `api_key_file` value. The
/// token itself is never echoed and never appears in any config file.
fn persist_wizard_token(
    console: &mut dyn Console,
    config_path: &Path,
    name: &str,
    token: &str,
) -> anyhow::Result<String> {
    console.say("Protect the stored key with a passphrase? Enter uses a machine-local key.");
    let pass = console.ask_secret("Passphrase (echoes as *): ")?;
    let backends_dir = config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.join("backends"))
        .ok_or_else(|| anyhow::anyhow!("config path has no parent directory"))?;
    let passphrase = {
        let trimmed = pass.trim();
        (!trimmed.is_empty()).then(|| newt_core::secrets::SecretString::from(trimmed.to_string()))
    };
    let path = newt_core::secrets::store_token(&backends_dir, name, token, passphrase.as_ref())
        .map_err(|e| anyhow::anyhow!(e))?;
    let reference = collapse_home(&path);
    console.say(&format!("  → stored encrypted at {reference}"));
    Ok(reference)
}

// ---------------------------------------------------------------------------
// Agent commit identity (one question, Enter skips)
// ---------------------------------------------------------------------------

/// Offer to set the agent commit identity after a successful write. Parses
/// `Name <email>`; saves via [`newt_core::AgentIdentity::save`] — the ONE
/// sanctioned writer (never open-code a second). Non-fatal: the backend
/// config already landed, so failures here only print.
fn offer_identity(console: &mut dyn Console) {
    let Some(path) = newt_core::AgentIdentity::user_identity_path() else {
        return;
    };
    if path.exists() {
        return; // already configured — don't nag on re-runs
    }
    let raw = match console
        .ask("Attribution for agent commits — \"Name <email>\" (Enter to keep the default): ")
    {
        Ok(raw) => raw,
        Err(_) => return, // Esc here must not undo a completed setup
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return;
    }
    let Some((name, email)) = parse_identity_line(raw) else {
        console.say("  Expected \"Name <email>\" — skipped (set later with `newt identity set`).");
        return;
    };
    let identity = newt_core::AgentIdentity {
        name,
        email,
        ..Default::default()
    };
    match identity.save(&path) {
        Ok(()) => console.say(&format!("  Wrote {}.", path.display())),
        Err(e) => console.say(&format!("  Could not write identity ({e}) — skipped.")),
    }
}

/// Parse `Name <email>` into its parts. Pure.
fn parse_identity_line(raw: &str) -> Option<(String, String)> {
    let (name, rest) = raw.split_once('<')?;
    let email = rest.strip_suffix('>')?.trim();
    let name = name.trim();
    if name.is_empty() || email.is_empty() || !email.contains('@') {
        return None;
    }
    Some((name.to_string(), email.to_string()))
}

/// Render `path` as `~/…` when it sits under the home directory, else as an
/// absolute path.
///
/// Checks `USERPROFILE` as well as `HOME` so Windows collapses too, and always
/// returns *something*: a portable-looking path is a nicety, and must never be
/// the reason a configured credential goes unrecorded.
fn collapse_home(path: &Path) -> String {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from);
    if let Some(home) = home {
        if let Ok(rel) = path.strip_prefix(&home) {
            return format!("~/{}", rel.display());
        }
    }
    path.display().to_string()
}

// ---------------------------------------------------------------------------
// Model selection (probe → numbered list → pick, with manual fallback)
// ---------------------------------------------------------------------------

async fn pick_model(
    console: &mut dyn Console,
    client: &reqwest::Client,
    url: &str,
) -> anyhow::Result<String> {
    console.say(&format!("Probing {url} for installed models…"));
    let models = fetch_ollama_models(client, url).await;

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

    select_model(console, &models)
}

/// Above this many models, a flat numbered list stops being a menu and starts
/// being a wall — a llama.cpp router routinely serves 30+. At or below it, the
/// list is faster to read than any filter prompt would be.
const FILTER_THRESHOLD: usize = 9;

/// Choose one entry from `models`, filtering first when the list is long.
///
/// Deliberately built on the line-based [`Console`] rather than a raw-mode
/// arrow-key widget: setup frequently runs over SSH, piped, or with stdin
/// redirected, and a raw-mode picker would either hang or have to be bypassed
/// in exactly those cases. Filtering gets the same "find it among 36" result
/// while staying pipe-safe and unit-testable.
fn select_model(console: &mut dyn Console, models: &[String]) -> anyhow::Result<String> {
    let idx = select_row(console, models, "models")?;
    Ok(models[idx].clone())
}

/// The generic filterable numbered picker `select_model` and
/// `select_preset` share: same threshold, same blank-filter and
/// no-match-falls-back-to-all semantics, same pipe-safety. Returns the
/// index into the ORIGINAL `rows` slice.
fn select_row(console: &mut dyn Console, rows: &[String], noun: &str) -> anyhow::Result<usize> {
    let mut pool: Vec<(usize, &String)> = rows.iter().enumerate().collect();

    if pool.len() > FILTER_THRESHOLD {
        console.say(&format!("\n{} {noun} available.", pool.len()));
        let needle = console.ask("Filter (blank = show all): ")?;
        let needle = needle.trim().to_ascii_lowercase();
        if !needle.is_empty() {
            let matched: Vec<(usize, &String)> = pool
                .iter()
                .filter(|(_, row)| row.to_ascii_lowercase().contains(&needle))
                .copied()
                .collect();
            // A filter that matches nothing falls back to the full list rather
            // than dead-ending the operator in an empty menu.
            if matched.is_empty() {
                console.say(&format!("  No match for {needle:?}; showing all."));
            } else {
                pool = matched;
            }
        }
    }

    console.say(&format!("\nAvailable {noun}:"));
    for (i, (_, row)) in pool.iter().enumerate() {
        console.say(&format!("  {}) {row}", i + 1));
    }
    let ans = console.ask("Choose [1]: ")?;
    let picked = parse_choice(&ans, pool.len()).map(|n| n - 1).unwrap_or(0);
    Ok(pool[picked].0)
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
// The endpoint fetchers moved to `newt_core::backend_probe` (#1136) so the
// TUI session, setup, and doctor share one probe path.
use newt_core::backend_probe::fetch_ollama_models;

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

/// Normalise a user-typed endpoint into a full `scheme://host:port` URL.
///
/// Accepts a bare host (`REDACTED-HOST`), `host:port`, or a full URL
/// (`https://REDACTED-HOST`). A bare host gets `default_scheme` and
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

/// Expand a setup target into canonical base URLs. Explicit URLs and
/// `host:port` targets stay singular; bare hosts use the configured discovery
/// ports without inferring a wire protocol from the port number.
fn candidate_endpoints(target: &str, discovery: &Discovery) -> anyhow::Result<Vec<String>> {
    let target = target.trim();
    if target.is_empty() {
        anyhow::bail!("setup target cannot be empty");
    }
    let has_scheme = target.contains("://");
    let url_input = if has_scheme {
        target.to_string()
    } else {
        format!("http://{target}")
    };
    let mut url = reqwest::Url::parse(&url_input)
        .map_err(|e| anyhow::anyhow!("invalid setup target `{target}`: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        anyhow::bail!(
            "unsupported setup URL scheme `{}`; use http or https",
            url.scheme()
        );
    }
    if !url.username().is_empty() || url.password().is_some() {
        anyhow::bail!("do not put credentials in the setup URL; use --token-env or --token-file");
    }
    if url.host_str().is_none() {
        anyhow::bail!("setup target `{target}` has no hostname");
    }
    if url.query().is_some() || url.fragment().is_some() {
        anyhow::bail!("setup target must not contain a query string or fragment");
    }

    let explicit_port =
        url.port().is_some() || (!has_scheme && !target.starts_with('[') && target.contains(':'));
    if has_scheme || explicit_port {
        strip_probe_suffix(&mut url);
        return Ok(vec![url.as_str().trim_end_matches('/').to_string()]);
    }
    if url.path() != "/" {
        anyhow::bail!("a bare setup host cannot contain a path; supply a full URL instead");
    }

    let mut ports = Vec::new();
    for port in discovery
        .ollama_ports
        .iter()
        .chain(discovery.vllm_ports.iter())
        .copied()
    {
        if !ports.contains(&port) {
            ports.push(port);
        }
    }
    if ports.is_empty() {
        anyhow::bail!("[discovery] contains no ports to probe");
    }
    let mut endpoints = Vec::with_capacity(ports.len());
    for port in ports {
        let mut candidate = url.clone();
        candidate
            .set_port(Some(port))
            .map_err(|()| anyhow::anyhow!("cannot apply port {port} to `{target}`"))?;
        endpoints.push(candidate.as_str().trim_end_matches('/').to_string());
    }
    Ok(endpoints)
}

/// Probe every candidate endpoint concurrently (`detect_endpoint` — the ONE
/// probe core `newt setup <target>` and the wizard's custom-host door share).
/// Returns hits in candidate order plus human-readable failures (an endpoint
/// that answers with no models counts as a failure).
async fn probe_candidates_concurrently(
    client: &reqwest::Client,
    candidates: &[String],
    api_key: Option<&str>,
) -> anyhow::Result<(Vec<EndpointProbeResult>, Vec<String>)> {
    let mut tasks = tokio::task::JoinSet::new();
    for (index, endpoint) in candidates.iter().cloned().enumerate() {
        let client = client.clone();
        let api_key = api_key.map(str::to_string);
        tasks.spawn(async move {
            let result =
                newt_core::backend_probe::detect_endpoint(&client, &endpoint, api_key.as_deref())
                    .await;
            (index, endpoint, result)
        });
    }

    let mut ordered: Vec<Option<EndpointProbeResult>> = vec![None; candidates.len()];
    let mut failures: Vec<Option<String>> = vec![None; candidates.len()];
    while let Some(joined) = tasks.join_next().await {
        let (index, endpoint, result) = joined.map_err(|e| anyhow::anyhow!(e))?;
        match result {
            Ok(hit) if hit.models.is_empty() => {
                failures[index] = Some(format!(
                    "{endpoint}: endpoint answered but listed no models"
                ));
            }
            Ok(hit) => ordered[index] = Some(hit),
            Err(error) => failures[index] = Some(format!("{endpoint}: {error}")),
        }
    }
    Ok((
        ordered.into_iter().flatten().collect(),
        failures.into_iter().flatten().collect(),
    ))
}

fn validate_authenticated_target(target: &str) -> anyhow::Result<()> {
    let target = target.trim();
    if !target.contains("://") {
        anyhow::bail!(
            "authenticated setup needs an explicit URL including its scheme; use https:// so \
             the bearer token is not sent to inferred ports or plaintext transport"
        );
    }
    let url = reqwest::Url::parse(target)
        .map_err(|error| anyhow::anyhow!("invalid authenticated setup URL `{target}`: {error}"))?;
    if url.scheme() == "https" {
        return Ok(());
    }
    let loopback = url.host_str().is_some_and(|host| {
        let host = host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(host);
        host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    });
    if url.scheme() == "http" && loopback {
        return Ok(());
    }
    anyhow::bail!(
        "refusing to send a bearer token to `{target}` over plaintext transport; use an https:// \
         URL (http:// is allowed only for loopback)"
    )
}

fn strip_probe_suffix(url: &mut reqwest::Url) {
    let path = url.path().trim_end_matches('/');
    let base = ["/v1/models", "/api/tags", "/v1"]
        .iter()
        .find_map(|suffix| path.strip_suffix(suffix))
        .unwrap_or(path)
        .to_string();
    url.set_path(if base.is_empty() { "/" } else { &base });
}

fn backend_name(endpoint: &str) -> anyhow::Result<String> {
    let url = reqwest::Url::parse(endpoint)
        .map_err(|e| anyhow::anyhow!("invalid detected endpoint `{endpoint}`: {e}"))?;
    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("detected endpoint `{endpoint}` has no hostname"))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| anyhow::anyhow!("detected endpoint `{endpoint}` has no port"))?;
    let mut slug = String::new();
    let mut last_was_separator = false;
    for ch in host.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            last_was_separator = false;
        } else if !last_was_separator && !slug.is_empty() {
            slug.push('-');
            last_was_separator = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        anyhow::bail!("cannot derive a backend name from `{endpoint}`");
    }
    Ok(format!("{slug}-{port}"))
}

fn resolve_setup_token(
    token_env: Option<&str>,
    token_file: Option<&Path>,
) -> anyhow::Result<Option<String>> {
    if token_env.is_some() && token_file.is_some() {
        anyhow::bail!("use only one of --token-env or --token-file");
    }
    if let Some(name) = token_env {
        if name.trim().is_empty() {
            anyhow::bail!("--token-env needs a non-empty environment variable name");
        }
        return std::env::var(name)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .map(Some)
            .ok_or_else(|| {
                anyhow::anyhow!("token environment variable `{name}` is unset or empty")
            });
    }
    if let Some(path) = token_file {
        let reference = path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("token file path is not valid UTF-8"))?;
        let backend = BackendConfig {
            api_key_file: Some(reference.to_string()),
            ..Default::default()
        };
        return backend.resolve_api_key().map(Some).ok_or_else(|| {
            anyhow::anyhow!(
                "token file `{}` is missing or contains no token",
                path.display()
            )
        });
    }
    Ok(None)
}

fn backend_from_probe(
    probe: &EndpointProbeResult,
    token_env: Option<&str>,
    token_file: Option<&Path>,
) -> anyhow::Result<BackendConfig> {
    let url = reqwest::Url::parse(&probe.endpoint)
        .map_err(|e| anyhow::anyhow!("invalid detected endpoint `{}`: {e}", probe.endpoint))?;
    let token_file = token_file
        .map(|path| {
            path.to_str()
                .map(str::to_string)
                .ok_or_else(|| anyhow::anyhow!("token file path is not valid UTF-8"))
        })
        .transpose()?;
    Ok(BackendConfig {
        name: backend_name(&probe.endpoint)?,
        endpoint: probe.endpoint.clone(),
        // Adopt the WARM model as the default hint when the probe reports
        // one — a resident model answers immediately; install order says
        // nothing (mirrors adopt()'s multiplexer precedence).
        model: probe.warm.first().or(probe.models.first()).cloned(),
        tiers: vec![Tier::Fast, Tier::Standard, Tier::Complex, Tier::Review],
        kind: Some(probe.kind),
        api_key_file: token_file,
        api_key_env: token_env.map(str::to_string),
        serving: Some(probe.serving),
        engine: probe.engine,
        host: url.host_str().map(str::to_string),
        provenance: Some(newt_core::config::BackendProvenance {
            source: Some(format!(
                "newt setup v{} (auto-detected {:?})",
                crate::VERSION,
                probe.kind
            )),
            probed: Some(chrono::Local::now().format("%Y-%m-%d").to_string()),
            derived_serving: Some(true),
        }),
        ..Default::default()
    })
}

fn persist_detected_setup(
    config_path: &Path,
    probes: &[EndpointProbeResult],
    token_env: Option<&str>,
    token_file: Option<&Path>,
) -> anyhow::Result<Vec<PathBuf>> {
    if probes.is_empty() {
        anyhow::bail!("cannot persist an empty endpoint probe result");
    }
    if let Some(parent) = config_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let _lock = acquire_setup_lock(config_path)?;
    let old_config = read_setup_config(config_path)?;
    let backend_dir = config_path.with_file_name("backends");
    let existing = read_existing_setup_backends(&backend_dir)?;
    let mut used_names: HashSet<String> = existing.iter().map(|item| item.name.clone()).collect();
    let token_file_ref = token_file.and_then(Path::to_str);
    let mut planned = Vec::with_capacity(probes.len());

    for probe in probes {
        let normalized = normalize_setup_endpoint(&probe.endpoint)?;
        let base_name = backend_name(&probe.endpoint)?;
        if let Some(found) = existing
            .iter()
            .filter(|item| {
                item.endpoint.as_deref() == Some(normalized.as_str())
                    && item.matches_token_reference(token_env, token_file_ref)
                    && item.matches_probe(probe)
            })
            .min_by_key(|item| (item.name != base_name, item.name.as_str()))
        {
            planned.push(PlannedSetupBackend {
                name: found.name.clone(),
                endpoint: normalized,
                path: found.path.clone(),
                body: None,
            });
            continue;
        }
        if let Some(found) = planned.iter().find(|item: &&PlannedSetupBackend| {
            item.endpoint == normalized
                && item.matches_generated_reference(token_env, token_file_ref)
        }) {
            planned.push(PlannedSetupBackend {
                name: found.name.clone(),
                endpoint: normalized,
                path: found.path.clone(),
                body: None,
            });
            continue;
        }

        let name = allocate_backend_name(&base_name, &mut used_names);
        let mut backend = backend_from_probe(probe, token_env, token_file)?;
        backend.name.clone_from(&name);
        let body = toml::to_string(&backend)?;
        planned.push(PlannedSetupBackend {
            path: backend_dir.join(format!("{name}.toml")),
            name,
            endpoint: normalized,
            body: Some(body.into_bytes()),
        });
    }

    let default_name = &planned[0].name;
    let updated_config = Config::with_default_backend(&old_config, default_name)?;
    commit_setup_plan(config_path, &old_config, &updated_config, &planned)
}

#[derive(Debug)]
struct ExistingSetupBackend {
    name: String,
    path: PathBuf,
    endpoint: Option<String>,
    api_key_env: Option<String>,
    api_key_file: Option<String>,
    kind: Option<BackendKind>,
    serving: Option<newt_core::Serving>,
    model: Option<String>,
    generated_by_setup: bool,
}

impl ExistingSetupBackend {
    fn matches_token_reference(&self, env: Option<&str>, file: Option<&str>) -> bool {
        self.api_key_env.as_deref() == env && self.api_key_file.as_deref() == file
    }

    fn matches_probe(&self, probe: &EndpointProbeResult) -> bool {
        let kind_matches = self.kind == Some(probe.kind);
        let serving_matches = self.serving.is_none_or(|serving| serving == probe.serving);
        let model_matches = self
            .model
            .as_ref()
            .is_none_or(|model| probe.models.contains(model));
        kind_matches
            && serving_matches
            && model_matches
            && (!self.generated_by_setup || (self.serving.is_some() && self.model.is_some()))
    }
}

#[derive(Debug)]
struct PlannedSetupBackend {
    name: String,
    endpoint: String,
    path: PathBuf,
    body: Option<Vec<u8>>,
}

impl PlannedSetupBackend {
    fn matches_generated_reference(&self, env: Option<&str>, file: Option<&str>) -> bool {
        let Some(body) = self.body.as_deref() else {
            return true;
        };
        toml::from_slice::<BackendConfig>(body).is_ok_and(|backend| {
            backend.api_key_env.as_deref() == env && backend.api_key_file.as_deref() == file
        })
    }
}

fn read_setup_config(path: &Path) -> anyhow::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error.into()),
    }
}

fn normalize_setup_endpoint(endpoint: &str) -> anyhow::Result<String> {
    let url = reqwest::Url::parse(endpoint)
        .map_err(|error| anyhow::anyhow!("invalid backend endpoint `{endpoint}`: {error}"))?;
    Ok(url.as_str().trim_end_matches('/').to_string())
}

fn read_existing_setup_backends(dir: &Path) -> anyhow::Result<Vec<ExistingSetupBackend>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    paths.sort();

    let mut backends = Vec::with_capacity(paths.len());
    for path in paths {
        let Some(name) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        let parsed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|body| toml::from_str::<BackendConfig>(&body).ok());
        backends.push(ExistingSetupBackend {
            name,
            endpoint: parsed
                .as_ref()
                .and_then(|backend| normalize_setup_endpoint(&backend.endpoint).ok()),
            api_key_env: parsed
                .as_ref()
                .and_then(|backend| backend.api_key_env.clone()),
            api_key_file: parsed
                .as_ref()
                .and_then(|backend| backend.api_key_file.clone()),
            kind: parsed.as_ref().and_then(|backend| backend.kind),
            serving: parsed.as_ref().and_then(|backend| backend.serving),
            model: parsed.as_ref().and_then(|backend| backend.model.clone()),
            generated_by_setup: parsed.as_ref().is_some_and(|backend| {
                backend
                    .provenance
                    .as_ref()
                    .and_then(|provenance| provenance.source.as_deref())
                    .is_some_and(|source| source.starts_with("newt setup v"))
            }),
            path,
        });
    }
    Ok(backends)
}

fn allocate_backend_name(base: &str, used: &mut HashSet<String>) -> String {
    if used.insert(base.to_string()) {
        return base.to_string();
    }
    for suffix in 2_u32.. {
        let candidate = format!("{base}-{suffix}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }
    unreachable!("u32 backend-name suffix space exhausted")
}

#[derive(Debug)]
struct SetupLock(PathBuf);

impl Drop for SetupLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn acquire_setup_lock(config_path: &Path) -> anyhow::Result<SetupLock> {
    let filename = config_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.toml");
    let path = config_path.with_file_name(format!(".{filename}.setup.lock"));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(_) => Ok(SetupLock(path)),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => anyhow::bail!(
            "another setup process is updating {}; remove {} only if that process has stopped",
            config_path.display(),
            path.display()
        ),
        Err(error) => Err(error.into()),
    }
}

fn setup_config_destination(path: &Path) -> anyhow::Result<PathBuf> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Ok(std::fs::canonicalize(path)?),
        Ok(_) => Ok(path.to_path_buf()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(path.to_path_buf()),
        Err(error) => Err(error.into()),
    }
}

fn setup_file_permissions(path: &Path) -> anyhow::Result<Option<std::fs::Permissions>> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(Some(metadata.permissions())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn stage_setup_file(
    destination: &Path,
    body: &[u8],
    permissions: Option<&std::fs::Permissions>,
) -> anyhow::Result<PathBuf> {
    let parent = destination
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", destination.display()))?;
    std::fs::create_dir_all(parent)?;
    let filename = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("setup");
    for attempt in 0_u16..100 {
        let temp = parent.join(format!(
            ".{filename}.newt-{}-{attempt}.tmp",
            std::process::id()
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = match options.open(&temp) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        };
        let result = file
            .write_all(body)
            .and_then(|()| {
                if let Some(permissions) = permissions {
                    file.set_permissions(permissions.clone())?;
                }
                Ok(())
            })
            .and_then(|()| file.sync_all());
        if let Err(error) = result {
            let _ = std::fs::remove_file(&temp);
            return Err(error.into());
        }
        return Ok(temp);
    }
    anyhow::bail!(
        "could not allocate a temporary file beside {}",
        destination.display()
    )
}

#[derive(Default)]
struct SetupCommitGuard {
    temporary: Vec<PathBuf>,
    created: Vec<PathBuf>,
    committed: bool,
}

impl SetupCommitGuard {
    fn stage(
        &mut self,
        destination: &Path,
        body: &[u8],
        permissions: Option<&std::fs::Permissions>,
    ) -> anyhow::Result<PathBuf> {
        let path = stage_setup_file(destination, body, permissions)?;
        self.temporary.push(path.clone());
        Ok(path)
    }

    fn finish(mut self) -> Vec<PathBuf> {
        self.committed = true;
        std::mem::take(&mut self.created)
    }
}

impl Drop for SetupCommitGuard {
    fn drop(&mut self) {
        for path in &self.temporary {
            let _ = std::fs::remove_file(path);
        }
        if !self.committed {
            for path in &self.created {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

fn commit_backend_no_clobber(temp: &Path, destination: &Path) -> anyhow::Result<()> {
    match std::fs::hard_link(temp, destination) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Err(anyhow::anyhow!(
            "backend {} appeared while setup was running; retry setup",
            destination.display()
        )),
        Err(link_error) => {
            let result = (|| -> io::Result<()> {
                let mut source = std::fs::File::open(temp)?;
                let mut options = std::fs::OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt as _;
                    options.mode(0o600);
                }
                let mut destination_file = options.open(destination)?;
                let copy_result = std::io::copy(&mut source, &mut destination_file)
                    .and_then(|_| {
                        destination_file.set_permissions(source.metadata()?.permissions())
                    })
                    .and_then(|()| destination_file.sync_all());
                if copy_result.is_err() {
                    drop(destination_file);
                    let _ = std::fs::remove_file(destination);
                }
                copy_result.map(|_| ())
            })();
            result.map_err(|fallback_error| {
                anyhow::anyhow!(
                    "could not create backend {} without overwriting a file \
                     (hard link: {link_error}; no-clobber copy: {fallback_error})",
                    destination.display()
                )
            })
        }
    }
}

fn commit_setup_plan(
    config_path: &Path,
    old_config: &str,
    updated_config: &str,
    planned: &[PlannedSetupBackend],
) -> anyhow::Result<Vec<PathBuf>> {
    let mut guard = SetupCommitGuard::default();
    let mut staged_backends = Vec::new();
    for backend in planned {
        if let Some(body) = backend.body.as_deref() {
            staged_backends.push((
                guard.stage(&backend.path, body, None)?,
                backend.path.clone(),
            ));
        }
    }
    let config_destination = setup_config_destination(config_path)?;
    let config_permissions = setup_file_permissions(&config_destination)?;
    let config_stage = if updated_config != old_config {
        Some(guard.stage(
            &config_destination,
            updated_config.as_bytes(),
            config_permissions.as_ref(),
        )?)
    } else {
        None
    };
    let filename = config_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.toml");
    let backup_path = config_path.with_file_name(format!("{filename}.bak"));
    let backup_stage = if !old_config.is_empty() && updated_config != old_config {
        Some(guard.stage(
            &backup_path,
            old_config.as_bytes(),
            config_permissions.as_ref(),
        )?)
    } else {
        None
    };
    let previous_backup_stage = if backup_stage.is_some() {
        match std::fs::read(&backup_path) {
            Ok(body) => {
                let permissions = setup_file_permissions(&backup_path)?;
                Some(guard.stage(&backup_path, &body, permissions.as_ref())?)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        }
    } else {
        None
    };

    if config_stage.is_some() && read_setup_config(config_path)? != old_config {
        anyhow::bail!(
            "{} changed while setup was preparing its update; retry setup",
            config_path.display()
        );
    }

    for (temp, destination) in &staged_backends {
        commit_backend_no_clobber(temp, destination)?;
        guard.created.push(destination.clone());
    }
    if config_stage.is_some() && read_setup_config(config_path)? != old_config {
        anyhow::bail!(
            "{} changed while setup was preparing its update; retry setup",
            config_path.display()
        );
    }
    if let Some(temp) = backup_stage.as_ref() {
        std::fs::rename(temp, &backup_path)?;
    }
    if let Some(temp) = config_stage.as_ref() {
        if let Err(config_error) = std::fs::rename(temp, &config_destination) {
            let restore_result = if let Some(previous) = previous_backup_stage.as_ref() {
                std::fs::rename(previous, &backup_path)
            } else {
                std::fs::remove_file(&backup_path).or_else(|error| {
                    if error.kind() == io::ErrorKind::NotFound {
                        Ok(())
                    } else {
                        Err(error)
                    }
                })
            };
            if let Err(restore_error) = restore_result {
                anyhow::bail!(
                    "could not update {} ({config_error}); also could not restore its previous \
                     backup ({restore_error})",
                    config_path.display()
                );
            }
            return Err(config_error.into());
        }
    }
    Ok(guard.finish())
}

/// Build the new-shape setup result (#1140): a backend DROP-IN (one endpoint,
/// one file) + a minimal config whose `default_backend` points at it. No
/// legacy `[dgx]` block, no inline `[[backends]]` — the chimera is dead.
fn build_backend_pair(
    name: &str,
    endpoint: &str,
    model: &str,
    kind: BackendKind,
    serving: newt_core::Serving,
    api_key_env: Option<String>,
    source_note: &str,
) -> (Config, BackendConfig) {
    let backend = BackendConfig {
        name: name.to_string(),
        endpoint: endpoint.to_string(),
        // A hint, not authority — session start adopts served reality (#1139).
        model: Some(model.to_string()),
        tiers: vec![Tier::Fast, Tier::Standard, Tier::Complex, Tier::Review],
        kind: Some(kind),
        api_key_env,
        serving: Some(serving),
        provenance: Some(newt_core::config::BackendProvenance {
            source: Some(format!("newt setup v{} ({source_note})", crate::VERSION)),
            probed: Some(chrono::Local::now().format("%Y-%m-%d").to_string()),
            derived_serving: Some(true),
        }),
        ..Default::default()
    };
    let config = Config {
        backends: vec![], // the drop-in IS the backend list
        default_backend: Some(backend.name.clone()),
        ..Default::default()
    };
    (config, backend)
}

fn build_ollama_config(
    _base: Config,
    node_name: &str,
    kind: EndpointKind,
    url: &str,
    model: &str,
) -> (Config, BackendConfig) {
    build_backend_pair(
        node_name,
        url,
        model,
        BackendKind::Ollama,
        newt_core::Serving::Multiplexer,
        None,
        kind.as_str(),
    )
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

    /// Read the backend drop-in `<config dir>/backends/<name>.toml` the new
    /// writer (#1140) produces beside the config file.
    fn read_dropin(config_path: &std::path::Path, name: &str) -> BackendConfig {
        let p = config_path
            .with_file_name("backends")
            .join(format!("{name}.toml"));
        toml::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap()
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
            candidate_endpoints("dgx1.home.lab", &discovery).unwrap(),
            vec![
                "http://dgx1.home.lab:11434",
                "http://dgx1.home.lab:8000",
                "http://dgx1.home.lab:8080",
            ]
        );
        assert_eq!(
            candidate_endpoints("http://dgx1.home.lab:8080/v1", &discovery).unwrap(),
            vec!["http://dgx1.home.lab:8080"]
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
            candidate_endpoints("dgx1.home.lab", &discovery).unwrap(),
            vec!["http://dgx1.home.lab:8000", "http://dgx1.home.lab:8080",]
        );
        assert!(
            candidate_endpoints("http://user:secret@dgx1.home.lab:8000", &discovery)
                .unwrap_err()
                .to_string()
                .contains("credentials")
        );
    }

    #[test]
    fn authenticated_targets_require_an_explicit_secure_transport() {
        assert!(validate_authenticated_target("dgx1.home.lab:8000").is_err());
        assert!(validate_authenticated_target("http://dgx1.home.lab:8000").is_err());
        assert!(validate_authenticated_target("https://dgx1.home.lab:8000").is_ok());
        assert!(validate_authenticated_target("http://127.0.0.1:8000").is_ok());
        assert!(validate_authenticated_target("http://[::1]:8000").is_ok());
    }

    #[test]
    fn detected_backend_name_is_stable_and_filesystem_safe() {
        assert_eq!(
            backend_name("http://dgx1.home.lab:8000").unwrap(),
            "dgx1-home-lab-8000"
        );
        assert_eq!(
            backend_name("https://[2001:db8::1]:8080").unwrap(),
            "2001-db8-1-8080"
        );
    }

    fn openai_hit(
        endpoint: &str,
        models: &[&str],
    ) -> newt_core::backend_probe::EndpointProbeResult {
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
    fn detected_backend_carries_served_truth_and_secret_references_only() {
        let token_file = std::path::Path::new("~/.newt/tokens/dgx1");
        let backend = backend_from_probe(
            &openai_hit("http://dgx1.home.lab:8080", &["qwen3-coder", "gpt-oss"]),
            Some("DGX_TOKEN"),
            Some(token_file),
        )
        .unwrap();
        assert_eq!(backend.name, "dgx1-home-lab-8080");
        assert_eq!(backend.host.as_deref(), Some("dgx1.home.lab"));
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
            openai_hit("http://dgx1.home.lab:8000", &["ornith"]),
            openai_hit("http://dgx1.home.lab:8080", &["qwen3-coder", "gpt-oss"]),
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
            Some("dgx1-home-lab-8000")
        );
        let vllm = read_dropin(&path, "dgx1-home-lab-8000");
        let router = read_dropin(&path, "dgx1-home-lab-8080");
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
        let occupied = backend_dir.join("dgx1-home-lab-8000.toml");
        let hand_authored = concat!(
            "# operator-owned backend\n",
            "name = \"ignored-by-filename\"\n",
            "endpoint = \"http://dgx1-home-lab:8000\"\n",
            "model = \"hand-model\"\n",
            "tiers = [\"FAST\"]\n",
            "kind = \"openai\"\n",
        );
        std::fs::write(&occupied, hand_authored).unwrap();
        let hits = vec![openai_hit("http://dgx1.home.lab:8000", &["detected-model"])];

        let written = persist_detected_setup(&config_path, &hits, None, None).unwrap();

        assert_eq!(std::fs::read_to_string(&occupied).unwrap(), hand_authored);
        assert_eq!(written.len(), 1);
        assert_eq!(
            written[0].file_name().and_then(|name| name.to_str()),
            Some("dgx1-home-lab-8000-2.toml")
        );
        assert_eq!(
            Config::load(&config_path)
                .unwrap()
                .default_backend
                .as_deref(),
            Some("dgx1-home-lab-8000-2")
        );

        let first_bytes = std::fs::read(&written[0]).unwrap();
        let rerun = persist_detected_setup(&config_path, &hits, None, None).unwrap();
        assert!(rerun.is_empty(), "the collision alias should be reused");
        assert_eq!(std::fs::read(&written[0]).unwrap(), first_bytes);
        assert!(!backend_dir.join("dgx1-home-lab-8000-3.toml").exists());
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
            "endpoint = \"http://dgx1.home.lab:8080/\"\n",
            "model = \"operator-model\"\n",
            "tiers = [\"STANDARD\", \"REVIEW\"]\n",
            "kind = \"openai\"\n",
            "num_ctx = 32768\n",
        );
        std::fs::write(&existing, hand_authored).unwrap();
        let hits = vec![openai_hit(
            "http://dgx1.home.lab:8080",
            &["detected-model", "operator-model"],
        )];

        let written = persist_detected_setup(&config_path, &hits, None, None).unwrap();

        assert!(written.is_empty());
        assert_eq!(std::fs::read_to_string(&existing).unwrap(), hand_authored);
        assert!(!backend_dir.join("dgx1-home-lab-8080.toml").exists());
        assert_eq!(
            Config::load(&config_path)
                .unwrap()
                .default_backend
                .as_deref(),
            Some("operator-dgx")
        );
    }

    #[serial_test::serial(real_fs)]
    #[test]
    fn detected_setup_preserves_but_does_not_select_a_stale_operator_dropin() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let backend_dir = dir.path().join("backends");
        std::fs::create_dir_all(&backend_dir).unwrap();
        let existing = backend_dir.join("dgx1-home-lab-8080.toml");
        let hand_authored = concat!(
            "# preserve even when stale\n",
            "name = \"dgx1-home-lab-8080\"\n",
            "endpoint = \"http://dgx1.home.lab:8080\"\n",
            "model = \"retired-model\"\n",
            "tiers = [\"STANDARD\"]\n",
            "kind = \"openai\"\n",
        );
        std::fs::write(&existing, hand_authored).unwrap();
        let hits = vec![openai_hit("http://dgx1.home.lab:8080", &["current-model"])];

        let written = persist_detected_setup(&config_path, &hits, None, None).unwrap();

        assert_eq!(std::fs::read_to_string(existing).unwrap(), hand_authored);
        assert_eq!(written.len(), 1);
        assert_eq!(
            Config::load(&config_path)
                .unwrap()
                .default_backend
                .as_deref(),
            Some("dgx1-home-lab-8080-2")
        );
    }

    #[serial_test::serial(real_fs)]
    #[test]
    fn detected_setup_does_not_reuse_a_different_auth_reference() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let backend_dir = dir.path().join("backends");
        std::fs::create_dir_all(&backend_dir).unwrap();
        let existing = backend_dir.join("dgx1-home-lab-8000.toml");
        let body = concat!(
            "name = \"dgx1-home-lab-8000\"\n",
            "endpoint = \"http://dgx1.home.lab:8000\"\n",
            "model = \"model\"\n",
            "tiers = [\"FAST\"]\n",
            "kind = \"openai\"\n",
            "serving = \"instance\"\n",
            "api_key_env = \"UNRELATED_TOKEN\"\n",
        );
        std::fs::write(&existing, body).unwrap();
        let hits = vec![openai_hit("http://dgx1.home.lab:8000", &["model"])];

        let written = persist_detected_setup(&config_path, &hits, None, None).unwrap();

        assert_eq!(std::fs::read_to_string(existing).unwrap(), body);
        assert_eq!(written.len(), 1);
        assert_eq!(
            written[0].file_name().and_then(|name| name.to_str()),
            Some("dgx1-home-lab-8000-2.toml")
        );
    }

    #[serial_test::serial(real_fs)]
    #[test]
    fn detected_setup_does_not_reuse_stale_generated_served_truth() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let backend_dir = dir.path().join("backends");
        std::fs::create_dir_all(&backend_dir).unwrap();
        let existing = backend_dir.join("dgx1-home-lab-8000.toml");
        let body = concat!(
            "name = \"dgx1-home-lab-8000\"\n",
            "endpoint = \"http://dgx1.home.lab:8000\"\n",
            "model = \"old-model\"\n",
            "tiers = [\"FAST\"]\n",
            "kind = \"openai\"\n",
            "serving = \"instance\"\n",
            "\n[provenance]\n",
            "source = \"newt setup v0.7.2 (auto-detected Openai)\"\n",
        );
        std::fs::write(&existing, body).unwrap();
        let hits = vec![openai_hit("http://dgx1.home.lab:8000", &["new-model"])];

        let written = persist_detected_setup(&config_path, &hits, None, None).unwrap();

        assert_eq!(std::fs::read_to_string(existing).unwrap(), body);
        assert_eq!(written.len(), 1);
        assert_eq!(
            read_dropin(&config_path, "dgx1-home-lab-8000-2")
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
        let hits = vec![openai_hit("http://dgx1.home.lab:8000", &["model"])];

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
        let hits = vec![openai_hit("http://dgx1.home.lab:8000", &["model"])];

        persist_detected_setup(&config_path, &hits, None, None).unwrap();

        assert!(std::fs::symlink_metadata(&config_path)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(std::fs::read_to_string(&real_config)
            .unwrap()
            .contains("default_backend"));
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
            },
            PlannedSetupBackend {
                name: "second".into(),
                endpoint: "http://second:8000".into(),
                path: blocked_parent.join("second.toml"),
                body: Some(b"name = \"second\"\n".to_vec()),
            },
        ];

        assert!(
            commit_setup_plan(&config_path, "", "default_backend = \"first\"\n", &planned).is_err()
        );
        let leftovers = std::fs::read_dir(&backend_dir)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .collect::<Vec<_>>();
        assert!(leftovers.is_empty(), "leftover staged files: {leftovers:?}");
        assert!(!config_path.exists());
    }

    #[serial_test::serial(real_fs)]
    #[test]
    fn setup_lock_blocks_a_second_writer_and_can_be_reacquired() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");

        let first = acquire_setup_lock(&config_path).unwrap();
        let error = acquire_setup_lock(&config_path).unwrap_err();
        assert!(error.to_string().contains("another setup process"));
        drop(first);

        let reacquired = acquire_setup_lock(&config_path).unwrap();
        drop(reacquired);
        assert!(!dir.path().join(".config.toml.setup.lock").exists());
    }

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
        let router = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": "qwen"}, {"id": "gpt-oss"}]
            })))
            .mount(&router)
            .await;
        let discovery = newt_core::config::Discovery {
            hosts: vec![],
            ollama_ports: vec![],
            vllm_ports: vec![vllm.address().port(), router.address().port()],
        };
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let client = reqwest::Client::new();
        let mut console = ScriptedConsole::new(&[]);

        run_target_with(
            &mut console,
            &client,
            &config_path,
            TargetSetupRequest {
                target: "127.0.0.1",
                token_env: None,
                token_file: None,
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
        let mut console = ScriptedConsole::new(&[]);

        run_target_with(
            &mut console,
            &reqwest::Client::new(),
            &config_path,
            TargetSetupRequest {
                target: "127.0.0.1",
                token_env: None,
                token_file: None,
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
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let mut console = ScriptedConsole::new(&["n"]);

        run_target_with(
            &mut console,
            &reqwest::Client::new(),
            &config_path,
            TargetSetupRequest {
                target: &server.uri(),
                token_env: None,
                token_file: None,
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
        let mut console = ScriptedConsole::new(&[]);

        let error = run_target_with(
            &mut console,
            &reqwest::Client::new(),
            &dir.path().join("config.toml"),
            TargetSetupRequest {
                target: "127.0.0.1",
                token_env: None,
                token_file: Some(&token_path),
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
        let dir = tempfile::tempdir().unwrap();
        let token_path = dir.path().join("token");
        std::fs::write(&token_path, "secret-value\n").unwrap();
        let config_path = dir.path().join("config.toml");
        let client = reqwest::Client::new();
        let mut console = ScriptedConsole::new(&[]);

        run_target_with(
            &mut console,
            &client,
            &config_path,
            TargetSetupRequest {
                target: &server.uri(),
                token_env: None,
                token_file: Some(&token_path),
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
        let mut console = ScriptedConsole::new(&[]);

        let err = run_target_with(
            &mut console,
            &client,
            &config_path,
            TargetSetupRequest {
                target: &server.uri(),
                token_env: None,
                token_file: None,
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
        let mut console = ScriptedConsole::new(&[]);

        let error = run_target_with(
            &mut console,
            &client,
            &config_path,
            TargetSetupRequest {
                target: &server.uri(),
                token_env: None,
                token_file: None,
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
        let models = newt_core::backend_probe::fetch_openai_models_auth(
            &client,
            &server.uri(),
            Some("sekrit"),
        )
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
        let mut console = ScriptedConsole::new(&["1", &server.uri(), "2", "y"]);
        run_with(&mut console, &client, &path).await.unwrap();

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
                ResponseTemplate::new(401)
                    .set_body_json(serde_json::json!({"error": "Unauthorized"})),
            )
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");
        // Pin the config dir: the machine identity for blank-passphrase
        // encryption lives under it.
        let prev = std::env::var_os(newt_core::config::NEWT_CONFIG_DIR_ENV);
        std::env::set_var(newt_core::config::NEWT_CONFIG_DIR_ENV, dir.path());
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
        let mut console = ScriptedConsole::new(&["bad-key", "", "1", "Y", "good-key", ""]);
        let result =
            configure_preset(&mut console, &reqwest::Client::new(), &preset, &cfg_path).await;
        match prev {
            Some(v) => std::env::set_var(newt_core::config::NEWT_CONFIG_DIR_ENV, v),
            None => std::env::remove_var(newt_core::config::NEWT_CONFIG_DIR_ENV),
        }
        let (_cfg, backend) = result.unwrap();
        let t = console.transcript();
        assert!(t.contains("✗ key rejected (HTTP 401)"), "{t}");
        assert!(t.contains("✓ key accepted"), "{t}");
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
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");
        let client = reqwest::Client::new();
        let host2_name = format!("127-0-0-1-{}", s2.address().port());
        // ollama door → write → add another → custom host door → write →
        // stop → pick backend 2 as the default.
        let mut console = ScriptedConsole::new(&[
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
        run_with(&mut console, &client, &cfg_path).await.unwrap();

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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let client = reqwest::Client::new();
        // custom host=2, host=<mock url>, endpoint=1, model=1, write=Y
        let mut console = ScriptedConsole::new(&["2", &server.uri(), "1", "1", "y"]);
        run_with(&mut console, &client, &path).await.unwrap();

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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let client = reqwest::Client::new();
        // custom host=2, host, endpoint=1, model=<Enter> (default = warm), write=Y
        let mut console = ScriptedConsole::new(&["2", &server.uri(), "1", "", "y"]);
        run_with(&mut console, &client, &path).await.unwrap();

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
        let mut console = ScriptedConsole::new(&[
            "1",
            "http://127.0.0.1:1", // connection refused
            "phi3:mini",
            "y",
        ]);
        run_with(&mut console, &client, &path).await.unwrap();
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
    fn select_preset_lists_available_and_notes_unavailable_rows() {
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
        let mut console = ScriptedConsole::new(&["4"]);
        let picked = select_preset(&mut console, &presets).unwrap();
        assert_eq!(picked.name, "openrouter");
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
    fn select_row_filter_maps_back_to_original_indices() {
        let rows: Vec<String> = (1..=12).map(|i| format!("row-{i}")).collect();
        // Filter to "row-1" matches row-1, row-10..12; pick 2 → "row-10"
        // (original index 9) — the picker must return ORIGINAL indices.
        let mut console = ScriptedConsole::new(&["row-1", "2"]);
        let idx = select_row(&mut console, &rows, "rows").unwrap();
        assert_eq!(idx, 9);
    }

    // --- custom-host / preset integration tests ------------------------------

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

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        // Pin the config dir: the machine identity for blank-passphrase
        // encryption lives under it.
        let prev = std::env::var_os(newt_core::config::NEWT_CONFIG_DIR_ENV);
        std::env::set_var(newt_core::config::NEWT_CONFIG_DIR_ENV, dir.path());
        newt_core::secrets::session().reset_for_test();
        let client = reqwest::Client::new();

        let server_with_v1 = format!("{}/v1", server.uri());
        // custom host=2, host (with /v1 — stripped), key (hidden), endpoint=1,
        // model=1, passphrase=<Enter: machine key>, write=Y
        let mut console =
            ScriptedConsole::new(&["2", &server_with_v1, "test-remote-key", "1", "1", "", "y"]);
        run_with(&mut console, &client, &path).await.unwrap();

        let name = format!("127-0-0-1-{}", server.address().port());
        let dropin = read_dropin(&path, &name);
        assert_eq!(dropin.effective_model(), Some("example/model-a"));
        assert!(!dropin.endpoint.ends_with("/v1"), "probe suffix stripped");
        let token_ref = dropin.api_key_file.as_deref().expect("key recorded");
        assert!(
            token_ref.ends_with(".token.age"),
            "encrypted ref: {token_ref}"
        );
        let token_path = path
            .with_file_name("backends")
            .join(format!("{name}.token.age"));
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
        match prev {
            Some(v) => std::env::set_var(newt_core::config::NEWT_CONFIG_DIR_ENV, v),
            None => std::env::remove_var(newt_core::config::NEWT_CONFIG_DIR_ENV),
        }
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
        std::env::remove_var("NEWT_TEST_PRESET_KEY");
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
        let mut console = ScriptedConsole::new(&["", "1"]);
        let (_cfg, backend) = configure_preset(&mut console, &client, &preset, &path)
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
        std::env::remove_var("NEWT_TEST_PRESET_KEY");
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
        let prev = std::env::var_os(newt_core::config::NEWT_CONFIG_DIR_ENV);
        std::env::set_var(newt_core::config::NEWT_CONFIG_DIR_ENV, dir.path());
        newt_core::secrets::session().reset_for_test();
        let client = reqwest::Client::new();
        // key (hidden), passphrase, model=1
        let mut console = ScriptedConsole::new(&["sk-preset-secret", "open sesame", "1"]);
        let (_cfg, backend) = configure_preset(&mut console, &client, &preset, &path)
            .await
            .unwrap();
        assert!(backend.api_key_env.is_none());
        let token_ref = backend.api_key_file.as_deref().expect("encrypted ref");
        assert!(token_ref.ends_with("gatedcloud.token.age"));
        assert_eq!(backend.effective_model(), Some("gated-model"));
        let body =
            std::fs::read_to_string(dir.path().join("backends/gatedcloud.token.age")).unwrap();
        assert!(body.starts_with("-----BEGIN AGE ENCRYPTED FILE-----"));
        assert!(!body.contains("sk-preset-secret"));
        assert!(!console.transcript().contains("sk-preset-secret"));

        newt_core::secrets::session().reset_for_test();
        match prev {
            Some(v) => std::env::set_var(newt_core::config::NEWT_CONFIG_DIR_ENV, v),
            None => std::env::remove_var(newt_core::config::NEWT_CONFIG_DIR_ENV),
        }
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
        std::env::set_var("NEWT_TEST_PRESET_KEY", "sk-from-env");
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
        // use exported?=<Enter: yes>, model=1
        let mut console = ScriptedConsole::new(&["", "1"]);
        let (_cfg, backend) = configure_preset(&mut console, &client, &preset, &path)
            .await
            .unwrap();
        std::env::remove_var("NEWT_TEST_PRESET_KEY");
        assert_eq!(backend.api_key_env.as_deref(), Some("NEWT_TEST_PRESET_KEY"));
        assert!(backend.api_key_file.is_none(), "env reference only");
        assert_eq!(backend.effective_model(), Some("env-model"));
    }

    /// Regression heir: the old plaintext writer used `var_os("HOME")?`, so
    /// on Windows — where the variable is `USERPROFILE` — the `?` bailed and
    /// the key went unrecorded. The encrypted writer must likewise record a
    /// usable (absolute) reference even with no home to collapse against.
    #[test]
    #[serial_test::serial(real_fs)]
    fn a_token_reference_is_recorded_even_when_home_is_unset() {
        let saved = (std::env::var_os("HOME"), std::env::var_os("USERPROFILE"));
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var_os(newt_core::config::NEWT_CONFIG_DIR_ENV);
        // The machine identity needs a config root even with HOME unset.
        std::env::set_var(newt_core::config::NEWT_CONFIG_DIR_ENV, dir.path());
        newt_core::secrets::session().reset_for_test();
        // SAFETY: guarded by the `real_fs` serial lane, and restored below.
        unsafe {
            std::env::remove_var("HOME");
            std::env::remove_var("USERPROFILE");
        }

        let path = dir.path().join("config.toml");
        // passphrase=<Enter: machine key>
        let mut console = ScriptedConsole::new(&[""]);
        let recorded = persist_wizard_token(&mut console, &path, "example", "a-secret")
            .expect("a supplied key must always be recorded, home dir or not");

        // SAFETY: same lane; restore before asserting so a failure cannot leak.
        unsafe {
            if let Some(v) = saved.0 {
                std::env::set_var("HOME", v);
            }
            if let Some(v) = saved.1 {
                std::env::set_var("USERPROFILE", v);
            }
        }

        assert!(
            !recorded.starts_with('~'),
            "with no home to collapse against, the path stays absolute: {recorded}"
        );
        assert!(recorded.ends_with("example.token.age"));
        let body = std::fs::read_to_string(&recorded).unwrap();
        assert!(body.starts_with("-----BEGIN AGE ENCRYPTED FILE-----"));
        assert!(!body.contains("a-secret"), "never plaintext on disk");

        newt_core::secrets::session().reset_for_test();
        match prev {
            Some(v) => std::env::set_var(newt_core::config::NEWT_CONFIG_DIR_ENV, v),
            None => std::env::remove_var(newt_core::config::NEWT_CONFIG_DIR_ENV),
        }
    }

    // --- model selector (#1452): a llama.cpp router serves 30+ models, so the
    // operator must never have to type an id exactly. ---

    #[test]
    fn a_short_list_is_shown_directly_with_no_filter_prompt() {
        let models: Vec<String> = (1..=3).map(|i| format!("model-{i}")).collect();
        let mut console = ScriptedConsole::new(&["2"]);
        assert_eq!(select_model(&mut console, &models).unwrap(), "model-2");
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
        let mut console = ScriptedConsole::new(&["qwen", "2"]);
        assert_eq!(
            select_model(&mut console, &models).unwrap(),
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
        let mut console = ScriptedConsole::new(&["CODER", "1"]);
        assert_eq!(select_model(&mut console, &models).unwrap(), "Qwen3-Coder");
    }

    /// A filter that matches nothing must not dead-end the operator in an empty
    /// menu — it falls back to the whole list.
    #[test]
    fn a_filter_matching_nothing_falls_back_to_the_full_list() {
        let models: Vec<String> = (1..=20).map(|i| format!("model-{i}")).collect();
        let mut console = ScriptedConsole::new(&["zzz-no-such-model", "3"]);
        assert_eq!(select_model(&mut console, &models).unwrap(), "model-3");
        assert!(console.transcript().contains("showing all"));
    }

    #[test]
    fn a_blank_filter_shows_everything() {
        let models: Vec<String> = (1..=15).map(|i| format!("model-{i}")).collect();
        let mut console = ScriptedConsole::new(&["", "15"]);
        assert_eq!(select_model(&mut console, &models).unwrap(), "model-15");
    }

    /// An out-of-range or unparseable choice takes the first entry rather than
    /// erroring out mid-setup.
    #[test]
    fn an_invalid_choice_falls_back_to_the_first_entry() {
        let models: Vec<String> = vec!["a".into(), "b".into()];
        for answer in ["", "99", "nonsense", "0", "-1"] {
            let mut console = ScriptedConsole::new(&[answer]);
            assert_eq!(select_model(&mut console, &models).unwrap(), "a");
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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let client = reqwest::Client::new();
        // custom host, empty host (reprompt), then real host, endpoint=1, model=1, write=Y
        let mut console = ScriptedConsole::new(&["2", "", &server.uri(), "1", "1", "y"]);
        run_with(&mut console, &client, &path).await.unwrap();
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
}
