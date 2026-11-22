//! **The active posture** — `/posture <name>`'s resolved binding (#2009 PR10c).
//!
//! Moved down from `newt-tui` because §5.1's ledger named it: the Permissions
//! section's WRITE half could not land while the posture lived in a `run_chat`
//! local, since `settings_form::apply` is a pure function with no view into
//! the session loop — *a receipt writer cannot read a local* (#1999).
//!
//! `ActivePosture` is plain data over `Caveats`. `build_posture` takes the two
//! config tables it resolves against (`[modes]`, `[permission_presets]`) and the
//! skill loader as a CLOSURE, so this crate names neither `Config` nor the skills
//! crate: the caller still supplies `newt_skills::load_body_from` over the
//! config-rooted search dirs, exactly as the `use_skill` tool resolves them.
//!
//! The value is a process-global mutex-protected binding. Settings and slash
//! commands install it here; accepted turns clone one snapshot for guidance
//! and enforcement. `GlobalSettingsGuard` restores the complete binding for
//! tests, separately from the preference-action accumulator.

use std::collections::BTreeMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::caveats::Caveats;
use crate::preset::NamedPermissionPreset;

/// One named permission-posture binding (`[modes.<name>]`, retained for
/// compatibility): the atomic binding `/posture <name>` applies.
///
/// ```toml
/// [modes.triage]
/// skill   = "oncall-triage"        # skill body to preload (use_skill path)
/// preset  = "readonly-triage"      # [permission_presets.<name>] to clamp to
/// framing = "On-call triage: investigate, do not change production."
/// ```
///
/// Every field is optional so a posture can do any subset (e.g. preset-only, or
/// framing-only). A `skill`/`preset` that names a missing entry is reported as
/// an error by the command rather than silently ignored — a posture that claims
/// a clamp it never applied would be a false security claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ModeConfig {
    /// Skill name to preload (the same `use_skill` / `load_body_from` path).
    /// `None` ⇒ no skill is loaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,
    /// `[permission_presets.<name>]` to apply as the session authority floor.
    /// `None` ⇒ authority unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// One-line framing injected into the system prompt. `None` ⇒ no framing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub framing: Option<String>,
}

/// The session's active `/posture` (issue #307): a configured skill/framing
/// binding plus its optional named-permission-preset clamp. Held by the session
/// next to `SessionCapability`; when configured, the clamp is `meet`-ed into
/// the effective caveats for every turn (and into the #263 gate's re-mint), so
/// it wins over both `--disable-ocap` and any interactive session-grant.
///
/// `None` (no posture active) means only that no posture-supplied clamp is
/// present. Session, operating-mode, persona, or other effective floors may
/// still narrow authority or force confined exec.
#[derive(Debug, Clone)]
pub struct ActivePosture {
    /// The posture name (the `<name>` in `/posture <name>`), for `/permissions`.
    pub name: String,
    /// The preset name that supplied the clamp (for reporting), or empty when
    /// this compatibility binding intentionally carries only skill/framing.
    pub preset_name: String,
    /// The authority ceiling (`NamedPermissionPreset::clamp`). The session's
    /// effective authority is `base.meet(&clamp)`.
    pub clamp: Caveats,
    /// One-line human summary of the clamp (for `/permissions`).
    pub clamp_summary: String,
    /// The skill this posture was activated with (`[modes.<name>] skill` AT activation),
    /// so `[skills] off` follows the binding even if the config is edited afterwards.
    pub skill: Option<String>,
    /// The validated skill guidance composed into each live turn.
    pub skill_body: Option<String>,
    /// Operator-defined framing composed into each live turn.
    pub framing: Option<String>,
}

impl ActivePosture {
    /// A compatibility binding without `preset` carries guidance only. Treat
    /// that as genuinely absent at every enforcement seam rather than passing
    /// an identity clamp that could still change the exec mechanism.
    #[must_use]
    pub fn permission_clamp(&self) -> Option<&Caveats> {
        (!self.preset_name.is_empty()).then_some(&self.clamp)
    }
}

