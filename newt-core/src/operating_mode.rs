//! **Operating modes (`/mode`) — working style, never authority.**
//!
//! Moved down from `newt-tui` (#2009 PR4b). Two reasons, both from the ADR:
//!
//! - **Shared functionality moves down into the minimal layer.** This is
//!   config-and-capability vocabulary — the keywords an operator types, the
//!   instruction text a prompt carries — and `agent_line_architecture.md`
//!   says contracts survive a rewrite while implementations do not. A headless
//!   wyvern worker dispatched an `admin` or `plan` caveat needs to mean the
//!   same thing newt means; it cannot depend on newt's TUI crate to find out.
//! - **A receipt writer cannot read a local** (§5). `/mode` is absorbed into
//!   `/settings` in this slice, and `settings_form::apply` is a pure function
//!   with no view into `run_chat`. The value has to live where both doors can
//!   reach it, and the type has to live below both of them.
//!
//! What did NOT move: `AutoModeState` / `PlanModeState`. Those are
//! conversation-scoped and lent to the loop through traits; their clearing is
//! deliberate per-boundary work that `newt-tui` owns. See
//! `session_operating_mode` for how the two halves stay consistent.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OperatingMode {
    #[default]
    Chat,
    Dev,
    Admin,
    Plan,
    Diagnose,
    Auto,
    FullAuto,
}

impl OperatingMode {
    #[must_use]
    pub fn from_keyword(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "chat" => Some(Self::Chat),
            "dev" | "developer" => Some(Self::Dev),
            "admin" | "sysadmin" => Some(Self::Admin),
            "plan" => Some(Self::Plan),
            "diagnose" | "diagnostic" => Some(Self::Diagnose),
            "auto" => Some(Self::Auto),
            "full-auto" | "full_auto" | "fullauto" => Some(Self::FullAuto),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Dev => "dev",
            Self::Admin => "admin",
            Self::Plan => "plan",
            Self::Diagnose => "diagnose",
            Self::Auto => "auto",
            Self::FullAuto => "full-auto",
        }
    }

    #[must_use]
    pub fn description(self) -> &'static str {
        match self {
            Self::Chat => {
                "Collaborate conversationally; answer directly and confirm consequential choices."
            }
            Self::Dev => {
                "Develop with TDD, worktree-safe Git habits, targeted tests, and full preflight before a PR."
            }
            Self::Admin => {
                "Do no harm, make minimal changes, respect privacy, and use elevated power responsibly."
            }
            Self::Plan => {
                "Write an actionable plan without changing files, running mutations, or altering external state."
            }
            Self::Diagnose => {
                "Gather evidence and identify root cause only; stop before planning or implementing a repair."
            }
            Self::Auto => {
                "Let the model choose a bounded working style per task and ask when a consequential decision is unresolved."
            }
            Self::FullAuto => {
                "Work safely to completion with minimal interruption, including tests and preflight."
            }
        }
    }

    #[must_use]
    pub fn instructions(self) -> &'static str {
        match self {
            Self::Chat => {
                "Collaborate with the human at a conversational pace. Answer questions directly. \
                 When action is requested, stay within the request and ask before making an \
                 unresolved consequential choice."
            }
            Self::Dev => {
                "Act as a disciplined developer. Inspect branch, worktree, and existing changes \
                 before editing; preserve unrelated work. Use TDD when feasible: establish the \
                 failing behavior, make the smallest coherent change, run targeted tests, then \
                 run the workspace's full preflight before proposing or pushing a PR. Ask the \
                 human when a product or architecture decision remains unresolved."
            }
            Self::Admin => {
                "Do no harm. Make minimal changes. Respect privacy. With great power comes great \
                 responsibility. Inspect first, protect secrets and user data, prefer reversible \
                 operations, and require a clear human decision before destructive or \
                 irreversible work."
            }
            Self::Plan => {
                "Analyze the request and write a concrete, sequenced plan. Do not modify files, \
                 run mutating commands, or alter external state. Surface unresolved decisions \
                 for the human. When the plan is ready, recommend /mode dev to implement it, or \
                 /mode admin for system administration."
            }
            Self::Diagnose => {
                "Seek only to understand. Inspect available read-only evidence and identify the \
                 root cause; do not plan, mutate the workspace, or implement the repair. Once the \
                 root cause is known, say: \"I have found the root cause. Would you like to \
                 switch to /mode plan to plan a fix?\""
            }
            Self::Auto => {
                "Use the effective style for this turn and adapt within its boundaries. For later \
                 action-shaped turns, select_operating_mode may choose chat, dev, admin, plan, or \
                 diagnose; it never selects full-auto. Protected ask, research, explanation, and \
                 plan intake still win. Ask the human whenever a consequential decision, \
                 tradeoff, or missing requirement is unresolved."
            }
            Self::FullAuto => {
                "Carry safe in-scope work through implementation, verification, and full \
                 preflight with minimal interruption. Inspect branch, worktree, and existing \
                 changes before editing; preserve unrelated work. Use TDD when feasible: \
                 establish the failing behavior, make the smallest coherent change, run targeted \
                 tests, then run the workspace's full preflight before proposing or pushing a \
                 PR. Make conservative reversible assumptions and iterate to completion. Ask \
                 only when blocked by required authority, a secret, destructive or irreversible \
                 action, or a consequential human choice."
            }
        }
    }

    /// Whether a MODEL may select this style behind `/mode auto`.
    ///
    /// `auto` is not a style (selecting it would be a loop) and `full-auto` is
    /// human-only: it authorises working to completion with minimal
    /// interruption, which is a decision an operator makes, not one the model
    /// makes for itself.
    #[must_use]
    pub fn is_model_selectable(self) -> bool {
        !matches!(self, Self::Auto | Self::FullAuto)
    }

    /// The styles a model may select, in menu order.
    #[must_use]
    pub fn model_selectable() -> Vec<&'static str> {
        Self::all()
            .iter()
            .filter(|m| m.is_model_selectable())
            .map(|m| m.as_str())
            .collect()
    }

    #[must_use]
    pub fn all() -> &'static [Self] {
        &[
            Self::Chat,
            Self::Dev,
            Self::Admin,
            Self::Plan,
            Self::Diagnose,
            Self::Auto,
            Self::FullAuto,
        ]
    }
}

