//! Tenacity — how hard the harness pushes the model from exploration to action.
//!
//! The measured failure this answers (2026-07-28, steering-regressions): even a
//! capable coding model, given valid context and a full time budget, will read /
//! search / plan indefinitely and never emit an edit. A clean 25-minute drive on
//! `qwen3-coder_30b` produced 41 read/inspect/plan tool calls and **zero**
//! mutations. Context fixes (objective spine, working-set pin) were necessary but
//! not sufficient: the model *had* the context and still would not act. The
//! bottleneck is action *initiation*, and tenacity is the dial for it — our
//! answer to little-coder's tight action loop.
//!
//! Tenacity is a single operator-facing LEVEL that maps to concrete
//! action-forcing knobs, so a per-model-family default can pick one level rather
//! than tune raw numbers. Higher tenacity forces the edit sooner and makes plan
//! mode hand off to a mandatory action. An explicitly selected
//! [`Tenacity::Relentless`] posture also raises the default tool-round budget;
//! automatic family defaults do not. [`Tenacity::Standard`] reproduces the
//! historical hardcoded behaviour (`READ_ONLY_NUDGE_AFTER = 3`), so it is a
//! behaviour-preserving default.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Bounded sentinel used for an operator-selected relentless run. This is high
/// enough to behave as "finish the objective" while remaining finite.
pub const RELENTLESS_TOOL_ROUND_TARGET: usize = 10_000;

/// How insistently the harness drives the model from reading to acting.
///
/// Ordered from most patient to most forcing. Small / over-exploring model
/// families default to a higher tenacity; frontier models that explore with
/// purpose can sit at [`Tenacity::Standard`] or [`Tenacity::Relaxed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Tenacity {
    /// Most patient: tolerate a long look-around before a forcing nudge, and
    /// leave plan-mode exit advisory. For models whose exploration is
    /// purposeful and who edit on their own.
    Relaxed,
    /// The historical default: nudge toward action after 3 read-only rounds;
    /// plan-mode exit is advisory. Behaviour-preserving.
    #[default]
    Standard,
    /// Force sooner (2 read-only rounds) and make `exit_plan_mode` hand off to a
    /// mandatory edit. For families that tend to over-explore.
    Insistent,
    /// Most forcing: nudge after a single read-only round and require an edit on
    /// plan-mode exit. For small models that otherwise never act.
    Relentless,
}

impl Tenacity {
    /// Number of consecutive read-only rounds tolerated before the harness
    /// injects an action-forcing nudge. Lower = more tenacious. `Standard` = 3,
    /// the historical `READ_ONLY_NUDGE_AFTER`.
    pub fn read_only_nudge_after(self) -> usize {
        match self {
            Self::Relaxed => 6,
            Self::Standard => 3,
            Self::Insistent => 2,
            Self::Relentless => 1,
        }
    }

    /// Whether leaving plan mode (`exit_plan_mode`) must hand off to a concrete
    /// edit — the harness steers the very next turn toward a mutation rather than
    /// letting the model slide back into more reading.
    pub fn exit_plan_requires_edit(self) -> bool {
        matches!(self, Self::Insistent | Self::Relentless)
    }

    /// Project the default tool-round budget when this level was selected by
    /// the operator. Relentless is the only level that changes the cap: it uses
    /// the shared effectively-unlimited target without lowering a larger
    /// configured value. Callers retain explicit round overrides as the final
    /// precedence layer, and should not apply this to automatic family defaults
    /// (small loop-prone models often resolve to relentless there).
    pub fn project_tool_round_limit(self, configured: usize) -> usize {
        if self == Self::Relentless {
            configured.max(RELENTLESS_TOOL_ROUND_TARGET)
        } else {
            configured
        }
    }

    /// Stable lowercase label — the wire/config/`/tenacity` spelling.
    pub fn label(self) -> &'static str {
        match self {
            Self::Relaxed => "relaxed",
            Self::Standard => "standard",
            Self::Insistent => "insistent",
            Self::Relentless => "relentless",
        }
    }

    /// One-line description of what this level does, for `/tenacity` and the
    /// footer indicator.
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

    /// All levels, patient → forcing (for `/tenacity` listings and menus).
    pub fn all() -> [Self; 4] {
        [
            Self::Relaxed,
            Self::Standard,
            Self::Insistent,
            Self::Relentless,
        ]
    }
}

