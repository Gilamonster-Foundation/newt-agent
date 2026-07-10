//! `newt summarizer` — inspect and manage the mid-loop summarizer backend.
//!
//! The default behavior on current `main` is "use the on-host embedded CPU
//! summarizer if the default mini-model is provisioned; otherwise degrade to the
//! session model with a warning". This command makes that state visible and lets
//! the operator provision the embedded model or persist a small
//! `~/.newt/summarizer.toml` override without hand-editing TOML.

use clap::Subcommand;
use newt_core::{BackendKind, Config, SummarizerConfig};
use newt_inference::palette;
use std::path::{Path, PathBuf};

#[derive(Subcommand, Debug)]
pub enum SummarizerCmd {
    /// Show the effective summarizer backend, config path, and tuning knobs.
    Show,
    /// Provision the default or named embedded mini-model (GGUF + tokenizer).
    Setup {
        /// Palette alias (e.g. `qwen2.5-0.5b`). Omit for the default summarizer.
        alias: Option<String>,
    },
    /// Persist an explicit embedded summarizer override in `summarizer.toml`.
    Embedded {
        /// Palette alias to pin. Omit for the default summarizer.
        alias: Option<String>,
    },
    /// Remove `summarizer.toml`, returning to the built-in default behavior.
    Clear,
    /// Set or clear the fallback model. Use `none` to clear it.
    Fallback {
        /// Model id, or `none` to clear the setting.
        model: String,
    },
    /// Set the summarizer request timeout in seconds.
    Timeout {
        /// Timeout in seconds.
        secs: u64,
    },
    /// Set the number of retries before falling back to the static marker.
    Retries {
        /// Retry count.
        count: u32,
    },
    /// Set or clear `keep_alive`. Use `none` to clear it.
    #[command(name = "keep-alive")]
    KeepAlive {
        /// Keep-alive value, or `none` to clear the setting.
        value: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EffectiveSummarizer {
    DefaultEmbedded {
        model: String,
        model_path: String,
    },
    DefaultDegradedSession {
        reason: String,
    },
    OverrideEmbedded {
        model: String,
        model_path: String,
    },
    OverrideBackend {
        kind: Option<BackendKind>,
        model: Option<String>,
        endpoint: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SummarizerStatus {
    pub config_path: PathBuf,
    pub config_exists: bool,
    pub config: SummarizerConfig,
    pub backend_override: bool,
    pub default_model: String,
    pub default_model_path: String,
    pub default_model_installed: bool,
    pub embedded_compiled: bool,
    pub effective: EffectiveSummarizer,
}

pub async fn run(cmd: Option<SummarizerCmd>) -> anyhow::Result<()> {
    match cmd.unwrap_or(SummarizerCmd::Show) {
        SummarizerCmd::Show => show(),
        SummarizerCmd::Setup { alias } => setup(alias.as_deref()).await,
        SummarizerCmd::Embedded { alias } => set_embedded(alias.as_deref()).await,
        SummarizerCmd::Clear => clear(),
        SummarizerCmd::Fallback { model } => {
            edit_config(|cfg| cfg.fallback_model = none_like(&model).map(str::to_string))?;
            print_updated("fallback_model", none_like(&model).unwrap_or("none"));
            Ok(())
        }
        SummarizerCmd::Timeout { secs } => {
            edit_config(|cfg| cfg.timeout_secs = secs)?;
            print_updated("timeout_secs", &secs.to_string());
            Ok(())
        }
        SummarizerCmd::Retries { count } => {
            edit_config(|cfg| cfg.retries = count)?;
            print_updated("retries", &count.to_string());
            Ok(())
        }
        SummarizerCmd::KeepAlive { value } => {
            edit_config(|cfg| cfg.keep_alive = none_like(&value).map(str::to_string))?;
            print_updated("keep_alive", none_like(&value).unwrap_or("none"));
            Ok(())
        }
    }
}

pub(crate) fn resolve_status() -> anyhow::Result<SummarizerStatus> {
    let (config, config_path, config_exists) = load_for_edit()?;
    let default_model = palette::default_model();
    let default_model_path = palette::local_gguf_path(default_model)
        .ok_or_else(|| anyhow::anyhow!("cannot resolve ~/.newt/models (no home dir)"))?;
    let default_model_path = default_model_path.to_string_lossy().into_owned();
    let default_model_installed = palette::resolve_local(default_model.name).is_some();
    let embedded_compiled = cfg!(feature = "embedded");
    let backend_override = has_backend_override(&config);
    let effective = resolve_effective_backend(
        &config,
        embedded_compiled,
        default_model.name,
        if default_model_installed {
            Some(default_model_path.as_str())
        } else {
            None
        },
    );
    Ok(SummarizerStatus {
        config_path,
        config_exists,
        config,
        backend_override,
        default_model: default_model.name.to_string(),
        default_model_path,
        default_model_installed,
        embedded_compiled,
        effective,
    })
}

pub(crate) fn has_backend_override(cfg: &SummarizerConfig) -> bool {
    cfg.kind.is_some() || cfg.endpoint.is_some() || cfg.model.is_some() || cfg.model_path.is_some()
}

pub(crate) fn resolve_effective_backend(
    cfg: &SummarizerConfig,
    embedded_compiled: bool,
    default_model: &str,
    default_model_path: Option<&str>,
) -> EffectiveSummarizer {
    if !has_backend_override(cfg) {
        return match (embedded_compiled, default_model_path) {
            (true, Some(path)) => EffectiveSummarizer::DefaultEmbedded {
                model: default_model.to_string(),
                model_path: path.to_string(),
            },
            (false, _) => EffectiveSummarizer::DefaultDegradedSession {
                reason: "this build lacks the `embedded` feature".to_string(),
            },
            (true, None) => EffectiveSummarizer::DefaultDegradedSession {
                reason: format!(
                    "default embedded model '{default_model}' is not provisioned; run `newt summarizer setup`"
                ),
            },
        };
    }
    if matches!(cfg.kind, Some(BackendKind::Embedded)) {
        return EffectiveSummarizer::OverrideEmbedded {
            model: cfg
                .model
                .clone()
                .unwrap_or_else(|| default_model.to_string()),
            model_path: cfg
                .model_path
                .clone()
                .unwrap_or_else(|| "(missing model_path)".to_string()),
        };
    }
    EffectiveSummarizer::OverrideBackend {
        kind: cfg.kind,
        model: cfg.model.clone(),
        endpoint: cfg.endpoint.clone(),
    }
}

fn show() -> anyhow::Result<()> {
    let status = resolve_status()?;
    println!("Summarizer — status\n");
    println!(
        "  config path                  : {}",
        status.config_path.display()
    );
    if status.config_exists {
        println!("  config file                  : present");
    } else {
        println!("  config file                  : absent (built-in defaults)");
    }
    println!(
        "  embedded feature             : {}",
        if status.embedded_compiled {
            "yes"
        } else {
            "no"
        }
    );
    println!("  default on-host model        : {}", status.default_model);
    println!(
        "  default model path           : {}",
        status.default_model_path
    );
    println!(
        "  default model installed      : {}",
        if status.default_model_installed {
            "yes"
        } else {
            "no"
        }
    );
    match &status.effective {
        EffectiveSummarizer::DefaultEmbedded { model, model_path } => {
            println!("  effective backend            : embedded default (on-host CPU)");
            println!("  effective model              : {model}");
            println!("  effective model_path         : {model_path}");
            if status.config_exists && !status.backend_override {
                println!("  backend override             : none (file only sets tuning knobs)");
            }
        }
        EffectiveSummarizer::DefaultDegradedSession { reason } => {
            println!("  effective backend            : session model (degraded default)");
            println!("  degrade reason               : {reason}");
        }
        EffectiveSummarizer::OverrideEmbedded { model, model_path } => {
            println!("  effective backend            : embedded override");
            println!("  effective model              : {model}");
            println!("  effective model_path         : {model_path}");
        }
        EffectiveSummarizer::OverrideBackend {
            kind,
            model,
            endpoint,
        } => {
            println!("  effective backend            : explicit override");
            println!(
                "  override kind                : {}",
                kind.map(|k| format!("{k:?}"))
                    .unwrap_or_else(|| "(inherits session kind)".to_string())
            );
            println!(
                "  override model               : {}",
                model.as_deref().unwrap_or("(inherits session model)")
            );
            println!(
                "  override endpoint            : {}",
                endpoint.as_deref().unwrap_or("(inherits session endpoint)")
            );
        }
    }
    println!(
        "  timeout_secs / retries       : {} / {}",
        status.config.timeout_secs, status.config.retries
    );
    println!(
        "  fallback_model               : {}",
        status.config.fallback_model.as_deref().unwrap_or("none")
    );
    println!(
        "  keep_alive                   : {}",
        status
            .config
            .keep_alive
            .as_deref()
            .unwrap_or("(inherits session)")
    );
    println!("\nSetup");
    println!("  - Provision the default model : newt summarizer setup");
    println!("  - Pin a specific embedded one : newt summarizer embedded [alias]");
    println!("  - Return to built-in default  : newt summarizer clear");
    Ok(())
}

async fn setup(alias: Option<&str>) -> anyhow::Result<()> {
    let model = crate::models_cmd::provision(alias).await?;
    println!("Embedded summarizer model ready: {}", model.name);
    let status = resolve_status()?;
    if status.backend_override {
        println!(
            "Note: summarizer.toml still pins its own backend; clear or edit it if you want the default embedded backend to apply."
        );
    } else {
        println!("New sessions will use the on-host embedded summarizer by default.");
    }
    Ok(())
}

async fn set_embedded(alias: Option<&str>) -> anyhow::Result<()> {
    let model = crate::models_cmd::provision(alias).await?;
    let model_path = palette::resolve_local(model.name).ok_or_else(|| {
        anyhow::anyhow!(
            "embedded model '{}' is still not fully provisioned after setup",
            model.name
        )
    })?;
    edit_config(|cfg| {
        cfg.kind = Some(BackendKind::Embedded);
        cfg.model = Some(model.name.to_string());
        cfg.model_path = Some(model_path.to_string_lossy().into_owned());
        cfg.endpoint = None;
        cfg.api_key_file = None;
        cfg.api_key_env = None;
    })?;
    let path = summarizer_config_path()?;
    println!(
        "Pinned summarizer backend to embedded model '{}' in {}",
        model.name,
        path.display()
    );
    Ok(())
}

fn clear() -> anyhow::Result<()> {
    let path = summarizer_config_path()?;
    if path.is_file() {
        std::fs::remove_file(&path)?;
        println!("Removed {}", path.display());
    } else {
        println!("No summarizer.toml present; built-in defaults already apply.");
    }
    Ok(())
}

fn print_updated(field: &str, value: &str) {
    println!("Updated summarizer.toml: {field} = {value}");
}

fn edit_config(mut f: impl FnMut(&mut SummarizerConfig)) -> anyhow::Result<()> {
    let (mut cfg, path, _exists) = load_for_edit()?;
    f(&mut cfg);
    save_config(&cfg, &path)?;
    Ok(())
}

fn load_for_edit() -> anyhow::Result<(SummarizerConfig, PathBuf, bool)> {
    let path = summarizer_config_path()?;
    if path.is_file() {
        let text = std::fs::read_to_string(&path)?;
        let cfg = SummarizerConfig::from_toml_str(&text)?;
        Ok((cfg, path, true))
    } else {
        Ok((SummarizerConfig::default(), path, false))
    }
}

fn save_config(cfg: &SummarizerConfig, path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = toml::to_string_pretty(cfg)?;
    std::fs::write(path, text)?;
    Ok(())
}

fn summarizer_config_path() -> anyhow::Result<PathBuf> {
    if let Ok(path) = std::env::var("NEWT_SUMMARIZER_CONFIG") {
        return Ok(PathBuf::from(path));
    }
    Config::user_config_dir()
        .map(|dir| dir.join("summarizer.toml"))
        .ok_or_else(|| anyhow::anyhow!("cannot resolve ~/.newt (no home dir)"))
}

fn none_like(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    if trimmed.eq_ignore_ascii_case("none")
        || trimmed.eq_ignore_ascii_case("off")
        || trimmed.eq_ignore_ascii_case("default")
        || trimmed.eq_ignore_ascii_case("unset")
        || trimmed.eq_ignore_ascii_case("clear")
    {
        None
    } else {
        Some(trimmed)
    }
}

#[cfg(test)]
mod tests {
    use super::{resolve_effective_backend, EffectiveSummarizer};
    use newt_core::{BackendKind, SummarizerConfig};

    #[test]
    fn default_behavior_prefers_embedded_when_available() {
        let cfg = SummarizerConfig::default();
        let got = resolve_effective_backend(
            &cfg,
            true,
            "qwen2.5-0.5b",
            Some("/models/qwen2.5-0.5b.gguf"),
        );
        assert_eq!(
            got,
            EffectiveSummarizer::DefaultEmbedded {
                model: "qwen2.5-0.5b".into(),
                model_path: "/models/qwen2.5-0.5b.gguf".into()
            }
        );
    }

    #[test]
    fn knob_only_file_keeps_default_backend_selection() {
        let cfg = SummarizerConfig {
            timeout_secs: 45,
            retries: 2,
            fallback_model: Some("llama-3.2-1b".into()),
            ..Default::default()
        };
        let got = resolve_effective_backend(
            &cfg,
            true,
            "qwen2.5-0.5b",
            Some("/models/qwen2.5-0.5b.gguf"),
        );
        assert!(matches!(got, EffectiveSummarizer::DefaultEmbedded { .. }));
    }

    #[test]
    fn missing_default_model_degrades_to_session() {
        let cfg = SummarizerConfig::default();
        let got = resolve_effective_backend(&cfg, true, "qwen2.5-0.5b", None);
        assert_eq!(
            got,
            EffectiveSummarizer::DefaultDegradedSession {
                reason: "default embedded model 'qwen2.5-0.5b' is not provisioned; run `newt summarizer setup`".into()
            }
        );
    }

    #[test]
    fn explicit_embedded_override_wins() {
        let cfg = SummarizerConfig {
            kind: Some(BackendKind::Embedded),
            model: Some("qwen2.5-1.5b".into()),
            model_path: Some("/models/qwen2.5-1.5b.gguf".into()),
            ..Default::default()
        };
        let got = resolve_effective_backend(&cfg, true, "qwen2.5-0.5b", None);
        assert_eq!(
            got,
            EffectiveSummarizer::OverrideEmbedded {
                model: "qwen2.5-1.5b".into(),
                model_path: "/models/qwen2.5-1.5b.gguf".into()
            }
        );
    }

    #[test]
    fn explicit_off_box_override_is_reported() {
        let cfg = SummarizerConfig {
            kind: Some(BackendKind::Openai),
            model: Some("gpt-4.1-mini".into()),
            endpoint: Some("https://api.openai.com".into()),
            ..Default::default()
        };
        let got = resolve_effective_backend(&cfg, true, "qwen2.5-0.5b", None);
        assert_eq!(
            got,
            EffectiveSummarizer::OverrideBackend {
                kind: Some(BackendKind::Openai),
                model: Some("gpt-4.1-mini".into()),
                endpoint: Some("https://api.openai.com".into())
            }
        );
    }
}
