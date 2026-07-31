//! First-run setup — offer, then bootstrap.
//!
//! Triggered when `~/.newt/config.toml` does not exist (and by `newt init`).
//!
//! A human at a terminal is **offered the interactive wizard** for ten seconds
//! before anything is written. Previously this path probed silently and
//! committed the operator to whatever endpoint answered first — so someone
//! whose inference lived on another box got a localhost config and no
//! indication a choice had been made for them (#1453).
//!
//! Everything else — CI, image builds, piped invocations — skips the offer
//! entirely and takes the original probe-and-write path, so unattended installs
//! behave exactly as before. The countdown is what makes *asking* safe by
//! default: without it, offering would mean blocking forever on a human who may
//! not be there.
//!
//! Agent commit identity is separate (`agent-identity.toml` /
//! `newt identity set`); a future setup dialog should collect it via
//! [`newt_core::AgentIdentity::save`].

use std::io::{self, Write};
use std::time::{Duration, Instant};

use newt_core::Config;

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Run first-run setup if `~/.newt/config.toml` does not exist; no-op when it
/// already does.
pub fn maybe_run(color: bool) -> anyhow::Result<()> {
    let config_path = match Config::user_config_path() {
        Some(p) => p,
        None => return Ok(()), // can't determine home dir — skip
    };
    if config_path.exists() {
        return Ok(());
    }
    // A human at a terminal gets asked. Anyone else — CI, an image build, a
    // piped invocation — falls straight through to the probe, so unattended
    // installs are unaffected.
    if offer_interactive(color, COUNTDOWN)? == FirstRun::Interactive {
        return crate::setup::run(color);
    }
    run_setup(color, &config_path)
}

/// How long the first-run offer waits before proceeding on its own.
const COUNTDOWN: Duration = Duration::from_secs(10);

/// What the operator chose at the first-run offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FirstRun {
    /// Run the interactive wizard — a key was pressed.
    Interactive,
    /// Probe and write a default config — the countdown elapsed, or there is
    /// nobody to ask.
    Defaults,
}

/// Offer the interactive wizard for `budget`, counting down in place.
///
/// The countdown exists so that *offering* is safe by default. Without it,
/// first run either blocks forever waiting for a human who may not be there, or
/// — as it did before — silently commits the operator to whatever endpoint it
/// happened to probe, never revealing that a choice existed.
///
/// Returns [`FirstRun::Defaults`] immediately when stdin or stdout is not a
/// terminal, or when raw mode cannot be entered: a prompt nobody can answer is
/// worse than no prompt.
pub(crate) fn offer_interactive(color: bool, budget: Duration) -> anyhow::Result<FirstRun> {
    use std::io::IsTerminal;

    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Ok(FirstRun::Defaults);
    }
    let dim = if color { "\x1b[38;2;100;100;100m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };

    println!();
    println!(
        "{dim}newt v{} — no configuration yet.{reset}",
        crate::VERSION
    );

    if crossterm::terminal::enable_raw_mode().is_err() {
        return Ok(FirstRun::Defaults);
    }
    let outcome = countdown(budget, dim, reset);
    let _ = crossterm::terminal::disable_raw_mode();

    let mut out = io::stdout();
    // Clear the countdown line so neither branch leaves it on screen.
    let _ = write!(out, "\r\x1b[K");
    let _ = out.flush();
    match &outcome {
        Ok(FirstRun::Interactive) => println!("running setup…\r"),
        _ => println!("{dim}continuing with defaults — `newt setup` to change{reset}\r"),
    }
    outcome
}

/// The raw-mode wait. Split out so the surrounding mode handling is not
/// duplicated across every early return.
fn countdown(budget: Duration, dim: &str, reset: &str) -> anyhow::Result<FirstRun> {
    let deadline = Instant::now() + budget;
    let mut out = io::stdout();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let _ = write!(
            out,
            "\r\x1b[K{dim}Press any key to choose your inference endpoint — \
             continuing in {}s…{reset}",
            left.as_secs() + 1
        );
        let _ = out.flush();
        // Poll in slices so the digit ticks while still reacting immediately.
        if crossterm::event::poll(left.min(Duration::from_millis(200)))?
            && matches!(crossterm::event::read()?, crossterm::event::Event::Key(_))
        {
            return Ok(FirstRun::Interactive);
        }
    }
    Ok(FirstRun::Defaults)
}

/// Force setup to run, (re)writing config even if it already exists. Used by
/// `newt init`.
pub fn run_init(color: bool) -> anyhow::Result<()> {
    let config_path =
        Config::user_config_path().unwrap_or_else(|| std::path::PathBuf::from("newt.toml"));
    run_setup(color, &config_path)
}

// ---------------------------------------------------------------------------
// Setup (no prompts — probe, auto-select, write)
// ---------------------------------------------------------------------------