/// Compose the tool-round safety valve without losing provenance. Automatic
/// persona/config/family tenacity is intentionally not accepted here: callers
/// pass only a direct operator choice, because small loop-prone families often
/// default to relentless and must not silently receive 10,000 rounds. An
/// explicit `/rounds`/`--max-rounds` value remains the outermost override.
#[must_use]
pub fn resolve_tool_round_limit(
    configured: usize,
    explicit_tenacity: Option<Tenacity>,
    explicit_rounds: Option<usize>,
) -> usize {
    explicit_rounds.unwrap_or_else(|| {
        explicit_tenacity
            .map(|level| level.project_tool_round_limit(configured))
            .unwrap_or(configured)
    })
}

impl fmt::Display for Tenacity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

impl FromStr for Tenacity {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "relaxed" => Ok(Self::Relaxed),
            "standard" | "default" => Ok(Self::Standard),
            "insistent" => Ok(Self::Insistent),
            "relentless" => Ok(Self::Relentless),
            other => Err(format!(
                "unknown tenacity '{other}' (relaxed|standard|insistent|relentless)"
            )),
        }
    }
}

/// The `[tenacity]` config section: a baseline level plus per-model-family
/// overrides. Pure data (the three-Cs "knowledge in data" rule) — a new family's
/// default is one map entry, not a new branch, mirroring the model-card
/// [`crate::model_card::family_defaults`] pattern.
///
/// ```toml
/// [tenacity]
/// default = "standard"
/// [tenacity.families]
/// nemotron = "relentless"   # small/over-exploring family → force sooner
/// qwen3    = "standard"
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TenacityConfig {
    /// Baseline level when no per-family override matches. `None` ⇒ [`Tenacity`]'s
    /// `Default` (`Standard`), so an empty `[tenacity]` changes nothing.
    pub default: Option<Tenacity>,
    /// Per-model-family overrides, keyed by the card's `family` label (e.g.
    /// `"qwen3"`, `"nemotron"`). Matched case-insensitively. Supersedes
    /// [`default`](Self::default); an explicit CLI `--tenacity` supersedes this.
    pub families: std::collections::BTreeMap<String, Tenacity>,
}

impl TenacityConfig {
    /// The configured level for a model `family` (case-insensitive): a per-family
    /// override if one matches, else [`default`](Self::default), else `Standard`.
    /// `family == None` (unknown/unresolved) skips straight to the default.
    pub fn resolve(&self, family: Option<&str>) -> Tenacity {
        family
            .and_then(|f| {
                self.families
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(f.trim()))
                    .map(|(_, v)| *v)
            })
            .or(self.default)
            .unwrap_or_default()
    }

    /// Attribute a family to a model for per-family resolution, without requiring
    /// a model card per model. The card's `family` wins when it names a configured
    /// family; otherwise infer it by matching a configured family KEY as a
    /// case-insensitive substring of the model NAME — so `"qwen3-coder_30b"` picks
    /// up a `[tenacity.families] qwen3` default, and the whole matrix (gemma,
    /// nemotron, deepseek, kimi, glm…) works from config alone. The match set is
    /// the operator's own `families` keys (data, not a hardcoded list). `None`
    /// when nothing matches ⇒ [`resolve`](Self::resolve) uses the default.
    pub fn family_for(&self, model: &str, card_family: Option<&str>) -> Option<String> {
        if let Some(fam) = card_family {
            let fam = fam.trim();
            if self.families.keys().any(|k| k.eq_ignore_ascii_case(fam)) {
                return Some(fam.to_string());
            }
        }
        let lname = model.to_ascii_lowercase();
        self.families
            .keys()
            .find(|k| lname.contains(&k.to_ascii_lowercase()))
            .cloned()
    }
}

