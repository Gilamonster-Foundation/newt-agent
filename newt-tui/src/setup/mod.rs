//! Inference-backend setup (`newt setup`).
//!
//! With no target, this is the human-driven first-run wizard. With a hostname,
//! URL, or `host:port`, it probes Ollama and OpenAI-compatible model-list APIs,
//! records every live endpoint, and adopts the models reported by the server.
//! Bare hosts expand through `[discovery]`; authenticated probing requires an
//! explicit HTTPS URL (HTTP only for loopback) so a bearer token is never
//! broadcast across guessed ports or sent over implicit plaintext transport.
//!
//! Operator I/O goes through C1's seam ([`Operator`]) so the whole
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

mod commit;
pub(crate) mod credentials;
mod operator;

use commit::*;

// **The transaction engine's second caller.** The rich-TUI backend panel
// (#1667) reaches these as `setup::…` from `chat.rs`, so they are re-exported
// by NAME rather than swept along by the glob above — a private `use` would
// hide them and the panel would stop compiling, which is exactly what the
// `rich-tui` clippy gate caught when this split first landed. Naming them
// also states the engine's public surface in one place.
#[cfg(feature = "rich-tui")]
pub(crate) use commit::{
    inline_backend_names, panel_backend_file_names, persist_panel_backend, remove_panel_backend,
};

use newt_core::agent_identity::Secret;
use newt_core::backend_probe::{EndpointProbeResult, GenerationCheck};
use newt_core::config::Discovery;
use newt_core::interaction_form::{self, NO, YES};
use newt_core::provider_preset::{
    self, list_models_for_preset, preset_support,
    validate_authenticated_url as validate_authenticated_target, PresetSupport, ProviderPreset,
};
use newt_core::{BackendConfig, BackendKind, Config, EndpointKind, OpenAiApi, Tier};
use newt_interaction::InteractionDefinition;
use operator::Operator;
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
    wizard_entry(&Operator::terminal(), Flow::Setup)
}

/// The first-run variant ([`crate::wizard::maybe_run`]): driven through
/// [`FirstRunConsole`] so Esc/Ctrl-C surface as catchable aborts (the caller
/// falls back to probe-and-write defaults), and the overwrite guard is
/// skipped (the caller already proved no config exists).
pub(crate) fn run_first_run(_color: bool) -> anyhow::Result<()> {
    // `FirstRunConsole` is gone: it existed so Esc/Ctrl-C surfaced as
    // catchable errors instead of SIGINT, and the seam reports those as
    // outcomes for every operator now.
    wizard_entry(&Operator::terminal(), Flow::FirstRun)
}

