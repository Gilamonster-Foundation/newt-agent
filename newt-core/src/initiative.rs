//! Initiative — how much the agent looks before acting.
//!
//! The measured failure this answers (2026-07-28, steering-regressions): even a
//! capable coding model, given valid context and a full time budget, will read /
//! search / plan indefinitely and never emit an edit. A clean 25-minute drive on
//! `qwen3-coder_30b` produced 41 read/inspect/plan tool calls and **zero**
//! mutations. The bottleneck is action *initiation*, and initiative is the dial
//! for it.
//!
//! One operator-facing level maps to two harness behaviours: how many
//! consecutive read-only rounds the loop tolerates before it nudges the model
//! to act (the numbers are config, `[initiative.rounds]`), and whether
//! `exit_plan_mode` must hand off to an edit. The default comes from the model
//! family (`[initiative.families]`): a family that over-explores starts at
//! `decisive` or `eager`.
//!
//! This was tenacity's mechanism until the split in
//! `docs/design/psyche-effort-dials.md` (slice 1b), which moved it here
//! unchanged apart from `patient`'s 12 rounds (the old bottom level nudged
//! after 6). [`crate::tenacity`] now covers pursuit only. This module is the
//! one owner of the active model family, since initiative is its only reader.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;
use std::sync::Mutex;

/// How much the harness lets the model look around before it pushes it to act.
///
/// Ordered from most patient to most forcing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Initiative {
    /// A long look-around before the nudge (12 rounds by default); plan exit
    /// is advisory. For models that explore with purpose, and tasks that read
    /// a large file in pieces.
    Patient,
    /// The historical default (3 rounds); plan exit is advisory.
    #[default]
    Measured,
    /// Nudge sooner (2 rounds) and make `exit_plan_mode` hand off to an edit.
    Decisive,
    /// Nudge after a single read-only round and require an edit on plan exit.
    /// For small models that otherwise never act.
    Eager,
}

impl Initiative {
    /// Consecutive read-only rounds tolerated before the action nudge, from
    /// the turn's captured `[initiative.rounds]`, or the installed config
    /// outside a turn (built-in defaults 12/3/2/1).
    #[must_use]
    pub fn read_only_nudge_after(self) -> usize {
        installed_rounds().get(self)
    }

    /// Whether leaving plan mode (`exit_plan_mode`) must hand off to a concrete
    /// edit rather than let the model slide back into reading.
    #[must_use]
    pub fn exit_plan_requires_edit(self) -> bool {
        matches!(self, Self::Decisive | Self::Eager)
    }

    /// Stable lowercase label: the wire, config and `/psyche initiative` spelling.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Patient => "patient",
            Self::Measured => "measured",
            Self::Decisive => "decisive",
            Self::Eager => "eager",
        }
    }

    /// One-line description of what this level does, with the round count
    /// the installed config gives it.
    #[must_use]
    pub fn describe(self) -> String {
        let edit = if self.exit_plan_requires_edit() {
            "exit_plan_mode requires an edit"
        } else {
            "exit_plan_mode is advisory"
        };
        format!(
            "force an edit after {} read-only round(s); {edit}",
            self.read_only_nudge_after()
        )
    }

    /// All levels, patient → eager.
    #[must_use]
    pub fn all() -> [Self; 4] {
        [Self::Patient, Self::Measured, Self::Decisive, Self::Eager]
    }
}

impl fmt::Display for Initiative {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

impl FromStr for Initiative {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim().to_ascii_lowercase();
        if s == "default" {
            return Ok(Self::default());
        }
        if let Some(level) = Self::all().into_iter().find(|l| l.label() == s) {
            return Ok(level);
        }
        if let Some(new) = crate::psyche_import::split_tenacity(&s) {
            return Err(format!(
                "'{s}' is a tenacity level from before the split; its initiative level is '{new}'"
            ));
        }
        let levels: Vec<&str> = Self::all().into_iter().map(Self::label).collect();
        Err(format!("unknown initiative '{s}' ({})", levels.join("|")))
    }
}

/// `[initiative.rounds]`: read-only rounds before the nudge, per level. A
/// level is a name for a number; retuning `decisive` here retunes every
/// family that selects it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct InitiativeRounds {
    pub patient: usize,
    pub measured: usize,
    pub decisive: usize,
    pub eager: usize,
}

impl Default for InitiativeRounds {
    fn default() -> Self {
        Self {
            patient: 12,
            measured: 3,
            decisive: 2,
            eager: 1,
        }
    }
}

