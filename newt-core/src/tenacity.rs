//! Tenacity — how long and hard the agent pursues the task.
//!
//! Normal retains optional verification. Resolute and relentless require fresh
//! verification before an Act turn can complete. Only a direct operator choice
//! of relentless lifts the tool-round cap; persona and config defaults never do.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

#[path = "tenacity_config.rs"]
mod config;
pub use config::{TenacityBudgets, TenacityConfig};

/// Bounded sentinel used for an operator-selected relentless run. This is high
/// enough to behave as "finish the objective" while remaining finite.
pub const RELENTLESS_TOOL_ROUND_TARGET: usize = 10_000;

/// How long the harness keeps the model pursuing the task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Tenacity {
    /// Stop at the first plausible finish, within the configured round limit.
    #[default]
    Normal,
    /// Admit bounded corrective reasoning after observed operational failures.
    Grit,
    /// Require fresh successful verification before completing an Act task.
    Resolute,
    /// Require verification and also lift the limit when chosen explicitly.
    Relentless,
}

impl Tenacity {
    /// Project the default tool-round budget when this level was selected by
    /// the operator. Relentless is the only level that changes the cap: it uses
    /// the shared effectively-unlimited target without lowering a larger
    /// configured value. Callers retain explicit round overrides as the final
    /// precedence layer.
    #[must_use]
    pub fn project_tool_round_limit(self, configured: usize) -> usize {
        if self == Self::Relentless {
            configured.max(RELENTLESS_TOOL_ROUND_TARGET)
        } else {
            configured
        }
    }

    /// Stable lowercase label — the wire/config/`/psyche tenacity` spelling.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Grit => "grit",
            Self::Resolute => "resolute",
            Self::Relentless => "relentless",
        }
    }

    /// One-line description of what this level does.
    #[must_use]
    pub fn describe(self) -> String {
        match self {
            Self::Normal => {
                "stop at the first plausible finish, within the configured round limit".to_string()
            }
            Self::Grit => "recover from observed failures within the captured retry allowance".to_string(),
            Self::Resolute => "require fresh successful verification before task completion".to_string(),
            Self::Relentless => format!(
                "require fresh verification; an explicit choice lifts the tool-round limit to at least {RELENTLESS_TOOL_ROUND_TARGET} \
                 (an explicit round limit still wins)"
            ),
        }
    }

    /// Whether task completion must carry fresh successful verification.
    #[must_use]
    pub fn requires_verification(self) -> bool {
        matches!(self, Self::Resolute | Self::Relentless)
    }

    /// Whether observed failures receive bounded harness corrective reasoning.
    #[must_use]
    pub fn recovers_failures(self) -> bool {
        self != Self::Normal
    }

    /// All levels, normal → relentless.
    #[must_use]
    pub fn all() -> [Self; 4] {
        [Self::Normal, Self::Grit, Self::Resolute, Self::Relentless]
    }
}

/// The tenacity level a [`ToolRoundLimit`] recorded, exactly as written.
///
/// Settings receipts embed [`ToolRoundLimit`] and are content-addressed, so a
/// line minted before the initiative split, carrying `relaxed`, `standard` or
/// `insistent`, must still decode AND re-encode to the same bytes, or
/// `is_intact` reports it as tampered. `untagged` keeps both arms a bare label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RecordedTenacity {
    Current(Tenacity),
    Legacy(LegacyTenacity),
}

/// Tenacity labels from before the initiative split; decode-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LegacyTenacity {
    Relaxed,
    Standard,
    Insistent,
}

impl RecordedTenacity {
    /// The label as recorded.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Current(t) => t.label(),
            Self::Legacy(LegacyTenacity::Relaxed) => "relaxed",
            Self::Legacy(LegacyTenacity::Standard) => "standard",
            Self::Legacy(LegacyTenacity::Insistent) => "insistent",
        }
    }
}

impl From<Tenacity> for RecordedTenacity {
    fn from(t: Tenacity) -> Self {
        Self::Current(t)
    }
}

