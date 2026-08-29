//! `newt providers` — the hosted-provider preset roster (`list`) and the
//! Hermes Agent importer (`import-hermes`).
//!
//! Thin IO shell: filesystem walks, env resolution, and printing live here;
//! the Python-literal parsing + preset mapping brains are pure in
//! [`crate::providers_import`] (the `dgx_pull.rs` discipline), and the
//! roster/expansion logic is `newt_core::provider_preset`.
//!
//! The importer NEVER executes plugin Python and NEVER writes key material:
//! an inline `api_key` found in a copied Hermes `config.yaml` produces an
//! export-or-paste instruction line instead.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use newt_core::provider_preset::{
    builtin_presets, expand_hermes_config, preset_support, resolve_presets, synthesized_env_var,
    HermesProviderBlock, PresetSupport, ProviderPreset,
};

use crate::providers_import;

#[derive(clap::Subcommand, Debug)]
pub enum ProvidersCmd {
    /// Merged roster: name, wire, endpoint, source, availability.
    List,
    /// Import Hermes Agent model-provider plugins + config.yaml providers
    /// as preset drop-ins — WITHOUT executing any plugin Python.
    ImportHermes {
        /// Hermes home directory (default: $HERMES_HOME, else ~/.hermes).
        #[arg(long)]
        hermes_home: Option<PathBuf>,
        /// Print what would be written/skipped without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Overwrite existing preset drop-ins.
        #[arg(long)]
        force: bool,
    },
}

pub fn run(cmd: ProvidersCmd) -> anyhow::Result<()> {
    match cmd {
        ProvidersCmd::List => list(),
        ProvidersCmd::ImportHermes {
            hermes_home,
            dry_run,
            force,
        } => import_hermes(hermes_home, dry_run, force),
    }
}

// ---------------------------------------------------------------------------
// `newt providers list`
// ---------------------------------------------------------------------------

/// Render the `newt providers` listing.
///
/// Extracted from `list` so the exact bytes are testable without a live
/// preset roster (#1916). Byte-identical to the `println!`s it replaces.
pub(crate) fn providers_table(rows: &[[String; 5]]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<16} {:<26} {:<20} {:<16} ENDPOINT",
        "NAME", "LABEL", "WIRE", "SOURCE"
    );
    for r in rows {
        let _ = writeln!(
            out,
            "{:<16} {:<26} {:<20} {:<16} {}",
            r[0], r[1], r[2], r[3], r[4]
        );
    }
    out
}

fn list() -> anyhow::Result<()> {
    let roster = resolve_presets(None);
    let builtins = builtin_presets();
    let mut rows: Vec<[String; 5]> = Vec::new();
    for p in &roster {
        // Value equality against the builtin entry detects a same-named
        // drop-in that actually changes something.
        let source = match builtins.iter().find(|b| b.name == p.name) {
            Some(b) if b == p => "builtin",
            Some(_) => "drop-in override",
            None => "drop-in",
        };
        let availability = match preset_support(p) {
            PresetSupport::Supported { endpoint, .. } => endpoint,
            PresetSupport::Unsupported { reason } => format!("(unavailable: {reason})"),
        };
        rows.push([
            p.name.clone(),
            p.label().to_string(),
            providers_import::serde_name(&p.api_mode).clone(),
            source.to_string(),
            availability,
        ]);
    }
    print!("{}", providers_table(&rows));
    Ok(())
}

// ---------------------------------------------------------------------------
// `newt providers import-hermes`
// ---------------------------------------------------------------------------

/// The two provider blocks a Hermes `config.yaml` may carry: the legacy
/// `custom_providers:` list and the v12+ `providers:` keyed map.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct HermesConfigYaml {
    custom_providers: HermesProviderBlock,
    providers: HermesProviderBlock,
}

