// Copyright (c) 2025-2026 newt contributors. Licensed under Apache-2.0.

//! # newt-tuner
//!
//! Model tuning configuration and management for Newt-Agent.
//! Provides per-model TOML configurations that ship defaults
//! but never overwrite user-customized settings.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Error type for tuning operations.
#[derive(Debug)]
pub enum TunerError {
    /// Failed to read the configuration directory.
    ReadConfigDir(String),
    /// Failed to parse a TOML file.
    ParseTOML(PathBuf, String),
    /// User config already exists — refusing to overwrite.
    UserConfigExists(PathBuf),
    /// Model not found in available configurations.
    ModelNotFound(String),
}

impl std::fmt::Display for TunerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReadConfigDir(path) => write!(f, "Cannot read config dir: {}", path),
            Self::ParseTOML(path, msg) => write!(f, "Failed to parse {}: {}", path.display(), msg),
            Self::UserConfigExists(path) => {
                write!(
                    f,
                    "User config exists at {}, refusing to overwrite",
                    path.display()
                )
            }
            Self::ModelNotFound(model) => write!(f, "Model not found: {}", model),
        }
    }
}

impl std::error::Error for TunerError {}

/// Inference-parameter overrides for a specific model name.
///
/// Matched against the active model by exact string equality.  Add entries
/// under `[[model_tuning]]` in `~/.newt/config.toml` to pin parameters
/// for models whose defaults cause problems (e.g. context overflow).
///
/// Human-authored entries are never touched by the auto-tuner.  Auto-tuned
/// entries are appended (not modified) when the harness gains high confidence
/// in its empirical measurements.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTuning {
    /// Model name as it appears in Ollama (e.g. `"nemotron3:33b"`).
    pub model: String,

    /// Ollama `options.num_ctx` — hard cap on KV-cache allocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub num_ctx: Option<u32>,

    /// Per-model context window (total tokens the backend accepts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,

    /// Per-model override of `[tui].real_context_discovery`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub real_context_discovery: Option<bool>,

    /// Per-model `mid_loop_trim_threshold` override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mid_loop_trim_threshold: Option<usize>,

    /// Per-model `mid_loop_trim_tokens` override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mid_loop_trim_tokens: Option<usize>,

    /// Per-model `max_tool_rounds` override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tool_rounds: Option<usize>,

    /// Per-model `[tui].workflow_grace_rounds` override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_grace_rounds: Option<usize>,

    /// Per-model `[tui].narration_nudge_cap` override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub narration_nudge_cap: Option<usize>,
}

/// Load tuning configuration for a specific model.
///
/// Returns the user's config if it exists, otherwise returns the bundled default.
pub fn load_model_config(model_name: &str) -> Result<toml::Value, TunerError> {
    let home = dirs::home_dir()
        .ok_or_else(|| TunerError::ReadConfigDir("Cannot determine home directory".to_string()))?;
    let config_dir = home.join(".newt").join("model_tuning");

    // Check for user config first
    let user_config_path = config_dir.join(format!("{}.toml", model_name));
    if user_config_path.exists() {
        return load_toml(&user_config_path)
            .map_err(|e| TunerError::ParseTOML(user_config_path, e.to_string()));
    }

    // Fall back to bundled default
    let default_path = config_dir.join(format!("{}.default.toml", model_name));
    if default_path.exists() {
        return load_toml(&default_path)
            .map_err(|e| TunerError::ParseTOML(default_path, e.to_string()));
    }

    Err(TunerError::ModelNotFound(model_name.to_string()))
}

/// Initialize the model tuning directory with bundled defaults.
pub fn init_model_tuning() -> Result<(), TunerError> {
    let home = dirs::home_dir()
        .ok_or_else(|| TunerError::ReadConfigDir("Cannot determine home directory".to_string()))?;
    let config_dir = home.join(".newt").join("model_tuning");

    if config_dir.exists() {
        return Ok(());
    }

    std::fs::create_dir_all(&config_dir).map_err(|e| {
        TunerError::ReadConfigDir(format!("Cannot create {}: {}", config_dir.display(), e))
    })?;

    // Copy bundled defaults (this would be implemented with embedded resources)
    Ok(())
}

/// Get the path to the model tuning configuration directory.
pub fn model_tuning_dir() -> Result<PathBuf, TunerError> {
    let home = dirs::home_dir()
        .ok_or_else(|| TunerError::ReadConfigDir("Cannot determine home directory".to_string()))?;
    Ok(home.join(".newt").join("model_tuning"))
}

fn load_toml(path: &Path) -> Result<toml::Value, String> {
    let content = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    toml::from_str(&content).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_tuning_dir() {
        let dir = model_tuning_dir().expect("Should return config dir");
        assert!(dir.ends_with(".newt/model_tuning"));
    }
}