/// Full resolution order, most-specific first: an explicit operator choice
/// (`--tenacity`) wins over the active `persona` declaration, which wins over
/// config per-family, which wins over the config default; `Standard` is the
/// floor. `config == None` ⇒ CLI-or-persona-or-`Standard`.
pub fn resolve_tenacity(
    cli: Option<Tenacity>,
    persona: Option<Tenacity>,
    config: Option<&TenacityConfig>,
    family: Option<&str>,
) -> Tenacity {
    cli.or(persona)
        .unwrap_or_else(|| config.map(|c| c.resolve(family)).unwrap_or_default())
}

// The three tenacity inputs, each stashed by the one site that knows it — the
// operator dial can't be threaded through every loop construction site. They are
// combined lazily by [`effective_tenacity`] via [`resolve_tenacity`], so each
// setter is independent and order-free:
//   - CLI `--tenacity` flag (highest), set in the CLI dispatch,
//   - the `[tenacity]` config, stashed by `Config::apply_runtime_settings`,
//   - the active model's family, set at model selection.
// All absent ⇒ [`Tenacity`]'s `Default` (`Standard`) — behaviour-preserving.
static CLI_TENACITY: std::sync::Mutex<Option<Tenacity>> = std::sync::Mutex::new(None);
static TENACITY_CONFIG: std::sync::Mutex<Option<TenacityConfig>> = std::sync::Mutex::new(None);
static ACTIVE_FAMILY: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
// The active persona's declared `tenacity:` — a resolution layer BELOW the CLI
// override and ABOVE the config/family default, set when a persona activates
// (review P1#3: a declared persona tenacity is now actually applied, not just
// rendered). `None` when no persona / the persona declares none.
static PERSONA_TENACITY: std::sync::Mutex<Option<Tenacity>> = std::sync::Mutex::new(None);

std::thread_local! {
    /// A driven turn resolves tenacity before crossing onto its dedicated
    /// thread. Keeping that value here makes every downstream resolver read in
    /// the turn (workflow steering and tool dispatch included) observe the same
    /// immutable posture without replacing the interactive process globals.
    static EFFECTIVE_TENACITY_OVERRIDE: std::cell::Cell<Option<Tenacity>> =
        const { std::cell::Cell::new(None) };
}

