//! Configuration loading for Newt-Agent.
//!
//! Resolution order: `$NEWT_CONFIG` env var, then `./newt.toml`,
//! `~/.newt/config.toml`, `/etc/newt/config.toml`. If none exist the
//! built-in defaults are used (a single Ollama backend on localhost).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{NewtError, Result};
use crate::router::Tier;

// ---------------------------------------------------------------------------
// Config types
// ---------------------------------------------------------------------------

/// Top-level Newt-Agent configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Inference backends (Ollama, vLLM, etc.).
    pub backends: Vec<BackendConfig>,

    /// External provider-plugin definitions.
    pub providers: Vec<ProviderConfig>,

    /// Default tier ordering used by the router when no per-backend
    /// override is specified.
    pub default_tier_order: Vec<Tier>,

    /// Optional NVIDIA DGX endpoint-management config powering the
    /// `newt dgx` command suite. `None` when unconfigured — newt never
    /// dials a DGX endpoint unless this (or a `NEWT_DGX_*` env var) is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dgx: Option<crate::dgx::DgxConfig>,

    /// TUI appearance and behaviour. `None` → built-in defaults apply.
    /// Overridable at runtime via `NEWT_CHAT_STYLE` and `NEWT_PROMPT`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tui: Option<TuiConfig>,
}

// ---------------------------------------------------------------------------
// TUI config
// ---------------------------------------------------------------------------

/// TUI appearance preferences stored under `[tui]` in `newt.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TuiConfig {
    /// Whether to show "newt" / "you" labels before the carets.
    pub chat_style: ChatStyle,

    /// PS1-style prompt template.
    ///
    /// Tokens: `\w` workspace basename, `\W` full path, `\h` hostname,
    /// `\v` newt version.  Default: `"\\w $ "` (compact) / `"you $ "` (verbose).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,

    /// Skip the full-screen ANSI art splash and show a compact header instead.
    /// Equivalent to the `--no-splash` CLI flag.
    #[serde(default)]
    pub no_splash: bool,

    /// Key binding mode for the chat input line.
    /// `"emacs"` (default) or `"vi"`. Also overridable via `NEWT_EDIT_MODE`.
    #[serde(default)]
    pub edit_mode: EditMode,
}

/// Key binding style for the chat REPL input line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum EditMode {
    /// Readline / emacs-style bindings (default).
    #[default]
    Emacs,
    /// Vi / vim-style bindings — Esc for normal mode, i for insert.
    Vi,
}

impl EditMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Emacs => "emacs",
            Self::Vi => "vi",
        }
    }

    pub fn toggle(&self) -> Self {
        match self {
            Self::Emacs => Self::Vi,
            Self::Vi => Self::Emacs,
        }
    }
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            chat_style: ChatStyle::Compact,
            prompt: None,
            no_splash: false,
            edit_mode: EditMode::Emacs,
        }
    }
}

/// Chat REPL display density.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ChatStyle {
    /// Just the caret symbol — no "newt" / "you" labels.
    #[default]
    Compact,
    /// Full "newt ▸" / "you $" labels before each message.
    Verbose,
}

impl ChatStyle {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Verbose => "verbose",
        }
    }

    pub fn toggle(&self) -> Self {
        match self {
            Self::Compact => Self::Verbose,
            Self::Verbose => Self::Compact,
        }
    }
}

/// A single inference backend entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendConfig {
    pub name: String,
    pub endpoint: String,
    pub model: String,
    pub tiers: Vec<Tier>,
}

/// A subprocess provider-plugin entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub env_pass: Vec<String>,
    pub tiers: Vec<Tier>,
}

// ---------------------------------------------------------------------------
// Default
// ---------------------------------------------------------------------------

