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

use crate::cognition::CognitionOverride;
use crate::config::Config;
use crate::role_profile::Cognition;
use crate::tenacity::{effective_tenacity, Tenacity};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

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
    /// Effective cognition (override > persona > off). `None` means no
    /// backend-specific reasoning controls are requested.
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

/// #1668: the OPERATOR-pinned preference subset a conversation carries across
/// resume — persisted on the conversation row (`preference_pin` column) and
/// applied through the existing setters when the conversation is resumed.
///
/// Every field records an *operator ACTION*, NEVER a resolved effective value
/// and never ambient session state: `backend` is the name a successful
/// `/backends <name>` picked, `model` the name a successful `/model <name>` /
/// psyche-panel spinner picked, `cognition` / `tenacity` the levels a
/// `/psyche` setter (or the panel's dirty dial) installed. A `None` field
/// means the operator never acted on that axis, so a session that only ever
/// *looks* — a bare `/backends` listing, a persona switch, a refused pick —
/// round-trips to an EMPTY pin.
///
/// **Why action-scoped and not ambient** (the 2026-08-13 adversarial review,
/// findings 1/2/3/7): snapshotting the session's live posture per turn cannot
/// tell an operator choice from a persona's route, from a previously-applied
/// pin's residue, or from a config default — so the pin absorbed all three and
/// then re-persisted itself into every conversation the session later visited.
/// Recording only the axes an operator action actually set makes that class of
/// leak unrepresentable: nothing is written unless an action fired, and an
/// action names exactly the axes it changed.
///
/// **Merge, never replace.** A new action updates only its own axes in the
/// stored pin ([`OperatorPreferencePin::merged`]); the untouched axes keep
/// whatever was stored, so a pin that fails open on apply survives verbatim.
///
/// **Precedence on resume** (highest first):
/// 1. this invocation's EXPLICIT inputs — `--backend-*`/`--model`-equivalent
///    env, `--cognition`, `--tenacity`, `--obsessive`, a `--loadout` axis
///    ([`PreferenceAxes`], recorded at launch): the pin never overrides an axis
///    the operator just typed;
/// 2. this conversation's pinned axes;
/// 3. the active persona's declared `backend:` / dials;
/// 4. the invocation baseline (config + sticky settings), which is also what
///    an *unpinned* axis resolves to after a conversation switch.
///
/// # Authority boundary: a pin carries PREFERENCE, never AUTHORITY
///
/// Every field here is operator-*preference* state — which endpoint to talk
/// to, which model, how hard to think, how hard to push. A pin may **never**
/// contain, encode, or indirectly select OCAP grants, `Caveats`, permission
/// clamps, enforcement floors, sandbox / filesystem / network capability,
/// credentials, API keys, endpoints, backend *definitions*, or execution
/// authorization of any kind. `backend` and `model` are NAMES, resolved
/// against the operator's own `Config` at apply time
/// ([`Self::apply_plan`] validates the name against the configured backend
/// list): a pin *selects among* backends the operator already configured; it
/// can never define or reach one, and keys and URLs always come from `Config`,
/// never from the row.
///
/// That boundary is not a style note; it is what makes this type's sparse,
/// fail-open restore SAFE. A missing, stale, or corrupt pin degrades to "run
/// with the invocation baseline preference", which is at worst inconvenient.
/// The same fail-open shape applied to authority would degrade to "run with
/// yesterday's grants" — silent privilege with no live decision behind it —
/// and this pin is written from a process global and read back on an unrelated
/// resume, exactly the ambient-state bleed that made the pre-review capture
/// leak one conversation's settings into every other one.
///
/// **The name is load-bearing.** `/posture` (#307, `ActivePosture` in
/// newt-tui) is a DIFFERENT concept that happens to share the word: an
/// authority CEILING holding a `Caveats` clamp, deliberately process-lifetime
/// and deliberately never persisted. This type is called
/// `OperatorPreferencePin` — not `PosturePin` — precisely so the two are not
/// one identifier apart: adding a `posture_preset` field here would be a
/// one-line change that silently gave an authority ceiling fail-open restore
/// semantics.
///
/// **Next author:** authority restores FAIL-CLOSED and belongs in the
/// capability / OCAP layer, which re-derives grants per session from config +
/// live consent. Do not add a field here for it. This is a CLOSED set of
/// concrete fields — no maps, no extension bag, no generic blob:
/// `pin_serializes_exactly_the_four_preference_axes` pins the serialized key
/// set so a new field cannot land without confronting this section, and
/// `deny_unknown_fields` makes a row carrying anything else a hard decode
/// error (the caller degrades it to a one-line notice + the baseline) rather
/// than a silently-tolerated smuggled key. The cost of that strictness is
/// deliberate: an older binary reading a row a newer one pinned refuses it
/// loudly and runs on the baseline, which is the right failure for a
/// convenience column and the right alarm for a tampered one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OperatorPreferencePin {
    /// The backend NAME (a `[[backends]]` entry) a successful `/backends
    /// <name>` picked, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// The model a successful `/model <name>` / panel spinner pick installed
    /// (`NEWT_DGX_MODEL`), if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The `/cognition` override in its stable string form: `"off"` or a
    /// [`Cognition`] label. `None` ⇒ the operator never overrode cognition
    /// ([`CognitionOverride::Unset`] — deliberately distinct from `"off"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cognition: Option<String>,
    /// The `/tenacity` session override, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenacity: Option<Tenacity>,
}