/// Which input decided the effective tool-round limit (#1965).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolRoundLimitSource {
    /// The configured value — `[tui].max_tool_rounds` or a per-model tuning.
    Config,
    /// An operator-selected tenacity level raised it.
    Tenacity,
    /// An explicit `/rounds` / `--max-rounds` value, the outermost override.
    Override,
}

impl ToolRoundLimitSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Tenacity => "tenacity",
            Self::Override => "override",
        }
    }
}

/// The effective tool-round limit **and how it was reached** (#1965).
///
/// # Why this is a struct and not a `usize`
///
/// [`resolve_tool_round_limit`] promised, in its own doc comment, to compose
/// this "without losing provenance" — and returned a bare number, so the
/// provenance was computed and discarded at the one site responsible for
/// keeping it. A session escalated 40 rounds to effectively unlimited and left
/// no record anywhere: not in config, not in a receipt, not in a turn row.
/// Runs then reached rounds 145, 236, 285 and 320.
///
/// Carrying the derivation in the return type means a caller cannot record the
/// number while dropping where it came from — the number and its justification
/// are one value. That is the same move as `#1908`'s: make the lossy call
/// impossible to write rather than remember to write it correctly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRoundLimit {
    /// The limit actually enforced this turn.
    pub rounds: usize,
    /// Which input won.
    pub source: ToolRoundLimitSource,
    /// What config alone would have given — kept so a reader can see the
    /// ESCALATION, not merely the result. "320 rounds" is a number; "320,
    /// from an override, over a configured 40" is an explanation.
    pub configured: usize,
    /// The operator-selected level, when one was in play, as recorded.
    pub tenacity: Option<RecordedTenacity>,
}

impl ToolRoundLimit {
    /// Whether the effective limit differs from what config alone would give —
    /// the condition worth telling an operator about.
    #[must_use]
    pub fn is_escalated(&self) -> bool {
        self.rounds != self.configured
    }
}

/// Compose the tool-round safety valve without losing provenance. Callers pass
/// only a direct operator choice ([`cli_tenacity`]), never a resolved default,
/// so no automatic layer can silently grant 10,000 rounds. An explicit
/// `/rounds`/`--max-rounds` value remains the outermost override.
///
/// Returns a [`ToolRoundLimit`] rather than a number, so the derivation cannot
/// be dropped on the way to a durable record — see that type's docs (#1965).
#[must_use]
pub fn resolve_tool_round_limit(
    configured: usize,
    explicit_tenacity: Option<Tenacity>,
    explicit_rounds: Option<usize>,
) -> ToolRoundLimit {
    if let Some(rounds) = explicit_rounds {
        return ToolRoundLimit {
            rounds,
            source: ToolRoundLimitSource::Override,
            configured,
            tenacity: explicit_tenacity.map(RecordedTenacity::from),
        };
    }
    // A tenacity level that does not RAISE the limit did not decide it — the
    // configured value did, and saying "tenacity" there would name a cause that
    // changed nothing.
    if let Some(level) = explicit_tenacity {
        let projected = level.project_tool_round_limit(configured);
        if projected != configured {
            return ToolRoundLimit {
                rounds: projected,
                source: ToolRoundLimitSource::Tenacity,
                configured,
                tenacity: Some(level.into()),
            };
        }
    }
    ToolRoundLimit {
        rounds: configured,
        source: ToolRoundLimitSource::Config,
        configured,
        tenacity: explicit_tenacity.map(RecordedTenacity::from),
    }
}

impl fmt::Display for Tenacity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

impl FromStr for Tenacity {
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
                "tenacity '{s}' was split: its read-before-acting half is now \
                 `--initiative {new}`; tenacity is normal|grit|resolute|relentless"
            ));
        }
        Err(format!(
            "unknown tenacity '{s}' (normal|grit|resolute|relentless)"
        ))
    }
}

