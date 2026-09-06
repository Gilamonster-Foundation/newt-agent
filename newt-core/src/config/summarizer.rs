//! Dedicated summarizer backend settings and separate-file resolution.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{NewtError, Result};

#[cfg(doc)]
use super::BackendConfig;
use super::{resolve_api_key_common, BackendKind, Config};

fn default_summarizer_timeout_secs() -> u64 {
    60
}

fn default_summarizer_retries() -> u32 {
    1
}

/// Dedicated configuration for the compression summarizer, loaded from
/// `~/.newt/summarizer.toml` (Step 24.10, #559). An absent file means
/// `SummarizerConfig::default()` — every field falls back to the session
/// backend, so behavior is unchanged from "summarizer reuses the session
/// model".
///
/// The point of the separate file is the **own-backend** fields
/// (`endpoint`/`model`/`kind`/`api_key_file`): a summarizer can run on a
/// different, fast box than the session model instead of contending with it
/// (the #548 field incident — a slow primary summarizer stalled ~189s before
/// the static marker). `timeout_secs` / `retries` / `fallback_model` are the
/// knobs that used to live under `[tui]` (moved here in 24.10).
///
/// Example `~/.newt/summarizer.toml`:
/// ```toml
/// endpoint = "http://REDACTED-HOST:11434"  # default: session backend URL
/// model    = "qwen2.5-coder:3b"            # default: session model
/// kind     = "ollama"                      # "ollama" | "openai"
/// timeout_secs   = 45
/// retries        = 1
/// fallback_model = "nemotron-mini:4b"      # else preference-list auto-pick (24.9)
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SummarizerConfig {
    /// Summarizer endpoint URL. `None` ⇒ reuse the session backend's URL.
    pub endpoint: Option<String>,
    /// Summarizer model. `None` ⇒ reuse the session backend's model.
    pub model: Option<String>,
    /// Backend protocol. `None` ⇒ reuse the session backend's kind.
    pub kind: Option<BackendKind>,
    /// For `kind = "embedded"` (#661 group C): the local GGUF model file for the
    /// in-process candle summarizer. Ignored for HTTP backends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_path: Option<String>,
    /// Bearer-token file (first non-empty line). `None` ⇒ reuse the session key.
    pub api_key_file: Option<String>,
    /// Bearer-token environment variable (checked before `api_key_file`).
    pub api_key_env: Option<String>,
    /// Per-request timeout (seconds). Default 60 — cold-loading a big model can
    /// legitimately exceed it; raise on a slow box that falls back to the marker.
    #[serde(default = "default_summarizer_timeout_secs")]
    pub timeout_secs: u64,
    /// Retry attempts before the static marker. Default 1 — each attempt can
    /// cost the full `timeout_secs` (the #548 189s incident was 3 × 60s).
    #[serde(default = "default_summarizer_retries")]
    pub retries: u32,
    /// Explicit fallback model. `None` ⇒ for an Ollama summarizer backend, the
    /// first installed small-model-preference-list entry is auto-picked (24.9).
    pub fallback_model: Option<String>,
    /// `keep_alive` for the warm + summary requests. `None` ⇒ inherit
    /// `[tui].keep_alive`.
    pub keep_alive: Option<String>,
}

impl Default for SummarizerConfig {
    fn default() -> Self {
        Self {
            endpoint: None,
            model: None,
            kind: None,
            model_path: None,
            api_key_file: None,
            api_key_env: None,
            timeout_secs: default_summarizer_timeout_secs(),
            retries: default_summarizer_retries(),
            fallback_model: None,
            keep_alive: None,
        }
    }
}

impl SummarizerConfig {
    /// Parse a `summarizer.toml` body. Pure — fully unit-testable without disk.
    pub fn from_toml_str(text: &str) -> Result<Self> {
        toml::from_str(text).map_err(|e| NewtError::Config(e.to_string()))
    }

    /// Load `~/.newt/summarizer.toml` (or `$NEWT_SUMMARIZER_CONFIG`). A missing
    /// file is not an error — it yields [`SummarizerConfig::default`] (reuse the
    /// session backend). Only a present-but-malformed file errors.
    pub fn resolve() -> Result<Self> {
        for path in Self::candidate_paths() {
            if path.is_file() {
                let text = std::fs::read_to_string(&path)?;
                return Self::from_toml_str(&text);
            }
        }
        Ok(Self::default())
    }

    /// Ordered candidate paths for `summarizer.toml`.
    fn candidate_paths() -> Vec<PathBuf> {
        let mut paths = Vec::new();
        if let Ok(p) = std::env::var("NEWT_SUMMARIZER_CONFIG") {
            paths.push(PathBuf::from(p));
        }
        if let Some(dir) = Config::user_config_dir() {
            paths.push(dir.join("summarizer.toml"));
        }
        paths
    }

    /// Resolve this summarizer's bearer token (env var first, then file —
    /// plaintext or encrypted), or `None` — the same
    /// [`resolve_api_key_common`] rule as [`BackendConfig::resolve_api_key`]
    /// (the mirrored body it used to carry is gone).
    pub fn resolve_api_key(&self) -> Option<String> {
        match resolve_api_key_common(self.api_key_env.as_deref(), self.api_key_file.as_deref()) {
            Ok(v) => return v,
            Err(e) => {
                crate::secrets::warn_once(self.api_key_file.as_deref().unwrap_or("summarizer"), &e);
            }
        }
        None
    }
}
