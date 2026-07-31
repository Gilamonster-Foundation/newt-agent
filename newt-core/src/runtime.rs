//! The single resolved snapshot of the runtime operator posture (#1139 / #1320).
//!
//! Cognition, tenacity, crew, the active persona, and the backend axis are each
//! resolved in their own module (over process-global dials + the `Config`); this
//! bundles them into ONE query — [`RuntimeSettingsSnapshot::resolve`] — so every
//! status surface (chat's `/psyche`, the `/psyche edit` summary, `solve`'s trace)
//! reads the same resolved posture instead of re-deriving each dial independently.
//!
//! This is the seam #1139's `BackendState` and the "resolve once" contract grow
//! from. It does NOT yet own the *mutation* of those globals — the setters
//! (`set_cli_cognition`, `apply_persona_backend`, …) still live at their call
//! sites; the snapshot is the read model. Widening it to own resolution end-to-end
//! (moving `BackendChoice` into core, threading one snapshot through dispatch) is
//! the remaining #1139 step, tracked separately.

use crate::config::Config;
use crate::role_profile::Cognition;
use crate::tenacity::{effective_tenacity, Tenacity};

/// The backend axis, layered honestly (#1139): what the config names, the
/// operator's live pin, the active persona's declaration, and the single winner
/// every entry point resolves to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackendState {
    /// The backend the resolved `Config` names as its `default_backend`.
    pub configured: Option<String>,
    /// The operator's live override (`$NEWT_PROVIDER` — set by `/backends`, a
    /// persona backend route, or a CLI pin), if any.
    pub operator: Option<String>,
    /// The active persona's declared `backend:`, if any.
    pub persona: Option<String>,
    /// The single backend name every entry point resolves to (via
    /// [`Config::select_configured_backend`]).
    pub effective: Option<String>,
    /// The effective model of the resolved backend (`None` ⇒ the server decides).
    pub model: Option<String>,
}

/// The full runtime operator posture, resolved ONCE from the process globals +
/// `Config`. The single query for "what is active".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeSettingsSnapshot {
    /// Effective cognition (override > persona > off). `None` ⇒ no reasoning.effort.
    pub cognition: Option<Cognition>,
    /// Effective tenacity (CLI > persona > config/family > Standard).
    pub tenacity: Tenacity,
    /// Crew launch gate (`NEWT_TEAM`).
    pub crew: bool,
    /// The active persona name, if any (session state — supplied by the caller).
    pub persona: Option<String>,
    /// The layered backend axis.
    pub backend: BackendState,
}

impl RuntimeSettingsSnapshot {
    /// Resolve the posture from the process globals + `cfg`. `persona` and
    /// `persona_backend` are session state (the active persona + its declared
    /// backend) the caller supplies — they are not newt-core globals.
    #[must_use]
    pub fn resolve(cfg: &Config, persona: Option<&str>, persona_backend: Option<&str>) -> Self {
        let effective = cfg.select_configured_backend();
        Self {
            cognition: crate::cognition::effective_cognition(),
            tenacity: effective_tenacity(),
            crew: std::env::var_os("NEWT_TEAM").is_some(),
            persona: persona.map(str::to_string),
            backend: BackendState {
                configured: cfg.default_backend.clone().filter(|s| !s.is_empty()),
                operator: std::env::var("NEWT_PROVIDER")
                    .ok()
                    .filter(|s| !s.is_empty()),
                persona: persona_backend.map(str::to_string),
                effective: effective.map(|b| b.name.clone()).filter(|n| !n.is_empty()),
                model: effective
                    .and_then(|b| b.effective_model())
                    .map(str::to_string),
            },
        }
    }

    /// A one-line posture summary — the shared render for `/psyche`, the
    /// `/psyche edit` apply line, and the `solve` trace.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "psyche · persona {} · cognition {} · tenacity {} · crew {}",
            self.persona.as_deref().unwrap_or("none"),
            self.cognition.map_or("off", Cognition::label),
            self.tenacity.label(),
            if self.crew { "on" } else { "off" },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BackendConfig, Config};
    use crate::test_guard::GlobalSettingsGuard;

    fn cfg_with(default: Option<&str>, backends: &[(&str, &str)]) -> Config {
        Config {
            default_backend: default.map(str::to_string),
            backends: backends
                .iter()
                .map(|(n, e)| BackendConfig {
                    name: (*n).to_string(),
                    endpoint: (*e).to_string(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn resolve_reads_effective_dials_and_the_selected_backend() {
        let _g = GlobalSettingsGuard::acquire();
        crate::tenacity::clear_cli_tenacity();
        crate::tenacity::set_persona_tenacity(Some(Tenacity::Relentless));
        crate::cognition::set_cli_cognition(crate::cognition::CognitionOverride::Set(
            Cognition::Contemplating,
        ));
        // SAFETY: single-threaded guarded test.
        unsafe {
            std::env::remove_var("NEWT_PROVIDER");
            std::env::remove_var("NEWT_TEAM");
        }
        let mut cfg = cfg_with(
            Some("sol"),
            &[("other", "http://o:1"), ("sol", "http://s:1")],
        );
        // Give the winner an explicit model so BackendState.model (resolved via
        // effective_model) is exercised, not just its None default.
        cfg.backends
            .iter_mut()
            .find(|b| b.name == "sol")
            .expect("sol backend present")
            .model = Some("sol-large".to_string());
        let snap = RuntimeSettingsSnapshot::resolve(&cfg, Some("bob"), Some("sol"));

        assert_eq!(snap.cognition, Some(Cognition::Contemplating));
        assert_eq!(snap.tenacity, Tenacity::Relentless);
        assert!(!snap.crew);
        assert_eq!(snap.persona.as_deref(), Some("bob"));
        // default_backend precedence beats first-listed `other`.
        assert_eq!(snap.backend.effective.as_deref(), Some("sol"));
        assert_eq!(snap.backend.configured.as_deref(), Some("sol"));
        // the effective backend's model rides through to BackendState.model.
        assert_eq!(snap.backend.model.as_deref(), Some("sol-large"));
        assert_eq!(snap.backend.persona.as_deref(), Some("sol"));
        assert!(snap.summary().contains("persona bob"));
        assert!(snap.summary().contains("tenacity relentless"));
    }

    #[test]
    fn operator_pin_and_crew_gate_are_reflected() {
        let _g = GlobalSettingsGuard::acquire();
        // SAFETY: single-threaded guarded test.
        unsafe {
            std::env::set_var("NEWT_PROVIDER", "other");
            std::env::set_var("NEWT_TEAM", "1");
        }
        let cfg = cfg_with(
            Some("sol"),
            &[("other", "http://o:1"), ("sol", "http://s:1")],
        );
        let snap = RuntimeSettingsSnapshot::resolve(&cfg, None, None);
        assert!(snap.crew, "NEWT_TEAM gates crew on");
        assert_eq!(snap.backend.operator.as_deref(), Some("other"));
        // NEWT_PROVIDER pin beats default_backend for the effective winner.
        assert_eq!(snap.backend.effective.as_deref(), Some("other"));
        assert!(snap.summary().contains("crew on"));
    }
}