/// Resolve and validate a `/posture <name>` invocation against the configured
/// `[modes]` and `[permission_presets]` tables and the skill loader,
/// WITHOUT mutating anything — the atomic-or-nothing core of the command. A
/// missing posture or any resource it explicitly names is an `Err`: a posture
/// that silently skipped a configured clamp or guidance would be a false
/// claim. A binding may intentionally omit its preset, skill, or framing. On
/// success the caller applies every configured effect together.
///
/// `load_skill` is the skill-body loader seam (production wires the same
/// `use_skill` / `newt_skills::load_body_from` path; tests inject a closure
/// over a mock skills dir) — so skill loading is NOT reimplemented here.
pub fn build_posture<E: std::fmt::Display>(
    name: &str,
    modes: &BTreeMap<String, ModeConfig>,
    presets: &BTreeMap<String, NamedPermissionPreset>,
    mut load_skill: impl FnMut(&str) -> Result<String, E>,
) -> anyhow::Result<ActivePosture> {
    let mode_cfg = modes.get(name).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown posture: '{name}' (no [modes.{name}] compatibility entry in config)"
        )
    })?;

    // Resolve the preset clamp (if the posture names one). A named-but-missing
    // preset is a hard error — never a silent no-clamp.
    let (preset_name, clamp, clamp_summary) = match &mode_cfg.preset {
        Some(preset_name) => {
            let preset = presets.get(preset_name).ok_or_else(|| {
                anyhow::anyhow!(
                    "posture '{name}' names preset '{preset_name}' but no \
                     [permission_presets.{preset_name}] is defined"
                )
            })?;
            (preset_name.clone(), preset.clamp(), preset.summary())
        }
        // A posture with no preset imposes no clamp (identity) — still valid
        // for skill + framing composition.
        None => (String::new(), Caveats::top(), "unconstrained".to_string()),
    };

    // Preload the skill body (if named) through the injected loader. A
    // named-but-unloadable skill is a hard error.
    let skill_body = match &mode_cfg.skill {
        Some(skill_name) => Some(
            load_skill(skill_name)
                .map_err(|e| anyhow::anyhow!("posture '{name}' skill '{skill_name}': {e}"))?,
        ),
        None => None,
    };

    Ok(ActivePosture {
        name: name.to_string(),
        preset_name,
        clamp,
        clamp_summary,
        skill: mode_cfg.skill.clone(),
        skill_body,
        framing: mode_cfg.framing.clone(),
    })
}

/// The posture the operator has installed for this session, if any.
static ACTIVE_POSTURE: Mutex<Option<ActivePosture>> = Mutex::new(None);

/// Install (or clear, with `None`) the session's active posture.
///
/// The ONE writer: `/posture` and `/settings posture` both land here, so the
/// verb and the field cannot install different bindings. Persona changes keep
/// the installed session binding.
pub fn set_active_posture(posture: Option<ActivePosture>) {
    if let Ok(mut slot) = ACTIVE_POSTURE.lock() {
        *slot = posture;
    }
}

