//! `newt tunings` — inspect, export, import, and reset model tuning data.
//!
//! Tuning data is maintained automatically by the harness in
//! `~/.newt/model-capabilities.json`. The community format is a shareable TOML
//! file that can be pasted into a gist or forum post.

use std::path::Path;

use clap::Subcommand;
use newt_core::tuning::{
    community_tunings_path, load_community_tunings, save_community_tunings, CommunityTunings,
    TuneSource, TuningProfile,
};

#[derive(Subcommand, Debug)]
pub enum TuningsCmd {
    /// Show resolved tuning for one or all known models.
    Show {
        /// Model name to show (e.g. "nemotron3:33b"). Omit to show all.
        model: Option<String>,
    },
    /// Export current tuning data as a community-shareable TOML file.
    ///
    /// The output can be pasted into a gist, forum post, or shared repo.
    /// Others import it with: newt tunings import <file>
    Export {
        /// Write to this file instead of stdout.
        #[arg(long, short)]
        output: Option<std::path::PathBuf>,
    },
    /// Merge a community TOML file into the local tuning data.
    ///
    /// Incoming profiles with higher confidence replace existing ones.
    /// New models are appended. Existing high-confidence profiles are kept.
    Import {
        /// Path to the community tunings TOML file.
        file: std::path::PathBuf,
    },
    /// Clear empirical tuning data for a model (or all models).
    ///
    /// The next inference session will re-probe and re-learn the context window.
    Reset {
        /// Model name to reset. Omit to reset all.
        model: Option<String>,
    },
}

pub fn run(cmd: TuningsCmd, _config_path: Option<&Path>) -> anyhow::Result<()> {
    match cmd {
        TuningsCmd::Show { model } => cmd_show(model.as_deref()),
        TuningsCmd::Export { output } => cmd_export(output.as_deref()),
        TuningsCmd::Import { file } => cmd_import(&file),
        TuningsCmd::Reset { model } => cmd_reset(model.as_deref()),
    }
}

// ---------------------------------------------------------------------------
// Helpers: read capabilities JSON as raw serde_json::Value
// ---------------------------------------------------------------------------

/// Load `~/.newt/model-capabilities.json` as a raw JSON object so we can
/// read both the base fields (`conformance`, `tested_date`) and the tuning
/// extension fields added by feat/ctx-window-probe — regardless of whether
/// those fields are in the compiled `CapabilityEntry` struct.
fn load_caps_raw() -> serde_json::Value {
    let Some(path) = newt_core::config::Config::user_config_path()
        .map(|p| p.with_file_name("model-capabilities.json"))
    else {
        return serde_json::Value::Object(Default::default());
    };
    let Ok(data) = std::fs::read_to_string(&path) else {
        return serde_json::Value::Object(Default::default());
    };
    serde_json::from_str(&data).unwrap_or_else(|_| serde_json::Value::Object(Default::default()))
}

fn caps_path() -> Option<std::path::PathBuf> {
    newt_core::config::Config::user_config_path()
        .map(|p| p.with_file_name("model-capabilities.json"))
}

fn fmt_k(n: u32) -> String {
    if n >= 1024 {
        format!("{}k", n / 1024)
    } else {
        n.to_string()
    }
}

// ---------------------------------------------------------------------------
// show
// ---------------------------------------------------------------------------

