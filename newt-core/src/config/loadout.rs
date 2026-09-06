//! Named loadout composition, reference validation, and disk loading.
//!
//! Loadouts select backend, kit, profile, and persona axes with non-authority
//! overrides. Crew role and dispatch-policy configuration live in `crew`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::Config;

/// One named loadout (`[loadouts.<name>]` / `~/.newt/loadouts/<name>.toml`) — the
/// top-level composition the user *loads* (`docs/design/loadout-composition.md`).
/// Every field is optional and is a **name reference** into the surface that owns
/// that axis; the loadout itself stores nothing but the selection + per-axis
/// overrides. It carries **no authority** — `settings` cannot widen caveats.
///
/// ```toml
/// [loadouts.dev-nemotron]
/// provider = "dgx"          # → the catalog/provider card (#387)
/// model    = "nemotron@deep"
/// kit      = "nemotron"     # → a [bundles.<name>] (the loadable kit unit)
/// profile  = "nemotron"     # → a [profiles.<name>] (optional; the bundle implies it)
/// role     = "python-developer"   # → ~/.newt/personas/<name>.md
///   [loadouts.dev-nemotron.settings]
///   num_ctx = 24576
///   framing = "Ship small, verify."
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Loadout {
    /// Provider id (→ the catalog/provider card). Resolution is Slice 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Model id, optionally `model@variant`. Resolution is Slice 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Bundle name (the loadable kit unit) — must name a `[bundles.<name>]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kit: Option<String>,
    /// Profile name — must name a `[profiles.<name>]`. Omitted ⇒ the bundle/model
    /// implies it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Role/persona name (`~/.newt/personas/<name>.md`). Not validated against the
    /// filesystem here — personas are resolved at session start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Per-axis overrides (parameters / prompt). Never authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<LoadoutSettings>,
}

/// Per-axis overrides a loadout may pin. **No authority axis** — a loadout cannot
/// widen caveats (`docs/design/loadout-composition.md` §Authority safety).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LoadoutSettings {
    /// Parameter axis: KV-cache window override (top of the `ModelTuning` chain).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub num_ctx: Option<u32>,
    /// Prompt axis: a one-line system-prompt framing (the `ModeConfig.framing` shape).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub framing: Option<String>,
}

impl Loadout {
    /// Validate the loadout's name references against `cfg`: a named `kit` must be a
    /// known bundle, a named `profile` must be a known, valid profile, and a named
    /// `provider` must name a `[backends]` entry (Slice 2 — the provider/model axis).
    /// A dangling reference is a hard error — a loadout that silently did nothing
    /// would be a false claim. The `@variant` half of `model` and `role` are resolved
    /// by their own surfaces later and are not checked here.
    ///
    /// # Errors
    /// The first dangling `kit`, `profile`, or `provider` reference, as a message.
    pub fn validate(&self, cfg: &Config) -> std::result::Result<(), String> {
        if let Some(kit) = &self.kit {
            cfg.resolve_bundle(kit)
                .map_err(|e| format!("loadout kit '{kit}': {e}"))?;
        }
        if let Some(profile) = &self.profile {
            cfg.resolve_profile(profile)
                .map_err(|e| format!("loadout profile '{profile}': {e}"))?;
        }
        if let Some(provider) = &self.provider {
            if !cfg.backends.iter().any(|b| &b.name == provider) {
                let known = if cfg.backends.is_empty() {
                    "none defined".to_string()
                } else {
                    cfg.backends
                        .iter()
                        .map(|b| b.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                return Err(format!(
                    "loadout provider '{provider}': no [backends] entry named '{provider}' (known: {known})"
                ));
            }
        }
        Ok(())
    }
}

impl Config {
    /// Merge per-file loadouts from the well-known `loadouts/` dirs next to the
    /// config: `~/.newt/loadouts/*.toml` first, then the project `.newt/loadouts/`
    /// (so project overrides home overrides inline `[loadouts.*]`). The filename
    /// stem is the loadout name. A malformed drop-in is skipped with a warning — it
    /// must not break startup. References *inside* a loadout are validated when it
    /// is selected (`--loadout`), not at load, mirroring the inline `[loadouts.*]`
    /// path.
    pub(super) fn merge_disk_loadouts(&mut self) {
        if let Some(dir) = Self::user_config_dir() {
            self.merge_loadouts_from_dir(&dir.join("loadouts"));
        }
        if let Some(proj) = Self::project_config_path() {
            if let Some(parent) = proj.parent() {
                self.merge_loadouts_from_dir(&parent.join("loadouts"));
            }
        }
    }

    /// Load `<dir>/*.toml` as loadouts (filename stem = name) into `self.loadouts`,
    /// last-wins on a name clash. A malformed file is skipped with a warning.
    pub(super) fn merge_loadouts_from_dir(&mut self, dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return; // no loadouts dir — fine
        };
        let mut paths: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "toml"))
            .collect();
        paths.sort();
        for path in paths {
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            match std::fs::read_to_string(&path).map(|t| toml::from_str::<Loadout>(&t)) {
                Ok(Ok(loadout)) => {
                    self.loadouts.insert(stem.to_string(), loadout);
                }
                Ok(Err(e)) => {
                    tracing::warn!(path = %path.display(), error = %e, "skipping malformed loadout file");
                }
                Err(_) => {}
            }
        }
    }
}