/// What applying a [`OperatorPreferencePin`] on resume should DO — resolved purely (no
/// env/global mutation here) so fail-open rules are unit-testable. The caller
/// performs the side effects through the existing setters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreferenceApplyPlan {
    /// Install this `/cognition` override; `None` ⇒ leave the dial alone.
    pub cognition: Option<CognitionOverride>,
    /// Install this `/tenacity` override; `None` ⇒ leave the dial alone.
    pub tenacity: Option<Tenacity>,
    /// What to do with the backend axis (`NEWT_PROVIDER`/`NEWT_DGX_MODEL`).
    pub backend_axis: BackendAxisAction,
    /// One-line fail-open notices (unknown pinned backend, unparseable
    /// cognition) for the caller to print. Empty when everything applies.
    pub notices: Vec<String>,
}

/// What a [`BackendAxisAction::Route`] does to the session-model override.
///
/// The distinction is load-bearing (#1668 review-2 finding 2): "the pin named
/// no model" and "this invocation OWNS the model axis" both used to arrive as
/// `None`, so routing a pinned backend under an operator-supplied
/// `NEWT_DGX_MODEL` DELETED that model — the opposite of the rule that an
/// explicit input for this run outranks a stored preference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteModel {
    /// The pin named a model — install it.
    Set(String),
    /// The pin named no model — clear the override so the backend's own
    /// default applies, exactly as `/backends <name>` does.
    Clear,
    /// This invocation owns the model axis (a flag or exported env named it):
    /// leave the operator's model exactly as they supplied it.
    Keep,
}

/// The backend-axis half of a [`PreferenceApplyPlan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendAxisAction {
    /// The pin says nothing usable about the backend axis — nothing was
    /// pinned, the pinned backend no longer exists, or this invocation's own
    /// flags own the axis. Fail-open: the caller applies its baseline for the
    /// axis rather than anything from the pin.
    Leave,
    /// Route to `provider`, with [`RouteModel`] saying what happens to the
    /// session-model override.
    Route { provider: String, model: RouteModel },
    /// Only the model override was pinned; the provider stays untouched
    /// (the `/model <name>` case).
    ModelOnly(String),
}

/// A set of posture axes, used to say which ones somebody else already owns
/// (#1668). Recorded at launch for THIS invocation's explicit inputs — an
/// `NEWT_PROVIDER`/`NEWT_DGX_MODEL` the operator exported or a `--backend-*`
/// with a destination, a `--loadout` axis, `--cognition`, `--tenacity`,
/// `--obsessive` — and consulted by [`OperatorPreferencePin::apply_plan`], which refuses
/// to overwrite an axis the operator just typed with a stored pin.
///
/// Deliberately NOT recorded: the #545 sticky `~/.newt/settings.toml` restore,
/// which is documented as the lowest-precedence rung — it is last run's choice,
/// not this run's, so a conversation's own pin outranks it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PreferenceAxes {
    /// The named backend (`NEWT_PROVIDER`).
    pub backend: bool,
    /// The session model override (`NEWT_DGX_MODEL`).
    pub model: bool,
    /// The `/cognition` dial.
    pub cognition: bool,
    /// The `/tenacity` dial.
    pub tenacity: bool,
}

impl PreferenceAxes {
    /// Union — recording an axis never un-records another.
    pub fn merge(&mut self, other: Self) {
        self.backend |= other.backend;
        self.model |= other.model;
        self.cognition |= other.cognition;
        self.tenacity |= other.tenacity;
    }

    /// `true` when no axis is in the set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// The axis names in this set, for an operator-facing notice.
    #[must_use]
    pub fn labels(&self) -> Vec<&'static str> {
        [
            (self.backend, "backend"),
            (self.model, "model"),
            (self.cognition, "cognition"),
            (self.tenacity, "tenacity"),
        ]
        .into_iter()
        .filter_map(|(on, label)| on.then_some(label))
        .collect()
    }
}