fn cmd_show(model: Option<&str>) -> anyhow::Result<()> {
    let caps = load_caps_raw();
    let community = load_community_tunings();

    let caps_obj = caps.as_object();
    let has_caps = caps_obj.map(|o| !o.is_empty()).unwrap_or(false);

    if !has_caps && community.profiles.is_empty() {
        println!("No tuning data found.");
        println!("Run a few inference sessions to let the harness gather data,");
        println!("or import a community profile with: newt tunings import <file>");
        return Ok(());
    }

    // Collect model names from both sources.
    let mut names: Vec<String> = caps_obj
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    for p in &community.profiles {
        if !names.iter().any(|n| n == &p.model) {
            names.push(p.model.clone());
        }
    }
    names.sort_unstable();

    // Filter if a specific model was requested.
    if let Some(target) = model {
        names.retain(|n| n == target);
        if names.is_empty() {
            anyhow::bail!("no tuning data for model '{target}'");
        }
    }

    println!(
        "{:<30}  {:>8}  {:>8}  {:>6}  Source",
        "Model", "Ctx Win", "Safe Ctx", "Conf"
    );
    println!("{}", "─".repeat(68));

    for name in &names {
        let cap_entry = caps_obj.and_then(|o| o.get(name.as_str()));
        let community_entry = community.find(name);

        let ctx_win = cap_entry
            .and_then(|e| e.get("context_window"))
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
            .or_else(|| community_entry.and_then(|p| p.context_window));
        let safe_ctx = cap_entry
            .and_then(|e| e.get("safe_context"))
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
            .or_else(|| community_entry.and_then(|p| p.safe_context));
        let conf = cap_entry
            .and_then(|e| e.get("tune_confidence"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| community_entry.map(|p| p.confidence.clone()))
            .unwrap_or_else(|| "none".to_string());
        let source = if cap_entry.is_some() {
            "empirical"
        } else {
            "community"
        };

        let ctx_str = ctx_win.map(fmt_k).unwrap_or_else(|| "—".to_string());
        let safe_str = safe_ctx.map(fmt_k).unwrap_or_else(|| "—".to_string());

        println!("{name:<30}  {ctx_str:>8}  {safe_str:>8}  {conf:>6}  {source}");
    }

    println!();
    if let Some(path) = community_tunings_path() {
        if path.exists() {
            println!("Community file: {}", path.display());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// export
// ---------------------------------------------------------------------------

fn cmd_export(output: Option<&std::path::Path>) -> anyhow::Result<()> {
    let caps = load_caps_raw();
    let version = env!("CARGO_PKG_VERSION");

    let mut ct = CommunityTunings::default();
    ct.format.generated_by = Some(format!("newt/{version}"));

    if let Some(obj) = caps.as_object() {
        for (model, entry) in obj {
            let context_window = entry
                .get("context_window")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            let safe_context = entry
                .get("safe_context")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);

            if context_window.is_none() && safe_context.is_none() {
                continue;
            }

            let confidence = entry
                .get("tune_confidence")
                .and_then(|v| v.as_str())
                .unwrap_or("none")
                .to_string();
            let data_points = entry
                .get("consecutive_ok")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;

            ct.profiles.push(TuningProfile {
                model: model.clone(),
                context_window,
                safe_context,
                mid_loop_trim_threshold: None,
                max_tool_rounds: None,
                tune_source: TuneSource::Empirical,
                confidence,
                data_points,
                notes: None,
            });
        }
    }

    // Include community profiles not covered by empirical data.
    let community = load_community_tunings();
    for p in community.profiles {
        if !ct.profiles.iter().any(|ep| ep.model == p.model) {
            ct.profiles.push(p);
        }
    }
    ct.profiles.sort_by(|a, b| a.model.cmp(&b.model));

    if ct.profiles.is_empty() {
        println!("No tuning data to export. Run some inference sessions first.");
        return Ok(());
    }

    let header = format!(
        "# newt community model tuning profiles · format v1\n\
         # Generated by newt/{version}\n\
         # Share as a gist or forum post. Import with:\n\
         #   newt tunings import <file>\n\n"
    );
    let body = toml::to_string_pretty(&ct)?;
    let full = format!("{header}{body}");

    match output {
        Some(path) => {
            std::fs::write(path, &full)?;
            println!("Tunings exported to {}", path.display());
        }
        None => print!("{full}"),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// import
// ---------------------------------------------------------------------------

fn cmd_import(file: &std::path::Path) -> anyhow::Result<()> {
    let data = std::fs::read_to_string(file)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", file.display()))?;
    let incoming: CommunityTunings = toml::from_str(&data)
        .map_err(|e| anyhow::anyhow!("TOML parse error in {}: {e}", file.display()))?;

    let n = incoming.profiles.len();
    let mut existing = load_community_tunings();
    existing.merge(incoming);
    save_community_tunings(&existing)
        .map_err(|e| anyhow::anyhow!("failed to save community tunings: {e}"))?;

    println!("Imported {n} profile(s) from {}", file.display());
    if let Some(path) = community_tunings_path() {
        println!("Saved to {}", path.display());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// reset
// ---------------------------------------------------------------------------

fn cmd_reset(model: Option<&str>) -> anyhow::Result<()> {
    let path = caps_path().ok_or_else(|| anyhow::anyhow!("cannot determine ~/.newt path"))?;

    let data = std::fs::read_to_string(&path).unwrap_or_else(|_| "{}".to_string());
    let mut json: serde_json::Value = serde_json::from_str(&data)?;

    let tuning_keys = [
        "context_window",
        "safe_context",
        "overflow_at",
        "max_ok_input",
        "consecutive_ok",
        "tune_confidence",
        "tune_date",
    ];

    let obj = json
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("capabilities JSON is not an object"))?;

    match model {
        Some(name) => {
            if let Some(entry) = obj.get_mut(name).and_then(|v| v.as_object_mut()) {
                for key in &tuning_keys {
                    entry.remove(*key);
                }
                println!("Reset tuning data for '{name}'.");
            } else {
                anyhow::bail!("model '{name}' not found in capabilities cache");
            }
        }
        None => {
            let count = obj.len();
            for entry in obj.values_mut() {
                if let Some(map) = entry.as_object_mut() {
                    for key in &tuning_keys {
                        map.remove(*key);
                    }
                }
            }
            println!("Reset tuning data for {count} model(s).");
        }
    }

    let out = serde_json::to_string_pretty(&json)?;
    std::fs::write(&path, out)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_k_abbreviates_kib_multiples() {
        assert_eq!(fmt_k(32768), "32k");
        assert_eq!(fmt_k(1024), "1k");
        // Integer division: sub-1k remainders are truncated, not rounded.
        assert_eq!(fmt_k(1536), "1k");
    }

    #[test]
    fn fmt_k_keeps_small_values_verbatim() {
        assert_eq!(fmt_k(512), "512");
        assert_eq!(fmt_k(1023), "1023");
        assert_eq!(fmt_k(0), "0");
    }
}