/// Restores the prior current-thread override on drop. The `Rc` marker keeps
/// the guard on the thread whose TLS slot it owns; driven turns use a
/// current-thread runtime, so the guard safely spans the whole async turn.
pub(crate) struct ScopedEffectiveTenacity {
    previous: Option<Tenacity>,
    _thread_bound: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl Drop for ScopedEffectiveTenacity {
    fn drop(&mut self) {
        let _ = EFFECTIVE_TENACITY_OVERRIDE.try_with(|slot| slot.set(self.previous));
    }
}

/// Override [`effective_tenacity`] on the current thread until the returned
/// guard drops. Overrides nest in lexical (LIFO) order.
pub(crate) fn scoped_effective_tenacity(level: Tenacity) -> ScopedEffectiveTenacity {
    let previous = EFFECTIVE_TENACITY_OVERRIDE.with(|slot| slot.replace(Some(level)));
    ScopedEffectiveTenacity {
        previous,
        _thread_bound: std::marker::PhantomData,
    }
}

/// Install the explicit CLI `--tenacity` override (highest priority). Call once,
/// before the agentic loop starts.
pub fn set_cli_tenacity(level: Tenacity) {
    if let Ok(mut slot) = CLI_TENACITY.lock() {
        *slot = Some(level);
    }
}

/// Clear the explicit CLI `--tenacity` override, so tenacity resolves from config
/// / family again. The complement of [`set_cli_tenacity`]: it lets a surface
/// express "inherit" (no override) rather than pinning the currently-resolved
/// value — e.g. the config panel must not persist an untouched dial.
pub fn clear_cli_tenacity() {
    if let Ok(mut slot) = CLI_TENACITY.lock() {
        *slot = None;
    }
}

/// The raw CLI `--tenacity` override, if one is installed (`None` = inherit from
/// config / family). Distinct from [`effective_tenacity`], which resolves the
/// full precedence ladder to a concrete level.
#[must_use]
pub fn cli_tenacity() -> Option<Tenacity> {
    CLI_TENACITY.lock().ok().and_then(|s| *s)
}

/// Install the resolved `[tenacity]` config (per-family + default). Called by
/// `Config::apply_runtime_settings`, the canonical runtime-application entry.
pub fn set_tenacity_config(config: TenacityConfig) {
    if let Ok(mut slot) = TENACITY_CONFIG.lock() {
        *slot = Some(config);
    }
}

/// Install the active model's family (from its model card) so per-family config
/// defaults apply. Called at model selection. `None` clears it.
pub fn set_active_model_family(family: Option<String>) {
    if let Ok(mut slot) = ACTIVE_FAMILY.lock() {
        *slot = family;
    }
}

/// Install the active persona's declared `tenacity:` (review P1#3). Called when a
/// persona activates (its declared level) or clears (`None`), so the declaration
/// is actually applied — below the CLI override, above the config/family default.
pub fn set_persona_tenacity(level: Option<Tenacity>) {
    if let Ok(mut slot) = PERSONA_TENACITY.lock() {
        *slot = level;
    }
}

/// The active persona's declared tenacity, if any (for status rendering).
#[must_use]
pub fn persona_tenacity() -> Option<Tenacity> {
    PERSONA_TENACITY.lock().ok().and_then(|s| *s)
}

/// The tenacity in effect, resolved most-specific first: the CLI `--tenacity`
/// flag, then the active persona's declared `tenacity:`, then the `[tenacity]`
/// config's per-family override for the active family, then the config default,
/// then `Standard`.
pub fn effective_tenacity() -> Tenacity {
    if let Some(level) = EFFECTIVE_TENACITY_OVERRIDE.with(std::cell::Cell::get) {
        return level;
    }
    let cli = CLI_TENACITY.lock().ok().and_then(|s| *s);
    let persona = PERSONA_TENACITY.lock().ok().and_then(|s| *s);
    let config = TENACITY_CONFIG.lock().ok().and_then(|s| s.clone());
    let family = ACTIVE_FAMILY.lock().ok().and_then(|s| s.clone());
    resolve_tenacity(cli, persona, config.as_ref(), family.as_deref())
}

/// The installed `[tenacity]` config, if any (for status rendering + the snapshot
/// used by the test guard). Read accessor for the otherwise write-only
/// [`TENACITY_CONFIG`].
#[must_use]
pub fn tenacity_config() -> Option<TenacityConfig> {
    TENACITY_CONFIG.lock().ok().and_then(|s| s.clone())
}

/// The active model family, if any (for the config-panel projection + the test
/// guard snapshot). Read accessor for the otherwise write-only [`ACTIVE_FAMILY`].
#[must_use]
pub fn active_model_family() -> Option<String> {
    ACTIVE_FAMILY.lock().ok().and_then(|s| s.clone())
}

/// Tenacity with **no CLI override and no persona layer** — just the config
/// per-family override for the active family, the config default, then `Standard`.
/// This is the value a persona that declares no `tenacity:` inherits, so the
/// config panel projects a selected persona's effective tenacity as
/// `persona.tenacity.unwrap_or(base_tenacity())`.
#[must_use]
pub fn base_tenacity() -> Tenacity {
    let config = TENACITY_CONFIG.lock().ok().and_then(|s| s.clone());
    let family = ACTIVE_FAMILY.lock().ok().and_then(|s| s.clone());
    resolve_tenacity(None, None, config.as_ref(), family.as_deref())
}

/// Attribute the active model's family for per-family `[tenacity]` resolution and
/// install it (`ACTIVE_FAMILY`). The card's `family` wins when a built-in card
/// names one; else the family is inferred from the model NAME against the
/// configured `[tenacity.families]` keys. Call at every point a session settles on
/// a model — solve AND chat (#1139: chat previously never did this, so per-family
/// defaults silently did not apply there). `config == None` ⇒ only a card family.
pub fn attribute_active_family(config: Option<&TenacityConfig>, model: &str) {
    let card_family = crate::model_card::builtin_card(model).and_then(|c| c.family);
    let family = config
        .and_then(|t| t.family_for(model, card_family.as_deref()))
        .or(card_family);
    set_active_model_family(family);
}

/// A complete snapshot of **every** mutable global that feeds
/// [`effective_tenacity`] — the CLI override, the persona layer, the `[tenacity]`
/// config, and the active model family. The test guard snapshots and restores
/// this as one unit so no tenacity-resolution input can leak between tests (the
/// earlier piecemeal guard missed `TENACITY_CONFIG` + `ACTIVE_FAMILY`, which
/// `Config::resolve` and the `solve` model-selection path mutate process-wide).
#[doc(hidden)]
pub struct TenacityRuntimeSnapshot {
    cli: Option<Tenacity>,
    persona: Option<Tenacity>,
    config: Option<TenacityConfig>,
    active_family: Option<String>,
}

/// Snapshot all four tenacity-resolution globals (see [`TenacityRuntimeSnapshot`]).
#[doc(hidden)]
#[must_use]
pub fn snapshot_runtime_state() -> TenacityRuntimeSnapshot {
    TenacityRuntimeSnapshot {
        cli: CLI_TENACITY.lock().ok().and_then(|s| *s),
        persona: PERSONA_TENACITY.lock().ok().and_then(|s| *s),
        config: TENACITY_CONFIG.lock().ok().and_then(|s| s.clone()),
        active_family: ACTIVE_FAMILY.lock().ok().and_then(|s| s.clone()),
    }
}

/// Restore all four tenacity-resolution globals from a snapshot (see
/// [`TenacityRuntimeSnapshot`]). Total: every input is overwritten, so a test
/// that installed a config / family / override is fully undone.
#[doc(hidden)]
pub fn restore_runtime_state(snapshot: TenacityRuntimeSnapshot) {
    if let Ok(mut s) = CLI_TENACITY.lock() {
        *s = snapshot.cli;
    }
    if let Ok(mut s) = PERSONA_TENACITY.lock() {
        *s = snapshot.persona;
    }
    if let Ok(mut s) = TENACITY_CONFIG.lock() {
        *s = snapshot.config;
    }
    if let Ok(mut s) = ACTIVE_FAMILY.lock() {
        *s = snapshot.active_family;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_is_the_behaviour_preserving_default() {
        // Standard must reproduce the historical hardcoded READ_ONLY_NUDGE_AFTER
        // so adopting tenacity changes nothing until an operator/family opts in.
        assert_eq!(Tenacity::default(), Tenacity::Standard);
        assert_eq!(Tenacity::Standard.read_only_nudge_after(), 3);
        assert!(!Tenacity::Standard.exit_plan_requires_edit());
    }

    #[test]
    fn scoped_override_is_nested_thread_local_and_restores_the_global_resolution() {
        use crate::test_guard::GlobalSettingsGuard;
        let _settings = GlobalSettingsGuard::acquire();
        set_cli_tenacity(Tenacity::Relaxed);
        assert_eq!(effective_tenacity(), Tenacity::Relaxed);

        let outer = scoped_effective_tenacity(Tenacity::Relentless);
        assert_eq!(effective_tenacity(), Tenacity::Relentless);

        set_cli_tenacity(Tenacity::Insistent);
        assert_eq!(
            effective_tenacity(),
            Tenacity::Relentless,
            "a concurrent global change cannot alter a captured turn posture"
        );

        {
            let _inner = scoped_effective_tenacity(Tenacity::Standard);
            assert_eq!(effective_tenacity(), Tenacity::Standard);
        }
        assert_eq!(effective_tenacity(), Tenacity::Relentless);

        let other_thread = std::thread::spawn(effective_tenacity)
            .join()
            .expect("tenacity probe thread");
        assert_eq!(
            other_thread,
            Tenacity::Insistent,
            "the override must stay local to the driven turn thread"
        );

        drop(outer);
        assert_eq!(effective_tenacity(), Tenacity::Insistent);
    }

    #[test]
    fn higher_tenacity_forces_action_sooner() {
        // The budget is monotonically non-increasing as tenacity rises.
        let budgets: Vec<usize> = Tenacity::all()
            .iter()
            .map(|t| t.read_only_nudge_after())
            .collect();
        assert_eq!(budgets, vec![6, 3, 2, 1]);
        for pair in budgets.windows(2) {
            assert!(pair[0] > pair[1], "budget must strictly drop with tenacity");
        }
    }

    #[test]
    fn only_the_two_most_forcing_levels_require_an_edit_on_plan_exit() {
        assert!(!Tenacity::Relaxed.exit_plan_requires_edit());
        assert!(!Tenacity::Standard.exit_plan_requires_edit());
        assert!(Tenacity::Insistent.exit_plan_requires_edit());
        assert!(Tenacity::Relentless.exit_plan_requires_edit());
    }

    #[test]
    fn relentless_can_project_an_operator_selected_round_target() {
        for level in [Tenacity::Relaxed, Tenacity::Standard, Tenacity::Insistent] {
            assert_eq!(level.project_tool_round_limit(40), 40);
        }
        assert_eq!(
            Tenacity::Relentless.project_tool_round_limit(40),
            RELENTLESS_TOOL_ROUND_TARGET
        );
        assert_eq!(
            Tenacity::Relentless.project_tool_round_limit(20_000),
            20_000,
            "the posture must not lower an already larger configured budget"
        );
        assert_eq!(
            resolve_tool_round_limit(40, Some(Tenacity::Relentless), Some(7)),
            7,
            "a direct round limit remains the outermost operator choice"
        );
    }

    #[test]
    fn parse_is_case_insensitive_and_round_trips_the_label() {
        for t in Tenacity::all() {
            assert_eq!(t.label().parse::<Tenacity>().unwrap(), t);
            assert_eq!(t.to_string(), t.label());
        }
        assert_eq!(
            "  RELENTLESS ".parse::<Tenacity>().unwrap(),
            Tenacity::Relentless
        );
        assert_eq!("default".parse::<Tenacity>().unwrap(), Tenacity::Standard);
        assert!("banana".parse::<Tenacity>().is_err());
    }

    #[test]
    fn ordering_runs_patient_to_forcing() {
        assert!(Tenacity::Relaxed < Tenacity::Standard);
        assert!(Tenacity::Standard < Tenacity::Insistent);
        assert!(Tenacity::Insistent < Tenacity::Relentless);
    }

    #[test]
    fn serde_uses_the_lowercase_label() {
        let json = serde_json::to_string(&Tenacity::Insistent).unwrap();
        assert_eq!(json, "\"insistent\"");
        let back: Tenacity = serde_json::from_str("\"relentless\"").unwrap();
        assert_eq!(back, Tenacity::Relentless);
    }

    fn cfg(default: Option<Tenacity>, fams: &[(&str, Tenacity)]) -> TenacityConfig {
        TenacityConfig {
            default,
            families: fams.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
        }
    }

    #[test]
    fn config_resolve_prefers_family_then_default_then_standard() {
        let c = cfg(
            Some(Tenacity::Relaxed),
            &[("nemotron", Tenacity::Relentless)],
        );
        // Known family → its override.
        assert_eq!(c.resolve(Some("nemotron")), Tenacity::Relentless);
        // Case-insensitive + trimmed.
        assert_eq!(c.resolve(Some("  NEMOTRON ")), Tenacity::Relentless);
        // Unknown family → the config default.
        assert_eq!(c.resolve(Some("qwen3")), Tenacity::Relaxed);
        // No family → the config default.
        assert_eq!(c.resolve(None), Tenacity::Relaxed);
        // Empty config → Standard.
        assert_eq!(
            TenacityConfig::default().resolve(Some("qwen3")),
            Tenacity::Standard
        );
        assert_eq!(cfg(None, &[]).resolve(None), Tenacity::Standard);
    }

    #[test]
    fn attribute_active_family_installs_card_then_substring_then_clears() {
        use crate::test_guard::GlobalSettingsGuard;
        let _g = GlobalSettingsGuard::acquire();

        // Card path: with no config at all (`config == None` ⇒ only a card family),
        // a built-in card that names a family installs it. ornith-1.0-35b → qwen3.
        set_active_model_family(None);
        attribute_active_family(None, "ornith-1.0-35b");
        assert_eq!(
            active_model_family().as_deref(),
            Some("qwen3"),
            "the built-in card's declared family is installed"
        );

        // Substring path: no card for this name, but the model NAME contains a
        // configured `[tenacity.families]` key → that family is inferred + installed.
        // This is the #1139 chat fix — chat now attributes family exactly as solve does.
        let c = cfg(None, &[("qwen3", Tenacity::Relentless)]);
        set_active_model_family(None);
        attribute_active_family(Some(&c), "Qwen3-Coder-30B");
        assert_eq!(
            active_model_family().as_deref(),
            Some("qwen3"),
            "a family is inferred from the model name against the config families"
        );

        // No card + no matching family → ACTIVE_FAMILY is CLEARED (not left stale),
        // so effective_tenacity falls back to the config default / Standard.
        set_active_model_family(Some("stale".to_string()));
        attribute_active_family(Some(&c), "mystery-model-7b");
        assert_eq!(
            active_model_family(),
            None,
            "an unrecognized model clears the family rather than leaving a stale one"
        );
    }

    #[test]
    fn resolve_tenacity_precedence_cli_over_persona_over_config() {
        let c = cfg(
            Some(Tenacity::Relaxed),
            &[("nemotron", Tenacity::Relentless)],
        );
        // CLI override beats even a matching per-family default AND a persona.
        assert_eq!(
            resolve_tenacity(
                Some(Tenacity::Standard),
                Some(Tenacity::Insistent),
                Some(&c),
                Some("nemotron")
            ),
            Tenacity::Standard
        );
        // No CLI → the persona declaration wins over config per-family (P1#3).
        assert_eq!(
            resolve_tenacity(None, Some(Tenacity::Insistent), Some(&c), Some("nemotron")),
            Tenacity::Insistent,
            "an active persona's declared tenacity is applied, not just rendered"
        );
        // No CLI, no persona → config per-family.
        assert_eq!(
            resolve_tenacity(None, None, Some(&c), Some("nemotron")),
            Tenacity::Relentless
        );
        // No CLI, no persona, unknown family → config default.
        assert_eq!(
            resolve_tenacity(None, None, Some(&c), Some("kimi")),
            Tenacity::Relaxed
        );
        // No config at all → CLI-or-persona-or-Standard.
        assert_eq!(
            resolve_tenacity(None, None, None, Some("nemotron")),
            Tenacity::Standard
        );
        assert_eq!(
            resolve_tenacity(None, Some(Tenacity::Insistent), None, None),
            Tenacity::Insistent
        );
        assert_eq!(
            resolve_tenacity(Some(Tenacity::Insistent), None, None, None),
            Tenacity::Insistent
        );
    }

    #[test]
    fn family_for_prefers_card_then_infers_from_the_model_name() {
        let c = cfg(
            None,
            &[
                ("qwen3", Tenacity::Standard),
                ("nemotron", Tenacity::Relentless),
            ],
        );
        // Card family that names a configured family wins.
        assert_eq!(
            c.family_for("whatever", Some("qwen3")).as_deref(),
            Some("qwen3")
        );
        // No card → infer from the model name (the matrix case).
        assert_eq!(
            c.family_for("qwen3-coder_30b", None).as_deref(),
            Some("qwen3")
        );
        assert_eq!(
            c.family_for("NVIDIA-Nemotron-3-Nano", None).as_deref(),
            Some("nemotron")
        );
        // No configured family matches the name → None (→ default level).
        assert_eq!(c.family_for("gemma-2-9b", None), None);
        // A card family NOT in the map falls through to name inference.
        assert_eq!(
            c.family_for("qwen3-coder_30b", Some("unlisted")).as_deref(),
            Some("qwen3")
        );
        // Resolving that inferred family gives the per-family level.
        let fam = c.family_for("qwen3-coder_30b", None);
        assert_eq!(c.resolve(fam.as_deref()), Tenacity::Standard);
    }

    #[test]
    fn snapshot_restore_round_trips_every_tenacity_resolution_global() {
        // CR3 area 4: the guard must isolate ALL FOUR inputs to effective_tenacity
        // — CLI override, persona layer, TENACITY_CONFIG, ACTIVE_FAMILY. Exercise
        // the exact snapshot/restore the guard's Drop runs.
        use crate::test_guard::GlobalSettingsGuard;
        let _g = GlobalSettingsGuard::acquire(); // serialize + final cleanup

        // Known-empty baseline → snapshot it.
        clear_cli_tenacity();
        set_persona_tenacity(None);
        set_tenacity_config(TenacityConfig::default());
        set_active_model_family(None);
        let snap = snapshot_runtime_state();

        // Mutate every axis.
        set_cli_tenacity(Tenacity::Relentless);
        set_persona_tenacity(Some(Tenacity::Insistent));
        set_tenacity_config(cfg(
            Some(Tenacity::Relaxed),
            &[("nemotron", Tenacity::Relentless)],
        ));
        set_active_model_family(Some("nemotron".to_string()));
        assert_eq!(cli_tenacity(), Some(Tenacity::Relentless));
        assert_eq!(persona_tenacity(), Some(Tenacity::Insistent));
        assert!(tenacity_config().is_some_and(|c| !c.families.is_empty()));
        assert_eq!(active_model_family().as_deref(), Some("nemotron"));

        // Restore — exactly what GlobalSettingsGuard::drop does — undoes all four.
        restore_runtime_state(snap);
        assert_eq!(cli_tenacity(), None, "CLI tenacity restored");
        assert_eq!(persona_tenacity(), None, "persona tenacity restored");
        assert_eq!(
            tenacity_config(),
            Some(TenacityConfig::default()),
            "TENACITY_CONFIG restored (the gap the piecemeal guard missed)"
        );
        assert_eq!(active_model_family(), None, "ACTIVE_FAMILY restored");
    }

    #[test]
    fn base_tenacity_ignores_cli_and_persona_overrides() {
        // The config-panel projection uses base_tenacity() as the value a persona
        // inherits when it declares none: it must strip the CLI + persona layers.
        use crate::test_guard::GlobalSettingsGuard;
        let _g = GlobalSettingsGuard::acquire();
        set_tenacity_config(cfg(
            Some(Tenacity::Relaxed),
            &[("nemotron", Tenacity::Relentless)],
        ));
        set_active_model_family(Some("nemotron".to_string()));
        set_cli_tenacity(Tenacity::Standard); // present…
        set_persona_tenacity(Some(Tenacity::Insistent)); // …present…
        assert_eq!(
            base_tenacity(),
            Tenacity::Relentless,
            "base strips CLI + persona, leaving the per-family override"
        );
        set_active_model_family(None);
        assert_eq!(
            base_tenacity(),
            Tenacity::Relaxed,
            "no family → the config default"
        );
    }

    #[test]
    fn guarded_state_is_restored_even_when_a_test_panics() {
        // CR3 area 4: restoration must survive a panic (Drop runs during unwind).
        use crate::test_guard::GlobalSettingsGuard;
        let sentinel = "panic-family-sentinel";
        let result = std::panic::catch_unwind(|| {
            let _g = GlobalSettingsGuard::acquire();
            set_active_model_family(Some(sentinel.to_string()));
            set_cli_tenacity(Tenacity::Relentless);
            assert_eq!(active_model_family().as_deref(), Some(sentinel));
            panic!("intentional panic inside a guarded test");
        });
        assert!(result.is_err(), "the guarded closure panicked as intended");
        let _g = GlobalSettingsGuard::acquire();
        assert_ne!(
            active_model_family().as_deref(),
            Some(sentinel),
            "GlobalSettingsGuard::drop restored ACTIVE_FAMILY during unwind"
        );
    }

    #[test]
    fn tenacity_config_parses_from_toml() {
        let c: TenacityConfig = toml::from_str(
            r#"
            default = "insistent"
            [families]
            nemotron = "relentless"
            qwen3 = "standard"
        "#,
        )
        .unwrap();
        assert_eq!(c.default, Some(Tenacity::Insistent));
        assert_eq!(c.resolve(Some("nemotron")), Tenacity::Relentless);
        assert_eq!(c.resolve(Some("qwen3")), Tenacity::Standard);
        assert_eq!(c.resolve(Some("gemma")), Tenacity::Insistent);
    }
}