fn wizard_entry(op: &Operator<'_>, flow: Flow) -> anyhow::Result<()> {
    let config_path =
        Config::user_config_path().unwrap_or_else(|| std::path::PathBuf::from("newt.toml"));
    let client = setup_http_client()?;
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(run_with_flow(op, &client, &config_path, flow))
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
    let op = Operator::terminal();
    run_target_with(
        &op,
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
    op: &Operator<'_>,
    client: &reqwest::Client,
    config_path: &Path,
    request: TargetSetupRequest<'_>,
    discovery: &Discovery,
) -> anyhow::Result<()> {
    run_target_with_persist(
        op,
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
    op: &Operator<'_>,
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
    op.say(&format!(
        "Probing {} candidate endpoint{} for Ollama or OpenAI-compatible APIs...",
        candidates.len(),
        if candidates.len() == 1 { "" } else { "s" }
    ));

    let (hits, mut failures) =
        probe_candidates_concurrently(client, &candidates, api_key.as_deref()).await?;
    if hits.is_empty() {
        for failure in failures {
            op.say(&format!("  {failure}"));
        }
        anyhow::bail!(
            "no supported inference API found for `{target}`; tried {}",
            candidates.join(", ")
        );
    }

    let (hits, generation_failures) =
        verify_target_hits(op, client, hits, model, api_key.as_deref()).await;
    failures.extend(generation_failures);
    if hits.is_empty() {
        for failure in failures {
            op.say(&format!("  {failure}"));
        }
        anyhow::bail!("no inference backend passed a minimal generation check for `{target}`");
    }

    if !failures.is_empty() {
        op.say(&format!(
            "Skipped {} candidate endpoint{}:",
            failures.len(),
            if failures.len() == 1 { "" } else { "s" }
        ));
        for failure in failures {
            op.say(&format!("  {failure}"));
        }
    }

    op.say(&format!(
        "Detected {} inference backend{}:",
        hits.len(),
        if hits.len() == 1 { "" } else { "s" }
    ));
    for hit in &hits {
        let backend = backend_from_verified_probe(hit, token_env, token_file.as_deref())?;
        op.say(&format!(
            "  {} ({:?}, {}, {} model{})",
            backend.name,
            backend.kind,
            backend.endpoint,
            hit.probe.models.len(),
            if hit.probe.models.len() == 1 { "" } else { "s" }
        ));
    }

    if !yes
        && !decide(
            op,
            &interaction_form::confirm(
                format!("Write backend files and update {}?", config_path.display()),
                "",
                "yes, write them",
                "no, stop here",
            ),
            // Writes files: blank must not consent.
            None,
        )?
    {
        op.say("Aborted. Nothing written.");
        return Ok(());
    }

    let written = persist(config_path, &hits, token_env, token_file.as_deref())?;
    for path in &written {
        op.say(&format!("Wrote {}.", path.display()));
    }
    op.say(&format!(
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
    op: &Operator<'_>,
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
        op.say(&format!(
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
                op.say("  ✓ generation accepted");
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
// Driver (fully testable: a scripted Operator + wiremock client)
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
    /// The plaintext, wrapped. `Secret` redacts its `Debug` and is not
    /// `Serialize`, so this record cannot reach a log line or a config file
    /// even through a `{:?}` on a panic path.
    token: Secret,
    passphrase: Option<newt_core::secrets::SecretString>,
    path: PathBuf,
    /// Where the key lives, as the type that cannot be empty and cannot hold
    /// the key. See `credentials::SealedSecret`.
    reference: credentials::SealedSecret,
}

fn persist_interactive_backend(
    op: &Operator<'_>,
    config_path: &Path,
    cfg: &Config,
    backend: &BackendConfig,
    pending_token: Option<&PendingWizardToken>,
) -> anyhow::Result<PathBuf> {
    persist_interactive_backend_with(
        op,
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
    op: &Operator<'_>,
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
    let token_reference = pending_token.map(|pending| pending.reference.handle().to_string());
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
                        pending.token.expose(),
                        pending.passphrase.as_ref(),
                    )
                    .map_err(|error| anyhow::anyhow!(error))?;
                    guard.created.push(destination.as_path().to_path_buf());
                    let resolved = newt_core::secrets::resolve_token_file(destination.as_path())
                        .map_err(|error| anyhow::anyhow!(error))?;
                    if resolved.as_deref() != pending_token.map(|pending| pending.token.expose()) {
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
        op.say(&format!("  → stored encrypted at {reference}"));
    }
    Ok(logical_backend_path)
}

type ConfiguredBackend = (Config, BackendConfig, Option<PendingWizardToken>);

/// The wizard flow, parameterised over its operator and HTTP client so it can be
/// exercised end-to-end in tests (production enters via [`run_with_flow`]).
#[cfg(test)]
async fn run_with(
    op: &Operator<'_>,
    client: &reqwest::Client,
    config_path: &Path,
) -> anyhow::Result<()> {
    run_with_flow(op, client, config_path, Flow::Setup).await
}

async fn run_with_flow(
    op: &Operator<'_>,
    client: &reqwest::Client,
    config_path: &Path,
    flow: Flow,
) -> anyhow::Result<()> {
    // First run already printed the branded crawl header; only a standalone
    // `newt setup` announces itself.
    if flow == Flow::Setup {
        op.say(&format!("newt v{} — interactive setup", crate::VERSION));
    }

    if flow == Flow::Setup
        && config_path.exists()
        && !decide(
            op,
            &interaction_form::confirm(
                format!(
                    "A config already exists at {}. Overwrite?",
                    config_path.display()
                ),
                "",
                "yes, overwrite it",
                "no, keep it",
            ),
            // Blank keeps the existing config — the no-op.
            Some(false),
        )?
    {
        op.say("Keeping the existing config. Nothing written.");
        return Ok(());
    }

    // Multi-backend loop: each pass configures + writes ONE backend, then
    // offers another round — so anthropic + ollama.com + a LAN box can all
    // land in one sitting. With several written, the default is picked at
    // the end (until then, each committed setup round selects its backend).
    let mut written: Vec<String> = Vec::new();
    loop {
        let (cfg, backend, pending_token) = match choose_backend(op)? {
            BackendChoice::LocalOllama => configure_ollama(op, client).await?,
            BackendChoice::CustomHost => configure_custom_host(op, client, config_path).await?,
            BackendChoice::HostedProvider => configure_hosted(op, client, config_path).await?,
        };

        // Preview before committing anything to disk: the backend drop-in is
        // the interesting file; config.toml just points at it.
        let preview = toml::to_string_pretty(&backend)
            .unwrap_or_else(|e| format!("# (could not render preview: {e})"));
        op.say(&format!("\nbackends/{}.toml:\n", backend.name));
        op.say(&preview);

        if !decide(
            op,
            &interaction_form::confirm(
                format!("Write to {}?", config_path.display()),
                "",
                "yes, write it",
                "no, skip this one",
            ),
            // Writes files: blank must not consent.
            None,
        )? {
            if written.is_empty() {
                op.say("Aborted. Nothing written.");
                return Ok(());
            }
            op.say("Skipped this one.");
        } else {
            let dropin = persist_interactive_backend(
                op,
                config_path,
                &cfg,
                &backend,
                pending_token.as_ref(),
            )?;
            op.say(&format!(
                "Wrote {} and {}.",
                config_path.display(),
                dropin.display()
            ));
            written.push(backend.name.clone());
        }

        if !decide(
            op,
            &interaction_form::confirm(
                "Add another backend?",
                "",
                "yes, add one",
                "no, that is all",
            ),
            // Blank ends the loop. A pipe that has run out has said all it
            // is going to; requiring an explicit `n` would fail a setup
            // that had otherwise completed.
            Some(false),
        )? {
            break;
        }
    }

    if written.len() > 1 {
        op.say("\nWhich backend should sessions start on?");
        let idx = select_row(op, &written, "backends")?;
        let chosen = &written[idx];
        // Rewrite ONLY default_backend, preserving the config's other keys
        // and comments (the same comment-preserving editor `newt setup
        // <target>` uses).
        persist_default_backend(config_path, chosen)?;
        op.say(&format!(
            "Default backend: {chosen} (/backends switches per session)."
        ));
    }

    op.say("Edit those files (or re-run `newt setup`) to change anything.");
    offer_identity(op);
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

/// Consecutive unusable answers at one decision before setup gives up.
const DECISION_ATTEMPTS: usize = 3;

/// Present a choice and return the id of the option the operator picked.
///
/// `blank` is what an EMPTY answer means, and the distinction it encodes is
/// the one D1a and D1b-2 settled:
///
/// * `Some(id)` — the menu displays a default (`[1]`), and Enter accepts what
///   is on screen. That is the operator's own displayed value, not the
///   machine deciding, and it is how every numbered picker in the wizard has
///   always behaved.
/// * `None` — a DECISION. Nothing is offered for free; blank re-asks. This is
///   the arm `decide` uses, because `is_yes(&ans, true)` reading blank as yes
///   is what let a short pipe answer a write confirm on the operator's
///   behalf.
///
/// An UNRECOGNISED answer always re-asks, in both arms. `parse_choice(&ans,
/// n).unwrap_or(1)` used to fold garbage into option 1 silently — the same
/// defect as `is_yes("maybe", true)`, one menu over.
///
/// Capped, because under a pipe an unusable answer is re-asked against the
/// same exhausted input and an uncapped loop is a hang.
///
/// # Errors
///
/// The read failure, or a bail after [`DECISION_ATTEMPTS`] unusable answers.
fn choose(
    op: &Operator<'_>,
    definition: &InteractionDefinition,
    blank: Option<&str>,
) -> anyhow::Result<String> {
    for _ in 0..DECISION_ATTEMPTS {
        let answer = op.ask(definition)?;
        let answer = answer.trim();
        if answer.is_empty() {
            if let Some(default) = blank {
                return Ok(default.to_string());
            }
        }
        if let Some(picked) = interaction_form::resolve(definition, answer) {
            return Ok(picked.as_str().to_string());
        }
        op.say(&format!(
            "  not one of the choices — enter {}",
            keys(definition)
        ));
    }
    anyhow::bail!("no usable answer after {DECISION_ATTEMPTS} attempts")
}

/// The accelerators a definition offers, for a retry message that stays
/// true to what was actually displayed.
///
/// Derived rather than written: a hardcoded "answer y or n" was right for the
/// two confirms it was written for and wrong for every menu, and it would
/// have gone stale the moment an option was added.
fn keys(definition: &InteractionDefinition) -> String {
    definition
        .controls
        .iter()
        .filter_map(|control| match &control.kind {
            newt_interaction::ControlKind::Choice { options } => Some(options),
            _ => None,
        })
        .flatten()
        .map(|option| option.key.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Ask a yes/no decision.
///
/// **`blank` may DECLINE; it may never COMMIT.** That is the rule D1a found
/// the hard way and this slice had to state precisely, because `[Y/n]` and
/// `[y/N]` were both spelled `is_yes(&ans, _)` and only one of them was
/// dangerous:
///
/// * `None` — blank re-asks. For every confirm whose "yes" DOES something
///   irreversible: writes a file, overwrites a config, adopts a credential.
///   `is_yes(&ans, true)` reading an EOF-induced empty answer as consent is
///   what let a short pipe write the crew file in D1a.
/// * `Some(false)` — blank declines. For a confirm whose "no" is the no-op:
///   *Overwrite?*, *Add another backend?*. Running out of input genuinely
///   means "no more", and forcing an explicit `n` would turn a completed
///   piped setup into a failure at its last prompt — `newt setup` over SSH
///   and through a pipe are both real.
///
/// It is a PARAMETER rather than a rule inside the function so the choice is
/// visible at every call site and a reviewer can check which way each default
/// falls without reading this doc.
///
/// **No advertised default, deliberately.** `is_yes(&ans, true)` — which both
/// call sites used — read blank AND every unrecognised word as YES. D1a found
/// that was not a style question: paired with a reader that returned `""` at
/// EOF, a short pipe answered a write confirm on the operator's behalf.
/// `interaction_form::confirm` offers `yes`/`no` as real options and
/// advertises neither; `resolve_typed` refuses anything else.
///
/// Capped for the same reason `CrewForm` caps its retries: under a pipe an
/// unusable answer is re-asked against the same exhausted input, so an
/// uncapped loop is a hang. Giving up is an ERROR rather than a guess — these
/// callers are deciding whether to adopt a credential or cancel setup, and
/// neither may be chosen for them.
///
/// # Errors
///
/// The read failure, or a bail after [`DECISION_ATTEMPTS`] unusable answers.
fn decide(
    op: &Operator<'_>,
    definition: &InteractionDefinition,
    blank: Option<bool>,
) -> anyhow::Result<bool> {
    let blank = blank.map(|yes| if yes { YES } else { NO });
    Ok(choose(op, definition, blank)? == YES)
}

fn choose_backend(op: &Operator<'_>) -> anyhow::Result<BackendChoice> {
    let picked = choose(
        op,
        &interaction_form::menu(
            "\nWhere does your model run?",
            "",
            &[
                ("1", "Ollama on this machine (http://127.0.0.1:11434)"),
                (
                    "2",
                    "Another machine (hostname or URL — newt probes Ollama / llama.cpp / vLLM)",
                ),
                (
                    "3",
                    "A hosted provider (OpenAI, Anthropic, OpenRouter, NVIDIA, … — API key)",
                ),
            ],
        ),
        Some("1"),
    )?;
    Ok(match picked.as_str() {
        "2" => BackendChoice::CustomHost,
        "3" => BackendChoice::HostedProvider,
        _ => BackendChoice::LocalOllama,
    })
}

// ---------------------------------------------------------------------------
// Ollama path
// ---------------------------------------------------------------------------

async fn configure_ollama(
    op: &Operator<'_>,
    client: &reqwest::Client,
) -> anyhow::Result<ConfiguredBackend> {
    let default_url = "http://127.0.0.1:11434";
    let raw = op.ask(&interaction_form::text_field(
        "Ollama host",
        format!("[{default_url}] — Enter keeps it"),
    ))?;
    let url = normalize_url(
        if raw.is_empty() { default_url } else { &raw },
        "http",
        11434,
    );

    let model = pick_model(op, client, &url).await?;
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
    op: &Operator<'_>,
    client: &reqwest::Client,
    config_path: &Path,
) -> anyhow::Result<ConfiguredBackend> {
    loop {
        let raw = op.ask(&interaction_form::text_field(
            "Host name or URL",
            "e.g. gpu-box, 10.0.0.5:8000, https://llm.example.net",
        ))?;
        let target = raw.trim().to_string();
        if target.is_empty() {
            op.say("  A host is required.");
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
                op.say(&format!("  {e}"));
                continue;
            }
        };
        op.say(&format!("Probing {}…", candidates.join(", ")));
        let (mut hits, mut failures) =
            probe_candidates_concurrently(client, &candidates, None).await?;

        // Authentication-required endpoints (the typed probe error): offer one
        // hidden key prompt and re-probe. The https/loopback transport rule
        // applies before any token leaves this process.
        let mut api_key: Option<Secret> = None;
        if hits.is_empty()
            && failures
                .iter()
                .any(|f| f.contains("authentication required"))
        {
            if let Some(key) =
                credentials::ask_secret(op, "API key (echoes as *, Enter to skip): ")?
            {
                validate_authenticated_target(&target)?;
                let (h, f) =
                    probe_candidates_concurrently(client, &candidates, Some(key.expose())).await?;
                hits = h;
                failures = f;
                api_key = Some(key);
            }
        }

        if hits.is_empty() {
            for failure in &failures {
                op.say(&format!("  {failure}"));
            }
            op.say(&format!(
                "\nNo inference API answered at {target} (tried {}).",
                candidates.join(", ")
            ));
            let picked = choose(
                op,
                &interaction_form::menu(
                    "",
                    "",
                    &[
                        ("1", "Try a different host"),
                        ("2", "Enter endpoint and model by hand (generation-tested)"),
                        ("3", "Cancel setup"),
                    ],
                ),
                Some("1"),
            )?;
            match picked.as_str() {
                "2" => return manual_backend_entry(op, client, config_path).await,
                "3" => {
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

        op.say(&format!(
            "\nFound {} endpoint{}:",
            hits.len(),
            if hits.len() == 1 { "" } else { "s" }
        ));
        let rows: Vec<(String, String)> = hits
            .iter()
            .enumerate()
            .map(|(i, hit)| ((i + 1).to_string(), format_endpoint_row(hit)))
            .collect();
        let entries: Vec<(&str, &str)> =
            rows.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let picked = choose(op, &interaction_form::menu("", "", &entries), Some("1"))?;
        let idx = picked.parse::<usize>().unwrap_or(1) - 1;
        let hit = &hits[idx];

        let ordered = order_models_warm_first(&hit.models, &hit.warm);
        let model = if ordered.is_empty() {
            ask_model_name(op)?
        } else {
            select_model(op, &ordered)?
        };

        let (api_key, api) =
            verify_custom_chat_with_retries(op, client, hit, &model, api_key).await?;

        let mut backend = backend_from_probe(hit, None, None)?;
        backend.model = Some(model);
        if backend.serving == Some(newt_core::Serving::Instance) {
            backend.api = api;
        }
        let pending_token = if let Some(key) = api_key {
            let pending = collect_wizard_token(op, &key, config_path, &backend.name)?;
            backend.api_key_file = Some(pending.reference.handle().to_string());
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
    op: &Operator<'_>,
    client: &reqwest::Client,
    hit: &EndpointProbeResult,
    model: &str,
    mut api_key: Option<Secret>,
) -> anyhow::Result<(Option<Secret>, Option<newt_core::config::OpenAiApi>)> {
    for attempt in 0..GENERATION_CHECK_ATTEMPTS {
        if api_key
            .as_ref()
            .is_some_and(|key| !key.expose().trim().is_empty())
        {
            validate_authenticated_target(&hit.endpoint)?;
        }
        op.say(&format!(
            "Testing a minimal generation against {}…",
            hit.endpoint
        ));
        match newt_core::backend_probe::verify_generation(
            client,
            hit.kind,
            None,
            &hit.endpoint,
            model,
            api_key.as_ref().map(Secret::expose),
        )
        .await
        {
            GenerationCheck::Accepted(api) => {
                op.say("  ✓ generation accepted");
                // A tool-free chat proves generation/authentication, but it
                // cannot prove that agent tool calls work on Chat Completions.
                // Leave Chat unpinned so the runtime tool-capability probe can
                // choose; Responses is safe to persist after definitive
                // Chat-unavailable fallback.
                let api = api.filter(|surface| *surface == newt_core::config::OpenAiApi::Responses);
                return Ok((api_key, api));
            }
            GenerationCheck::Rejected(code) => {
                op.say(&format!("  ✗ authentication rejected (HTTP {code})"));
                if attempt + 1 == GENERATION_CHECK_ATTEMPTS {
                    break;
                }
                validate_authenticated_target(&hit.endpoint)?;
                let Some(key) =
                    credentials::ask_secret(op, "API key (echoes as *, Enter to cancel): ")?
                else {
                    anyhow::bail!("setup cancelled after authentication rejection");
                };
                api_key = Some(key);
            }
            GenerationCheck::Unverified(reason) => {
                op.say(&format!("  Could not verify generation ({reason})."));
                anyhow::bail!("setup cancelled because generation verification did not pass");
            }
        }
    }
    anyhow::bail!("authentication was rejected {GENERATION_CHECK_ATTEMPTS} times")
}

/// Nothing answered but the operator knows the endpoint: collect the wire and
/// model, then require a real generation before returning a writable backend.
async fn manual_backend_entry(
    op: &Operator<'_>,
    client: &reqwest::Client,
    config_path: &Path,
) -> anyhow::Result<ConfiguredBackend> {
    let url = loop {
        let raw = op.ask(&interaction_form::text_field(
            "Endpoint URL",
            "e.g. http://host:8080",
        ))?;
        if !raw.trim().is_empty() {
            let normalized = normalize_url(raw.trim(), "http", 11434);
            break candidate_endpoints(&normalized, &Discovery::default())?
                .into_iter()
                .next()
                .expect("an explicit URL produces one candidate");
        }
        op.say("  An endpoint URL is required (e.g. http://host:8080).");
    };
    let picked = choose(
        op,
        &interaction_form::menu(
            "\nWire protocol:",
            "",
            &[
                ("1", "ollama            (POST /api/chat)"),
                (
                    "2",
                    "openai-compatible (POST /v1/chat/completions — llama.cpp, vLLM, gateways)",
                ),
            ],
        ),
        Some("1"),
    )?;
    let kind = match picked.as_str() {
        "2" => BackendKind::Openai,
        _ => BackendKind::Ollama,
    };
    let model = ask_model_name(op)?;
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
    let (api_key, api) = verify_custom_chat_with_retries(op, client, &hit, &model, None).await?;
    let (config, mut backend) =
        build_backend_pair(&name, &url, &model, kind, serving, None, "manual");
    let pending_token = if let Some(key) = api_key {
        let pending = collect_wizard_token(op, &key, config_path, &backend.name)?;
        backend.api_key_file = Some(pending.reference.handle().to_string());
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
    op: &Operator<'_>,
    client: &reqwest::Client,
    config_path: &Path,
) -> anyhow::Result<ConfiguredBackend> {
    let presets = provider_preset::resolve_presets(None);
    match select_hosted_provider(op, &presets)? {
        HostedProviderChoice::Preset(preset) => {
            configure_preset(op, client, &preset, config_path).await
        }
        HostedProviderChoice::CustomEndpoint => {
            configure_custom_host(op, client, config_path).await
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
    op: &Operator<'_>,
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
        op.say(&format!("  (unavailable: {label} — {reason})"));
    }
    if available.is_empty() {
        let _ = choose(
            op,
            &interaction_form::menu(
                "\nAvailable providers:",
                "",
                &[("0", "I have a URL (custom endpoint)")],
            ),
            Some("0"),
        )?;
        return Ok(HostedProviderChoice::CustomEndpoint);
    }
    let rows: Vec<String> = available
        .iter()
        .map(|(p, endpoint)| format!("{:<24}{}", p.label(), endpoint))
        .collect();
    match select_row_with_zero(op, &rows, "providers", "I have a URL (custom endpoint)")? {
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
    op: &Operator<'_>,
    client: &reqwest::Client,
    preset: &ProviderPreset,
    config_path: &Path,
) -> anyhow::Result<ConfiguredBackend> {
    op.say(&format!("\n{} — {}", preset.label(), preset.base_url));
    if let Some(description) = &preset.description {
        op.say(description);
    }
    if let Some(signup) = &preset.signup_url {
        op.say(&format!("Create an API key at {signup}"));
    }
    if !preset.default_headers.is_empty() {
        // Limitation L1 (docs/provider-presets.md): carried, not sent.
        op.say(&format!(
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
            let use_it = decide(
                op,
                &interaction_form::confirm(
                    format!("${var} is set in this shell. Use it?"),
                    "",
                    "yes, use it",
                    "no, paste a key instead",
                ),
                // Adopts a credential: blank must not consent.
                None,
            )?;
            if use_it {
                // Record the var that actually resolved, not [0].
                WizardCred {
                    api_key_env: Some(var),
                    api_key_file: None,
                    probe_key: Some(Secret::new(value)),
                    pending_token: None,
                }
            } else {
                preset_pasted_key(op, preset, config_path)?
            }
        } else {
            preset_pasted_key(op, preset, config_path)?
        }
    };

    if cred.probe_key.is_some() {
        let PresetSupport::Supported { endpoint, .. } = preset_support(preset) else {
            anyhow::bail!("preset {} is not usable on this build", preset.name);
        };
        validate_authenticated_target(&endpoint)?;
    }

    op.say(&format!(
        "Probing {} for available models…",
        preset.base_url
    ));
    let models =
        list_models_for_preset(client, preset, cred.probe_key.as_ref().map(Secret::expose)).await;
    let model = match models {
        Ok(m) if !m.is_empty() => select_model(op, &m)?,
        Ok(_) | Err(_) => {
            op.say("  Could not list models (endpoint unreachable or key not yet usable).");
            // Fallback ladder: curated list → single default → free entry.
            match preset.fallback_models.len() {
                0 => ask_model_name(op)?,
                1 => {
                    let default = &preset.fallback_models[0];
                    let raw = op.ask(&interaction_form::text_field(
                        "Model name",
                        format!("[{default}] — Enter keeps it"),
                    ))?;
                    if raw.trim().is_empty() {
                        default.clone()
                    } else {
                        raw.trim().to_string()
                    }
                }
                _ => select_model(op, &preset.fallback_models)?,
            }
        }
    };

    // Field note: a bad token used to sail through setup (list endpoints
    // aren't uniformly auth-gated) and only 401 on the first real message.
    // Test the credential NOW, with re-entry on rejection.
    let (cred, verified_api) =
        verify_key_with_retries(op, client, preset, config_path, &model, cred).await?;

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
    /// Held only to live-test the key before anything is written. Wrapped,
    /// so the one place that needs the bytes has to say `expose()`.
    probe_key: Option<Secret>,
    pending_token: Option<PendingWizardToken>,
}

/// Live-test the selected model before anything is written, with up to two
/// credential re-entries on 401/403. A public model catalog is never treated
/// as authentication evidence.
async fn verify_key_with_retries(
    op: &Operator<'_>,
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
            .as_ref()
            .is_some_and(|key| !key.expose().trim().is_empty())
        {
            validate_authenticated_target(&endpoint)?;
        }
        op.say(&format!(
            "Testing a minimal generation against {}…",
            preset.base_url
        ));
        match newt_core::backend_probe::verify_generation(
            client,
            kind,
            api,
            &endpoint,
            model,
            cred.probe_key.as_ref().map(Secret::expose),
        )
        .await
        {
            GenerationCheck::Accepted(api) => {
                op.say("  ✓ generation accepted");
                let api = api.filter(|surface| *surface == newt_core::config::OpenAiApi::Responses);
                return Ok((cred, api));
            }
            GenerationCheck::Rejected(code) => {
                op.say(&format!("  ✗ authentication rejected (HTTP {code})"));
                if attempt + 1 == GENERATION_CHECK_ATTEMPTS {
                    break;
                }
                if !decide(
                    op,
                    &interaction_form::confirm(
                        "Re-enter the key?",
                        "",
                        "yes, re-enter it",
                        "no, cancel setup",
                    ),
                    // Cancelling aborts setup; neither way is a no-op, so
                    // the operator says which.
                    None,
                )? {
                    anyhow::bail!("setup cancelled after authentication rejection");
                }
                cred = preset_pasted_key(op, preset, config_path)?;
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
    op: &Operator<'_>,
    preset: &ProviderPreset,
    config_path: &Path,
) -> anyhow::Result<WizardCred> {
    let key = credentials::ask_secret(op, "API key (echoes as *, Enter to skip): ")?;
    let Some(key) = key else {
        let var = preset
            .env_vars
            .first()
            .cloned()
            .unwrap_or_else(|| provider_preset::synthesized_env_var(&preset.name));
        op.say(&format!(
            "  No key — writing the backend anyway; export ${var} before use."
        ));
        return Ok(WizardCred {
            api_key_env: Some(var),
            api_key_file: None,
            probe_key: None,
            pending_token: None,
        });
    };
    let pending = collect_wizard_token(op, &key, config_path, &preset.name)?;
    let reference = pending.reference.handle().to_string();
    Ok(WizardCred {
        api_key_env: None,
        api_key_file: Some(reference),
        probe_key: Some(key),
        pending_token: Some(pending),
    })
}

fn collect_wizard_token(
    op: &Operator<'_>,
    token: &Secret,
    config_path: &Path,
    name: &str,
) -> anyhow::Result<PendingWizardToken> {
    op.say("Protect the stored key with a passphrase? Enter uses a machine-local key.");
    // An empty passphrase is a real choice — it selects the machine-local
    // identity — so `None` here means "no passphrase", not "read failed".
    let passphrase = credentials::ask_secret(op, "Passphrase (echoes as *): ")?
        .map(|pass| newt_core::secrets::SecretString::from(pass.expose().to_string()));
    let path = versioned_wizard_token_path(config_path, name)?;
    let reference = credentials::SealedSecret::new(&collapse_home(&path))?;
    Ok(PendingWizardToken {
        token: token.clone(),
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
    op: &Operator<'_>,
    _config_path: &Path,
    _name: &str,
    pending: &PendingWizardToken,
) -> anyhow::Result<String> {
    // The single point of use that genuinely needs the bytes: the age
    // encryptor. `expose()` is called here and its result is never stored.
    newt_core::secrets::store_token_at(
        &pending.path,
        pending.token.expose(),
        pending.passphrase.as_ref(),
    )
    .map_err(|e| anyhow::anyhow!(e))?;
    let reference = pending.reference.handle().to_string();
    op.say(&format!("  → stored encrypted at {reference}"));
    Ok(reference)
}

// ---------------------------------------------------------------------------
// Agent commit identity (one question, Enter skips)
// ---------------------------------------------------------------------------

/// Offer to set the agent commit identity after a successful write. Parses
/// `Name <email>`; saves via [`newt_core::AgentIdentity::save`] — the ONE
/// sanctioned writer (never open-code a second). Non-fatal: the backend
/// config already landed, so failures here only print.
fn offer_identity(op: &Operator<'_>) {
    let Some(path) = newt_core::AgentIdentity::user_identity_path() else {
        return;
    };
    if path.exists() {
        return; // already configured — don't nag on re-runs
    }
    let raw = match op.ask(&interaction_form::text_field(
        "Attribution for agent commits",
        "\"Name <email>\" — Enter keeps the default",
    )) {
        Ok(raw) => raw,
        Err(_) => return, // Esc here must not undo a completed setup
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return;
    }
    let Some((name, email)) = parse_identity_line(raw) else {
        op.say("  Expected \"Name <email>\" — skipped (set later with `newt identity set`).");
        return;
    };
    let identity = newt_core::AgentIdentity {
        name,
        email,
        ..Default::default()
    };
    match identity.save(&path) {
        Ok(()) => op.say(&format!("  Wrote {}.", path.display())),
        Err(e) => op.say(&format!("  Could not write identity ({e}) — skipped.")),
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
    op: &Operator<'_>,
    client: &reqwest::Client,
    url: &str,
) -> anyhow::Result<String> {
    op.say(&format!("Probing {url} for installed models…"));
    let models = fetch_ollama_models(client, url).await;

    let models = match models {
        Ok(m) if !m.is_empty() => m,
        Ok(_) => {
            op.say("  Endpoint answered but listed no models.");
            return ask_model_name(op);
        }
        Err(e) => {
            op.say(&format!("  Could not reach the endpoint ({e})."));
            return ask_model_name(op);
        }
    };

    select_model(op, &models)
}

/// Above this many models, a flat numbered list stops being a menu and starts
/// being a wall — a llama.cpp router routinely serves 30+. At or below it, the
/// list is faster to read than any filter prompt would be.
const FILTER_THRESHOLD: usize = 9;

/// Choose one entry from `models`, filtering first when the list is long.
///
/// Deliberately built on the line-based seam rather than a raw-mode
/// arrow-key widget: setup frequently runs over SSH, piped, or with stdin
/// redirected, and a raw-mode picker would either hang or have to be bypassed
/// in exactly those cases. Filtering gets the same "find it among 36" result
/// while staying pipe-safe and unit-testable.
fn select_model(op: &Operator<'_>, models: &[String]) -> anyhow::Result<String> {
    let idx = select_row(op, models, "models")?;
    Ok(models[idx].clone())
}

/// The generic filterable numbered picker `select_model` and
/// `select_hosted_provider` share: same threshold, same blank-filter and
/// no-match-falls-back-to-all semantics, same pipe-safety. Returns the
/// index into the ORIGINAL `rows` slice.
fn select_row(op: &Operator<'_>, rows: &[String], noun: &str) -> anyhow::Result<usize> {
    Ok(select_row_with_zero(op, rows, noun, "")?.expect("no zero choice was offered"))
}

fn select_row_with_zero(
    op: &Operator<'_>,
    rows: &[String],
    noun: &str,
    zero: &str,
) -> anyhow::Result<Option<usize>> {
    let mut pool: Vec<(usize, &String)> = rows.iter().enumerate().collect();

    if pool.len() > FILTER_THRESHOLD {
        op.say(&format!("\n{} {noun} available.", pool.len()));
        let hint = if zero.is_empty() {
            "blank = show all".to_string()
        } else {
            format!("blank = show all, 0 = {zero}")
        };
        let needle = op.ask(&interaction_form::text_field("Filter", hint))?;
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
                op.say(&format!("  No match for {needle:?}; showing all."));
            } else {
                pool = matched;
            }
        }
    }

    let mut rows: Vec<(String, String)> = Vec::with_capacity(pool.len() + 1);
    if !zero.is_empty() {
        rows.push(("0".to_string(), zero.to_string()));
    }
    rows.extend(
        pool.iter()
            .enumerate()
            .map(|(i, (_, row))| ((i + 1).to_string(), (*row).clone())),
    );
    let entries: Vec<(&str, &str)> = rows.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let picked = choose(
        op,
        &interaction_form::menu(format!("\nAvailable {noun}:"), "", &entries),
        Some("1"),
    )?;
    // The zero entry, when offered, means "none of these".
    if !zero.is_empty() && picked == "0" {
        return Ok(None);
    }
    let index = picked.parse::<usize>().unwrap_or(1) - 1;
    Ok(Some(pool[index].0))
}

fn ask_model_name(op: &Operator<'_>) -> anyhow::Result<String> {
    let default = "llama3.1:8b";
    let raw = op.ask(&interaction_form::text_field(
        "Model name",
        format!("[{default}] — Enter keeps it"),
    ))?;
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
mod tests;