/// The operating mode this session is in: the operator's session selection,
/// else the default (`chat`).
///
/// # Why a process-global and not a `run_chat` local (#2009 PR4b)
///
/// It was a local. `settings_form::apply` is a pure function reached from a
/// form, a deep link and a shim, none of which can see into the session loop —
/// which is §5's precondition verbatim: **a receipt writer cannot read a
/// local.** Same move `/markdown` made in PR4a, and deliberately the same
/// shape, under the same #1850 lock.
///
/// **This owns the VALUE, not the consequences.** Choosing a mode explicitly
/// also supersedes any stale model-selected style, and those states are
/// conversation-scoped and owned by `newt-tui`. The session loop clears them
/// when it observes this value change, so there is one writer for the value
/// and one named place where its consequence happens.
#[must_use]
pub fn session_operating_mode() -> OperatingMode {
    std::env::var("NEWT_MODE")
        .ok()
        .as_deref()
        .and_then(OperatingMode::from_keyword)
        .unwrap_or_default()
}

/// Install the operator's session mode. The one writer; both `/mode` and
/// `/settings mode` go through it.
pub fn set_session_operating_mode(mode: OperatingMode) {
    crate::process_env::set_var("NEWT_MODE", mode.as_str());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every keyword round-trips through the pair an operator actually uses.
    #[test]
    fn the_keywords_and_the_tokens_are_one_vocabulary() {
        for mode in OperatingMode::all() {
            assert_eq!(
                OperatingMode::from_keyword(mode.as_str()),
                Some(*mode),
                "`{}` does not parse back to itself",
                mode.as_str()
            );
            assert!(!mode.description().is_empty());
            assert!(!mode.instructions().is_empty());
        }
        assert_eq!(OperatingMode::from_keyword("nonsense"), None);
    }

    /// The aliases the verb accepted, kept by the move.
    #[test]
    fn the_verbs_aliases_survived_the_move_to_core() {
        for (alias, want) in [
            ("developer", OperatingMode::Dev),
            ("sysadmin", OperatingMode::Admin),
            ("diagnostic", OperatingMode::Diagnose),
            ("full_auto", OperatingMode::FullAuto),
            ("fullauto", OperatingMode::FullAuto),
            ("FULL-AUTO", OperatingMode::FullAuto),
        ] {
            assert_eq!(OperatingMode::from_keyword(alias), Some(want), "{alias}");
        }
    }

    #[test]
    fn the_session_value_defaults_to_chat_and_round_trips() {
        let _guard = crate::test_guard::GlobalSettingsGuard::acquire();
        crate::process_env::remove_var("NEWT_MODE");
        assert_eq!(session_operating_mode(), OperatingMode::Chat);

        set_session_operating_mode(OperatingMode::Plan);
        assert_eq!(session_operating_mode(), OperatingMode::Plan);

        // An unparseable value is the default, not a panic: the variable is
        // process-global and something else may have written it.
        crate::process_env::set_var("NEWT_MODE", "nonsense");
        assert_eq!(session_operating_mode(), OperatingMode::Chat);
    }
}