/// What ONE operator posture action changed (#1668) — the per-axis record the
/// capture path merges into the conversation's stored [`OperatorPreferencePin`].
///
/// An axis is `None` when the action did not touch it (leave the stored value
/// alone), `Some(Some(v))` when the action set it to `v`, and `Some(None)` when
/// the action explicitly CLEARED it (`/backends <name>` clears the model
/// override; `/psyche tenacity auto` releases the dial) — clearing unpins the
/// axis so it resolves from the invocation baseline again.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreferenceActions {
    /// The named backend the operator routed to.
    pub backend: Option<Option<String>>,
    /// The session model override the operator installed.
    pub model: Option<Option<String>>,
    /// The `/cognition` override the operator chose ([`CognitionOverride::Unset`]
    /// is the operator choosing `auto`, which unpins the axis).
    pub cognition: Option<CognitionOverride>,
    /// The `/tenacity` override the operator chose (`Some(None)` = `auto`).
    pub tenacity: Option<Option<Tenacity>>,
}

impl PreferenceActions {
    /// `true` when no axis was acted on — nothing to persist.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Fold a later action set over this one: `other`'s acted axes win, this
    /// one's untouched axes survive. Used to hold actions PENDING while the
    /// conversation has no durable row yet.
    pub fn merge(&mut self, other: Self) {
        if other.backend.is_some() {
            self.backend = other.backend;
        }
        if other.model.is_some() {
            self.model = other.model;
        }
        if other.cognition.is_some() {
            self.cognition = other.cognition;
        }
        if other.tenacity.is_some() {
            self.tenacity = other.tenacity;
        }
    }
}

// #1668: the posture-action accumulator. Operator posture commands are spread
// across the command modules (`commands::model`, `commands::settings`, the
// psyche panel) and none of them can reach the session's conversation state, so
// — exactly like `cognition`'s `CLI_COGNITION` — the *fact that an action
// succeeded* is stashed process-global at the point where success is known, and
// the chat loop drains it once per iteration into a single merged pin write.
static PREFERENCE_ACTIONS: Mutex<PreferenceActions> = Mutex::new(PreferenceActions {
    backend: None,
    model: None,
    cognition: None,
    tenacity: None,
});

// #1668: the axes THIS INVOCATION's explicit inputs own (see [`PreferenceAxes`]).
// Recorded once at launch by `newt-cli`, read by every pin apply.
static CLI_PREFERENCE_AXES: Mutex<PreferenceAxes> = Mutex::new(PreferenceAxes {
    backend: false,
    model: false,
    cognition: false,
    tenacity: false,
});

/// Record a SUCCESSFUL `/backends <name>` (or an equivalent named-backend
/// pick): the backend axis takes `name` and the model axis is CLEARED, exactly
/// as the command does to `NEWT_DGX_MODEL` so the backend's own default model
/// applies. Call only where success is known — never for a listing, a refused
/// name, or a persona's route.
pub fn mark_backend_pick(name: &str) {
    if let Ok(mut slot) = PREFERENCE_ACTIONS.lock() {
        slot.backend = Some(Some(name.to_string()));
        slot.model = Some(None);
    }
}

/// Record a SUCCESSFUL model pick (`/model <name>`, `/backend ollama <name>`,
/// the psyche panel's spinner) — after the #1122 served-validation gate, never
/// before it.
pub fn mark_model_pick(name: &str) {
    if let Ok(mut slot) = PREFERENCE_ACTIONS.lock() {
        slot.model = Some(Some(name.to_string()));
    }
}

/// Record an operator `/cognition` choice (`off` / a level / `auto`), at the
/// same point the override is installed.
pub fn mark_cognition_choice(o: CognitionOverride) {
    if let Ok(mut slot) = PREFERENCE_ACTIONS.lock() {
        slot.cognition = Some(o);
    }
}

/// Record an operator `/tenacity` choice — `None` is `auto` (override cleared).
pub fn mark_tenacity_choice(t: Option<Tenacity>) {
    if let Ok(mut slot) = PREFERENCE_ACTIONS.lock() {
        slot.tenacity = Some(t);
    }
}

/// Take the actions accumulated since the last drain, leaving the accumulator
/// empty. The chat loop is the ONE caller: exactly one drain per iteration
/// feeding exactly one merged pin write.
#[must_use]
pub fn drain_preference_actions() -> PreferenceActions {
    PREFERENCE_ACTIONS
        .lock()
        .map(|mut slot| std::mem::take(&mut *slot))
        .unwrap_or_default()
}

/// Record the posture axes this invocation's explicit inputs own (unions with
/// what is already recorded). Called by `newt-cli` at launch, before the TUI
/// applies any conversation pin.
pub fn record_cli_preference_axes(axes: PreferenceAxes) {
    if let Ok(mut slot) = CLI_PREFERENCE_AXES.lock() {
        slot.merge(axes);
    }
}

/// The posture axes this invocation's explicit inputs own.
#[must_use]
pub fn cli_preference_axes() -> PreferenceAxes {
    CLI_PREFERENCE_AXES.lock().map(|s| *s).unwrap_or_default()
}

/// Both #1668 process globals, snapshotted as one unit so the shared test guard
/// can restore them (an action marked by one test must never be drained by the
/// next, and a recorded CLI axis must not suppress another test's pin apply).
#[doc(hidden)]
pub struct PreferenceRuntimeSnapshot {
    actions: PreferenceActions,
    cli_axes: PreferenceAxes,
}

