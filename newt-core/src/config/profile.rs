//! Harness technique profiles, bundle selection, and bundle drop-in loading.
//!
//! This family selects technique composition and its knobs. Loadout/crew assembly
//! and permission authority remain with their existing owners.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::Config;

/// The known harness techniques a profile may compose — the registry the
/// validator checks against. A profile naming a technique outside this set is
/// rejected (an unknown technique a profile claims but cannot apply would be a
/// false claim). Extend this as techniques land (R3 `fact_preserving_compression`,
/// R4 `self_grounding`, …).
pub const KNOWN_TECHNIQUES: &[&str] = &[
    "knowledge_base", // R1 — inject the authoritative import surface (#74)
    "verify_gate",    // R2 — revert files with fabricated imports (#73)
    "retry",          // revert-retry loop over the gate's revert set
];

/// One named profile (`[profiles.<name>]`): the harness techniques to compose for
/// a model family / context, plus each technique's knob settings.
///
/// ```toml
/// [profiles.nemotron]
/// techniques = ["knowledge_base", "verify_gate", "retry"]
///
/// [profiles.nemotron.verify_gate]
/// surface_match = "exact"        # SurfaceMatch — leaf-exact (the complete-gate default)
///
/// [profiles.nemotron.retry]
/// max_retries = 2
/// ```
///
/// A knob table only takes effect when its technique is enabled. An unknown
/// technique name is an error ([`ProfileConfig::validate`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProfileConfig {
    /// The ordered set of techniques this profile composes. Empty ⇒ the profile
    /// applies no techniques (equivalent to the `default`/light profile).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub techniques: Vec<String>,
    /// Knobs for the `verify_gate` technique (applied iff it is enabled).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_gate: Option<VerifyGateKnobs>,
    /// Knobs for the `retry` technique (applied iff it is enabled).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryKnobs>,
}

/// Tunable knobs for the `verify_gate` technique.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct VerifyGateKnobs {
    /// How strictly the project surface is matched. Default `Exact` — the
    /// adversarially-complete setting (the retry-Goodhart finding).
    #[serde(default)]
    pub surface_match: crate::verify_gate::SurfaceMatch,
    /// How strictly the gate ACTS on flagged output — the tier. Default
    /// `RevertRetry` (today's behavior when the `retry` technique is on); lower
    /// tiers (`off`/`advisory`/`revert_once`) trade enforcement for latitude.
    #[serde(default)]
    pub tier: crate::verify_gate::VerifyTier,
}

/// Tunable knobs for the `retry` technique.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryKnobs {
    /// Maximum revert-retry attempts. Default 2.
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
}

const fn default_max_retries() -> u32 {
    2
}

impl Default for RetryKnobs {
    fn default() -> Self {
        Self {
            max_retries: default_max_retries(),
        }
    }
}

impl ProfileConfig {
    /// Validate the profile against the [component registry](crate::kit): every
    /// named technique must be a known component, and every component's
    /// `presupposes` must also be enabled (e.g. `retry` presupposes `verify_gate`).
    /// A presupposition gap is a **load-time** error, not a silent partial apply.
    ///
    /// # Errors
    /// Returns the first unknown-technique or unmet-presupposition as a message.
    pub fn validate(&self) -> std::result::Result<(), String> {
        for t in &self.techniques {
            let Some(entry) = crate::kit::component(t) else {
                return Err(format!(
                    "unknown technique '{t}' in profile (known: {})",
                    KNOWN_TECHNIQUES.join(", ")
                ));
            };
            for pre in entry.presupposes {
                if !self.techniques.iter().any(|x| x == pre) {
                    return Err(format!(
                        "technique '{t}' presupposes '{pre}', which the profile does not enable"
                    ));
                }
            }
        }
        Ok(())
    }

    /// Whether this profile enables `technique`.
    #[must_use]
    pub fn enables(&self, technique: &str) -> bool {
        self.techniques.iter().any(|t| t == technique)
    }

    /// The effective `verify_gate` knobs (defaults when unset).
    #[must_use]
    pub fn verify_gate_knobs(&self) -> VerifyGateKnobs {
        self.verify_gate.unwrap_or_default()
    }

