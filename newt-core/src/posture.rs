//! **The active posture** — `/posture <name>`'s resolved binding (#2009 PR10c).
//!
//! Moved down from `newt-tui` because §5.1's ledger named it: the Permissions
//! section's WRITE half could not land while the posture lived in a `run_chat`
//! local, since `settings_form::apply` is a pure function with no view into
//! the session loop — *a receipt writer cannot read a local* (#1999).
//!
//! Everything here was already core-shaped. `ActivePosture` is plain data over
//! `Caveats`, and `build_posture` already took the skill loader as a CLOSURE,
//! which is what let it move without dragging the skills path with it: the
//! caller still supplies `newt_skills::load_body_from` over the config-rooted
//! search dirs, exactly as the `use_skill` tool resolves them.
//!
//! The value is a process-global under the #1850 lock, the same shape as
//! `session_markdown_mode`, `session_operating_mode`,
//! `session_compaction_trigger_policy` and `session_spill_lines`. A fifth
//! spelling of "the operator pinned this for the session" is how they come to
//! disagree.

use std::sync::Mutex;

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
    pub clamp: crate::Caveats,
    /// One-line human summary of the clamp (for `/permissions`).
    pub clamp_summary: String,
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
    pub fn permission_clamp(&self) -> Option<&crate::Caveats> {
        (!self.preset_name.is_empty()).then_some(&self.clamp)
    }
}

/// Resolve and validate a `/posture <name>` invocation against config + skills,
/// WITHOUT mutating anything — the atomic-or-nothing core of the command. A
/// missing posture or any resource it explicitly names is an `Err`: a posture
/// that silently skipped a configured clamp or guidance would be a false
/// claim. A binding may intentionally omit its preset, skill, or framing. On
/// success the caller applies every configured effect together.
///
/// `load_skill` is the skill-body loader seam (production wires the same
/// `use_skill` / `newt_skills::load_body_from` path; tests inject a closure
/// over a mock skills dir) — so skill loading is NOT reimplemented here.
pub fn build_posture(
    name: &str,
    cfg: &crate::Config,
    mut load_skill: impl FnMut(&str) -> newt_skills::Result<String>,
) -> anyhow::Result<ActivePosture> {
    let mode_cfg = cfg.modes.get(name).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown posture: '{name}' (no [modes.{name}] compatibility entry in config)"
        )
    })?;

    // Resolve the preset clamp (if the posture names one). A named-but-missing
    // preset is a hard error — never a silent no-clamp.
    let (preset_name, clamp, clamp_summary) = match &mode_cfg.preset {
        Some(preset_name) => {
            let preset = cfg.permission_presets.get(preset_name).ok_or_else(|| {
                anyhow::anyhow!(
                    "posture '{name}' names preset '{preset_name}' but no \
                     [permission_presets.{preset_name}] is defined"
                )
            })?;
            (preset_name.clone(), preset.clamp(), preset.summary())
        }
        // A posture with no preset imposes no clamp (identity) — still valid
        // for skill + framing composition.
        None => (
            String::new(),
            crate::Caveats::top(),
            "unconstrained".to_string(),
        ),
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
        skill_body,
        framing: mode_cfg.framing.clone(),
    })
}

/// The posture the operator has installed for this session, if any.
static ACTIVE_POSTURE: Mutex<Option<ActivePosture>> = Mutex::new(None);

/// Install (or clear, with `None`) the session's active posture.
///
/// The ONE writer: `/posture`, `/settings posture` and a persona swap all land
/// here, so the verb and the field cannot install different bindings.
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

    /// The session slot round-trips and clears — the whole reason the value
    /// moved here (#2009 PR10c): a pure `settings_form::apply` can install it,
    /// and `/posture` reads back exactly what was installed.
    #[test]
    fn the_session_posture_round_trips_and_clears() {
        let _guard = crate::test_guard::GlobalSettingsGuard::acquire();
        set_active_posture(None);
        assert!(active_posture().is_none());

        set_active_posture(Some(ActivePosture {
            name: "locked".to_string(),
            preset_name: "strict".to_string(),
            clamp: crate::Caveats::default(),
            clamp_summary: "deny writes".to_string(),
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
            clamp: crate::Caveats::default(),
            clamp_summary: String::new(),
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
}