/// Snapshot the #1668 posture globals (see [`PreferenceRuntimeSnapshot`]).
#[doc(hidden)]
#[must_use]
pub fn snapshot_runtime_state() -> PreferenceRuntimeSnapshot {
    PreferenceRuntimeSnapshot {
        actions: PREFERENCE_ACTIONS
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default(),
        cli_axes: cli_preference_axes(),
    }
}

/// Restore the #1668 posture globals from a snapshot.
#[doc(hidden)]
pub fn restore_runtime_state(snapshot: PreferenceRuntimeSnapshot) {
    if let Ok(mut slot) = PREFERENCE_ACTIONS.lock() {
        *slot = snapshot.actions;
    }
    if let Ok(mut slot) = CLI_PREFERENCE_AXES.lock() {
        *slot = snapshot.cli_axes;
    }
}

impl OperatorPreferencePin {
    /// This pin with `actions`' acted axes updated and every UNACTED axis kept
    /// verbatim — the only way a pin is ever written (#1668).
    ///
    /// Per-axis merge is what makes the pin honest under partial failure: an
    /// axis that failed open on apply (a since-removed backend, an unparseable
    /// cognition) is never mentioned by a later action, so it survives in the
    /// row exactly as stored instead of being overwritten by whatever the
    /// session happened to resolve to.
    #[must_use]
    pub fn merged(&self, actions: &PreferenceActions) -> Self {
        let mut next = self.clone();
        if let Some(backend) = &actions.backend {
            next.backend = backend.clone();
        }
        if let Some(model) = &actions.model {
            next.model = model.clone();
        }
        if let Some(cognition) = actions.cognition {
            next.cognition = Self::cognition_field(cognition);
        }
        if let Some(tenacity) = actions.tenacity {
            next.tenacity = tenacity;
        }
        next
    }

    /// The stable string form of a `/cognition` override for the pin:
    /// `Unset` ⇒ `None` (nothing pinned), `Off` ⇒ `"off"`, `Set(l)` ⇒ label.
    #[must_use]
    pub fn cognition_field(o: CognitionOverride) -> Option<String> {
        match o {
            CognitionOverride::Unset => None,
            CognitionOverride::Off => Some("off".to_string()),
            CognitionOverride::Set(level) => Some(level.label().to_string()),
        }
    }

    /// Parse a pinned cognition string back to its override form
    /// (inverse of [`cognition_field`](Self::cognition_field)).
    fn parse_cognition(s: &str) -> Result<CognitionOverride, String> {
        if s.trim().eq_ignore_ascii_case("off") {
            return Ok(CognitionOverride::Off);
        }
        s.parse::<Cognition>().map(CognitionOverride::Set)
    }

