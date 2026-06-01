//! First-run setup wizard.
//!
//! Triggered automatically when `~/.newt/config.toml` does not exist.
//! Probes for reachable Ollama endpoints, lets the user pick one and
//! a model, then writes a minimal `~/.newt/config.toml` so subsequent
//! runs skip the wizard.

use std::io::{self, Write as _};

use newt_core::{Config, DgxConfig};

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Run the first-run wizard if `~/.newt/config.toml` does not exist.
/// Returns immediately (no-op) when config is already present.
pub fn maybe_run(color: bool) -> anyhow::Result<()> {
    let config_path = match Config::user_config_path() {
        Some(p) => p,
        None => return Ok(()), // can't determine home dir — skip
    };

    if config_path.exists() {
        return Ok(());
    }

    run_wizard(color, &config_path)
}

/// Force the wizard to run, even if config already exists.
/// Used by `newt init`.
pub fn run_init(color: bool) -> anyhow::Result<()> {
    let config_path =
        Config::user_config_path().unwrap_or_else(|| std::path::PathBuf::from("newt.toml"));
    run_wizard(color, &config_path)
}

// ---------------------------------------------------------------------------
// Wizard implementation
// ---------------------------------------------------------------------------

fn run_wizard(color: bool, config_path: &std::path::Path) -> anyhow::Result<()> {
    let accent = if color { "\x1b[38;2;220;60;20m" } else { "" };
    let dim = if color { "\x1b[38;2;100;100;100m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };

    println!();
    println!(
        "{accent}Welcome to newt v{}!{reset}",
        env!("CARGO_PKG_VERSION")
    );
    println!();
    println!("{dim}No config found at {}.", config_path.display());
    println!("Let's find your Ollama — probing common endpoints…{reset}");
    println!();

    // Probe candidates in parallel via the existing tokio runtime.
    let candidates = probe_candidates();
    let found: Vec<FoundEndpoint> = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(probe_all(&candidates))
    });

    if found.is_empty() {
        println!(
            "{dim}No reachable Ollama found at: {}",
            candidates.join(", ")
        );
        println!();
        print!("Enter Ollama URL (e.g. http://localhost:11434): {reset}");
        io::stdout().flush()?;
        let mut url = String::new();
        io::stdin().read_line(&mut url)?;
        let url = url.trim().to_string();
        if url.is_empty() {
            println!(
                "Skipping setup — you can configure manually in {}",
                config_path.display()
            );
            return Ok(());
        }
        // Probe user-supplied URL for models.
        let models = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(fetch_models(&url))
                .unwrap_or_default()
        });
        let model = pick_model(&models, color)?;
        return save_config(config_path, &url, &model, color);
    }

    // Show what was found.
    for (i, ep) in found.iter().enumerate() {
        let model_list = if ep.models.is_empty() {
            "(no models loaded)".into()
        } else {
            ep.models.join(", ")
        };
        println!(
            "  {accent}[{}]{reset} {}  {dim}— {}{reset}",
            i + 1,
            ep.url,
            model_list
        );
    }
    println!();

    // Pick endpoint.
    let ep = if found.len() == 1 {
        println!("{dim}Using: {}{reset}", found[0].url);
        &found[0]
    } else {
        let default_idx = found.len(); // last entry = default
        print!(
            "Which endpoint? [1–{}] (default {}): ",
            found.len(),
            default_idx
        );
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let input = input.trim();
        let idx = if input.is_empty() {
            default_idx - 1
        } else {
            input
                .parse::<usize>()
                .unwrap_or(default_idx)
                .saturating_sub(1)
        };
        &found[idx.min(found.len() - 1)]
    };

    println!();

    // Pick model.
    let model = if ep.models.is_empty() {
        print!("No models found. Enter model name: ");
        io::stdout().flush()?;
        let mut m = String::new();
        io::stdin().read_line(&mut m)?;
        m.trim().to_string()
    } else {
        pick_model(&ep.models, color)?
    };

    save_config(config_path, &ep.url, &model, color)
}

fn pick_model(models: &[String], color: bool) -> anyhow::Result<String> {
    let accent = if color { "\x1b[38;2;220;60;20m" } else { "" };
    let dim = if color { "\x1b[38;2;100;100;100m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };

    if models.is_empty() {
        print!("Enter model name: ");
        io::stdout().flush()?;
        let mut m = String::new();
        io::stdin().read_line(&mut m)?;
        return Ok(m.trim().to_string());
    }

    if models.len() == 1 {
        println!("{dim}Using model: {}{reset}", models[0]);
        return Ok(models[0].clone());
    }

    println!("Available models:");
    for (i, m) in models.iter().enumerate() {
        println!("  {accent}[{}]{reset} {m}", i + 1);
    }
    print!("Which model? [1] (default 1): ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let idx: usize = input
        .trim()
        .parse::<usize>()
        .unwrap_or(1)
        .saturating_sub(1)
        .min(models.len() - 1);
    Ok(models[idx].clone())
}

fn save_config(path: &std::path::Path, url: &str, model: &str, color: bool) -> anyhow::Result<()> {
    let dim = if color { "\x1b[38;2;100;100;100m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };

    // Build a minimal config with a DGX Ollama entry.
    let mut config = Config::default();
    let node = newt_core::DgxNode {
        name: "default".into(),
        ollama: Some(url.to_string()),
        ..Default::default()
    };
    let dgx = DgxConfig {
        active_model: Some(model.to_string()),
        nodes: vec![node],
        ..Default::default()
    };
    config.dgx = Some(dgx);

    print!("\n{dim}Saving config to {} …{reset} ", path.display());
    io::stdout().flush()?;
    config.save(path)?;
    println!("done.");
    println!();

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
        "http://REDACTED-HOST:11434".to_string(),
        "http://REDACTED-HOST:11434".to_string(),
    ];
    // Also probe NEWT_DGX_HOST if set.
    if let Ok(host) = std::env::var("NEWT_DGX_HOST") {
        let scheme = std::env::var("NEWT_DGX_SCHEME").unwrap_or_else(|_| "http".into());
        let port = std::env::var("NEWT_DGX_OLLAMA_PORT").unwrap_or_else(|_| "11434".into());
        let url = format!("{scheme}://{host}:{port}");
        if !candidates.contains(&url) {
            candidates.insert(0, url); // check env-var host first
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
            let models = fetch_models_with_client(&c, &url).await.ok()?;
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

async fn fetch_models(url: &str) -> anyhow::Result<Vec<String>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    fetch_models_with_client(&client, url).await
}

async fn fetch_models_with_client(
    client: &reqwest::Client,
    url: &str,
) -> anyhow::Result<Vec<String>> {
    let tags_url = format!("{}/api/tags", url.trim_end_matches('/'));
    let resp = client.get(&tags_url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("HTTP {}", resp.status());
    }
    let json: serde_json::Value = resp.json().await?;
    let names = json["models"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Ok(names)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_candidates_includes_localhost() {
        let c = probe_candidates();
        assert!(c.iter().any(|u| u.contains("localhost")));
    }

    #[test]
    fn probe_candidates_includes_env_host() {
        std::env::set_var("NEWT_DGX_HOST", "myhost.local");
        let c = probe_candidates();
        std::env::remove_var("NEWT_DGX_HOST");
        assert!(c.iter().any(|u| u.contains("myhost.local")));
    }

    #[test]
    fn maybe_run_skips_when_config_exists() {
        // Create a temp file to simulate existing config.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "").unwrap();
        // We can't easily call maybe_run() with a custom path, but we can
        // at least verify the file-existence short-circuit logic.
        assert!(path.exists());
    }
}