    /// The effective `retry` knobs (defaults when unset).
    #[must_use]
    pub fn retry_knobs(&self) -> RetryKnobs {
        self.retry.unwrap_or_default()
    }
}

/// One named bundle (`[bundles.<name>]`) — the loadable unit of the model support
/// kit. It pins which model families it applies to and which profile each resolves
/// to, shipping the `[profiles.*]` it references.
///
/// ```toml
/// [bundles.nemotron]
/// about = "Support bundle for the nemotron family"
/// applies_to = ["nemotron"]                 # EXACT typed family names (card metadata)
/// default_profile = "nemotron"
/// families = { "nemotron" = "nemotron", "qwen3" = "qwen-coder" }
/// ```
///
/// A bundle carries **no authority** — there is deliberately no caveats/preset
/// field; it recombines vetted parts, it cannot grant (`docs/design/model-support-kit.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BundleConfig {
    /// One-line provenance, shown in the startup banner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    // INERT-CODE-RATCHET: F07 WIRE: bundle about text is parsed but never reaches the promised startup banner.
    pub about: Option<String>,
    /// Model-id prefixes this bundle auto-applies to (longest-prefix-wins). Empty ⇒
    /// a use-case bundle: chosen only via explicit `--bundle`, never auto-inferred.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applies_to: Vec<String>,
    /// Profile applied when this bundle is selected and no `families` entry matches.
    /// Must name a key in `[profiles.*]`. `None` ⇒ no profile (the light path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_profile: Option<String>,
    /// model-family-prefix → profile name (longest-prefix-wins over `default_profile`).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub families: std::collections::BTreeMap<String, String>,
}

/// The active-profile selection + how it was chosen (for honest banner output).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfilePick {
    /// The chosen profile name (to feed [`Config::resolve_profile`]).
    pub name: String,
    /// Which selector won.
    pub via: PickVia,
}

/// How a [`ProfilePick`] was selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickVia {
    /// An explicit `--profile` / `NEWT_PROFILE`.
    Profile,
    /// An explicit `--bundle <name>`.
    Bundle(String),
    /// A bundle inferred from the model via `applies_to`.
    InferredBundle(String),
}

impl Config {
    /// Look up and validate a named profile (`[profiles.<name>]`). The caller
    /// selects it via `--profile <name>` / `NEWT_PROFILE`.
    ///
    /// # Errors
    /// `no such profile` when the name is undefined; the validation error when
    /// the profile names an unknown technique — a `--profile` that silently did
    /// nothing would be a false claim, so both fail loudly.
    pub fn resolve_profile(&self, name: &str) -> std::result::Result<&ProfileConfig, String> {
        let profile = self.profiles.get(name).ok_or_else(|| {
            let known = if self.profiles.is_empty() {
                "none defined".to_string()
            } else {
                self.profiles.keys().cloned().collect::<Vec<_>>().join(", ")
            };
            format!("no such profile (known: {known})")
        })?;
        profile.validate()?;
        Ok(profile)
    }

    /// Look up a named bundle (`[bundles.<name>]`).
    ///
    /// # Errors
    /// `no such bundle` when undefined — a `--bundle` that silently did nothing
    /// would be a false claim.
    pub fn resolve_bundle(&self, name: &str) -> std::result::Result<&BundleConfig, String> {
        self.bundles.get(name).ok_or_else(|| {
            let known = if self.bundles.is_empty() {
                "none defined".to_string()
            } else {
                self.bundles.keys().cloned().collect::<Vec<_>>().join(", ")
            };
            format!("no such bundle (known: {known})")
        })
    }