    /// `true` when nothing is pinned — the round-trip form of a session that
    /// never touched the dials, and of every pre-#1668 row (`'{}'` backfill).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Resolve what applying this pin should do, validated against the
    /// `configured` backend names (fail-open, never an error) and yielding to
    /// `owned` — the axes THIS invocation's explicit inputs already own
    /// ([`PreferenceAxes`], normally [`cli_preference_axes`]):
    ///
    /// - an axis in `owned` is dropped from the plan entirely: the operator's
    ///   just-typed flag beats a stored pin, and nothing is written back
    ///   (capture is action-only), so the pin survives for the next run;
    /// - a pinned backend that IS configured routes (with the pinned model as
    ///   the override — `None` clears it, `/backends` semantics);
    /// - a pinned backend that is NO LONGER configured contributes NOTHING to
    ///   the plan, with a notice (its pinned model belonged to that backend's
    ///   context, so it is dropped too) — the caller's baseline reset then
    ///   lands that axis on the invocation baseline;
    /// - a model-only pin applies over whatever provider the caller resolved;
    /// - an unparseable cognition string contributes nothing, with a notice;
    /// - an empty pin contributes nothing at all (`Leave`, no notices).
    ///
    /// `Leave` therefore means "this pin says nothing about the backend axis",
    /// NOT "keep whatever is currently live": the caller resets to the
    /// invocation baseline first, so an unpinned or unusable axis lands there
    /// rather than inheriting the previous conversation's route.
    #[must_use]
    pub fn apply_plan(&self, configured: &[&str], owned: PreferenceAxes) -> PreferenceApplyPlan {
        let mut notices = Vec::new();
        let cognition = self
            .cognition
            .as_deref()
            .filter(|_| !owned.cognition)
            .and_then(|s| {
                Self::parse_cognition(s)
                    .map_err(|e| notices.push(format!("pinned cognition ignored: {e}")))
                    .ok()
            });
        // An owned axis is invisible to the plan — including for the
        // "backend pinned but gone" notice, which would otherwise nag about a
        // pin the operator's own flag already superseded this run.
        let backend = self.backend.as_ref().filter(|_| !owned.backend);
        let model = self.model.as_ref().filter(|_| !owned.model);
        let backend_axis = match (backend, model) {
            (None, None) => BackendAxisAction::Leave,
            (Some(name), model) => {
                if configured.contains(&name.as_str()) {
                    BackendAxisAction::Route {
                        provider: name.clone(),
                        // `Keep` ONLY when this run owns the model axis. A pin
                        // that simply names no model still clears the override
                        // (the `/backends <name>` rule) — conflating the two
                        // is what deleted an operator's exported model.
                        model: match (model, owned.model) {
                            (Some(m), _) => RouteModel::Set(m.clone()),
                            (None, true) => RouteModel::Keep,
                            (None, false) => RouteModel::Clear,
                        },
                    }
                } else {
                    notices.push(format!(
                        "pinned backend '{name}' is no longer configured — \
                         using this run's baseline backend"
                    ));
                    BackendAxisAction::Leave
                }
            }
            (None, Some(model)) => BackendAxisAction::ModelOnly(model.clone()),
        };
        PreferenceApplyPlan {
            cognition,
            tenacity: self.tenacity.filter(|_| !owned.tenacity),
            backend_axis,
            notices,
        }
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

    // ------------------------------------------------------------------
    // #1668: OperatorPreferencePin — action-scoped capture, serde round-trip, and apply
    // planning.
    // ------------------------------------------------------------------

    #[test]
    fn a_session_with_no_operator_action_writes_nothing() {
        // The load-bearing #1668 contract, and the 2026-08-13 review's root
        // cause: capture is ACTION-scoped, so ambient session state — a
        // persona's dials, a config default, a previously-applied pin's
        // residue — can never become a pin. With no action marked the drain is
        // empty, and merging an empty drain leaves the stored pin identical.
        let _g = GlobalSettingsGuard::acquire();
        let _ = drain_preference_actions();
        // Ambient state a per-turn snapshot would have swept up:
        crate::cognition::set_cli_cognition(CognitionOverride::Off);
        crate::tenacity::set_cli_tenacity(Tenacity::Relentless);
        crate::cognition::set_persona_cognition(Some(Cognition::Contemplating));
        crate::tenacity::set_persona_tenacity(Some(Tenacity::Insistent));

        let actions = drain_preference_actions();
        assert!(actions.is_empty(), "no action marked: {actions:?}");
        let stored = OperatorPreferencePin::default();
        assert_eq!(stored.merged(&actions), stored);
        assert_eq!(
            serde_json::to_string(&stored.merged(&actions)).unwrap(),
            "{}"
        );
    }

    #[test]
    fn each_action_marks_exactly_its_own_axes() {
        let _g = GlobalSettingsGuard::acquire();
        let _ = drain_preference_actions();

        // `/backends <name>`: routes AND clears the model override, exactly as
        // the command clears NEWT_DGX_MODEL. Dials untouched.
        mark_backend_pick("sol");
        let a = drain_preference_actions();
        assert_eq!(a.backend, Some(Some("sol".to_string())));
        assert_eq!(a.model, Some(None), "/backends clears the model override");
        assert_eq!(a.cognition, None);
        assert_eq!(a.tenacity, None);

        // `/model <name>`: the model axis only — the backend the operator
        // happens to be ON (which a persona may have routed) is NOT adopted.
        mark_model_pick("m1");
        let a = drain_preference_actions();
        assert_eq!(a.backend, None, "a model pick must not touch the backend");
        assert_eq!(a.model, Some(Some("m1".to_string())));

        // The dial setters, including their `auto` (clear) forms.
        mark_cognition_choice(CognitionOverride::Off);
        mark_tenacity_choice(Some(Tenacity::Relentless));
        let a = drain_preference_actions();
        assert_eq!(a.cognition, Some(CognitionOverride::Off));
        assert_eq!(a.tenacity, Some(Some(Tenacity::Relentless)));
        assert_eq!((a.backend, a.model), (None, None));

        mark_cognition_choice(CognitionOverride::Unset);
        mark_tenacity_choice(None);
        let a = drain_preference_actions();
        assert_eq!(
            a.cognition,
            Some(CognitionOverride::Unset),
            "`auto` is an action too — it UNPINS the axis"
        );
        assert_eq!(a.tenacity, Some(None));

        // Drain is destructive: the same action is never written twice.
        assert!(drain_preference_actions().is_empty());
    }

    #[test]
    fn merge_updates_only_the_acted_axes() {
        // Review finding 3: an axis nobody acted on keeps its STORED value —
        // so a pin that failed open on apply survives verbatim.
        let stored = OperatorPreferencePin {
            backend: Some("retired-dgx".into()),
            model: Some("nemotron-340b".into()),
            cognition: Some("off".into()),
            tenacity: Some(Tenacity::Relentless),
        };
        let dial_only = PreferenceActions {
            cognition: Some(CognitionOverride::Set(Cognition::Glancing)),
            ..Default::default()
        };
        let merged = stored.merged(&dial_only);
        assert_eq!(merged.backend.as_deref(), Some("retired-dgx"), "kept");
        assert_eq!(merged.model.as_deref(), Some("nemotron-340b"), "kept");
        assert_eq!(merged.tenacity, Some(Tenacity::Relentless), "kept");
        assert_eq!(merged.cognition.as_deref(), Some("glancing"));

        // A cleared axis UNPINS it (back to the invocation baseline), and is
        // distinguishable from "not acted on".
        let cleared = OperatorPreferencePin::default().merged(&PreferenceActions {
            tenacity: Some(None),
            cognition: Some(CognitionOverride::Unset),
            model: Some(None),
            backend: None,
        });
        assert!(
            cleared.is_empty(),
            "clearing every axis unpins: {cleared:?}"
        );

        // A `/backends` pick over a model-pinned row drops the stale model.
        let repointed = stored.merged(&PreferenceActions {
            backend: Some(Some("sol".into())),
            model: Some(None),
            ..Default::default()
        });
        assert_eq!(repointed.backend.as_deref(), Some("sol"));
        assert_eq!(repointed.model, None);

        // Serde round-trip preserves every field of a fully merged pin.
        let json = serde_json::to_string(&merged).unwrap();
        assert_eq!(
            serde_json::from_str::<OperatorPreferencePin>(&json).unwrap(),
            merged
        );
    }

    #[test]
    fn pending_actions_fold_latest_wins_per_axis() {
        // Actions are held PENDING while a conversation has no durable row
        // yet; folding must keep every axis, latest write winning.
        let mut pending = PreferenceActions {
            backend: Some(Some("sol".into())),
            model: Some(None),
            ..Default::default()
        };
        pending.merge(PreferenceActions {
            model: Some(Some("m2".into())),
            tenacity: Some(Some(Tenacity::Relaxed)),
            ..Default::default()
        });
        assert_eq!(pending.backend, Some(Some("sol".to_string())), "kept");
        assert_eq!(pending.model, Some(Some("m2".to_string())), "latest wins");
        assert_eq!(pending.tenacity, Some(Some(Tenacity::Relaxed)));
        assert_eq!(
            OperatorPreferencePin::default().merged(&pending),
            OperatorPreferencePin {
                backend: Some("sol".into()),
                model: Some("m2".into()),
                tenacity: Some(Tenacity::Relaxed),
                cognition: None,
            }
        );
    }

    #[test]
    fn the_empty_column_backfill_parses_to_the_empty_pin() {
        // Pre-#1668 rows carry the additive DEFAULT '{}' — that must decode
        // as "nothing pinned", never an error and never a partial pin.
        let pin: OperatorPreferencePin = serde_json::from_str("{}").unwrap();
        assert!(pin.is_empty());
        let plan = pin.apply_plan(&["sol"], PreferenceAxes::default());
        assert_eq!(plan.backend_axis, BackendAxisAction::Leave);
        assert!(plan.notices.is_empty());
    }

    #[test]
    fn cognition_field_round_trips_every_override_state() {
        assert_eq!(
            OperatorPreferencePin::cognition_field(CognitionOverride::Unset),
            None
        );
        assert_eq!(
            OperatorPreferencePin::cognition_field(CognitionOverride::Off).as_deref(),
            Some("off")
        );
        assert_eq!(
            OperatorPreferencePin::cognition_field(CognitionOverride::Set(Cognition::Glancing))
                .as_deref(),
            Some("glancing")
        );
        // And back through apply_plan: "off" ⇒ Off, a label ⇒ Set(level).
        let off = OperatorPreferencePin {
            cognition: Some("off".into()),
            ..Default::default()
        };
        assert_eq!(
            off.apply_plan(&[], PreferenceAxes::default()).cognition,
            Some(CognitionOverride::Off),
            "'off' must restore the explicit Off override, not Unset"
        );
        let set = OperatorPreferencePin {
            cognition: Some("contemplating".into()),
            ..Default::default()
        };
        assert_eq!(
            set.apply_plan(&[], PreferenceAxes::default()).cognition,
            Some(CognitionOverride::Set(Cognition::Contemplating))
        );
    }

    #[test]
    fn unparseable_pinned_cognition_fails_open_with_a_notice() {
        let pin = OperatorPreferencePin {
            cognition: Some("transcending".into()),
            tenacity: Some(Tenacity::Insistent),
            ..Default::default()
        };
        let plan = pin.apply_plan(&[], PreferenceAxes::default());
        assert_eq!(plan.cognition, None, "bad string must leave the dial alone");
        assert_eq!(
            plan.tenacity,
            Some(Tenacity::Insistent),
            "other dials still apply"
        );
        assert_eq!(plan.notices.len(), 1);
        assert!(
            plan.notices[0].contains("transcending"),
            "{:?}",
            plan.notices
        );
    }

    #[test]
    fn pinned_backend_routes_when_still_configured() {
        let pin = OperatorPreferencePin {
            backend: Some("sol".into()),
            model: Some("gpt-5.6-sol".into()),
            ..Default::default()
        };
        let plan = pin.apply_plan(&["other", "sol"], PreferenceAxes::default());
        assert_eq!(
            plan.backend_axis,
            BackendAxisAction::Route {
                provider: "sol".into(),
                model: RouteModel::Set("gpt-5.6-sol".into()),
            }
        );
        assert!(plan.notices.is_empty());

        // A backend pin WITHOUT a model clears the override (/backends
        // semantics): Route with RouteModel::Clear, not Leave.
        let bare = OperatorPreferencePin {
            backend: Some("sol".into()),
            ..Default::default()
        };
        assert_eq!(
            bare.apply_plan(&["sol"], PreferenceAxes::default())
                .backend_axis,
            BackendAxisAction::Route {
                provider: "sol".into(),
                model: RouteModel::Clear,
            }
        );
    }

    #[test]
    fn an_owned_model_axis_survives_a_pinned_backend_route() {
        // #1668 review-2 finding 2 (HIGH). `NEWT_DGX_MODEL=x newt --resume A`
        // where A pins {backend, model}: the model axis is OWNED by this
        // invocation, so the pin's model is filtered out — but the route must
        // then KEEP the operator's model, not clear it. Clearing was the old
        // behavior and it deleted the very value the operator supplied, while
        // the caller printed a notice claiming the flag had won.
        let pin = OperatorPreferencePin {
            backend: Some("sol".into()),
            model: Some("pinned-model".into()),
            ..Default::default()
        };
        let owned = PreferenceAxes {
            model: true,
            ..Default::default()
        };
        assert_eq!(
            pin.apply_plan(&["sol"], owned).backend_axis,
            BackendAxisAction::Route {
                provider: "sol".into(),
                model: RouteModel::Keep,
            },
            "an owned model axis is KEPT, never cleared, by a pinned route"
        );

        // The same pin with the axis NOT owned still clears to the backend's
        // default only when the pin itself names no model.
        let bare = OperatorPreferencePin {
            backend: Some("sol".into()),
            ..Default::default()
        };
        assert_eq!(
            bare.apply_plan(&["sol"], owned).backend_axis,
            BackendAxisAction::Route {
                provider: "sol".into(),
                model: RouteModel::Keep,
            },
            "owning the axis keeps the operator's model even when the pin names none"
        );
    }

    #[test]
    fn missing_pinned_backend_fails_open_and_drops_its_model() {
        // The #1668 fail-open contract: a pinned backend that is no longer
        // configured must not crash, must not reroute, and must not smear its
        // model (pinned in that backend's context) onto the current backend.
        let pin = OperatorPreferencePin {
            backend: Some("retired-dgx".into()),
            model: Some("nemotron-340b".into()),
            tenacity: Some(Tenacity::Relentless),
            ..Default::default()
        };
        let plan = pin.apply_plan(&["sol", "other"], PreferenceAxes::default());
        assert_eq!(plan.backend_axis, BackendAxisAction::Leave);
        assert_eq!(plan.notices.len(), 1);
        assert!(
            plan.notices[0].contains("retired-dgx"),
            "{:?}",
            plan.notices
        );
        assert!(plan.notices[0].contains("no longer configured"));
        // The dials still apply — fail-open is per-axis, not all-or-nothing.
        assert_eq!(plan.tenacity, Some(Tenacity::Relentless));
    }

    #[test]
    fn model_only_pin_applies_over_the_current_provider() {
        let pin = OperatorPreferencePin {
            model: Some("qwen3-coder_30b".into()),
            ..Default::default()
        };
        assert_eq!(
            pin.apply_plan(&[], PreferenceAxes::default()).backend_axis,
            BackendAxisAction::ModelOnly("qwen3-coder_30b".into())
        );
    }

    #[test]
    fn this_invocations_explicit_flags_beat_the_pin_per_axis() {
        // Review findings 4 + 9: the freshest operator intent must win. An axis
        // this invocation's flags own is dropped from the plan — including its
        // fail-open notice — while every other axis still applies. Nothing is
        // written back either (capture is action-only), so the pin survives for
        // the next run.
        let pin = OperatorPreferencePin {
            backend: Some("sol".into()),
            model: Some("m1".into()),
            cognition: Some("off".into()),
            tenacity: Some(Tenacity::Relentless),
        };
        let plan = pin.apply_plan(
            &["sol"],
            PreferenceAxes {
                cognition: true,
                backend: true,
                ..Default::default()
            },
        );
        assert_eq!(plan.cognition, None, "--cognition wins for the invocation");
        assert_eq!(
            plan.backend_axis,
            BackendAxisAction::ModelOnly("m1".into()),
            "the flag owns the provider; the unowned model axis still applies"
        );
        assert_eq!(plan.tenacity, Some(Tenacity::Relentless), "unowned applies");
        assert!(plan.notices.is_empty());

        // A pin whose backend is gone but whose axis the flags own must not
        // nag about a pin the flag already superseded this run.
        let stale = OperatorPreferencePin {
            backend: Some("retired-dgx".into()),
            ..Default::default()
        };
        let plan = stale.apply_plan(
            &["sol"],
            PreferenceAxes {
                backend: true,
                ..Default::default()
            },
        );
        assert_eq!(plan.backend_axis, BackendAxisAction::Leave);
        assert!(plan.notices.is_empty(), "{:?}", plan.notices);
    }

    #[test]
    fn recorded_cli_axes_union_per_axis() {
        let _g = GlobalSettingsGuard::acquire();
        record_cli_preference_axes(PreferenceAxes {
            cognition: true,
            ..Default::default()
        });
        record_cli_preference_axes(PreferenceAxes {
            backend: true,
            ..Default::default()
        });
        let axes = cli_preference_axes();
        assert!(axes.cognition && axes.backend);
        assert!(!axes.model && !axes.tenacity, "recording is per-axis");
        assert_eq!(axes.labels(), vec!["backend", "cognition"]);
        assert!(!axes.is_empty());
        assert!(PreferenceAxes::default().is_empty());
    }

    // ------------------------------------------------------------------
    // #1668 invariant: a pin carries BEHAVIOR, never AUTHORITY.
    // ------------------------------------------------------------------

    #[test]
    fn pin_serializes_exactly_the_four_preference_axes() {
        // The extension guard. A pin is safe to restore FAIL-OPEN only because
        // every field is convenience state; authority must fail CLOSED and
        // belongs in the capability / OCAP layer. Adding a field here breaks
        // this test on purpose — if the new field is authority-shaped, it does
        // not belong in a pin at all.
        let full = OperatorPreferencePin {
            backend: Some("sol".into()),
            model: Some("m1".into()),
            cognition: Some("off".into()),
            tenacity: Some(Tenacity::Relentless),
        };
        let value: serde_json::Value = serde_json::to_value(&full).unwrap();
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("a pin serializes as an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["backend", "cognition", "model", "tenacity"],
            "a pin may carry ONLY these four preference axes; it is operator preference, never authority — audit any new field against the type doc first"
        );
    }

    #[test]
    fn an_authority_shaped_pin_field_is_refused_not_absorbed() {
        // A hostile / corrupt `posture` column that smuggles authority-shaped
        // keys must not decode into anything the session then honors. Strict
        // decode (`deny_unknown_fields`) refuses the row outright — the store
        // surfaces the error, the resume path degrades it to a one-line notice
        // and runs on the invocation baseline — so no grant, sandbox setting,
        // or enforcement floor can ride a pin into a session.
        for hostile in [
            r#"{"backend":"sol","ocap":["fs:/"]}"#,
            r#"{"grants":{"net":true}}"#,
            r#"{"sandbox":"off","cognition":"off"}"#,
            r#"{"enforcement_floor":0}"#,
        ] {
            let decoded = serde_json::from_str::<OperatorPreferencePin>(hostile);
            assert!(
                decoded.is_err(),
                "an authority-shaped key must be refused, got {decoded:?} from {hostile}"
            );
        }
        // Wrong TYPES on the real axes are refused the same way — a pin never
        // half-decodes into a partially-trusted posture.
        for malformed in [
            r#"{"backend":42}"#,
            r#"{"tenacity":"nonsense"}"#,
            "not json",
        ] {
            assert!(
                serde_json::from_str::<OperatorPreferencePin>(malformed).is_err(),
                "a malformed pin must be refused: {malformed}"
            );
        }
    }

    #[test]
    fn a_pin_plan_never_yields_anything_but_the_preference_axes() {
        // The apply side of the same invariant: whatever a row contains, the
        // PLAN a caller can act on is only ever the two dials plus the
        // backend/model axis — there is no channel here to widen authority.
        let pin = OperatorPreferencePin {
            backend: Some("sol".into()),
            model: Some("m1".into()),
            cognition: Some("off".into()),
            tenacity: Some(Tenacity::Relentless),
        };
        // Exhaustive destructure: a new PreferenceApplyPlan field fails to compile
        // here, forcing the next author past the invariant above.
        let PreferenceApplyPlan {
            cognition,
            tenacity,
            backend_axis,
            notices,
        } = pin.apply_plan(&["sol"], PreferenceAxes::default());
        assert_eq!(cognition, Some(CognitionOverride::Off));
        assert_eq!(tenacity, Some(Tenacity::Relentless));
        assert!(matches!(backend_axis, BackendAxisAction::Route { .. }));
        assert!(notices.is_empty());
    }
}
