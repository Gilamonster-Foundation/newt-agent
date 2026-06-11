//! Ollama model warm-up for the agentic loop (moved from `newt-tui`, Step 9.7).
//! Pre-loads a cold model into VRAM so the first turn doesn't eat the 30–60 s
//! load time, and skips the load entirely when `/api/ps` says it's resident.

use super::display::{print_newt, print_retry_indicator};
use super::tui_retry_policy;
use crate::retry::with_backoff_notify;

/// Check whether `model` is currently loaded in Ollama's VRAM via `/api/ps`.
/// Returns `true` when the model is resident and ready; `false` when cold.
/// Silently returns `false` on any network or parse error so the caller always
/// falls through to the warm-up path — a false negative just means we warm
/// unnecessarily, which is safe.
fn is_model_resident(endpoint: &str, model: &str) -> bool {
    let ps_url = format!("{}/api/ps", endpoint.trim_end_matches('/'));
    let Ok(json) = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            let resp = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()?
                .get(&ps_url)
                .send()
                .await?;
            resp.json::<serde_json::Value>()
                .await
                .map_err(anyhow::Error::from)
        })
    }) else {
        return false;
    };
    json["models"]
        .as_array()
        .map(|arr| arr.iter().any(|m| m["name"].as_str() == Some(model)))
        .unwrap_or(false)
}

/// After a `/model <name>` switch, warm the new model if it isn't already
/// resident in Ollama's VRAM. Blocks until the model is loaded (or the
/// warm-up itself fails), then prints a ready line.
///
/// Skipped silently on non-Ollama endpoints (caller's responsibility).
pub fn warmup_if_cold(endpoint: &str, model: &str, keep_alive: &str, color: bool, verbose: bool) {
    if is_model_resident(endpoint, model) {
        print_newt(
            &format!("✓ {model} — already resident, ready"),
            color,
            verbose,
        );
        return;
    }

    // Print the warning BEFORE blocking so the user sees it immediately.
    let msg = format!("⏳ {model} is cold — warming up (large models can take 30–60 s)…");
    if color {
        crossterm::execute!(
            std::io::stdout(),
            crossterm::style::SetForegroundColor(crossterm::style::Color::Rgb {
                r: 200,
                g: 140,
                b: 0,
            }),
            crossterm::style::Print(format!("▸  {msg}\n")),
            crossterm::style::ResetColor,
        )
        .ok();
    } else {
        println!("▸  {msg}");
    }
    use std::io::Write as _;
    std::io::stdout().flush().ok();

    // Large models (70b Q8) can take 60+ seconds to load; use a generous
    // timeout and the same retry policy as the rest of the TUI.
    let warm_url = format!("{}/api/generate", endpoint.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": model,
        "keep_alive": keep_alive,
        "stream": false,
    });
    let retry = tui_retry_policy();
    let result = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(600))
                .build()?;
            with_backoff_notify(
                &retry,
                || async {
                    let resp = client
                        .post(&warm_url)
                        .json(&body)
                        .send()
                        .await
                        .map_err(|e| anyhow::anyhow!("request failed: {e}"))?;
                    if !resp.status().is_success() {
                        anyhow::bail!("Ollama {}", resp.status());
                    }
                    resp.json::<serde_json::Value>()
                        .await
                        .map_err(anyhow::Error::from)
                },
                |attempt, delay| print_retry_indicator(attempt, delay, color),
            )
            .await
        })
    });

    match result {
        Ok(json) => {
            let ready_msg = match json["load_duration"].as_u64() {
                Some(ns) if ns > 0 => {
                    format!("✓ {model} — loaded in {:.1}s, ready", ns as f64 / 1e9)
                }
                _ => format!("✓ {model} — ready"),
            };
            print_newt(&ready_msg, color, verbose);
        }
        Err(e) => print_newt(
            &format!("⚠ warm-up failed: {e} — first response may be slow"),
            color,
            verbose,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// `is_model_resident` returns `false` for a model not in the `/api/ps` list.
    #[tokio::test(flavor = "multi_thread")]
    async fn is_model_resident_returns_false_when_absent() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/ps"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [{"name": "other-model:7b"}]
            })))
            .mount(&server)
            .await;

        assert!(!is_model_resident(&server.uri(), "wanted-model:13b"));
    }

    /// `is_model_resident` returns `true` when the model appears in `/api/ps`.
    #[tokio::test(flavor = "multi_thread")]
    async fn is_model_resident_returns_true_when_present() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/ps"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [
                    {"name": "other:7b"},
                    {"name": "wanted-model:13b"}
                ]
            })))
            .mount(&server)
            .await;

        assert!(is_model_resident(&server.uri(), "wanted-model:13b"));
    }

    /// `is_model_resident` returns `false` (safe default) when the endpoint
    /// returns an error — the caller falls through to the warm-up path.
    #[tokio::test(flavor = "multi_thread")]
    async fn is_model_resident_returns_false_on_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/ps"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        assert!(!is_model_resident(&server.uri(), "any-model"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn warmup_skips_generate_when_model_already_resident() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/ps"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [{"name": "warm-model:8b"}]
            })))
            .mount(&server)
            .await;
        // The early-resident return must NOT hit /api/generate.
        Mock::given(method("POST"))
            .and(path("/api/generate"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        warmup_if_cold(&server.uri(), "warm-model:8b", "5m", false, false);
        server.verify().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn warmup_loads_cold_model_via_generate() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/ps"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"models": []})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/generate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "load_duration": 1_500_000_000u64
            })))
            .expect(1)
            .mount(&server)
            .await;

        warmup_if_cold(&server.uri(), "cold-model:70b", "5m", false, false);
        server.verify().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn warmup_failure_is_non_fatal() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/ps"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"models": []})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/generate"))
            .respond_with(ResponseTemplate::new(404).set_body_string("no such model"))
            .mount(&server)
            .await;

        // Must not panic or hang — the warning path swallows the error.
        warmup_if_cold(&server.uri(), "ghost-model", "5m", false, false);
    }
}
