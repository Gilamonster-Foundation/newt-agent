//! Hermes-compatible MCP OAuth token loader.
//!
//! The hermes-agent stores OAuth tokens for HTTP MCP servers in
//! `~/.hermes/mcp-tokens/<name>.json`. This module reads those tokens and
//! refreshes them when expired so newt can connect to the same servers without
//! requiring a separate auth flow.
//!
//! Token file layout (all under `~/.hermes/mcp-tokens/`):
//!   `<name>.json`        — access_token, refresh_token, expires_at (Unix f64)
//!   `<name>.meta.json`   — token_endpoint (string)
//!   `<name>.client.json` — client_id (string)
//!
//! When a token is expired (or missing), we attempt a refresh_token grant using
//! the token_endpoint + client_id from the companion files. On success the new
//! tokens are written back so hermes picks them up too. If refresh fails (e.g.
//! the refresh_token itself expired, or the server requires a fresh interactive
//! browser flow), we log a warning and return `None` — the caller then tries to
//! connect without auth and will see the 401 from the server.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// The fields we care about from `<name>.json`.
#[derive(Debug, Clone, Deserialize, Serialize)]
struct TokenFile {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    /// Unix timestamp (floating-point seconds) when the access_token expires.
    #[serde(default)]
    expires_at: Option<f64>,
    // Pass through the rest so we can rewrite the file without losing fields.
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct MetaFile {
    token_endpoint: String,
}

#[derive(Debug, Deserialize)]
struct ClientFile {
    client_id: String,
}

/// Response from the refresh token endpoint.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: Option<f64>,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

fn hermes_token_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let dir = PathBuf::from(home).join(".hermes").join("mcp-tokens");
    if dir.is_dir() { Some(dir) } else { None }
}

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Try to refresh an expired access_token using the refresh_token grant.
/// Returns the new access_token on success.
async fn try_refresh(
    name: &str,
    tok: &TokenFile,
    token_dir: &Path,
) -> Option<String> {
    let refresh_token = tok.refresh_token.as_deref()?;

    let meta: MetaFile = read_json(&token_dir.join(format!("{name}.meta.json")))?;
    let reg: ClientFile = read_json(&token_dir.join(format!("{name}.client.json")))?;

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .ok()?;

    let resp = http
        .post(&meta.token_endpoint)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", &reg.client_id),
        ])
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        tracing::warn!(
            "MCP OAuth refresh for `{name}` failed: HTTP {}",
            resp.status()
        );
        return None;
    }

    let new_tok: TokenResponse = resp.json().await.ok()?;

    // Write the refreshed tokens back so hermes picks them up.
    let expires_at = new_tok
        .expires_in
        .map(|secs| unix_now() + secs);

    let mut updated = tok.extra.clone();
    updated.insert(
        "access_token".into(),
        serde_json::Value::String(new_tok.access_token.clone()),
    );
    if let Some(rt) = &new_tok.refresh_token {
        updated.insert(
            "refresh_token".into(),
            serde_json::Value::String(rt.clone()),
        );
    } else {
        // Keep the old refresh_token if the server didn't rotate it.
        updated.insert(
            "refresh_token".into(),
            serde_json::Value::String(refresh_token.to_owned()),
        );
    }
    if let Some(ea) = expires_at {
        updated.insert(
            "expires_at".into(),
            serde_json::Value::from(ea),
        );
    }
    if let Some(ei) = new_tok.expires_in {
        updated.insert("expires_in".into(), serde_json::Value::from(ei));
    }
    // Merge extra fields from the response.
    for (k, v) in &new_tok.extra {
        updated.entry(k.clone()).or_insert_with(|| v.clone());
    }

    let path = token_dir.join(format!("{name}.json"));
    if let Ok(json) = serde_json::to_string_pretty(&updated) {
        let _ = std::fs::write(&path, json);
    }

    tracing::info!("MCP OAuth token refreshed for `{name}`");
    Some(new_tok.access_token)
}

/// Return a valid Bearer token for `server_name` if one is stored in
/// `~/.hermes/mcp-tokens/`. Refreshes the token if it is expired.
/// Returns `None` if no token is available (no file, refresh failed, etc.).
pub async fn load_bearer_token(server_name: &str) -> Option<String> {
    let dir = hermes_token_dir()?;
    let path = dir.join(format!("{server_name}.json"));
    let tok: TokenFile = read_json(&path)?;

    // Consider the token valid if it expires more than 30 seconds from now.
    let is_valid = tok
        .expires_at
        .map(|ea| ea - unix_now() > 30.0)
        .unwrap_or(false);

    if is_valid {
        tracing::debug!("MCP OAuth token for `{server_name}` is current");
        return Some(tok.access_token.clone());
    }

    tracing::debug!(
        "MCP OAuth token for `{server_name}` is expired — attempting refresh"
    );
    try_refresh(server_name, &tok, &dir).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_now_is_plausible() {
        // 2025-01-01 in Unix time is about 1_735_689_600.
        assert!(unix_now() > 1_735_689_600.0);
    }
}