// The explicit operator choice (`--tenacity`, `/psyche tenacity`, a pin, the
// obsessive posture). Only this raw choice may lift the round cap.
static CLI_TENACITY: std::sync::Mutex<Option<Tenacity>> = std::sync::Mutex::new(None);
static PERSONA_TENACITY: std::sync::Mutex<Option<Tenacity>> = std::sync::Mutex::new(None);
static TENACITY_CONFIG: std::sync::Mutex<Option<TenacityConfig>> = std::sync::Mutex::new(None);

/// Resolve pursuit independently of the operator-only cap calculation.
pub fn resolve_tenacity(
    cli: Option<Tenacity>,
    persona: Option<Tenacity>,
    config: Option<&TenacityConfig>,
    family: Option<&str>,
) -> Tenacity {
    cli.or(persona)
        .unwrap_or_else(|| config.map(|c| c.resolve(family)).unwrap_or_default())
}

/// Install the active persona declaration; clearing it restores inheritance.
pub fn set_persona_tenacity(level: Option<Tenacity>) {
    if let Ok(mut slot) = PERSONA_TENACITY.lock() {
        *slot = level;
    }
}

pub fn persona_tenacity() -> Option<Tenacity> {
    PERSONA_TENACITY.lock().ok().and_then(|slot| *slot)
}

/// Publish config defaults without granting an operator round-cap override.
pub fn set_tenacity_config(config: TenacityConfig) {
    if let Ok(mut slot) = TENACITY_CONFIG.lock() {
        *slot = Some(config);
    }
}

pub fn tenacity_config() -> Option<TenacityConfig> {
    TENACITY_CONFIG.lock().ok().and_then(|slot| slot.clone())
}

/// The family/config baseline shown by inherited panel settings.
pub fn base_tenacity() -> Tenacity {
    resolve_tenacity(
        None,
        None,
        tenacity_config().as_ref(),
        crate::initiative::active_model_family().as_deref(),
    )
}

pub fn model_default_tenacity() -> Option<Tenacity> {
    tenacity_config()?.family_default(crate::initiative::active_model_family().as_deref())
}
// #1998: the two inputs to `resolve_tool_round_limit` that were NOT here.
//
// Its other inputs have always been process globals in this module. The last — the `/rounds` session override — was a local variable inside
// `run_chat`, which is exactly what #1965's evidence complains about: "the
// effective limit is recomputed per dispatch … and a session-local
// `max_tool_rounds_override` echoed only to the truncated alternate-screen
// terminal". A local cannot be read by a receipt writer, by a status line, or
// by anything outside the one function that declares it, which is why the
// number that ran a session to round 320 was unrecoverable afterwards.
//
// `CONFIGURED_TOOL_ROUNDS` is the config/model-tuned baseline the override is
// derived AGAINST, installed by the session when the active model settles —
// the same shape and the same reason as `initiative`'s active family. With both here,
// the whole derivation is computable from anywhere.
static SESSION_TOOL_ROUNDS: std::sync::Mutex<Option<usize>> = std::sync::Mutex::new(None);
static CONFIGURED_TOOL_ROUNDS: std::sync::Mutex<Option<usize>> = std::sync::Mutex::new(None);

std::thread_local! {
    /// A driven turn resolves tenacity before crossing onto its dedicated
    /// thread. Keeping that value here makes every downstream resolver read in
    /// the turn (workflow steering and tool dispatch included) observe the same
    /// immutable posture without replacing the interactive process globals.
    static EFFECTIVE_TENACITY_OVERRIDE: std::cell::Cell<Option<Tenacity>> =
        const { std::cell::Cell::new(None) };
    static BUDGET_OVERRIDE: std::cell::Cell<Option<TenacityBudgets>> =
        const { std::cell::Cell::new(None) };
}