impl Default for Config {
    fn default() -> Self {
        Self {
            backends: vec![BackendConfig {
                name: "ollama".into(),
                endpoint: "http://127.0.0.1:11434".into(),
                model: "llama3.1:8b".into(),
                tiers: vec![Tier::Fast, Tier::Standard, Tier::Complex, Tier::Review],
            }],
            providers: Vec::new(),
            default_tier_order: vec![Tier::Fast, Tier::Standard, Tier::Complex, Tier::Review],
            dgx: None,
            tui: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

impl Config {
    /// Load configuration from an explicit file path.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)?;
        toml::from_str(&text).map_err(|e| NewtError::Config(e.to_string()))
    }

    /// Resolve configuration by searching well-known locations.
    ///
    /// Search order:
    /// 1. `$NEWT_CONFIG` environment variable
    /// 2. `./newt.toml`
    /// 3. `~/.newt/config.toml`
    /// 4. `/etc/newt/config.toml`
    ///
    /// Returns `Config::default()` if none of the candidates exist.
    pub fn resolve() -> Result<Self> {
        let candidates = Self::candidate_paths();
        for path in &candidates {
            if path.is_file() {
                return Self::load(path);
            }
        }
        Ok(Self::default())
    }

    /// The user-writable config path: `~/.newt/config.toml`.
    /// This is the first path `resolve()` reads and the target for `save()`.
    pub fn user_config_path() -> Option<PathBuf> {
        home_dir().map(|h| h.join(".newt").join("config.toml"))
    }

    /// Serialize this config and write it to `path`, creating parent dirs if needed.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(NewtError::Io)?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|e| NewtError::Config(e.to_string()))?;
        std::fs::write(path, text).map_err(NewtError::Io)
    }

    /// Build the ordered list of candidate config file paths.
    fn candidate_paths() -> Vec<PathBuf> {
        let mut paths = Vec::new();

        if let Ok(p) = std::env::var("NEWT_CONFIG") {
            paths.push(PathBuf::from(p));
        }

        paths.push(PathBuf::from("./newt.toml"));

        if let Some(home) = home_dir() {
            paths.push(home.join(".newt").join("config.toml"));
        }

        paths.push(PathBuf::from("/etc/newt/config.toml"));
        paths
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Best-effort home directory lookup without pulling in the `dirs` crate.
fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
        .map(PathBuf::from)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn defaults_are_sensible() {
        let cfg = Config::default();
        assert_eq!(cfg.backends.len(), 1);
        assert_eq!(cfg.providers.len(), 0);
        assert_eq!(cfg.default_tier_order.len(), 4);
    }

    #[test]
    fn load_happy_path() {
        let toml_text = r#"
[[backends]]
name = "local-ollama"
endpoint = "http://localhost:11434"
model = "mistral:7b"
tiers = ["FAST", "STANDARD"]

[[providers]]
name = "cloud"
command = "newt-cloud-shim"
env_pass = ["CLOUD_TOKEN"]
tiers = ["COMPLEX", "REVIEW"]

default_tier_order = ["FAST", "STANDARD", "COMPLEX", "REVIEW"]
"#;
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(toml_text.as_bytes()).unwrap();
        f.flush().unwrap();

        let cfg = Config::load(f.path()).unwrap();
        assert_eq!(cfg.backends.len(), 1);
        assert_eq!(cfg.backends[0].name, "local-ollama");
        assert_eq!(cfg.backends[0].model, "mistral:7b");
        assert_eq!(cfg.backends[0].tiers, vec![Tier::Fast, Tier::Standard]);
        assert_eq!(cfg.providers.len(), 1);
        assert_eq!(cfg.providers[0].name, "cloud");
        assert_eq!(cfg.providers[0].env_pass, vec!["CLOUD_TOKEN".to_string()]);
    }

    #[test]
    fn missing_file_returns_io_error() {
        let result = Config::load(Path::new("/tmp/newt-does-not-exist-12345.toml"));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, NewtError::Io(_)),
            "expected Io error, got: {err:?}"
        );
    }

    #[test]
    fn malformed_toml_returns_config_error() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"{{{{").unwrap();
        f.flush().unwrap();

        let result = Config::load(f.path());
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, NewtError::Config(_)),
            "expected Config error, got: {err:?}"
        );
    }

    #[test]
    fn resolve_returns_default_when_no_file() {
        // Use a temp dir as cwd and clear env to ensure no candidates match.
        let dir = tempfile::tempdir().unwrap();

        // Save & clear environment to isolate the test.
        let saved_config = std::env::var("NEWT_CONFIG").ok();
        let saved_home = std::env::var("HOME").ok();
        std::env::remove_var("NEWT_CONFIG");
        std::env::set_var("HOME", dir.path());

        // Run resolve from inside the temp dir so ./newt.toml won't exist.
        let prev_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        let cfg = Config::resolve().unwrap();

        // Restore environment.
        std::env::set_current_dir(prev_dir).unwrap();
        if let Some(v) = saved_home {
            std::env::set_var("HOME", v);
        }
        if let Some(v) = saved_config {
            std::env::set_var("NEWT_CONFIG", v);
        }

        assert_eq!(cfg.backends.len(), 1);
        assert_eq!(cfg.backends[0].name, "ollama");
    }

    #[test]
    fn config_default_has_no_dgx() {
        assert!(Config::default().dgx.is_none());
    }

    #[test]
    fn config_with_dgx_roundtrips() {
        let cfg = Config {
            dgx: Some(crate::dgx::DgxConfig::home_template()),
            ..Config::default()
        };
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back = toml::from_str::<Config>(&text).unwrap();
        let dgx = back.dgx.expect("dgx should round-trip");
        assert_eq!(dgx.active_node.as_deref(), Some("home"));
        assert_eq!(dgx.nodes.len(), 1);
        assert_eq!(dgx.formations.len(), 2);
    }
}
