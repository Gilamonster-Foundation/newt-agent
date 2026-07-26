//! First-run setup — functional, non-interactive.
//!
//! Triggered automatically when `~/.newt/config.toml` does not exist (and by
//! `newt init`). Probes for a reachable Ollama endpoint, auto-selects the
//! best one and a model, and writes a minimal `~/.newt/config.toml` so
//! subsequent runs skip setup. There is **no** interactive UI: this is a
//! functional bootstrap, not a settings UX. Edit `~/.newt/config.toml`
//! directly to change anything (see `newt config` to print the resolved view).

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
    run_setup(color, &config_path)
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
    println!(
        "{accent}newt v{} — first-run setup{reset}",
        env!("CARGO_PKG_VERSION")
    );
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
            source: Some(format!("newt init v{}", env!("CARGO_PKG_VERSION"))),
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
    let mut candidates = vec![
        "http://localhost:11434".to_string(),
        "http://REDACTED-HOST:11434".to_string(),
        "http://REDACTED-HOST:11434".to_string(),
    ];
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
