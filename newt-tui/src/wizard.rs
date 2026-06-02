//! First-run setup — functional, non-interactive.
//!
//! Triggered automatically when `~/.newt/config.toml` does not exist (and by
//! `newt init`). Probes for a reachable Ollama endpoint, auto-selects the
//! best one and a model, and writes a minimal `~/.newt/config.toml` so
//! subsequent runs skip setup. There is **no** interactive UI: this is a
//! functional bootstrap, not a settings UX. Edit `~/.newt/config.toml`
//! directly to change anything (see `newt config` to print the resolved view).

use newt_core::{Config, DgxConfig};

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
            let model = ep
                .models
                .into_iter()
                .next()
                .unwrap_or_else(|| "llama3.1:8b".to_string());
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

fn save_config(path: &std::path::Path, url: &str, model: &str) -> anyhow::Result<()> {
    let mut config = Config::default();
    let node = newt_core::DgxNode {
        name: "default".into(),
        ollama: Some(url.to_string()),
        ..Default::default()
    };
    config.dgx = Some(DgxConfig {
        active_model: Some(model.to_string()),
        nodes: vec![node],
        ..Default::default()
    });
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

fn probe_candidates() -> Vec<String> {
    let mut candidates = vec![
        "http://localhost:11434".to_string(),
        "http://dgx1.home.lab:11434".to_string(),
        "http://ollama.home.lab:11434".to_string(),
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

    #[test]
    fn save_config_writes_endpoint_and_model() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        save_config(&path, "http://localhost:11434", "gemma4:e2b").unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("11434"));
        assert!(written.contains("gemma4:e2b"));
        // Round-trips through the real loader.
        let cfg = Config::load(&path).unwrap();
        assert_eq!(
            cfg.dgx.and_then(|d| d.active_model).as_deref(),
            Some("gemma4:e2b")
        );
    }
}