impl InitiativeRounds {
    /// The round count for `level`.
    #[must_use]
    pub fn get(&self, level: Initiative) -> usize {
        match level {
            Initiative::Patient => self.patient,
            Initiative::Measured => self.measured,
            Initiative::Decisive => self.decisive,
            Initiative::Eager => self.eager,
        }
    }
}

/// The `[initiative]` config section: a baseline level, per-model-family
/// defaults, and the round count behind each level. Pure data: a new family's
/// default is one map entry.
///
/// ```toml
/// [initiative]
/// default = "measured"
/// [initiative.families]
/// nemotron = "eager"   # small/over-exploring family → act sooner
/// [initiative.rounds]
/// patient = 12
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct InitiativeConfig {
    /// Baseline level when no per-family default matches. `None` ⇒ `Measured`.
    pub default: Option<Initiative>,
    /// Per-model-family defaults, keyed by the card's `family` label and
    /// matched case-insensitively. Supersedes [`default`](Self::default).
    pub families: BTreeMap<String, Initiative>,
    /// The number behind each level.
    pub rounds: InitiativeRounds,
}

impl InitiativeConfig {
    /// The configured level for a model `family` (case-insensitive): a
    /// per-family default if one matches, else [`default`](Self::default),
    /// else `Measured`. `family == None` skips straight to the default.
    #[must_use]
    pub fn resolve(&self, family: Option<&str>) -> Initiative {
        self.family_default(family)
            .or(self.default)
            .unwrap_or_default()
    }

    /// The per-family default for `family`, if the table names it. Equality
    /// on the family LABEL (case-insensitive), never containment.
    #[must_use]
    pub fn family_default(&self, family: Option<&str>) -> Option<Initiative> {
        let family = family?.trim();
        self.families
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(family))
            .map(|(_, v)| *v)
    }
}

/// Full resolution order, most specific first: an explicit operator choice
/// (`--initiative`, `/psyche initiative`, a pin) wins over the active persona's
/// declaration, which wins over the config's per-family default, then the
/// config default; `Measured` is the floor.
#[must_use]
pub fn resolve_initiative(
    cli: Option<Initiative>,
    persona: Option<Initiative>,
    config: Option<&InitiativeConfig>,
    family: Option<&str>,
) -> Initiative {
    cli.or(persona)
        .unwrap_or_else(|| config.map(|c| c.resolve(family)).unwrap_or_default())
}

// Each input is stashed by the one site that knows it and combined lazily by
// `effective_initiative`, so the setters are independent and order-free.
static CLI_INITIATIVE: Mutex<Option<Initiative>> = Mutex::new(None);
static PERSONA_INITIATIVE: Mutex<Option<Initiative>> = Mutex::new(None);
static INITIATIVE_CONFIG: Mutex<Option<InitiativeConfig>> = Mutex::new(None);
static ACTIVE_FAMILY: Mutex<Option<String>> = Mutex::new(None);

std::thread_local! {
    /// A driven turn captures the level AND its configured numeric budgets;
    /// config publication cannot retune the nudge during the turn.
    static EFFECTIVE_INITIATIVE_OVERRIDE: std::cell::Cell<Option<(Initiative, InitiativeRounds)>> =
        const { std::cell::Cell::new(None) };
}

