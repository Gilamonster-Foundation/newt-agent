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
use newt_core::backend_probe::{EndpointProbeResult, GenerationCheck};
use newt_core::config::Discovery;
use newt_core::provider_preset::{
    self, list_models_for_preset, preset_support,
    validate_authenticated_url as validate_authenticated_target, PresetSupport, ProviderPreset,
};
use newt_core::{BackendConfig, BackendKind, Config, EndpointKind, OpenAiApi, Tier};
use std::collections::HashSet;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};

const SETUP_HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const GENERATION_CHECK_ATTEMPTS: usize = 3;

fn setup_http_client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(SETUP_HTTP_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

// Model: GPT-5 | Harness: Codex | Operator: Shawn Hartsock | Time: 13:18 EDT | Date: 2026-08-12

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
    let client = setup_http_client()?;
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
    model: Option<&str>,
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
    let client = setup_http_client()?;
    let mut console = StdinConsole;
    run_target_with(
        &mut console,
        &client,
        &config_path,
        TargetSetupRequest {
            target,
            token_env,
            token_file,
            model,
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
    model: Option<&'a str>,
    yes: bool,
}

async fn run_target_with(
    console: &mut dyn Console,
    client: &reqwest::Client,
    config_path: &Path,
    request: TargetSetupRequest<'_>,
    discovery: &Discovery,
) -> anyhow::Result<()> {
    run_target_with_persist(
        console,
        client,
        config_path,
        request,
        discovery,
        |config_path, hits, token_env, token_file| {
            persist_verified_setup(config_path, hits, token_env, token_file)
        },
    )
    .await
}

async fn run_target_with_persist(
    console: &mut dyn Console,
    client: &reqwest::Client,
    config_path: &Path,
    request: TargetSetupRequest<'_>,
    discovery: &Discovery,
    persist: impl FnOnce(
        &Path,
        &[VerifiedTargetHit],
        Option<&str>,
        Option<&Path>,
    ) -> anyhow::Result<Vec<PathBuf>>,
) -> anyhow::Result<()> {
    let TargetSetupRequest {
        target,
        token_env,
        token_file,
        model,
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

    let (hits, mut failures) =
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

    let (hits, generation_failures) =
        verify_target_hits(console, client, hits, model, api_key.as_deref()).await;
    failures.extend(generation_failures);
    if hits.is_empty() {
        for failure in failures {
            console.say(&format!("  {failure}"));
        }
        anyhow::bail!("no inference backend passed a minimal generation check for `{target}`");
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
        let backend = backend_from_verified_probe(hit, token_env, token_file.as_deref())?;
        console.say(&format!(
            "  {} ({:?}, {}, {} model{})",
            backend.name,
            backend.kind,
            backend.endpoint,
            hit.probe.models.len(),
            if hit.probe.models.len() == 1 { "" } else { "s" }
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

    let written = persist(config_path, &hits, token_env, token_file.as_deref())?;
    for path in &written {
        console.say(&format!("Wrote {}.", path.display()));
    }
    console.say(&format!(
        "Configuration ready at {}.",
        config_path.display()
    ));
    Ok(())
}

#[derive(Debug, Clone)]
struct VerifiedTargetHit {
    probe: EndpointProbeResult,
    /// The surface that answered the setup generation. Chat stays unpinned in
    /// newly written config (runtime still owns the tool-capability probe), but
    /// retaining it here lets a rerun recognize a runtime-probed drop-in as the
    /// same backend instead of allocating `-2`, `-3`, ... forever.
    api: Option<OpenAiApi>,
}

async fn verify_target_hits(
    console: &mut dyn Console,
    client: &reqwest::Client,
    hits: Vec<EndpointProbeResult>,
    requested_model: Option<&str>,
    api_key: Option<&str>,
) -> (Vec<VerifiedTargetHit>, Vec<String>) {
    let supplied_key = api_key.is_some_and(|key| !key.trim().is_empty());
    let mut verified = Vec::with_capacity(hits.len());
    let mut failures = Vec::new();
    for mut hit in hits {
        let Some(model) = requested_model
            .map(str::to_string)
            .or_else(|| hit.warm.first().or(hit.models.first()).cloned())
        else {
            failures.push(format!(
                "{} listed no model to generation-test",
                hit.endpoint
            ));
            continue;
        };
        console.say(&format!(
            "Testing a minimal generation at {} with {model}…",
            hit.endpoint
        ));
        match newt_core::backend_probe::verify_generation(
            client,
            hit.kind,
            None,
            &hit.endpoint,
            &model,
            api_key,
        )
        .await
        {
            GenerationCheck::Accepted(api) => {
                console.say("  ✓ generation accepted");
                hit.models = vec![model.clone()];
                hit.warm = vec![model];
                verified.push(VerifiedTargetHit { probe: hit, api });
            }
            GenerationCheck::Rejected(code) if supplied_key => failures.push(format!(
                "{} rejected the token or model authorization for {model} (HTTP {code}); check --token-file/--token-env and model access",
                hit.endpoint
            )),
            GenerationCheck::Rejected(code) => failures.push(format!(
                "{} requires authentication or model authorization for {model} (HTTP {code}); supply --token-file or --token-env",
                hit.endpoint
            )),
            GenerationCheck::Unverified(reason) => failures.push(format!(
                "{} could not generate with {model}: {reason}",
                hit.endpoint
            )),
        }
    }
    (verified, failures)
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

struct PendingWizardToken {
    token: String,
    passphrase: Option<newt_core::secrets::SecretString>,
    path: PathBuf,
    reference: String,
}

fn persist_interactive_backend(
    console: &mut dyn Console,
    config_path: &Path,
    cfg: &Config,
    backend: &BackendConfig,
    pending_token: Option<&PendingWizardToken>,
) -> anyhow::Result<PathBuf> {
    persist_interactive_backend_with(
        console,
        config_path,
        cfg,
        backend,
        pending_token,
        |staged, destination| destination.durable_replace(staged),
        |staged, destination| {
            destination
                .durable_replace(staged)
                .map_err(anyhow::Error::from)
        },
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SetupCommitStep {
    PersistVersionedToken,
    PublishBackendTuple,
    SelectBackend,
}

const SETUP_COMMIT_STEPS: [SetupCommitStep; 3] = [
    SetupCommitStep::PersistVersionedToken,
    SetupCommitStep::PublishBackendTuple,
    SetupCommitStep::SelectBackend,
];

/// One ordering choke point for the setup transaction.  The injected operation
/// keeps failpoint tests filesystem-free while production supplies the durable
/// file actions.  Publishing the backend only after its immutable credential
/// exists is the coherence invariant.
fn run_setup_commit(
    mut apply: impl FnMut(SetupCommitStep) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    for step in SETUP_COMMIT_STEPS {
        apply(step)?;
    }
    Ok(())
}

fn persist_interactive_backend_with(
    console: &mut dyn Console,
    config_path: &Path,
    cfg: &Config,
    backend: &BackendConfig,
    pending_token: Option<&PendingWizardToken>,
    publish_backend: impl FnOnce(
        &Path,
        &newt_core::atomic_fs::ResolvedPath,
    ) -> Result<(), newt_core::atomic_fs::DurableReplaceError>,
    commit_config: impl FnOnce(&Path, &newt_core::atomic_fs::ResolvedPath) -> anyhow::Result<()>,
) -> anyhow::Result<PathBuf> {
    if let Some(parent) = config_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let setup_lock = acquire_setup_lock(config_path)?;

    let logical_backend_path = config_path
        .with_file_name("backends")
        .join(format!("{}.toml", backend.name));
    let backend_destination = setup_config_destination(&logical_backend_path)?;
    let config_destination = &setup_lock.destination;
    let token_destination = pending_token
        .map(|pending| setup_config_destination(&pending.path))
        .transpose()?;
    let token_reference = pending_token.map(|pending| pending.reference.clone());
    if let Some(reference) = token_reference.as_deref() {
        if backend.api_key_file.as_deref() != Some(reference) {
            anyhow::bail!("internal token-reference mismatch for {}", backend.name);
        }
    }

    let backend_body = toml::to_string(backend)?;
    let config_body = toml::to_string_pretty(cfg)?;
    let mut backend_published = false;
    let result = (|| {
        let mut guard = SetupCommitGuard::default();
        let backend_permissions = setup_file_permissions(backend_destination.as_path())?;
        let backend_stage = guard.stage(
            &backend_destination,
            backend_body.as_bytes(),
            backend_permissions.as_ref(),
        )?;
        let config_permissions = setup_file_permissions(config_destination.as_path())?;
        let config_stage = guard.stage(
            config_destination,
            config_body.as_bytes(),
            config_permissions.as_ref(),
        )?;
        let mut publish_backend = Some(publish_backend);
        let mut commit_config = Some(commit_config);
        run_setup_commit(|step| match step {
            SetupCommitStep::PersistVersionedToken => {
                if let (Some(pending), Some(destination)) =
                    (pending_token, token_destination.as_ref())
                {
                    match std::fs::symlink_metadata(destination.as_path()) {
                        Ok(_) => anyhow::bail!(
                            "allocated credential path {} already exists; retry setup",
                            destination.as_path().display()
                        ),
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.into()),
                    }
                    newt_core::secrets::store_token_at_resolved(
                        destination,
                        &pending.token,
                        pending.passphrase.as_ref(),
                    )
                    .map_err(|error| anyhow::anyhow!(error))?;
                    guard.created.push(destination.as_path().to_path_buf());
                    let resolved = newt_core::secrets::resolve_token_file(destination.as_path())
                        .map_err(|error| anyhow::anyhow!(error))?;
                    if resolved.as_deref() != pending_token.map(|pending| pending.token.as_str()) {
                        anyhow::bail!("stored token verification failed for {}", backend.name);
                    }
                }
                Ok(())
            }
            SetupCommitStep::PublishBackendTuple => {
                let publication = publish_backend
                    .take()
                    .expect("setup backend publication is called exactly once")(
                    &backend_stage,
                    &backend_destination,
                );
                if let Err(error) = publication {
                    if error.committed() {
                        guard
                            .created
                            .push(backend_destination.as_path().to_path_buf());
                        guard.retain_created();
                        backend_published = true;
                    }
                    return Err(error.into());
                }
                guard
                    .created
                    .push(backend_destination.as_path().to_path_buf());
                // From here forward the immutable token/backend tuple is a
                // valid commit that lock-free readers may retain. Never roll it
                // back or delete its credential if later config selection
                // fails; rerunning setup safely completes the final phase.
                guard.retain_created();
                backend_published = true;
                Ok(())
            }
            SetupCommitStep::SelectBackend => {
                // Replacing the backend above is the coherent
                // endpoint/credential commit. Config only selects that
                // already-complete tuple.
                commit_config
                    .take()
                    .expect("setup config commit is called exactly once")(
                    &config_stage,
                    config_destination,
                )?;
                guard
                    .created
                    .push(config_destination.as_path().to_path_buf());
                Ok(())
            }
        })?;
        let _ = guard.finish();
        Ok(())
    })();
    if let Err(error) = result {
        if backend_published {
            anyhow::bail!(
                "backend {} and its credential were published coherently, but setup could not \
                 finish updating {} ({error:#}); re-run setup to finish",
                backend.name,
                config_path.display()
            );
        }
        return Err(error);
    }
    if let Some(reference) = token_reference {
        console.say(&format!("  → stored encrypted at {reference}"));
    }
    Ok(logical_backend_path)
}

type ConfiguredBackend = (Config, BackendConfig, Option<PendingWizardToken>);

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
    // the end (until then, each committed setup round selects its backend).
    let mut written: Vec<String> = Vec::new();
    loop {
        let (cfg, backend, pending_token) = match choose_backend(console)? {
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
            let dropin = persist_interactive_backend(
                console,
                config_path,
                &cfg,
                &backend,
                pending_token.as_ref(),
            )?;
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
        persist_default_backend(config_path, chosen)?;
        console.say(&format!(
            "Default backend: {chosen} (/backends switches per session)."
        ));
    }

    console.say("Edit those files (or re-run `newt setup`) to change anything.");
    offer_identity(console);
    Ok(())
}

fn persist_default_backend(config_path: &Path, chosen: &str) -> anyhow::Result<()> {
    let setup_lock = acquire_setup_lock(config_path)?;
    let destination = &setup_lock.destination;
    let old_text = read_setup_config(destination.as_path())?;
    let new_text = Config::with_default_backend(&old_text, chosen)?;
    if new_text == old_text {
        return Ok(());
    }
    let permissions = setup_file_permissions(destination.as_path())?;
    let staged = stage_setup_file(destination, new_text.as_bytes(), permissions.as_ref())?;
    if let Err(error) = destination.durable_replace(&staged) {
        let _ = std::fs::remove_file(staged);
        return Err(error.into());
    }
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
) -> anyhow::Result<ConfiguredBackend> {
    let default_url = "http://127.0.0.1:11434";
    let raw = console.ask(&format!("Ollama host [{default_url}]: "))?;
    let url = normalize_url(
        if raw.is_empty() { default_url } else { &raw },
        "http",
        11434,
    );

    let model = pick_model(console, client, &url).await?;
    let (config, backend) = build_ollama_config(
        Config::default(),
        "default",
        EndpointKind::Ollama,
        &url,
        &model,
    );
    Ok((config, backend, None))
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
) -> anyhow::Result<ConfiguredBackend> {
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
            console.say("  2) Enter endpoint and model by hand (generation-tested)");
            console.say("  3) Cancel setup");
            let ans = console.ask("Choose [1]: ")?;
            match parse_choice(&ans, 3).unwrap_or(1) {
                2 => return manual_backend_entry(console, client, config_path).await,
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

        let (api_key, api) =
            verify_custom_chat_with_retries(console, client, hit, &model, api_key).await?;

        let mut backend = backend_from_probe(hit, None, None)?;
        backend.model = Some(model);
        if backend.serving == Some(newt_core::Serving::Instance) {
            backend.api = api;
        }
        let pending_token = if let Some(key) = api_key {
            let pending = collect_wizard_token(console, &key, config_path, &backend.name)?;
            backend.api_key_file = Some(pending.reference.clone());
            Some(pending)
        } else {
            None
        };
        let config = Config {
            backends: vec![], // the drop-in IS the backend list
            default_backend: Some(backend.name.clone()),
            ..Default::default()
        };
        return Ok((config, backend, pending_token));
    }
}

async fn verify_custom_chat_with_retries(
    console: &mut dyn Console,
    client: &reqwest::Client,
    hit: &EndpointProbeResult,
    model: &str,
    mut api_key: Option<String>,
) -> anyhow::Result<(Option<String>, Option<newt_core::config::OpenAiApi>)> {
    for attempt in 0..GENERATION_CHECK_ATTEMPTS {
        if api_key.as_deref().is_some_and(|key| !key.trim().is_empty()) {
            validate_authenticated_target(&hit.endpoint)?;
        }
        console.say(&format!(
            "Testing a minimal generation against {}…",
            hit.endpoint
        ));
        match newt_core::backend_probe::verify_generation(
            client,
            hit.kind,
            None,
            &hit.endpoint,
            model,
            api_key.as_deref(),
        )
        .await
        {
            GenerationCheck::Accepted(api) => {
                console.say("  ✓ generation accepted");
                // A tool-free chat proves generation/authentication, but it
                // cannot prove that agent tool calls work on Chat Completions.
                // Leave Chat unpinned so the runtime tool-capability probe can
                // choose; Responses is safe to persist after definitive
                // Chat-unavailable fallback.
                let api = api.filter(|surface| *surface == newt_core::config::OpenAiApi::Responses);
                return Ok((api_key, api));
            }
            GenerationCheck::Rejected(code) => {
                console.say(&format!("  ✗ authentication rejected (HTTP {code})"));
                if attempt + 1 == GENERATION_CHECK_ATTEMPTS {
                    break;
                }
                validate_authenticated_target(&hit.endpoint)?;
                let key = console.ask_secret("API key (echoes as *, Enter to cancel): ")?;
                let key = key.trim().to_string();
                if key.is_empty() {
                    anyhow::bail!("setup cancelled after authentication rejection");
                }
                api_key = Some(key);
            }
            GenerationCheck::Unverified(reason) => {
                console.say(&format!("  Could not verify generation ({reason})."));
                anyhow::bail!("setup cancelled because generation verification did not pass");
            }
        }
    }
    anyhow::bail!("authentication was rejected {GENERATION_CHECK_ATTEMPTS} times")
}

/// Nothing answered but the operator knows the endpoint: collect the wire and
/// model, then require a real generation before returning a writable backend.
async fn manual_backend_entry(
    console: &mut dyn Console,
    client: &reqwest::Client,
    config_path: &Path,
) -> anyhow::Result<ConfiguredBackend> {
    let url = loop {
        let raw = console.ask("Endpoint URL: ")?;
        if !raw.trim().is_empty() {
            let normalized = normalize_url(raw.trim(), "http", 11434);
            break candidate_endpoints(&normalized, &Discovery::default())?
                .into_iter()
                .next()
                .expect("an explicit URL produces one candidate");
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
    let hit = EndpointProbeResult {
        endpoint: url.clone(),
        kind,
        models: vec![model.clone()],
        serving,
        engine: None,
        warm: vec![],
    };
    let (api_key, api) =
        verify_custom_chat_with_retries(console, client, &hit, &model, None).await?;
    let (config, mut backend) =
        build_backend_pair(&name, &url, &model, kind, serving, None, "manual");
    let pending_token = if let Some(key) = api_key {
        let pending = collect_wizard_token(console, &key, config_path, &backend.name)?;
        backend.api_key_file = Some(pending.reference.clone());
        Some(pending)
    } else {
        None
    };
    if backend.serving == Some(newt_core::Serving::Instance) {
        backend.api = api;
    }
    Ok((config, backend, pending_token))
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
) -> anyhow::Result<ConfiguredBackend> {
    let presets = provider_preset::resolve_presets(None);
    match select_hosted_provider(console, &presets)? {
        HostedProviderChoice::Preset(preset) => {
            configure_preset(console, client, &preset, config_path).await
        }
        HostedProviderChoice::CustomEndpoint => {
            configure_custom_host(console, client, config_path).await
        }
    }
}

/// Filterable roster picker. Unsupported presets (oauth-auth drop-ins,
/// bedrock modes, unroutable base URLs) are listed as "(unavailable: …)"
/// notes with the reason — visible, never numbered, never silently dropped.
#[derive(Debug, Clone, PartialEq)]
enum HostedProviderChoice {
    CustomEndpoint,
    Preset(Box<ProviderPreset>),
}

fn select_hosted_provider(
    console: &mut dyn Console,
    presets: &[ProviderPreset],
) -> anyhow::Result<HostedProviderChoice> {
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
        console.say("\nAvailable providers:");
        console.say("  0) I have a URL (custom endpoint)");
        let _ = console.ask("Choose [0]: ")?;
        return Ok(HostedProviderChoice::CustomEndpoint);
    }
    let rows: Vec<String> = available
        .iter()
        .map(|(p, endpoint)| format!("{:<24}{}", p.label(), endpoint))
        .collect();
    match select_row_with_zero(
        console,
        &rows,
        "providers",
        "I have a URL (custom endpoint)",
    )? {
        Some(idx) => Ok(HostedProviderChoice::Preset(Box::new(
            available[idx].0.clone(),
        ))),
        None => Ok(HostedProviderChoice::CustomEndpoint),
    }
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
) -> anyhow::Result<ConfiguredBackend> {
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

    let cred = if preset.env_vars.is_empty() {
        WizardCred {
            api_key_env: None,
            api_key_file: None,
            probe_key: None,
            pending_token: None,
        }
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
                WizardCred {
                    api_key_env: Some(var),
                    api_key_file: None,
                    probe_key: Some(value),
                    pending_token: None,
                }
            } else {
                preset_pasted_key(console, preset, config_path)?
            }
        } else {
            preset_pasted_key(console, preset, config_path)?
        }
    };

    if cred.probe_key.is_some() {
        let PresetSupport::Supported { endpoint, .. } = preset_support(preset) else {
            anyhow::bail!("preset {} is not usable on this build", preset.name);
        };
        validate_authenticated_target(&endpoint)?;
    }

    console.say(&format!(
        "Probing {} for available models…",
        preset.base_url
    ));
    let models = list_models_for_preset(client, preset, cred.probe_key.as_deref()).await;
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
    let (cred, verified_api) =
        verify_key_with_retries(console, client, preset, config_path, &model, cred).await?;

    let mut backend = provider_preset::backend_from_preset(
        preset,
        &model,
        cred.api_key_env,
        cred.api_key_file,
        crate::VERSION,
    )
    .map_err(|reason| anyhow::anyhow!("preset {} is not usable: {reason}", preset.name))?;
    if verified_api.is_some() && backend.serving == Some(newt_core::Serving::Instance) {
        backend.api = verified_api;
    }
    let config = Config {
        backends: vec![], // the drop-in IS the backend list
        default_backend: Some(backend.name.clone()),
        ..Default::default()
    };
    Ok((config, backend, cred.pending_token))
}

struct WizardCred {
    api_key_env: Option<String>,
    api_key_file: Option<String>,
    probe_key: Option<String>,
    pending_token: Option<PendingWizardToken>,
}

/// Live-test the selected model before anything is written, with up to two
/// credential re-entries on 401/403. A public model catalog is never treated
/// as authentication evidence.
async fn verify_key_with_retries(
    console: &mut dyn Console,
    client: &reqwest::Client,
    preset: &ProviderPreset,
    config_path: &Path,
    model: &str,
    mut cred: WizardCred,
) -> anyhow::Result<(WizardCred, Option<newt_core::config::OpenAiApi>)> {
    let provider_preset::PresetSupport::Supported {
        kind,
        api,
        endpoint,
    } = provider_preset::preset_support(preset)
    else {
        anyhow::bail!("preset {} is not usable on this build", preset.name);
    };
    for attempt in 0..GENERATION_CHECK_ATTEMPTS {
        if cred
            .probe_key
            .as_deref()
            .is_some_and(|key| !key.trim().is_empty())
        {
            validate_authenticated_target(&endpoint)?;
        }
        console.say(&format!(
            "Testing a minimal generation against {}…",
            preset.base_url
        ));
        match newt_core::backend_probe::verify_generation(
            client,
            kind,
            api,
            &endpoint,
            model,
            cred.probe_key.as_deref(),
        )
        .await
        {
            GenerationCheck::Accepted(api) => {
                console.say("  ✓ generation accepted");
                let api = api.filter(|surface| *surface == newt_core::config::OpenAiApi::Responses);
                return Ok((cred, api));
            }
            GenerationCheck::Rejected(code) => {
                console.say(&format!("  ✗ authentication rejected (HTTP {code})"));
                if attempt + 1 == GENERATION_CHECK_ATTEMPTS {
                    break;
                }
                let ans = console.ask("Re-enter the key? [Y/n] ")?;
                if !is_yes(&ans, true) {
                    anyhow::bail!("setup cancelled after authentication rejection");
                }
                cred = preset_pasted_key(console, preset, config_path)?;
            }
            GenerationCheck::Unverified(reason) => {
                anyhow::bail!("minimal generation verification failed: {reason}");
            }
        }
    }
    anyhow::bail!("authentication was rejected {GENERATION_CHECK_ATTEMPTS} times")
}

/// The paste path shared by both preset branches: hidden input; Enter skips
/// (env reference recorded, nothing stored); a pasted token goes through the
/// encrypted store. Returns (api_key_env, api_key_file, probe key).
#[allow(clippy::type_complexity)]
fn preset_pasted_key(
    console: &mut dyn Console,
    preset: &ProviderPreset,
    config_path: &Path,
) -> anyhow::Result<WizardCred> {
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
        return Ok(WizardCred {
            api_key_env: Some(var),
            api_key_file: None,
            probe_key: None,
            pending_token: None,
        });
    }
    let pending = collect_wizard_token(console, &key, config_path, &preset.name)?;
    let reference = pending.reference.clone();
    Ok(WizardCred {
        api_key_env: None,
        api_key_file: Some(reference),
        probe_key: Some(key),
        pending_token: Some(pending),
    })
}

fn collect_wizard_token(
    console: &mut dyn Console,
    token: &str,
    config_path: &Path,
    name: &str,
) -> anyhow::Result<PendingWizardToken> {
    console.say("Protect the stored key with a passphrase? Enter uses a machine-local key.");
    let pass = console.ask_secret("Passphrase (echoes as *): ")?;
    let passphrase = {
        let trimmed = pass.trim();
        (!trimmed.is_empty()).then(|| newt_core::secrets::SecretString::from(trimmed.to_string()))
    };
    let path = versioned_wizard_token_path(config_path, name)?;
    let reference = collapse_home(&path);
    Ok(PendingWizardToken {
        token: token.to_string(),
        passphrase,
        path,
        reference,
    })
}

fn versioned_wizard_token_path(config_path: &Path, name: &str) -> anyhow::Result<PathBuf> {
    Ok(config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.join("backends"))
        .ok_or_else(|| anyhow::anyhow!("config path has no parent directory"))?
        .join(format!(
            "{name}.token.{}.age",
            newt_core::atomic_fs::unique_suffix()
        )))
}

#[cfg(test)]
fn persist_wizard_token(
    console: &mut dyn Console,
    _config_path: &Path,
    _name: &str,
    pending: &PendingWizardToken,
) -> anyhow::Result<String> {
    newt_core::secrets::store_token_at(&pending.path, &pending.token, pending.passphrase.as_ref())
        .map_err(|e| anyhow::anyhow!(e))?;
    let reference = pending.reference.clone();
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
/// `select_hosted_provider` share: same threshold, same blank-filter and
/// no-match-falls-back-to-all semantics, same pipe-safety. Returns the
/// index into the ORIGINAL `rows` slice.
fn select_row(console: &mut dyn Console, rows: &[String], noun: &str) -> anyhow::Result<usize> {
    Ok(select_row_with_zero(console, rows, noun, "")?.expect("no zero choice was offered"))
}

fn select_row_with_zero(
    console: &mut dyn Console,
    rows: &[String],
    noun: &str,
    zero: &str,
) -> anyhow::Result<Option<usize>> {
    let mut pool: Vec<(usize, &String)> = rows.iter().enumerate().collect();

    if pool.len() > FILTER_THRESHOLD {
        console.say(&format!("\n{} {noun} available.", pool.len()));
        let prompt = if zero.is_empty() {
            "Filter (blank = show all): ".to_string()
        } else {
            format!("Filter (blank = show all, 0 = {zero}): ")
        };
        let needle = console.ask(&prompt)?;
        if !zero.is_empty() && needle.trim() == "0" {
            return Ok(None);
        }
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
    if !zero.is_empty() {
        console.say(&format!("  0) {zero}"));
    }
    for (i, (_, row)) in pool.iter().enumerate() {
        console.say(&format!("  {}) {row}", i + 1));
    }
    let ans = console.ask("Choose [1]: ")?;
    if !zero.is_empty() && ans.trim() == "0" {
        return Ok(None);
    }
    let picked = parse_choice(&ans, pool.len()).map(|n| n - 1).unwrap_or(0);
    Ok(Some(pool[picked].0))
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

fn backend_from_verified_probe(
    verified: &VerifiedTargetHit,
    token_env: Option<&str>,
    token_file: Option<&Path>,
) -> anyhow::Result<BackendConfig> {
    let mut backend = backend_from_probe(&verified.probe, token_env, token_file)?;
    // Responses is a definitive fallback after Chat was absent. A successful
    // tool-free Chat request does not establish tool-call compatibility, so the
    // runtime capability probe must remain authoritative for that surface.
    backend.api = verified
        .api
        .filter(|surface| *surface == OpenAiApi::Responses);
    Ok(backend)
}

fn persist_verified_setup(
    config_path: &Path,
    probes: &[VerifiedTargetHit],
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
    let setup_lock = acquire_setup_lock(config_path)?;
    let old_config = read_setup_config(setup_lock.destination.as_path())?;
    let backend_dir = config_path.with_file_name("backends");
    let existing = read_existing_setup_backends(&backend_dir)?;
    let mut used_names: HashSet<String> = existing.iter().map(|item| item.name.clone()).collect();
    let token_file_ref = token_file.and_then(Path::to_str);
    let mut planned = Vec::with_capacity(probes.len());

    for verified in probes {
        let probe = &verified.probe;
        let normalized = normalize_setup_endpoint(&probe.endpoint)?;
        let base_name = backend_name(&probe.endpoint)?;
        if let Some(found) = existing
            .iter()
            .filter(|item| {
                item.endpoint.as_deref() == Some(normalized.as_str())
                    && item.matches_token_reference(token_env, token_file_ref)
                    && item.matches_probe(verified)
            })
            .min_by_key(|item| (item.name != base_name, item.name.as_str()))
        {
            planned.push(PlannedSetupBackend {
                name: found.name.clone(),
                endpoint: normalized,
                path: found.path.clone(),
                body: None,
                replace: false,
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
                replace: false,
            });
            continue;
        }

        let name = allocate_backend_name(&base_name, &mut used_names);
        let mut backend = backend_from_verified_probe(verified, token_env, token_file)?;
        backend.name.clone_from(&name);
        let body = toml::to_string(&backend)?;
        planned.push(PlannedSetupBackend {
            path: backend_dir.join(format!("{name}.toml")),
            name,
            endpoint: normalized,
            body: Some(body.into_bytes()),
            replace: false,
        });
    }

    let default_name = &planned[0].name;
    let updated_config = Config::with_default_backend(&old_config, default_name)?;
    // The wizard only ever CREATES drop-ins (no `replace`), so the after-commit
    // warning sink stays empty here; the backend panel's edit path is the one
    // that can fill it.
    let mut warnings = Vec::new();
    commit_setup_plan(
        config_path,
        &setup_lock.destination,
        &old_config,
        &updated_config,
        &planned,
        &mut warnings,
    )
}

/// Test-only compatibility wrapper for the lower-level persistence regressions
/// that construct already-detected probes without running generation.
#[cfg(test)]
fn persist_detected_setup(
    config_path: &Path,
    probes: &[EndpointProbeResult],
    token_env: Option<&str>,
    token_file: Option<&Path>,
) -> anyhow::Result<Vec<PathBuf>> {
    let verified: Vec<VerifiedTargetHit> = probes
        .iter()
        .cloned()
        .map(|probe| VerifiedTargetHit { probe, api: None })
        .collect();
    persist_verified_setup(config_path, &verified, token_env, token_file)
}

#[derive(Debug)]
struct ExistingSetupBackend {
    name: String,
    path: PathBuf,
    endpoint: Option<String>,
    api_key_env: Option<String>,
    api_key_file: Option<String>,
    kind: Option<BackendKind>,
    api: Option<newt_core::config::OpenAiApi>,
    serving: Option<newt_core::Serving>,
    model: Option<String>,
    generated_by_setup: bool,
}

impl ExistingSetupBackend {
    fn matches_token_reference(&self, env: Option<&str>, file: Option<&str>) -> bool {
        self.api_key_env.as_deref() == env && self.api_key_file.as_deref() == file
    }

    fn matches_probe(&self, verified: &VerifiedTargetHit) -> bool {
        let probe = &verified.probe;
        let kind_matches = self.kind == Some(probe.kind);
        let api_matches = self.api.is_none()
            || self.api == verified.api
            // Setup deliberately persists Chat acceptance without a surface
            // pin so runtime capability probing may select Responses.  Such a
            // writeback is the same verified backend, not a name collision.
            || (self.api == Some(OpenAiApi::Responses)
                && verified.api == Some(OpenAiApi::ChatCompletions));
        let serving_matches = self.serving.is_none_or(|serving| serving == probe.serving);
        let model_matches = self
            .model
            .as_ref()
            .is_none_or(|model| probe.models.contains(model));
        kind_matches
            && api_matches
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
    /// `true` = durably REPLACE an existing drop-in (the backend panel's edit,
    /// #1667); `false` = create-only, refusing to clobber a file that appeared
    /// concurrently (the setup wizard's add semantics, #1660).
    replace: bool,
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
            api: parsed.as_ref().and_then(|backend| backend.api),
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
struct SetupLock {
    destination: newt_core::atomic_fs::ResolvedPath,
    _guard: newt_core::atomic_fs::LockGuard,
}

fn acquire_setup_lock(config_path: &Path) -> anyhow::Result<SetupLock> {
    let destination = setup_config_destination(config_path)?;
    let guard = newt_core::atomic_fs::acquire_lock(&destination.lock_path())?;
    Ok(SetupLock {
        destination,
        _guard: guard,
    })
}

fn setup_config_destination(path: &Path) -> anyhow::Result<newt_core::atomic_fs::ResolvedPath> {
    newt_core::atomic_fs::ResolvedPath::resolve(path)
}

fn setup_file_permissions(path: &Path) -> anyhow::Result<Option<std::fs::Permissions>> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(Some(metadata.permissions())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn stage_setup_file(
    destination: &newt_core::atomic_fs::ResolvedPath,
    body: &[u8],
    permissions: Option<&std::fs::Permissions>,
) -> anyhow::Result<PathBuf> {
    destination.stage_with_permissions(body, permissions, true)
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
        destination: &newt_core::atomic_fs::ResolvedPath,
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

    fn retain_created(&mut self) {
        self.committed = true;
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

fn commit_backend_no_clobber(
    temp: &Path,
    destination: &newt_core::atomic_fs::ResolvedPath,
) -> anyhow::Result<()> {
    match destination.durable_create(temp) {
        Ok(()) => Ok(()),
        Err(error)
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|error| error.kind() == io::ErrorKind::AlreadyExists) =>
        {
            Err(anyhow::anyhow!(
                "backend {} appeared while setup was running; retry setup",
                destination.as_path().display()
            ))
        }
        Err(error) => Err(error.context(format!(
            "could not durably create backend {} without overwriting it",
            destination.as_path().display()
        ))),
    }
}

/// Classify a durable-replace failure on a drop-in the plan REPLACES (#1667
/// review §10). An **after-commit** failure means the rename already succeeded
/// — the new bytes ARE the file, only the parent-directory fsync failed — so it
/// is a durability WARNING, never a "save failed" that would leave the caller
/// reporting a write that is visibly on disk as lost. A before-commit failure is
/// a real failure and propagates.
fn replace_warning(
    result: Result<(), newt_core::atomic_fs::DurableReplaceError>,
) -> Result<Option<String>, newt_core::atomic_fs::DurableReplaceError> {
    match result {
        Ok(()) => Ok(None),
        Err(error) if error.committed() => Ok(Some(error.to_string())),
        Err(error) => Err(error),
    }
}

/// `warnings` collects non-fatal after-commit durability problems (see
/// [`replace_warning`]): the bytes are on disk, but a sync step failed. The
/// caller reports them alongside a SUCCESSFUL write.
fn commit_setup_plan(
    config_path: &Path,
    config_destination: &newt_core::atomic_fs::ResolvedPath,
    old_config: &str,
    updated_config: &str,
    planned: &[PlannedSetupBackend],
    warnings: &mut Vec<String>,
) -> anyhow::Result<Vec<PathBuf>> {
    commit_setup_plan_with(
        config_path,
        config_destination,
        old_config,
        updated_config,
        planned,
        warnings,
        |staged, destination| destination.durable_replace(staged),
    )
}

fn commit_setup_plan_with(
    config_path: &Path,
    config_destination: &newt_core::atomic_fs::ResolvedPath,
    old_config: &str,
    updated_config: &str,
    planned: &[PlannedSetupBackend],
    warnings: &mut Vec<String>,
    commit_config: impl FnOnce(
        &Path,
        &newt_core::atomic_fs::ResolvedPath,
    ) -> Result<(), newt_core::atomic_fs::DurableReplaceError>,
) -> anyhow::Result<Vec<PathBuf>> {
    let mut guard = SetupCommitGuard::default();
    let mut staged_backends = Vec::new();
    for backend in planned {
        if let Some(body) = backend.body.as_deref() {
            let destination = setup_config_destination(&backend.path)?;
            staged_backends.push((
                guard.stage(&destination, body, None)?,
                destination,
                backend.replace,
            ));
        }
    }
    let config_permissions = setup_file_permissions(config_destination.as_path())?;
    let config_stage = if updated_config != old_config {
        Some(guard.stage(
            config_destination,
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
    let backup_destination = setup_config_destination(&backup_path)?;
    let backup_stage = if !old_config.is_empty() && updated_config != old_config {
        Some(guard.stage(
            &backup_destination,
            old_config.as_bytes(),
            config_permissions.as_ref(),
        )?)
    } else {
        None
    };
    let previous_backup_stage = if backup_stage.is_some() {
        match std::fs::read(backup_destination.as_path()) {
            Ok(body) => {
                let permissions = setup_file_permissions(backup_destination.as_path())?;
                Some(guard.stage(&backup_destination, &body, permissions.as_ref())?)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        }
    } else {
        None
    };

    if config_stage.is_some() && read_setup_config(config_destination.as_path())? != old_config {
        anyhow::bail!(
            "{} changed while setup was preparing its update; retry setup",
            config_path.display()
        );
    }

    for (temp, destination, replace) in &staged_backends {
        if *replace {
            // The backend panel's EDIT (#1667): durably replace the existing
            // drop-in. Deliberately NOT registered for rollback — the original
            // bytes are gone once replaced, and the new content is itself a
            // valid drop-in, so a later failure must not delete it. An
            // after-commit sync failure is a warning, not a failure: the file
            // on disk IS the edit (review §10).
            warnings.extend(replace_warning(destination.durable_replace(temp))?);
        } else {
            commit_backend_no_clobber(temp, destination)?;
            guard.created.push(destination.as_path().to_path_buf());
        }
    }
    if config_stage.is_some() && read_setup_config(config_destination.as_path())? != old_config {
        anyhow::bail!(
            "{} changed while setup was preparing its update; retry setup",
            config_path.display()
        );
    }
    if let Some(temp) = backup_stage.as_ref() {
        backup_destination.durable_replace(temp)?;
    }
    if let Some(temp) = config_stage.as_ref() {
        if let Err(config_error) = commit_config(temp, config_destination) {
            if config_error.committed() {
                // The new config may already select these drop-ins. Keep every
                // prerequisite and its old-config backup even though the
                // replacement's parent-directory sync failed.
                guard.retain_created();
                return Err(config_error.into());
            }
            let restore_result = if let Some(previous) = previous_backup_stage.as_ref() {
                backup_destination
                    .durable_replace(previous)
                    .map_err(anyhow::Error::from)
            } else {
                std::fs::remove_file(backup_destination.as_path())
                    .or_else(|error| {
                        if error.kind() == io::ErrorKind::NotFound {
                            Ok(())
                        } else {
                            Err(error)
                        }
                    })
                    .map_err(anyhow::Error::from)
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

// ---------------------------------------------------------------------------
// Backend panel persistence (#1667) — REUSES the wizard's crash-safe machinery
// (acquire_setup_lock → plan → commit_setup_plan, #1660); the panel never gets
// a second write path.
// ---------------------------------------------------------------------------

/// The result of a panel save: the written path plus any non-fatal durability
/// warnings (the bytes ARE on disk — see [`replace_warning`], review §10).
#[cfg(feature = "rich-tui")]
#[derive(Debug)]
pub(crate) struct PanelSave {
    pub path: PathBuf,
    pub warnings: Vec<String>,
}

/// A valid backend file-stem (the panel's name grammar, shared by the write and
/// delete paths so a traversal shape can never reach the filesystem).
#[cfg(feature = "rich-tui")]
fn valid_panel_backend_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// The TOML keys the panel form manages, paired with the dirty flags that say
/// whether the operator actually touched each one (three Cs: the mapping is
/// data, so a new form field is one row here). Only DIRTY keys are overlaid.
#[cfg(feature = "rich-tui")]
fn dirty_dropin_edits(
    edit: &crate::backend_panel::BackendEdit,
) -> Vec<(&'static str, Option<String>)> {
    let dirty = edit.dirty;
    [
        (
            "kind",
            dirty.kind,
            edit.kind.map(|kind| kind.label().to_string()),
        ),
        (
            "endpoint",
            dirty.endpoint,
            Some(edit.endpoint.trim().to_string()).filter(|url| !url.is_empty()),
        ),
        ("model", dirty.model, edit.model.clone()),
        ("api_key_env", dirty.api_key_env, edit.api_key_env.clone()),
        (
            "api_key_file",
            dirty.api_key_file,
            edit.api_key_file.clone(),
        ),
    ]
    .into_iter()
    .filter(|(_, dirty, _)| *dirty)
    .map(|(key, _, value)| (key, value))
    .collect()
}

/// Write the panel's add/edit form as the drop-in `backends/<name>.toml`, under
/// the setup lock, via the staged [`commit_setup_plan`] commit.
///
/// An EDIT (`edit.replace`) **re-reads the file at SAVE time** and overlays only
/// the keys the operator actually changed ([`dirty_dropin_edits`]), through
/// `BackendConfig::with_dropin_edits` — a `toml_edit` overlay, so comments, key
/// order, and keys `BackendConfig` does not model survive (review §6 and §8;
/// the old serde round-trip silently destroyed both, and re-applying the whole
/// panel-open prefill silently reverted a concurrent writer's untouched fields).
/// A field the form never touched — the `kind` dial included — is left
/// byte-for-byte alone, which is the persistence half of the review §1 fix (the
/// dial half is `KIND_LADDER` + `begin_edit`'s fail-closed refusal).
///
/// **Residual race:** the read → stage → replace window is guarded by the setup
/// lock, so any *newt* writer serializes behind it; a foreign editor writing the
/// drop-in inside that window still loses its change to the overlay. Narrowing
/// that further needs a content hash / O_EXCL exchange the drop-in format does
/// not carry yet.
///
/// An ADD refuses to clobber an existing file. `config.toml` is never rewritten
/// here (the default pointer is the chooser's job, not the editor's).
#[cfg(feature = "rich-tui")]
pub(crate) fn persist_panel_backend(
    config_path: &Path,
    edit: &crate::backend_panel::BackendEdit,
) -> anyhow::Result<PanelSave> {
    anyhow::ensure!(!edit.name.trim().is_empty(), "backend needs a name");
    anyhow::ensure!(
        valid_panel_backend_name(edit.name.trim()),
        "invalid backend name '{}'",
        edit.name
    );
    if let Some(parent) = config_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let setup_lock = acquire_setup_lock(config_path)?;
    let old_config = read_setup_config(setup_lock.destination.as_path())?;
    let path = config_path
        .with_file_name("backends")
        .join(format!("{}.toml", edit.name));
    let body = if edit.replace {
        let existing = std::fs::read_to_string(&path)
            .map_err(|error| anyhow::anyhow!("read {}: {error}", path.display()))?;
        BackendConfig::with_dropin_edits(&existing, &dirty_dropin_edits(edit))
            .map_err(|error| anyhow::anyhow!("edit {}: {error}", path.display()))?
    } else if path.exists() {
        anyhow::bail!("backend '{}' already exists — edit it instead", edit.name);
    } else {
        let backend = BackendConfig {
            name: edit.name.clone(),
            endpoint: edit.endpoint.clone(),
            kind: edit.kind,
            model: edit.model.clone(),
            api_key_env: edit.api_key_env.clone(),
            api_key_file: edit.api_key_file.clone(),
            ..BackendConfig::default()
        };
        toml::to_string(&backend)?
    };
    // Parse what we are about to write: the drop-in must still be a valid
    // backend after the overlay, and the plan wants the normalized endpoint.
    let parsed: BackendConfig = toml::from_str(&body)
        .map_err(|error| anyhow::anyhow!("{} would become invalid: {error}", path.display()))?;
    let endpoint = if parsed.endpoint.trim().is_empty() {
        // A `kind = "embedded"` drop-in has a model_path, not a URL.
        String::new()
    } else {
        normalize_setup_endpoint(&parsed.endpoint)?
    };
    let planned = [PlannedSetupBackend {
        name: edit.name.clone(),
        endpoint,
        path: path.clone(),
        body: Some(body.into_bytes()),
        replace: edit.replace,
    }];
    // old == updated: the plan stages/commits ONLY the drop-in; config.toml is
    // left byte-for-byte alone (no backup dance, no default_backend rewrite).
    let mut warnings = Vec::new();
    commit_setup_plan(
        config_path,
        &setup_lock.destination,
        &old_config,
        &old_config,
        &planned,
        &mut warnings,
    )?;
    Ok(PanelSave { path, warnings })
}

/// The `default_backend` a config.toml TEXT names, if any.
#[cfg(feature = "rich-tui")]
fn default_backend_in(config_text: &str) -> Option<String> {
    toml::from_str::<toml::Value>(config_text)
        .ok()?
        .get("default_backend")?
        .as_str()
        .map(str::to_string)
}

/// The `[[backends]]` names declared INLINE in a config.toml text.
#[cfg(feature = "rich-tui")]
fn inline_backend_names_in(config_text: &str) -> Vec<String> {
    toml::from_str::<toml::Value>(config_text)
        .ok()
        .as_ref()
        .and_then(|value| value.get("backends"))
        .and_then(toml::Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.get("name").and_then(toml::Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The `[[backends]]` names declared inline in the config.toml at `config_path`
/// — a drop-in that shares one of these names does NOT fully shadow it: the
/// merge re-inherits `api_key_*` / `tiers` the drop-in omits
/// (`Config::merge_backends_from_dir`), so clearing an auth field in the panel
/// can silently come back (review §4). The panel marks those rows and says so
/// in the save note.
#[cfg(feature = "rich-tui")]
pub(crate) fn inline_backend_names(config_path: &Path) -> Vec<String> {
    std::fs::read_to_string(config_path)
        .map(|text| inline_backend_names_in(&text))
        .unwrap_or_default()
}

/// Delete the drop-in `backends/<name>.toml` under the setup lock, durably
/// syncing the parent directory — the panel's `:d <name>` (#1667). Returns the
/// operator-visible notes the caller must report (a `default_backend` repoint, a
/// non-durable delete).
///
/// **The durable default pointer is part of this transaction** (review §2/§7/§11):
/// removing the backend `config.toml`'s `default_backend` names would leave a
/// dangling pointer, which `Config::select_backend` treats as a hard
/// `UnknownNamed` operator error (the ACP worker `bail!`s on it, and no
/// settings.toml mask exists there). So when the removed name IS the default,
/// this refuses unless the same transaction hands over `repoint_default_to` —
/// the backend the caller just applied — and then repoints `default_backend`
/// (comment-preserving, via `Config::with_default_backend`) BEFORE unlinking, so
/// a failed delete can never leave the pointer dangling.
///
/// The caller (the panel) additionally refuses to remove the ACTIVE backend
/// unless a different named selection is applied in the same transaction; this
/// function guards the filesystem invariants (a sane file-stem name, the file
/// existing) and the durable pointer.
#[cfg(feature = "rich-tui")]
pub(crate) fn remove_panel_backend(
    config_path: &Path,
    name: &str,
    repoint_default_to: Option<&str>,
) -> anyhow::Result<Vec<String>> {
    anyhow::ensure!(
        valid_panel_backend_name(name),
        "invalid backend name '{name}'"
    );
    if let Some(new) = repoint_default_to {
        anyhow::ensure!(
            valid_panel_backend_name(new),
            "invalid backend name '{new}'"
        );
        anyhow::ensure!(
            new != name,
            "cannot repoint default_backend at '{new}' while removing it"
        );
    }
    let setup_lock = acquire_setup_lock(config_path)?;
    let old_config = read_setup_config(setup_lock.destination.as_path())?;
    let backends_dir = config_path.with_file_name("backends");
    let path = backends_dir.join(format!("{name}.toml"));
    anyhow::ensure!(
        path.exists(),
        "no backend drop-in named '{name}' ({})",
        path.display()
    );
    let mut notes = Vec::new();
    if default_backend_in(&old_config).as_deref() == Some(name) {
        let Some(new) = repoint_default_to else {
            anyhow::bail!(
                "'{name}' is config.toml's default_backend — removing it would leave a dangling \
                 default (a hard 'unknown backend' error for `newt solve` and the ACP worker); \
                 dial another named backend first so the switch and the removal happen in one \
                 transaction"
            );
        };
        anyhow::ensure!(
            backends_dir.join(format!("{new}.toml")).exists()
                || inline_backend_names_in(&old_config)
                    .iter()
                    .any(|n| n == new),
            "cannot repoint default_backend at unknown backend '{new}'"
        );
        let updated_config = Config::with_default_backend(&old_config, new)?;
        let mut warnings = Vec::new();
        commit_setup_plan(
            config_path,
            &setup_lock.destination,
            &old_config,
            &updated_config,
            &[],
            &mut warnings,
        )?;
        notes.extend(warnings);
        notes.push(format!(
            "default_backend now points at '{new}' ({})",
            config_path.display()
        ));
    }
    match std::fs::remove_file(&path) {
        Ok(()) => {
            // Surface a non-durable delete the way the write side surfaces a
            // post-rename sync failure (review §9) — the unlink HAPPENED, so
            // this is a warning on a success, not an error.
            if let Err(error) = newt_core::atomic_fs::sync_parent(&path) {
                notes.push(format!(
                    "removed {}, but could not durably sync its parent directory: {error:#}",
                    path.display()
                ));
            }
            Ok(notes)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(anyhow::anyhow!(
            "no backend drop-in named '{name}' ({})",
            path.display()
        )),
        Err(error) => Err(error.into()),
    }
}

/// The names of the per-file backend drop-ins next to `config_path` — which
/// chooser entries the panel may edit/remove (inline `[[backends]]` in
/// config.toml stay read-only there). Reuses the wizard's directory reader.
#[cfg(feature = "rich-tui")]
pub(crate) fn panel_backend_file_names(config_path: &Path) -> Vec<String> {
    read_existing_setup_backends(&config_path.with_file_name("backends"))
        .map(|found| found.into_iter().map(|item| item.name).collect())
        .unwrap_or_default()
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
            probe_key: Some("replacement-secret".into()),
            pending_token: None,
        };
        let mut console = ScriptedConsole::new(&[]);
        let result = verify_key_with_retries(
            &mut console,
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
        let mut console = ScriptedConsole::new(&[]);
        let (_, api) = verify_custom_chat_with_retries(
            &mut console,
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
        let mut console = ScriptedConsole::new(&[]);

        let error = verify_custom_chat_with_retries(
            &mut console,
            &reqwest::Client::new(),
            &hit,
            "model",
            Some(BEARER_SENTINEL.into()),
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
        let mut console = ScriptedConsole::new(&["first-key", "second-key", "must-remain"]);

        let error = verify_custom_chat_with_retries(
            &mut console,
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
            console.answers.front().map(String::as_str),
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
            probe_key: Some("initial-key".into()),
            pending_token: None,
        };
        let mut console =
            ScriptedConsole::new(&["Y", "first-key", "", "Y", "second-key", "", "must-remain"]);

        let result = verify_key_with_retries(
            &mut console,
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
            console.answers.front().map(String::as_str),
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
            token: "new-secret".into(),
            passphrase: Some(newt_core::secrets::SecretString::from("test-passphrase")),
            path: versioned_token.clone(),
            reference: versioned_reference,
        };
        let mut console = ScriptedConsole::new(&[]);
        let result = persist_interactive_backend_with(
            &mut console,
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
            token: "new-secret".into(),
            passphrase: Some(newt_core::secrets::SecretString::from("test-passphrase")),
            path: versioned_token.clone(),
            reference: token_reference,
        };

        let error = persist_interactive_backend_with(
            &mut ScriptedConsole::new(&[]),
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
            token: "new-secret".into(),
            passphrase: Some(newt_core::secrets::SecretString::from("test-passphrase")),
            path: new_token,
            reference: new_reference,
        };
        let cfg = Config {
            default_backend: Some("example".into()),
            ..Default::default()
        };
        persist_interactive_backend(
            &mut ScriptedConsole::new(&[]),
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
        let mut console = ScriptedConsole::new(&[]);

        let error = persist_interactive_backend_with(
            &mut console,
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

    /// Real-resource grounding for setup idempotency after the runtime records
    /// its definitive OpenAI wire surface in a generated backend drop-in.
    #[ignore = "real-resource: weekly/release tier; touches the filesystem"]
    #[serial_test::serial(real_fs)]
    #[test]
    fn detected_setup_reuses_a_runtime_api_writeback_without_suffixing() {
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
        newt_core::writeback_probed_backend(&BackendConfig {
            name,
            endpoint: "https://inference.example.test".into(),
            model: Some("model".into()),
            kind: Some(BackendKind::Openai),
            api: Some(OpenAiApi::Responses),
            serving: Some(verified[0].probe.serving),
            ..Default::default()
        })
        .unwrap();
        let runtime_bytes = std::fs::read(&backend_path).unwrap();

        let written = persist_verified_setup(&config_path, &verified, None, None).unwrap();

        assert!(written.is_empty(), "runtime writeback should be reused");
        assert_eq!(std::fs::read_dir(&backend_dir).unwrap().count(), 1);
        assert_eq!(std::fs::read(&backend_path).unwrap(), runtime_bytes);
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
            &mut ScriptedConsole::new(&[]),
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
        let written: BackendConfig =
            toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
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
            body: Some(
                b"name = \"example\"\nendpoint = \"https://inference.example.test\"\n".to_vec(),
            ),
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
        let mut console = ScriptedConsole::new(&[]);

        let error = run_target_with_persist(
            &mut console,
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
        let mut console = ScriptedConsole::new(&[]);

        run_target_with(
            &mut console,
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
        let mut console = ScriptedConsole::new(&[]);

        run_target_with(
            &mut console,
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
        let mut console = ScriptedConsole::new(&["n"]);

        run_target_with(
            &mut console,
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
        let mut console = ScriptedConsole::new(&[]);

        let error = run_target_with(
            &mut console,
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
        let mut console = ScriptedConsole::new(&[]);

        run_target_with(
            &mut console,
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
        let mut console = ScriptedConsole::new(&[]);

        let err = run_target_with(
            &mut console,
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
        let mut console = ScriptedConsole::new(&[]);

        let error = run_target_with(
            &mut console,
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
        let mut console = ScriptedConsole::new(&["bad-key", "", "1", "Y", "good-key", ""]);
        let result =
            configure_preset(&mut console, &reqwest::Client::new(), &preset, &cfg_path).await;
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
        mount_openai_chat(&server).await;
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
        mount_ollama_chat(&server).await;
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
        let mut console = ScriptedConsole::new(&["4"]);
        let picked = select_hosted_provider(&mut console, &presets).unwrap();
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
        let mut console = ScriptedConsole::new(&["0"]);

        let picked = select_hosted_provider(&mut console, &presets).unwrap();

        assert_eq!(picked, HostedProviderChoice::CustomEndpoint);
        assert!(console
            .transcript()
            .contains("0) I have a URL (custom endpoint)"));
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

    #[test]
    fn zero_choice_is_available_before_filtering_a_large_roster() {
        let rows: Vec<String> = (1..=12).map(|i| format!("provider-{i}")).collect();
        let mut console = ScriptedConsole::new(&["0"]);

        let picked = select_row_with_zero(
            &mut console,
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
        let mut console = ScriptedConsole::new(&[
            "3",
            "0",
            &server_with_v1,
            "test-remote-key",
            "1",
            "1",
            "",
            "y",
        ]);

        run_with_flow(&mut console, &reqwest::Client::new(), &path, Flow::FirstRun)
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
        let mut console =
            ScriptedConsole::new(&["2", &server_with_v1, "test-remote-key", "1", "1", "", "y"]);
        run_with(&mut console, &client, &path).await.unwrap();

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
        let mut console = ScriptedConsole::new(&["", "1"]);
        let (_cfg, backend, _pending) = configure_preset(&mut console, &client, &preset, &path)
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
        let mut console = ScriptedConsole::new(&["sk-preset-secret", "open sesame", "1"]);
        let (_cfg, backend, pending) = configure_preset(&mut console, &client, &preset, &path)
            .await
            .unwrap();
        assert!(backend.api_key_env.is_none());
        let token_ref = backend.api_key_file.as_deref().expect("encrypted ref");
        assert!(token_ref.contains("gatedcloud.token."));
        assert!(token_ref.ends_with(".age"));
        assert_eq!(backend.effective_model(), Some("gated-model"));
        let pending = pending.expect("token is held until final write");
        assert_eq!(
            persist_wizard_token(&mut console, &path, "gatedcloud", &pending).unwrap(),
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
        // use exported?=<Enter: yes>, model=1
        let mut console = ScriptedConsole::new(&["", "1"]);
        let (_cfg, backend, _pending) = configure_preset(&mut console, &client, &preset, &path)
            .await
            .unwrap();
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
        let dir = tempfile::tempdir().unwrap();
        // The machine identity needs a config root even with HOME unset.
        let _config_env = EnvVarGuard::set(newt_core::config::NEWT_CONFIG_DIR_ENV, dir.path());
        newt_core::secrets::session().reset_for_test();
        let _home = EnvVarGuard::remove("HOME");
        let _userprofile = EnvVarGuard::remove("USERPROFILE");

        let path = dir.path().join("config.toml");
        // passphrase=<Enter: machine key>
        let mut console = ScriptedConsole::new(&[""]);
        let pending = collect_wizard_token(&mut console, "a-secret", &path, "example").unwrap();
        let recorded = persist_wizard_token(&mut console, &path, "example", &pending)
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
        mount_ollama_chat(&server).await;
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