fn run_setup(color: bool, config_path: &std::path::Path) -> anyhow::Result<()> {
    let accent = if color { "\x1b[38;2;220;60;20m" } else { "" };
    let dim = if color { "\x1b[38;2;100;100;100m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };

    println!();
    println!("{accent}newt v{} — first-run setup{reset}", crate::VERSION);
    println!("{dim}Probing common Ollama endpoints…{reset}");

    let candidates = probe_candidates();
    let found = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(probe_all(&candidates))
    });

    // Auto-select: the first reachable endpoint (probe order is priority order —
    // NEWT_DGX_HOST first, then localhost, then home-lab hosts) and its first
    // model. Fall back to localhost + a sensible default when nothing answers,
    // so a config file always gets written for the user to edit.
    let (url, model, note) = match found.into_iter().next() {
        Some(ep) => {
            // Skip embedding-only models (issue: first-run picked nomic-embed-text
            // as the chat default — it can't converse). Fall back to a real chat
            // model name when the endpoint served only embedding models.
            let model = pick_default_model(&ep.models).unwrap_or_else(|| "llama3.1:8b".to_string());
            (ep.url, model, "reachable")
        }
        None => (
            "http://localhost:11434".to_string(),
            "llama3.1:8b".to_string(),
            "no endpoint answered — wrote a default, edit to point at yours",
        ),
    };

    save_config(config_path, &url, &model)?;
    println!(
        "{dim}wrote {} → {url}  ({model})  [{note}]{reset}",
        config_path.display()
    );
    println!("{dim}edit that file to change endpoints, model, or permissions{reset}");
    println!();
    Ok(())
}

/// Write the first-run config (#1140, epic #1126): ONE backend drop-in
/// `~/.newt/backends/ollama.toml` (endpoint + probed-model hint + provenance)
/// and a minimal `config.toml` whose `default_backend` points at it. No legacy
/// `[dgx]` block, no inline `[[backends]]` — the two-dialect chimera is dead.
fn save_config(path: &std::path::Path, url: &str, model: &str) -> anyhow::Result<()> {
    let backend = newt_core::BackendConfig {
        name: "ollama".into(),
        endpoint: url.to_string(),
        // A HINT, not authority: session start adopts what the server
        // actually serves (#1139); this keeps offline launches sane.
        model: Some(model.to_string()),
        kind: Some(newt_core::BackendKind::Ollama),
        serving: Some(newt_core::Serving::Multiplexer),
        provenance: Some(newt_core::config::BackendProvenance {
            source: Some(format!("newt init v{}", crate::VERSION)),
            probed: Some(chrono::Local::now().format("%Y-%m-%d").to_string()),
            derived_serving: Some(true),
        }),
        ..Default::default()
    };
    newt_core::write_backend_dropin(path, &backend).map_err(|e| anyhow::anyhow!(e))?;
    let config = Config {
        backends: vec![], // the drop-in IS the backend list
        default_backend: Some(backend.name.clone()),
        ..Default::default()
    };
    config.save(path)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Probing
// ---------------------------------------------------------------------------

struct FoundEndpoint {
    url: String,
    models: Vec<String>,
}

/// Whether a model name looks embedding-only — a heuristic on the name (Ollama's
/// `/api/tags` doesn't flag capability). Auto-setup must not pick one as the chat
/// default; catches the common families (`nomic-embed-text`, `mxbai-embed`,
/// `*-embed-*`). A mislabelled model is one `config.toml` edit away.
fn is_embedding_model(name: &str) -> bool {
    name.to_ascii_lowercase().contains("embed")
}

/// The auto-setup default model: the first served model that isn't
/// embedding-only, else `None` (the caller supplies the chat-model fallback, so
/// an endpoint serving only embedding models still writes a usable name to edit).
fn pick_default_model(models: &[String]) -> Option<String> {
    models.iter().find(|m| !is_embedding_model(m)).cloned()
}

fn probe_candidates() -> Vec<String> {
    // Localhost only. Two `http://REDACTED-HOST:11434` entries used to sit here
    // — a previous scrub replaced real hostnames with the placeholder text and
    // left them in as live probe targets, so every first run spent 2s apiece
    // resolving a host literally named "REDACTED-HOST". Operator-specific hosts
    // belong in the env override below, never baked into a public default.
    let mut candidates = vec!["http://localhost:11434".to_string()];
    // Probe NEWT_DGX_HOST first when set.
    if let Ok(host) = std::env::var("NEWT_DGX_HOST") {
        let scheme = std::env::var("NEWT_DGX_SCHEME").unwrap_or_else(|_| "http".into());
        let port = std::env::var("NEWT_DGX_OLLAMA_PORT").unwrap_or_else(|_| "11434".into());
        let url = format!("{scheme}://{host}:{port}");
        if !candidates.contains(&url) {
            candidates.insert(0, url);
        }
    }
    candidates
}

async fn probe_all(candidates: &[String]) -> Vec<FoundEndpoint> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .unwrap_or_default();

    let mut handles = Vec::new();
    for url in candidates {
        let url = url.clone();
        let c = client.clone();
        handles.push(tokio::spawn(async move {
            let models = fetch_models(&c, &url).await.ok()?;
            Some(FoundEndpoint { url, models })
        }));
    }

    let mut found = Vec::new();
    for h in handles {
        if let Ok(Some(ep)) = h.await {
            found.push(ep);
        }
    }
    found
}

