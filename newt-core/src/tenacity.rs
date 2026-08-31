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
    /// The operator-selected level, when one was in play.
    pub tenacity: Option<Tenacity>,
}

impl ToolRoundLimit {
    /// Whether the effective limit differs from what config alone would give —
    /// the condition worth telling an operator about.
    #[must_use]
    pub fn is_escalated(&self) -> bool {
        self.rounds != self.configured
    }
}

/// Compose the tool-round safety valve without losing provenance. Automatic
/// persona/config/family tenacity is intentionally not accepted here: callers
/// pass only a direct operator choice, because small loop-prone families often
/// default to relentless and must not silently receive 10,000 rounds. An
/// explicit `/rounds`/`--max-rounds` value remains the outermost override.
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
            tenacity: explicit_tenacity,
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
                tenacity: Some(level),
            };
        }
    }
    ToolRoundLimit {
        rounds: configured,
        source: ToolRoundLimitSource::Config,
        configured,
        tenacity: explicit_tenacity,
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
// #1998: the two inputs to `resolve_tool_round_limit` that were NOT here.
//
// Three of its four inputs have always been process globals in this module.
// The fourth — the `/rounds` session override — was a local variable inside
// `run_chat`, which is exactly what #1965's evidence complains about: "the
// effective limit is recomputed per dispatch … and a session-local
// `max_tool_rounds_override` echoed only to the truncated alternate-screen
// terminal". A local cannot be read by a receipt writer, by a status line, or
// by anything outside the one function that declares it, which is why the
// number that ran a session to round 320 was unrecoverable afterwards.
//
// `CONFIGURED_TOOL_ROUNDS` is the config/model-tuned baseline the override is
// derived AGAINST, installed by the session when the active model settles —
// the same shape and the same reason as `ACTIVE_FAMILY` above. With both here,
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
}

/// Restores the prior current-thread override on drop. The `Rc` marker keeps
/// the guard on the thread whose TLS slot it owns; driven turns use a
/// current-thread runtime, so the guard safely spans the whole async turn.
#[must_use]
pub struct ScopedEffectiveTenacity {
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
///
/// `pub` since #1669: a session that runs its turn on its own thread captures
/// this dial there, so the embedding crate needs it. Prefer
/// [`crate::psyche::capture_turn_psyche`] at a turn boundary, so tenacity is
/// never pinned without cognition.
pub fn scoped_effective_tenacity(level: Tenacity) -> ScopedEffectiveTenacity {
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

/// Install the active model's family so per-family config defaults apply;
/// `None` clears it. This is the ONLY attribution path (#1820): callers derive
/// the family from typed resolved-card metadata
/// (`ResolvedCapabilities::family_for_route`) at every point a session settles
/// on a model — chat AND solve (#1139). Deliberately, nothing infers a family
/// from the model NAME anymore: a cardless model whose name merely contains a
/// configured `[tenacity.families]` key gets NO family and falls to the
/// configured default (names are labels, never evidence — #1818/#1819). Opt
/// back in by writing a drop-in model card that names the family.
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
        cli: CLI_TENACITY.lock().ok().and_then(|s| *s),
        persona: PERSONA_TENACITY.lock().ok().and_then(|s| *s),
        config: TENACITY_CONFIG.lock().ok().and_then(|s| s.clone()),
        active_family: ACTIVE_FAMILY.lock().ok().and_then(|s| s.clone()),
        session_rounds: session_tool_rounds(),
        configured_rounds: configured_tool_rounds(),
    }
}

/// Restore every tenacity-resolution global from a snapshot (see
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
        assert_eq!(limit.tenacity, Some(Tenacity::Relentless));
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

    /// Companion to the exact-family migration (#1820). The red/green
    /// regression for the removal itself is
    /// `tests/tenacity_exact_family_ratchet.rs` (red on the pre-migration
    /// source); THESE assertions pin the surviving semantics: family reaches
    /// tenacity only through the typed seam, the label match is equality, and
    /// a session with no typed evidence gets the configured default — where
    /// before, a cardless model named like `my-nemotron-alias` picked up the
    /// `nemotron` level by name containment. The end-to-end solve negative
    /// lives in newt-cli/tests/solve_cli.rs
    /// (`a_cardless_nemotron_looking_alias_gets_no_family_tenacity`).
    #[test]
    fn family_arrives_only_through_the_typed_seam() {
        use crate::test_guard::GlobalSettingsGuard;
        let _g = GlobalSettingsGuard::acquire();

        set_tenacity_config(cfg(None, &[("nemotron", Tenacity::Relentless)]));

        // Typed evidence — a resolved card's family — installs and resolves.
        set_active_model_family(Some("nemotron".to_string()));
        assert_eq!(effective_tenacity(), Tenacity::Relentless);

        // The label match is equality (case-insensitive), never containment:
        // a family label that merely CONTAINS a configured key is not it.
        set_active_model_family(Some("nemotron-super".to_string()));
        assert_eq!(effective_tenacity(), Tenacity::Standard);

        // No typed evidence => no family. Tenacity never sees a model name,
        // so there is nothing to infer from — the deliberate behavior loss.
        set_active_model_family(None);
        assert_eq!(effective_tenacity(), Tenacity::Standard);
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
    fn snapshot_restore_round_trips_every_tenacity_resolution_global() {
        // CR3 area 4: the guard must isolate every input to effective_tenacity —
        // CLI override, persona layer, TENACITY_CONFIG, ACTIVE_FAMILY — and, since
        // #1998, the two round-cap globals as well. Exercise the exact
        // snapshot/restore the guard's Drop runs.
        use crate::test_guard::GlobalSettingsGuard;
        let _g = GlobalSettingsGuard::acquire(); // serialize + final cleanup

        // Known-empty baseline → snapshot it.
        clear_cli_tenacity();
        set_persona_tenacity(None);
        set_tenacity_config(TenacityConfig::default());
        set_active_model_family(None);
        set_session_tool_rounds(None);
        let snap = snapshot_runtime_state();

        // Mutate every axis.
        set_cli_tenacity(Tenacity::Relentless);
        set_persona_tenacity(Some(Tenacity::Insistent));
        set_tenacity_config(cfg(
            Some(Tenacity::Relaxed),
            &[("nemotron", Tenacity::Relentless)],
        ));
        set_active_model_family(Some("nemotron".to_string()));
        set_session_tool_rounds(Some(320));
        set_configured_tool_rounds(Some(40));
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
        assert_eq!(
            session_tool_rounds(),
            None,
            "the /rounds override restored — a leaked 320 would escalate the next test"
        );
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