/// Precedence: `--hermes-home` > `$HERMES_HOME` > `$HOME/.hermes`. Pure.
fn resolve_hermes_home(
    flag: Option<PathBuf>,
    hermes_env: Option<std::ffi::OsString>,
    home_env: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    if let Some(p) = flag {
        return Some(p);
    }
    if let Some(v) = hermes_env.filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(v));
    }
    home_env
        .filter(|v| !v.is_empty())
        .map(|h| PathBuf::from(h).join(".hermes"))
}

/// Filename slug for a preset drop-in (lowercase alnum-dash, the spirit of
/// setup.rs's `backend_name`); the TOML body keeps the original name. Pure.
fn filename_slug(name: &str) -> String {
    let mut slug = String::new();
    let mut sep = false;
    for ch in name.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            sep = false;
        } else if !sep && !slug.is_empty() {
            slug.push('-');
            sep = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "provider".to_string()
    } else {
        slug
    }
}

/// Write-side state for one import run: destination, mode flags, tallies.
struct ImportCtx {
    dir: PathBuf,
    dry_run: bool,
    force: bool,
    imported: usize,
    skipped: usize,
}

impl ImportCtx {
    fn skip(&mut self, who: &str, reason: &str) {
        println!("skip {who}: {reason}");
        self.skipped += 1;
    }

    fn write(&mut self, preset: &ProviderPreset, extras: &[String]) -> anyhow::Result<()> {
        let path = self
            .dir
            .join(format!("{}.toml", filename_slug(&preset.name)));
        if path.exists() && !self.force {
            let verb = if self.dry_run { "would-skip" } else { "skip" };
            println!(
                "{verb} {}: {} already exists (use --force to overwrite)",
                preset.name,
                path.display()
            );
            self.skipped += 1;
            return Ok(());
        }
        if self.dry_run {
            println!("would-write {} -> {}", preset.name, path.display());
            self.imported += 1;
            return Ok(());
        }
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        std::fs::write(&path, providers_import::render_preset_toml(preset, extras))
            .with_context(|| format!("writing {}", path.display()))?;
        println!("wrote {} -> {}", preset.name, path.display());
        self.imported += 1;
        Ok(())
    }
}

/// Sorted `(plugin-name, __init__.py path)` pairs under
/// `<home>/plugins/model-providers/`.
fn plugin_inits(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new(); // no plugins dir — fine
    };
    let mut out: Vec<(String, PathBuf)> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .filter_map(|p| {
            let name = p.file_name()?.to_str()?.to_string();
            let init = p.join("__init__.py");
            init.is_file().then_some((name, init))
        })
        .collect();
    out.sort();
    out
}