    /// The profile name `bundle` yields for `model`: the longest-prefix `families`
    /// match, else `default_profile`. `None` ⇒ the bundle applies no profile here.
    #[must_use]
    pub fn bundle_profile_for_family<'a>(
        &self,
        bundle: &'a BundleConfig,
        family: Option<&str>,
    ) -> Option<&'a str> {
        family
            .and_then(|fam| {
                bundle
                    .families
                    .iter()
                    .find(|(key, _)| key.as_str() == fam)
                    .map(|(_, p)| p.as_str())
            })
            .or(bundle.default_profile.as_deref())
    }

    /// Infer the bundle for the TYPED model family (the resolved card's
    /// declared metadata under the route-association gates — never a
    /// model-name prefix): a bundle applies when its `applies_to` names the
    /// family EXACTLY. No family ⇒ no automatic bundle — a qwen-LOOKING
    /// alias with no exact card gets no family behavior (the anti-substring
    /// law: names are labels, never evidence). Only bundles with a
    /// non-empty `applies_to` participate — a use-case bundle (empty
    /// `applies_to`) is never auto-inferred, only chosen explicitly via
    /// `--bundle`.
    #[must_use]
    pub fn infer_bundle_for_family(&self, family: Option<&str>) -> Option<(&str, &BundleConfig)> {
        let fam = family?;
        self.bundles
            .iter()
            .find(|(_, b)| b.applies_to.iter().any(|a| a == fam))
            .map(|(name, b)| (name.as_str(), b))
    }

    /// Resolve the active profile from the selectors + the TYPED model
    /// family: `--profile` (explicit) > `--bundle` (its profile for this
    /// family) > a bundle inferred from the exact family (`applies_to`) >
    /// `None`. Automatic selection keys on the resolved card's declared
    /// family under the route-association gates — NEVER on model-name
    /// prefixes. Returns the profile NAME + how it was chosen (for the
    /// banner).
    ///
    /// # Errors
    /// An unknown explicit `--bundle` is a hard error. An unknown explicit
    /// `--profile` is left for the caller's [`resolve_profile`](Self::resolve_profile)
    /// so the message stays profile-specific.
    pub fn pick_active_profile(
        &self,
        profile_flag: Option<&str>,
        bundle_flag: Option<&str>,
        family: Option<&str>,
    ) -> std::result::Result<Option<ProfilePick>, String> {
        if let Some(p) = profile_flag.filter(|s| !s.is_empty()) {
            return Ok(Some(ProfilePick {
                name: p.to_string(),
                via: PickVia::Profile,
            }));
        }
        if let Some(b) = bundle_flag.filter(|s| !s.is_empty()) {
            let bundle = self.resolve_bundle(b)?;
            return Ok(self
                .bundle_profile_for_family(bundle, family)
                .map(|p| ProfilePick {
                    name: p.to_string(),
                    via: PickVia::Bundle(b.to_string()),
                }));
        }
        if let Some((name, bundle)) = self.infer_bundle_for_family(family) {
            return Ok(self
                .bundle_profile_for_family(bundle, family)
                .map(|p| ProfilePick {
                    name: p.to_string(),
                    via: PickVia::InferredBundle(name.to_string()),
                }));
        }
        Ok(None)
    }

    /// Merge per-file bundles from the well-known `bundles/` dirs next to the
    /// config: `~/.newt/bundles/*.toml` first, then the project `.newt/bundles/`
    /// (so project overrides home overrides inline `[bundles.*]`). The filename
    /// stem is the bundle name. A malformed drop-in is skipped with a warning — it
    /// must not break startup.
    pub(super) fn merge_disk_bundles(&mut self) {
        if let Some(dir) = Self::user_config_dir() {
            self.merge_bundles_from_dir(&dir.join("bundles"));
        }
        if let Some(proj) = Self::project_config_path() {
            if let Some(parent) = proj.parent() {
                self.merge_bundles_from_dir(&parent.join("bundles"));
            }
        }
    }

    /// Load `<dir>/*.toml` as bundles (filename stem = name) into `self.bundles`,
    /// last-wins on a name clash. A malformed file is skipped with a warning.
    pub(super) fn merge_bundles_from_dir(&mut self, dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return; // no bundles dir — fine
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
            match std::fs::read_to_string(&path).map(|t| toml::from_str::<BundleConfig>(&t)) {
                Ok(Ok(bundle)) => {
                    self.bundles.insert(stem.to_string(), bundle);
                }
                Ok(Err(e)) => {
                    tracing::warn!(path = %path.display(), error = %e, "skipping malformed bundle file");
                }
                Err(_) => {}
            }
        }
    }
}