/// The session's active posture, cloned.
///
/// Cloned rather than lent: a caller holding a guard across a turn would hold
/// the lock across `/posture`'s next write, and the clamp is small.
#[must_use]
pub fn active_posture() -> Option<ActivePosture> {
    ACTIVE_POSTURE.lock().ok().and_then(|s| s.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialises the tests that touch the process-global slot.
    static SLOT: Mutex<()> = Mutex::new(());

    /// The session slot round-trips and clears — the whole reason the value
    /// moved here (#2009 PR10c): a pure `settings_form::apply` can install it,
    /// and `/posture` reads back exactly what was installed.
    #[test]
    fn the_session_posture_round_trips_and_clears() {
        let _guard = SLOT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        set_active_posture(None);
        assert!(active_posture().is_none());

        set_active_posture(Some(ActivePosture {
            name: "locked".to_string(),
            preset_name: "strict".to_string(),
            clamp: Caveats::default(),
            clamp_summary: "deny writes".to_string(),
            skill: None,
            skill_body: None,
            framing: None,
        }));
        let got = active_posture().expect("installed");
        assert_eq!(got.name, "locked");
        assert_eq!(got.clamp_summary, "deny writes");

        set_active_posture(None);
        assert!(active_posture().is_none(), "clearing releases the clamp");
    }

    /// **A binding with no preset carries NO clamp**, and says so at the
    /// enforcement seam rather than handing back an identity clamp that could
    /// still change the exec mechanism.
    #[test]
    fn a_binding_without_a_preset_reports_no_clamp() {
        let guidance_only = ActivePosture {
            name: "coach".to_string(),
            preset_name: String::new(),
            clamp: Caveats::default(),
            clamp_summary: String::new(),
            skill: None,
            skill_body: Some("be careful".to_string()),
            framing: None,
        };
        assert!(
            guidance_only.permission_clamp().is_none(),
            "guidance only must not present as a clamp"
        );

        let clamped = ActivePosture {
            preset_name: "strict".to_string(),
            ..guidance_only
        };
        assert!(clamped.permission_clamp().is_some());
    }

    fn one_mode(name: &str, mode: ModeConfig) -> BTreeMap<String, ModeConfig> {
        BTreeMap::from([(name.to_string(), mode)])
    }

    fn never_loaded(_: &str) -> Result<String, String> {
        panic!("no skill is named, so the loader must not be called")
    }

    /// `build_posture` takes the two tables and a loader, not a `Config`, so every
    /// refusal is testable here with no filesystem. An unnamed posture says where its
    /// entry belongs.
    #[test]
    fn an_unknown_posture_is_an_error_naming_the_missing_entry() {
        let error = build_posture("ghost", &BTreeMap::new(), &BTreeMap::new(), never_loaded)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("unknown posture: 'ghost'") && error.contains("[modes.ghost]"),
            "{error}"
        );
    }

    /// A posture that names a preset nobody defined is a hard error, never a silent
    /// no-clamp.
    #[test]
    fn a_named_but_undefined_preset_is_a_hard_error() {
        let modes = one_mode(
            "triage",
            ModeConfig {
                preset: Some("strict".into()),
                ..ModeConfig::default()
            },
        );
        let error = build_posture("triage", &modes, &BTreeMap::new(), never_loaded)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("names preset 'strict'")
                && error.contains("[permission_presets.strict]"),
            "{error}"
        );
    }

    /// A skill the loader cannot produce fails the whole posture, naming both.
    #[test]
    fn a_named_skill_that_fails_to_load_is_a_hard_error() {
        let modes = one_mode(
            "coach",
            ModeConfig {
                skill: Some("review".into()),
                ..ModeConfig::default()
            },
        );
        let error = build_posture("coach", &modes, &BTreeMap::new(), |name| {
            Err(format!("{name} not found"))
        })
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("posture 'coach' skill 'review': review not found"),
            "{error}"
        );
    }

    /// A complete binding resolves its preset clamp, skill body and framing together.
    #[test]
    fn a_complete_binding_resolves_preset_skill_and_framing_together() {
        let modes = one_mode(
            "audit",
            ModeConfig {
                skill: Some("review".into()),
                preset: Some("strict".into()),
                framing: Some("read only".into()),
            },
        );
        let strict = NamedPermissionPreset {
            readonly: true,
            ..NamedPermissionPreset::default()
        };
        let presets = BTreeMap::from([("strict".to_string(), strict.clone())]);
        let posture = build_posture::<String>("audit", &modes, &presets, |name| {
            Ok(format!("body of {name}"))
        })
        .unwrap();
        assert_eq!(
            (posture.name.as_str(), posture.preset_name.as_str()),
            ("audit", "strict")
        );
        assert_eq!(posture.skill.as_deref(), Some("review"));
        assert_eq!(posture.skill_body.as_deref(), Some("body of review"));
        assert_eq!(posture.framing.as_deref(), Some("read only"));
        assert_eq!(posture.clamp, strict.clamp());
        assert_eq!(posture.clamp_summary, strict.summary());
        assert!(posture.permission_clamp().is_some());
    }

    /// A binding may omit its preset, skill and framing: it is valid, imposes no clamp
    /// and loads nothing.
    #[test]
    fn a_binding_with_nothing_named_imposes_nothing() {
        let modes = one_mode("plain", ModeConfig::default());
        let posture = build_posture("plain", &modes, &BTreeMap::new(), never_loaded).unwrap();
        assert_eq!(posture.preset_name, "");
        assert_eq!(posture.clamp, Caveats::top());
        assert_eq!(posture.clamp_summary, "unconstrained");
        assert!(posture.skill.is_none() && posture.skill_body.is_none());
        assert!(posture.permission_clamp().is_none());
    }
}