async fn fetch_models(client: &reqwest::Client, url: &str) -> anyhow::Result<Vec<String>> {
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {

    #[test]
    fn pick_default_model_skips_embedding_models() {
        // Regression: first-run picked nomic-embed-text (an embedding model) as
        // the chat default because it sorted first. Prefer a real chat model.
        let models = vec![
            "nomic-embed-text:latest".to_string(),
            "qwen2.5-coder:7b".to_string(),
        ];
        assert_eq!(
            super::pick_default_model(&models).as_deref(),
            Some("qwen2.5-coder:7b")
        );
    }

    #[test]
    fn pick_default_model_none_when_all_embedding_or_empty() {
        // Only embedding models → None, so the caller keeps its chat-model
        // fallback rather than writing an unusable embedding default.
        let embed = vec![
            "nomic-embed-text:latest".to_string(),
            "mxbai-embed-large".to_string(),
        ];
        assert!(super::pick_default_model(&embed).is_none());
        assert!(super::pick_default_model(&[]).is_none());
    }

    use super::*;

    /// Non-terminal stdin/stdout must never see the offer: CI, image builds and
    /// piped invocations have to keep taking the silent probe path.
    #[test]
    fn a_non_terminal_never_gets_offered_the_wizard() {
        // The test harness captures stdout, so neither handle is a terminal —
        // which is exactly the unattended shape this must return on.
        assert_eq!(
            offer_interactive(false, Duration::from_secs(10)).unwrap(),
            FirstRun::Defaults
        );
    }

    /// And it must return *immediately*, not after burning the countdown. An
    /// unattended install that stalls ten seconds per launch is a regression
    /// even though it eventually does the right thing.
    #[test]
    fn the_offer_does_not_stall_an_unattended_run() {
        let start = Instant::now();
        let _ = offer_interactive(false, Duration::from_secs(10));
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "returned in {:?}; must not wait out the countdown with nobody to ask",
            start.elapsed()
        );
    }

    /// The budget is the contract: ten seconds, so a distracted operator has
    /// time to react and an unattended box is not held up for long.
    #[test]
    fn the_countdown_budget_is_ten_seconds() {
        assert_eq!(COUNTDOWN, Duration::from_secs(10));
    }

    /// The probe list must stay free of operator-specific hosts. Two
    /// `http://REDACTED-HOST:11434` entries once sat here — a scrub replaced
    /// real hostnames with the placeholder and left them in as LIVE targets, so
    /// every first run spent 2s apiece resolving a host literally named
    /// "REDACTED-HOST".
    #[test]
    fn the_probe_list_carries_no_baked_in_hosts() {
        for candidate in probe_candidates() {
            assert!(
                candidate.contains("localhost") || candidate.contains("127.0.0.1"),
                "first-run probes only loopback unless told otherwise; found {candidate}"
            );
        }
    }

    #[test]
    fn probe_candidates_includes_localhost() {
        assert!(probe_candidates().iter().any(|u| u.contains("localhost")));
    }

    #[test]
    fn probe_candidates_includes_env_host() {
        std::env::set_var("NEWT_DGX_HOST", "myhost.local");
        let c = probe_candidates();
        std::env::remove_var("NEWT_DGX_HOST");
        assert!(c.iter().any(|u| u.contains("myhost.local")));
        // env host is probed first
        assert!(c[0].contains("myhost.local"));
    }

    #[serial_test::serial(real_fs)]
    #[test]
    fn save_config_writes_endpoint_and_model() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        save_config(&path, "http://localhost:11434", "gemma4:e2b").unwrap();
        // The endpoint + model live in the DROP-IN now, not config.toml (#1140).
        let raw_dropin =
            std::fs::read_to_string(path.with_file_name("backends").join("ollama.toml")).unwrap();
        assert!(raw_dropin.contains("11434"));
        assert!(raw_dropin.contains("gemma4:e2b"));
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.contains("[dgx]"), "chimera dead: {written}");
        // Round-trips through the real loader: minimal config + drop-in.
        let cfg = Config::load(&path).unwrap();
        assert!(cfg.dgx.is_none(), "no legacy [dgx] block (#1140)");
        assert_eq!(cfg.default_backend.as_deref(), Some("ollama"));
        let dropin = path.with_file_name("backends").join("ollama.toml");
        let b: newt_core::BackendConfig =
            toml::from_str(&std::fs::read_to_string(&dropin).unwrap()).unwrap();
        assert_eq!(
            b.effective_model().map(str::to_string).as_deref(),
            Some("gemma4:e2b")
        );
    }
}