fn import_hermes(flag: Option<PathBuf>, dry_run: bool, force: bool) -> anyhow::Result<()> {
    let home = resolve_hermes_home(
        flag,
        std::env::var_os("HERMES_HOME"),
        std::env::var_os("HOME"),
    )
    .context("cannot resolve a Hermes home: pass --hermes-home, or set $HERMES_HOME / $HOME")?;
    let config_root = newt_core::Config::user_config_dir()
        .context("cannot resolve the newt config dir ($NEWT_CONFIG_DIR or ~/.newt)")?;
    let mut ctx = ImportCtx {
        dir: config_root.join("providers"),
        dry_run,
        force,
        imported: 0,
        skipped: 0,
    };

    // 1. plugins/model-providers/*/__init__.py — parsed as data, never run.
    for (plugin, init) in plugin_inits(&home.join("plugins").join("model-providers")) {
        let source = match std::fs::read_to_string(&init) {
            Ok(s) => s,
            Err(e) => {
                ctx.skip(&plugin, &format!("cannot read {}: {e}", init.display()));
                continue;
            }
        };
        match providers_import::extract_profiles(&source) {
            Err(reason) => ctx.skip(&plugin, &reason.to_string()),
            Ok(profiles) => {
                for kwargs in &profiles {
                    match providers_import::preset_from_kwargs(kwargs) {
                        Err(reason) => ctx.skip(&plugin, &reason.to_string()),
                        Ok((preset, extras)) => ctx.write(&preset, &extras)?,
                    }
                }
            }
        }
    }

    // 2. config.yaml — both the legacy `custom_providers:` list and the
    //    v12+ `providers:` map.
    let config_yaml = home.join("config.yaml");
    if config_yaml.is_file() {
        let parsed = std::fs::read_to_string(&config_yaml)
            .map_err(|e| e.to_string())
            .and_then(|body| {
                serde_yaml::from_str::<HermesConfigYaml>(&body).map_err(|e| e.to_string())
            });
        match parsed {
            Err(e) => ctx.skip("config.yaml", &e),
            Ok(yaml) => {
                let mut entries = yaml.custom_providers.entries();
                entries.extend(yaml.providers.entries());
                let (presets, keyed) = expand_hermes_config(&entries);
                for id in &keyed {
                    // A key_env REFERENCE maps to env_vars silently; only an
                    // inline api_key VALUE lands here. Name the var the
                    // preset actually carries.
                    let var = presets
                        .iter()
                        .find(|p| &p.name == id)
                        .and_then(|p| p.env_vars.first().cloned())
                        .unwrap_or_else(|| synthesized_env_var(id));
                    println!(
                        "found an api_key for {id} in Hermes config.yaml — newt never stores \
                         plaintext keys; export {var} or paste it when newt setup asks \
                         (stored encrypted)"
                    );
                }
                for preset in &presets {
                    match preset_support(preset) {
                        PresetSupport::Supported { .. } => ctx.write(preset, &[])?,
                        PresetSupport::Unsupported { reason } => ctx.skip(&preset.name, &reason),
                    }
                }
            }
        }
    }

    println!(
        "imported {}, skipped {} (see reasons above)",
        ctx.imported, ctx.skipped
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests — pure helpers only (the IO paths are covered by tests/providers_cli.rs)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hermes_home_precedence_flag_env_home() {
        let flag = Some(PathBuf::from("/x/hermes"));
        let env = Some(std::ffi::OsString::from("/y/hermes"));
        let home = Some(std::ffi::OsString::from("/home/u"));
        assert_eq!(
            resolve_hermes_home(flag, env.clone(), home.clone()),
            Some(PathBuf::from("/x/hermes"))
        );
        assert_eq!(
            resolve_hermes_home(None, env, home.clone()),
            Some(PathBuf::from("/y/hermes"))
        );
        assert_eq!(
            resolve_hermes_home(None, None, home),
            Some(PathBuf::from("/home/u/.hermes"))
        );
        assert_eq!(resolve_hermes_home(None, None, None), None);
        // Empty env values are unset.
        assert_eq!(
            resolve_hermes_home(
                None,
                Some(std::ffi::OsString::new()),
                Some(std::ffi::OsString::new())
            ),
            None
        );
    }

    #[test]
    fn filename_slug_is_lowercase_alnum_dash() {
        assert_eq!(filename_slug("acme-inference"), "acme-inference");
        assert_eq!(filename_slug("Acme Inference 2"), "acme-inference-2");
        assert_eq!(filename_slug("weird__name..x"), "weird-name-x");
        assert_eq!(filename_slug("---"), "provider");
        assert_eq!(filename_slug(""), "provider");
    }
}

#[cfg(test)]
mod d3c {
    /// **The byte golden for `newt providers` as it ships today** (#1916).
    /// Captured from the shipping renderer — see `models_cmd::d3c`.
    #[test]
    fn the_providers_listing_is_byte_exact() {
        let rows = [[
            "openai".to_string(),
            "OpenAI".to_string(),
            "responses".to_string(),
            "builtin".to_string(),
            "https://api.openai.com/v1".to_string(),
        ]];
        assert_eq!(
            super::providers_table(&rows),
            concat!(
                "NAME             LABEL                      WIRE                 SOURCE           ENDPOINT\n",
                "openai           OpenAI                     responses            builtin          https://api.openai.com/v1\n",
            )
        );
    }
}