/// Restores the prior current-thread override on drop. The `Rc` marker keeps
/// the guard on the thread whose TLS slot it owns; driven turns use a
/// current-thread runtime, so the guard safely spans the whole async turn.
#[must_use]
pub struct ScopedEffectiveTenacity {
    previous: Option<Tenacity>,
    previous_budgets: Option<TenacityBudgets>,
    _thread_bound: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl Drop for ScopedEffectiveTenacity {
    fn drop(&mut self) {
        let _ = EFFECTIVE_TENACITY_OVERRIDE.try_with(|slot| slot.set(self.previous));
        let _ = BUDGET_OVERRIDE.try_with(|slot| slot.set(self.previous_budgets));
    }
}

/// Override [`effective_tenacity`] on the current thread until the returned
/// guard drops. Overrides nest in lexical (LIFO) order.
///
/// `pub` since #1669: a session that runs its turn on its own thread captures
/// this dial there, so the embedding crate needs it. Prefer
/// [`crate::psyche::capture_turn_psyche`] at a turn boundary, so tenacity is
/// never pinned without the other dials.
pub fn scoped_effective_tenacity(level: Tenacity) -> ScopedEffectiveTenacity {
    scoped_tenacity_settings(level, effective_tenacity_budgets())
}

/// Install the level and numerical policy already captured by the turn owner.
pub fn scoped_tenacity_settings(
    level: Tenacity,
    budgets: TenacityBudgets,
) -> ScopedEffectiveTenacity {
    let previous_budgets = BUDGET_OVERRIDE.with(|slot| slot.replace(Some(budgets)));
    let previous = EFFECTIVE_TENACITY_OVERRIDE.with(|slot| slot.replace(Some(level)));
    ScopedEffectiveTenacity {
        previous,
        previous_budgets,
        _thread_bound: std::marker::PhantomData,
    }
}

/// A turn sees its frozen numerical policy; other threads resolve live config.
pub fn effective_tenacity_budgets() -> TenacityBudgets {
    BUDGET_OVERRIDE
        .with(std::cell::Cell::get)
        .unwrap_or_else(|| {
            tenacity_config()
                .map(|config| config.budgets)
                .unwrap_or_default()
        })
}

/// Install the explicit CLI `--tenacity` override (highest priority). Call once,
/// before the agentic loop starts.
pub fn set_cli_tenacity(level: Tenacity) {
    if let Ok(mut slot) = CLI_TENACITY.lock() {
        *slot = Some(level);
    }
}

/// Clear the explicit CLI override, restoring persona/config inheritance.
/// The complement of [`set_cli_tenacity`]: it lets a surface
/// express "inherit" (no override) rather than pinning the currently-resolved
/// value — e.g. the config panel must not persist an untouched dial.
pub fn clear_cli_tenacity() {
    if let Ok(mut slot) = CLI_TENACITY.lock() {
        *slot = None;
    }
}

/// The raw CLI `--tenacity` override, if one is installed (`None` = inherit).
/// Distinct from [`effective_tenacity`], which also honours a turn's capture.
#[must_use]
pub fn cli_tenacity() -> Option<Tenacity> {
    CLI_TENACITY.lock().ok().and_then(|s| *s)
}

/// The tenacity in effect: a turn's captured value, else the explicit
/// operator choice, persona, exact family default, config default, then Normal.
#[must_use]
pub fn effective_tenacity() -> Tenacity {
    if let Some(level) = EFFECTIVE_TENACITY_OVERRIDE.with(std::cell::Cell::get) {
        return level;
    }
    resolve_tenacity(
        cli_tenacity(),
        persona_tenacity(),
        tenacity_config().as_ref(),
        crate::initiative::active_model_family().as_deref(),
    )
}

/// A complete snapshot of **every** mutable global in this module — the CLI
/// override and the two round-cap inputs. The test guard snapshots and
/// restores this as one unit (with [`crate::initiative`]'s sibling snapshot)
/// so no input can leak between tests.
#[doc(hidden)]
pub struct TenacityRuntimeSnapshot {
    cli: Option<Tenacity>,
    persona: Option<Tenacity>,
    config: Option<TenacityConfig>,
    session_rounds: Option<usize>,
    configured_rounds: Option<usize>,
}

/// Install (or clear, with `None`) the operator's session tool-round override —
/// what `/rounds <n>` sets and `/rounds reset` releases.
///
/// This is the OUTERMOST input to [`resolve_tool_round_limit`]. Call it only
/// where the change is an operator decision, and record that decision: an
/// escalation here is the exact event #1965 was filed about.
pub fn set_session_tool_rounds(rounds: Option<usize>) {
    if let Ok(mut slot) = SESSION_TOOL_ROUNDS.lock() {
        *slot = rounds;
    }
}

/// The operator's session tool-round override, if one is installed.
#[must_use]
pub fn session_tool_rounds() -> Option<usize> {
    SESSION_TOOL_ROUNDS.lock().ok().and_then(|s| *s)
}

/// Install (or forget, with `None`) the config/model-tuned round cap for the
/// ACTIVE model — the baseline an override is measured against. Called by the
/// session wherever it already derives that number, the same way
/// `set_active_model_family` is.
///
/// `Option` for symmetry with [`set_session_tool_rounds`], and because
/// "forget the baseline" is a real state: no model has settled yet, and a
/// receipt written then must say so rather than reuse a stale number.
pub fn set_configured_tool_rounds(rounds: Option<usize>) {
    if let Ok(mut slot) = CONFIGURED_TOOL_ROUNDS.lock() {
        *slot = rounds;
    }
}

/// The installed config/model baseline, or `None` when no session has settled
/// a model yet.
///
/// Deliberately NOT defaulted to 40. A receipt that says "configured 40" when
/// nothing installed a baseline is a confident lie about where a number came
/// from, which is the failure this whole line exists to stop; `None` says
/// "unknown", and callers render that honestly.
#[must_use]
pub fn configured_tool_rounds() -> Option<usize> {
    CONFIGURED_TOOL_ROUNDS.lock().ok().and_then(|s| *s)
}

/// The cap a turn would run under right now, **with its derivation**, from the
/// globals alone (#1998).
///
/// `None` only when no baseline has been installed — see
/// [`configured_tool_rounds`]. This is what lets a receipt carry the whole
/// `ToolRoundLimit` rather than a bare number, without the writer needing the
/// config, the model card, or the session's locals.
#[must_use]
pub fn session_tool_round_limit() -> Option<ToolRoundLimit> {
    Some(resolve_tool_round_limit(
        configured_tool_rounds()?,
        cli_tenacity(),
        session_tool_rounds(),
    ))
}

/// Snapshot every tenacity-resolution global (see [`TenacityRuntimeSnapshot`]) —
/// including the two round-cap inputs #1998 moved here.
#[doc(hidden)]
#[must_use]
pub fn snapshot_runtime_state() -> TenacityRuntimeSnapshot {
    TenacityRuntimeSnapshot {
        cli: cli_tenacity(),
        persona: persona_tenacity(),
        config: tenacity_config(),
        session_rounds: session_tool_rounds(),
        configured_rounds: configured_tool_rounds(),
    }
}

/// Restore every tenacity-resolution global from a snapshot (see
/// [`TenacityRuntimeSnapshot`]). Total: every input is overwritten, so a test
/// that installed an override is fully undone.
#[doc(hidden)]
pub fn restore_runtime_state(snapshot: TenacityRuntimeSnapshot) {
    if let Ok(mut s) = CLI_TENACITY.lock() {
        *s = snapshot.cli;
    }
    set_persona_tenacity(snapshot.persona);
    if let Ok(mut s) = TENACITY_CONFIG.lock() {
        *s = snapshot.config;
    }
    if let Ok(mut s) = SESSION_TOOL_ROUNDS.lock() {
        *s = snapshot.session_rounds;
    }
    if let Ok(mut s) = CONFIGURED_TOOL_ROUNDS.lock() {
        *s = snapshot.configured_rounds;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The round-cap globals are readable from outside `run_chat`** — which
    /// is the whole point of moving them (#1998). A local could not be read by
    /// a receipt writer, which is why the escalation that produced #1965 was
    /// unrecoverable.
    #[test]
    fn the_session_override_and_its_baseline_round_trip() {
        let _g = crate::test_guard::GlobalSettingsGuard::acquire();
        set_session_tool_rounds(None);
        assert_eq!(session_tool_rounds(), None, "no override by default");
        set_session_tool_rounds(Some(320));
        assert_eq!(session_tool_rounds(), Some(320));
        set_session_tool_rounds(None);
        assert_eq!(session_tool_rounds(), None, "the override releases");

        set_configured_tool_rounds(Some(40));
        assert_eq!(configured_tool_rounds(), Some(40));
    }

    /// **The whole derivation is computable from the globals alone.**
    ///
    /// This is the capability the receipt needs: `320, from an override, over
    /// a configured 40` without the writer holding the config, the model card
    /// or the session's locals.
    #[test]
    fn the_derivation_is_computable_from_the_globals() {
        let _g = crate::test_guard::GlobalSettingsGuard::acquire();
        set_configured_tool_rounds(Some(40));
        set_cli_tenacity(Tenacity::Relentless);
        set_session_tool_rounds(Some(320));

        let limit = session_tool_round_limit().expect("a baseline is installed");
        assert_eq!(limit.rounds, 320);
        assert_eq!(limit.source, ToolRoundLimitSource::Override);
        assert_eq!(limit.configured, 40);
        assert_eq!(limit.tenacity, Some(Tenacity::Relentless.into()));
        assert!(limit.is_escalated(), "320 over a configured 40");

        // Releasing the override changes which input won — the field that
        // makes the record an explanation rather than a number.
        set_session_tool_rounds(None);
        let limit = session_tool_round_limit().expect("a baseline is installed");
        assert_eq!(limit.source, ToolRoundLimitSource::Tenacity);
        assert_eq!(limit.configured, 40);
    }

    /// **An uninstalled baseline says so, rather than guessing 40.**
    ///
    /// Anti-vacuous twin for the two above: if this returned a default, every
    /// assertion about `configured` would hold over a receipt that invented
    /// the number it claims the limit was measured against.
    #[test]
    fn no_baseline_means_no_derivation_not_a_default() {
        let _g = crate::test_guard::GlobalSettingsGuard::acquire();
        set_configured_tool_rounds(None);
        assert_eq!(configured_tool_rounds(), None);
        assert!(
            session_tool_round_limit().is_none(),
            "a derivation without a baseline would be a confident invention"
        );
    }

    #[test]
    fn normal_is_the_default_and_leaves_the_cap_alone() {
        assert_eq!(Tenacity::default(), Tenacity::Normal);
        assert_eq!(Tenacity::Normal.project_tool_round_limit(40), 40);
    }

    #[test]
    fn scoped_override_is_nested_thread_local_and_restores_the_global_resolution() {
        use crate::test_guard::GlobalSettingsGuard;
        let _settings = GlobalSettingsGuard::acquire();
        clear_cli_tenacity();
        assert_eq!(effective_tenacity(), Tenacity::Normal);

        let outer = scoped_effective_tenacity(Tenacity::Relentless);
        assert_eq!(effective_tenacity(), Tenacity::Relentless);

        {
            let _inner = scoped_effective_tenacity(Tenacity::Normal);
            assert_eq!(effective_tenacity(), Tenacity::Normal);
        }
        assert_eq!(effective_tenacity(), Tenacity::Relentless);

        let other_thread = std::thread::spawn(effective_tenacity)
            .join()
            .expect("tenacity probe thread");
        assert_eq!(
            other_thread,
            Tenacity::Normal,
            "the override must stay local to the driven turn thread"
        );

        drop(outer);
        set_cli_tenacity(Tenacity::Relentless);
        assert_eq!(effective_tenacity(), Tenacity::Relentless);
    }

    #[test]
    fn relentless_can_project_an_operator_selected_round_target() {
        assert_eq!(Tenacity::Normal.project_tool_round_limit(40), 40);
        assert_eq!(
            Tenacity::Relentless.project_tool_round_limit(40),
            RELENTLESS_TOOL_ROUND_TARGET
        );
        assert_eq!(
            Tenacity::Relentless.project_tool_round_limit(20_000),
            20_000,
            "the posture must not lower an already larger configured budget"
        );
        let overridden = resolve_tool_round_limit(40, Some(Tenacity::Relentless), Some(7));
        assert_eq!(
            (overridden.rounds, overridden.source),
            (7, ToolRoundLimitSource::Override),
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
        assert_eq!("default".parse::<Tenacity>().unwrap(), Tenacity::Normal);
        assert!("banana".parse::<Tenacity>().is_err());
    }

    /// Regression (slice 1b): an old tenacity label is refused with the
    /// initiative flag that replaces it. Before the split `insistent` parsed.
    #[test]
    fn an_old_level_is_refused_with_its_initiative_replacement() {
        for (old, new) in [
            ("relaxed", "patient"),
            ("standard", "measured"),
            ("insistent", "decisive"),
        ] {
            let err = old.parse::<Tenacity>().unwrap_err();
            assert!(err.contains(&format!("--initiative {new}")), "{err}");
        }
    }

    #[test]
    fn serde_uses_the_lowercase_label() {
        let json = serde_json::to_string(&Tenacity::Relentless).unwrap();
        assert_eq!(json, "\"relentless\"");
        let back: Tenacity = serde_json::from_str("\"normal\"").unwrap();
        assert_eq!(back, Tenacity::Normal);
        assert!(serde_json::from_str::<Tenacity>("\"standard\"").is_err());
    }

    /// A recorded level keeps its exact label through a decode/encode round
    /// trip, old vocabulary included: receipts are content-addressed.
    #[test]
    fn a_recorded_level_round_trips_old_and_new_labels_verbatim() {
        for label in ["relaxed", "standard", "insistent", "normal", "relentless"] {
            let json = format!("\"{label}\"");
            let r: RecordedTenacity = serde_json::from_str(&json).unwrap();
            assert_eq!(serde_json::to_string(&r).unwrap(), json);
            assert_eq!(r.label(), label);
        }
        assert_eq!(
            serde_json::from_str::<RecordedTenacity>("\"relentless\"").unwrap(),
            RecordedTenacity::Current(Tenacity::Relentless)
        );
    }

    #[test]
    fn snapshot_restore_round_trips_every_tenacity_global() {
        // The guard must isolate every input in this module: the CLI override
        // and, since #1998, the two round-cap globals.
        use crate::test_guard::GlobalSettingsGuard;
        let _g = GlobalSettingsGuard::acquire();

        clear_cli_tenacity();
        set_session_tool_rounds(None);
        set_configured_tool_rounds(None);
        let snap = snapshot_runtime_state();

        set_cli_tenacity(Tenacity::Relentless);
        set_session_tool_rounds(Some(320));
        set_configured_tool_rounds(Some(40));
        assert_eq!(cli_tenacity(), Some(Tenacity::Relentless));

        restore_runtime_state(snap);
        assert_eq!(cli_tenacity(), None, "CLI tenacity restored");
        assert_eq!(
            session_tool_rounds(),
            None,
            "the /rounds override restored — a leaked 320 would escalate the next test"
        );
        assert_eq!(configured_tool_rounds(), None);
    }

    #[test]
    fn guarded_state_is_restored_even_when_a_test_panics() {
        // CR3 area 4: restoration must survive a panic (Drop runs during unwind).
        use crate::test_guard::GlobalSettingsGuard;
        let result = std::panic::catch_unwind(|| {
            let _g = GlobalSettingsGuard::acquire();
            clear_cli_tenacity();
            set_session_tool_rounds(Some(4242));
            panic!("intentional panic inside a guarded test");
        });
        assert!(result.is_err(), "the guarded closure panicked as intended");
        let _g = GlobalSettingsGuard::acquire();
        assert_ne!(
            session_tool_rounds(),
            Some(4242),
            "GlobalSettingsGuard::drop restored the round override during unwind"
        );
    }
}

#[cfg(test)]
#[path = "tenacity_resolute_tests.rs"]
mod resolute_tests;

#[cfg(test)]
#[path = "tenacity_grit_tests.rs"]
mod grit_tests;