/// Restores the prior current-thread override on drop. The `Rc` marker keeps
/// the guard on the thread whose slot it owns.
#[must_use]
pub struct ScopedEffectiveInitiative {
    previous: Option<(Initiative, InitiativeRounds)>,
    _thread_bound: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl Drop for ScopedEffectiveInitiative {
    fn drop(&mut self) {
        let _ = EFFECTIVE_INITIATIVE_OVERRIDE.try_with(|slot| slot.set(self.previous));
    }
}

/// Pin [`effective_initiative`] and its numeric round budgets on this thread
/// until the guard drops. Nests in
/// LIFO order. Prefer [`crate::psyche::capture_turn_psyche`] at a turn
/// boundary, so no dial is pinned without the others.
pub fn scoped_effective_initiative(level: Initiative) -> ScopedEffectiveInitiative {
    // Inherit an outer capture's numbers when scopes nest.
    let rounds = installed_rounds();
    let previous = EFFECTIVE_INITIATIVE_OVERRIDE.with(|slot| slot.replace(Some((level, rounds))));
    ScopedEffectiveInitiative {
        previous,
        _thread_bound: std::marker::PhantomData,
    }
}

/// Install the explicit operator choice (highest priority).
pub fn set_cli_initiative(level: Initiative) {
    if let Ok(mut slot) = CLI_INITIATIVE.lock() {
        *slot = Some(level);
    }
}

/// Clear the explicit operator choice, so initiative resolves from the
/// persona / config / family again (`auto`).
pub fn clear_cli_initiative() {
    if let Ok(mut slot) = CLI_INITIATIVE.lock() {
        *slot = None;
    }
}

/// The explicit operator choice, if one is installed.
#[must_use]
pub fn cli_initiative() -> Option<Initiative> {
    CLI_INITIATIVE.lock().ok().and_then(|s| *s)
}

/// Install the active persona's declared `initiative` (`None` clears it).
/// Call at every persona activation, clear, and restore.
pub fn set_persona_initiative(level: Option<Initiative>) {
    if let Ok(mut slot) = PERSONA_INITIATIVE.lock() {
        *slot = level;
    }
}

/// The active persona's declared initiative, if any.
#[must_use]
pub fn persona_initiative() -> Option<Initiative> {
    PERSONA_INITIATIVE.lock().ok().and_then(|s| *s)
}

/// Install the resolved `[initiative]` config. Called by
/// `Config::publish_runtime_settings`.
pub fn set_initiative_config(config: InitiativeConfig) {
    if let Ok(mut slot) = INITIATIVE_CONFIG.lock() {
        *slot = Some(config);
    }
}

/// The installed `[initiative]` config, if any.
#[must_use]
pub fn initiative_config() -> Option<InitiativeConfig> {
    INITIATIVE_CONFIG.lock().ok().and_then(|s| s.clone())
}

fn installed_rounds() -> InitiativeRounds {
    if let Some((_, rounds)) = EFFECTIVE_INITIATIVE_OVERRIDE.with(std::cell::Cell::get) {
        return rounds;
    }
    INITIATIVE_CONFIG
        .lock()
        .ok()
        .and_then(|s| s.as_ref().map(|c| c.rounds))
        .unwrap_or_default()
}

/// Install the active model's family so per-family defaults apply; `None`
/// clears it. This is the ONLY attribution path (#1820): callers derive the
/// family from typed resolved-card metadata
/// (`ResolvedCapabilities::family_for_route`) at every point a session settles
/// on a model, chat AND solve (#1139). Nothing infers a family from the model
/// NAME: a cardless model whose name merely contains a configured
/// `[initiative.families]` key gets NO family and falls to the configured
/// default (names are labels, never evidence, #1818/#1819). Opt back in with a
/// drop-in model card that names the family.
pub fn set_active_model_family(family: Option<String>) {
    if let Ok(mut slot) = ACTIVE_FAMILY.lock() {
        *slot = family;
    }
}

/// The active model family, if any.
#[must_use]
pub fn active_model_family() -> Option<String> {
    ACTIVE_FAMILY.lock().ok().and_then(|s| s.clone())
}

/// The initiative in effect: a turn's captured value, else the operator's
/// choice, else the persona's, else the config per-family default for the
/// active family, else the config default, else `Measured`.
#[must_use]
pub fn effective_initiative() -> Initiative {
    if let Some((level, _)) = EFFECTIVE_INITIATIVE_OVERRIDE.with(std::cell::Cell::get) {
        return level;
    }
    resolve_initiative(
        cli_initiative(),
        persona_initiative(),
        initiative_config().as_ref(),
        active_model_family().as_deref(),
    )
}

/// Initiative with **no operator choice and no persona layer**: the model
/// default the panel shows as `(model default)` and a persona that declares
/// no `initiative` inherits.
#[must_use]
pub fn base_initiative() -> Initiative {
    resolve_initiative(
        None,
        None,
        initiative_config().as_ref(),
        active_model_family().as_deref(),
    )
}

/// The active model family's entry in `[initiative.families]`, if any: the
/// value the panel labels `(model default)`.
#[must_use]
pub fn model_default_initiative() -> Option<Initiative> {
    initiative_config()?.family_default(active_model_family().as_deref())
}

/// Every mutable global that feeds [`effective_initiative`], snapshotted as
/// one unit so the test guard can restore them all.
#[doc(hidden)]
pub struct InitiativeRuntimeSnapshot {
    cli: Option<Initiative>,
    persona: Option<Initiative>,
    config: Option<InitiativeConfig>,
    active_family: Option<String>,
}

/// Snapshot every initiative-resolution global.
#[doc(hidden)]
#[must_use]
pub fn snapshot_runtime_state() -> InitiativeRuntimeSnapshot {
    InitiativeRuntimeSnapshot {
        cli: cli_initiative(),
        persona: persona_initiative(),
        config: initiative_config(),
        active_family: active_model_family(),
    }
}

/// Restore every initiative-resolution global from a snapshot.
#[doc(hidden)]
pub fn restore_runtime_state(snapshot: InitiativeRuntimeSnapshot) {
    if let Ok(mut s) = CLI_INITIATIVE.lock() {
        *s = snapshot.cli;
    }
    if let Ok(mut s) = PERSONA_INITIATIVE.lock() {
        *s = snapshot.persona;
    }
    if let Ok(mut s) = INITIATIVE_CONFIG.lock() {
        *s = snapshot.config;
    }
    if let Ok(mut s) = ACTIVE_FAMILY.lock() {
        *s = snapshot.active_family;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_guard::GlobalSettingsGuard;

    fn cfg(default: Option<Initiative>, fams: &[(&str, Initiative)]) -> InitiativeConfig {
        InitiativeConfig {
            default,
            families: fams.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            ..InitiativeConfig::default()
        }
    }

    #[test]
    fn measured_is_the_default_and_keeps_the_historical_three_rounds() {
        let _g = GlobalSettingsGuard::acquire();
        set_initiative_config(InitiativeConfig::default());
        assert_eq!(Initiative::default(), Initiative::Measured);
        assert_eq!(Initiative::Measured.read_only_nudge_after(), 3);
        assert!(!Initiative::Measured.exit_plan_requires_edit());
    }

    /// The built-in rounds: 12/3/2/1. Patient's 12 is the one intended change
    /// from the old tenacity ladder (relaxed nudged after 6).
    #[test]
    fn built_in_rounds_run_patient_to_eager() {
        let _g = GlobalSettingsGuard::acquire();
        set_initiative_config(InitiativeConfig::default());
        let budgets: Vec<usize> = Initiative::all()
            .iter()
            .map(|l| l.read_only_nudge_after())
            .collect();
        assert_eq!(budgets, vec![12, 3, 2, 1]);
    }

    /// Regression (slice 1b): the rounds behind each level are config. Before
    /// the split they were constants in `Tenacity::read_only_nudge_after`, so
    /// no config could change them.
    #[test]
    fn rounds_come_from_the_installed_config() {
        let _g = GlobalSettingsGuard::acquire();
        let config: InitiativeConfig = toml::from_str(
            r#"
            [rounds]
            patient = 20
            decisive = 5
        "#,
        )
        .unwrap();
        set_initiative_config(config);
        assert_eq!(Initiative::Patient.read_only_nudge_after(), 20);
        assert_eq!(Initiative::Decisive.read_only_nudge_after(), 5);
        assert_eq!(
            Initiative::Measured.read_only_nudge_after(),
            3,
            "an unset level keeps its built-in number"
        );
        assert!(
            Initiative::Patient
                .describe()
                .contains("after 20 read-only"),
            "describe reads the installed config: {}",
            Initiative::Patient.describe()
        );
    }

    /// Regression (slice 1b): the exit-plan edit belongs to initiative.
    #[test]
    fn only_decisive_and_eager_require_an_edit_on_plan_exit() {
        assert!(!Initiative::Patient.exit_plan_requires_edit());
        assert!(!Initiative::Measured.exit_plan_requires_edit());
        assert!(Initiative::Decisive.exit_plan_requires_edit());
        assert!(Initiative::Eager.exit_plan_requires_edit());
    }

    #[test]
    fn parse_round_trips_labels_and_names_the_split_for_old_tenacity_labels() {
        for l in Initiative::all() {
            assert_eq!(l.label().parse::<Initiative>().unwrap(), l);
            assert_eq!(l.to_string(), l.label());
        }
        assert_eq!(" EAGER ".parse::<Initiative>().unwrap(), Initiative::Eager);
        assert_eq!(
            "default".parse::<Initiative>().unwrap(),
            Initiative::Measured
        );
        let err = "insistent".parse::<Initiative>().unwrap_err();
        assert!(err.contains("'decisive'"), "{err}");
        assert!("banana".parse::<Initiative>().is_err());
        assert_eq!(
            serde_json::to_string(&Initiative::Eager).unwrap(),
            "\"eager\""
        );
    }

    #[test]
    fn config_resolve_prefers_family_then_default_then_measured() {
        let c = cfg(
            Some(Initiative::Patient),
            &[("nemotron", Initiative::Eager)],
        );
        assert_eq!(c.resolve(Some("nemotron")), Initiative::Eager);
        assert_eq!(c.resolve(Some("  NEMOTRON ")), Initiative::Eager);
        assert_eq!(c.resolve(Some("qwen3")), Initiative::Patient);
        assert_eq!(c.resolve(None), Initiative::Patient);
        assert_eq!(
            InitiativeConfig::default().resolve(Some("qwen3")),
            Initiative::Measured
        );
    }

    #[test]
    fn resolve_initiative_precedence_cli_over_persona_over_config() {
        let c = cfg(
            Some(Initiative::Patient),
            &[("nemotron", Initiative::Eager)],
        );
        let r = |cli, persona, family| resolve_initiative(cli, persona, Some(&c), family);
        assert_eq!(
            r(
                Some(Initiative::Measured),
                Some(Initiative::Decisive),
                Some("nemotron")
            ),
            Initiative::Measured
        );
        assert_eq!(
            r(None, Some(Initiative::Decisive), Some("nemotron")),
            Initiative::Decisive
        );
        assert_eq!(r(None, None, Some("nemotron")), Initiative::Eager);
        assert_eq!(r(None, None, Some("kimi")), Initiative::Patient);
        assert_eq!(
            resolve_initiative(None, None, None, Some("nemotron")),
            Initiative::Measured
        );
    }

    /// Family reaches initiative only through the typed seam, and the label
    /// match is equality, never containment. The red/green regression for
    /// the removed name channel is `tests/tenacity_exact_family_ratchet.rs`.
    #[test]
    fn family_arrives_only_through_the_typed_seam() {
        let _g = GlobalSettingsGuard::acquire();
        clear_cli_initiative();
        set_persona_initiative(None);
        set_initiative_config(cfg(None, &[("nemotron", Initiative::Eager)]));
        set_active_model_family(Some("nemotron".to_string()));
        assert_eq!(effective_initiative(), Initiative::Eager);
        set_active_model_family(Some("nemotron-super".to_string()));
        assert_eq!(effective_initiative(), Initiative::Measured);
        set_active_model_family(None);
        assert_eq!(effective_initiative(), Initiative::Measured);
    }

    #[test]
    fn base_initiative_ignores_the_operator_and_persona_layers() {
        let _g = GlobalSettingsGuard::acquire();
        set_initiative_config(cfg(
            Some(Initiative::Patient),
            &[("nemotron", Initiative::Eager)],
        ));
        set_active_model_family(Some("nemotron".to_string()));
        set_cli_initiative(Initiative::Measured);
        set_persona_initiative(Some(Initiative::Decisive));
        assert_eq!(base_initiative(), Initiative::Eager);
        assert_eq!(effective_initiative(), Initiative::Measured);
        set_active_model_family(None);
        assert_eq!(base_initiative(), Initiative::Patient);
    }

    #[test]
    fn scoped_override_is_thread_local_and_nests() {
        let _g = GlobalSettingsGuard::acquire();
        set_cli_initiative(Initiative::Patient);
        let outer = scoped_effective_initiative(Initiative::Eager);
        set_cli_initiative(Initiative::Decisive);
        assert_eq!(effective_initiative(), Initiative::Eager);
        {
            let _inner = scoped_effective_initiative(Initiative::Measured);
            assert_eq!(effective_initiative(), Initiative::Measured);
        }
        assert_eq!(effective_initiative(), Initiative::Eager);
        let elsewhere = std::thread::spawn(effective_initiative).join().unwrap();
        assert_eq!(
            elsewhere,
            Initiative::Decisive,
            "the pin stays on its thread"
        );
        drop(outer);
        assert_eq!(effective_initiative(), Initiative::Decisive);
    }

    #[test]
    fn snapshot_restore_round_trips_every_initiative_global() {
        let _g = GlobalSettingsGuard::acquire();
        clear_cli_initiative();
        set_persona_initiative(None);
        set_initiative_config(InitiativeConfig::default());
        set_active_model_family(None);
        let snap = snapshot_runtime_state();

        set_cli_initiative(Initiative::Eager);
        set_persona_initiative(Some(Initiative::Decisive));
        set_initiative_config(cfg(None, &[("nemotron", Initiative::Eager)]));
        set_active_model_family(Some("nemotron".to_string()));

        restore_runtime_state(snap);
        assert_eq!(cli_initiative(), None);
        assert_eq!(persona_initiative(), None);
        assert_eq!(initiative_config(), Some(InitiativeConfig::default()));
        assert_eq!(active_model_family(), None);
    }
}

#[cfg(test)]
#[path = "initiative_regression_tests.rs"]
mod regression_tests;
