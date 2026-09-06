//! The cockpit's Esc / Ctrl-C precedence ladder (#2005 rung 7).
//!
//! Two halves live here, and both are the halves the `precedence-ladder` crate
//! deliberately refuses to own:
//!
//! - [`ESC_LADDER`] — the shipped table, read from `assets/esc_ladder.toml`.
//!   The knowledge is data; this module only loads it.
//! - [`trigger_name`] — the crossterm `KeyEvent` → trigger-name mapping. The
//!   crate takes an opaque string precisely so that no terminal type appears in
//!   its signatures (the classic watcher reads raw bytes off fd 0 and never
//!   sees a `KeyEvent` at all), so naming a key is the consumer's job.
//!
//! What the ladder answers is one bit in the cockpit: escape, or pass to the
//! editor. It does not dispatch — the editor still re-derives its own winner
//! through `Editor::input`. The claimant *names* earn their place through the
//! conformance test in `rich_input_tests/esc_ladder.rs` and, later, through
//! `Ladder::describe` driving the mode hint; do not read `Verdict::Claimed`'s
//! name as a routing decision.

use std::sync::LazyLock;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use precedence_ladder::Ladder;

/// The shipped Esc ladder.
///
/// `expect` is honest here rather than a hidden panic risk: the table is
/// `include_str!`d, so it is fixed at compile time and this module's
/// `the_shipped_table_parses_and_has_no_dead_rows` test proves this exact
/// string parses on every `cargo test`. There is no input path an operator can take
/// that reaches a different table.
pub(crate) static ESC_LADDER: LazyLock<Ladder> = LazyLock::new(|| {
    Ladder::from_toml(include_str!("../assets/esc_ladder.toml"))
        .expect("assets/esc_ladder.toml is compiled in and is checked by this module's tests")
});

/// What this key press is called in the ladder's table, or `None` for a key the
/// table does not mention.
///
/// Deliberately two arms and no configuration. Chord *naming* — modifiers,
/// aliases, normalization — is a different question from precedence, and herdr
/// already owns 2,300 lines of it; the first commit that teaches this function
/// to parse modifier syntax has forked that. Two triggers is what the table
/// has, so two arms is what this has.
pub(crate) fn trigger_name(key: &KeyEvent) -> Option<&'static str> {
    match key.code {
        KeyCode::Esc => Some("esc"),
        // Matches the predicate the presenter's Ctrl-C arm carried before the
        // ladder replaced it, character for character.
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Some("ctrl-c"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use precedence_ladder::{ClaimSet, Situation, Verdict};

    fn situation<'a>(claiming: &'a ClaimSet, work_running: bool) -> Situation<'a> {
        Situation {
            claiming,
            work_running,
        }
    }

    #[test]
    fn the_shipped_table_parses_and_has_no_dead_rows() {
        // Forces the `LazyLock`, so a malformed table fails here rather than
        // at an operator's first keystroke.
        let rungs: Vec<&str> = ESC_LADDER.claimants().collect();
        assert_eq!(
            rungs,
            vec!["palette", "vi-confirm", "vi-ex", "vi-insert", "vi-pending"],
            "the table's ORDER is the contract (esc_and_vi_contract.md §4); \
             reordering these rows is a behaviour change, not a cleanup"
        );
        assert_eq!(
            ESC_LADDER.collisions(),
            vec![],
            "a rung that can never fire is an authoring mistake, and the \
             author is the one who should learn about it"
        );
        assert_eq!(
            ESC_LADDER.hatch().reserved().collect::<Vec<_>>(),
            ["ctrl-c"]
        );
        assert_eq!(ESC_LADDER.fallthrough().collect::<Vec<_>>(), ["esc"]);
    }

    /// The eight rungs of the ADR, resolved against the shipped table.
    ///
    /// This is the whole contract in one table: what Esc does in each context,
    /// and the anti-vacuous twin for each — Ctrl-C, which must escape from
    /// EVERY one of them, or a claimant could strand the operator.
    #[test]
    fn esc_reaches_the_hatch_only_when_every_claimant_declined() {
        let interrupt = Verdict::Escape {
            action: "interrupt",
        };
        for (claiming, esc_while_working) in [
            (
                vec![],
                Verdict::Escape {
                    action: "interrupt",
                },
            ),
            (
                vec!["palette"],
                Verdict::Claimed {
                    claimant: "palette",
                    action: "close palette",
                },
            ),
            (
                vec!["vi-confirm"],
                Verdict::Claimed {
                    claimant: "vi-confirm",
                    action: "cancel [y/N]",
                },
            ),
            (
                vec!["vi-ex"],
                Verdict::Claimed {
                    claimant: "vi-ex",
                    action: "cancel :",
                },
            ),
            (
                vec!["vi-insert"],
                Verdict::Claimed {
                    claimant: "vi-insert",
                    action: "NORMAL",
                },
            ),
            (
                vec!["vi-pending"],
                Verdict::Claimed {
                    claimant: "vi-pending",
                    action: "cancel operator",
                },
            ),
            // The one that matters most: a pending operator AND a running
            // turn. Codex kills the turn here; rung 6 outranks rung 7, so
            // newt cancels the operator and keeps working.
            (
                vec!["vi-pending", "vi-insert"],
                Verdict::Claimed {
                    claimant: "vi-insert",
                    action: "NORMAL",
                },
            ),
        ] {
            let set: ClaimSet = claiming.iter().copied().collect();
            assert_eq!(
                ESC_LADDER.resolve("esc", &situation(&set, true)),
                esc_while_working,
                "esc, turn running, claiming {claiming:?}"
            );
            // ANTI-VACUOUS TWIN #1: Ctrl-C is reserved, so it escapes from
            // every one of those states. Without this the table above would
            // pass just as well if rungs 2..6 had swallowed the interrupt
            // outright — which is precisely the "vi is the one surface the
            // interrupt cannot reach" defect this PR exists to end.
            assert_eq!(
                ESC_LADDER.resolve("ctrl-c", &situation(&set, true)),
                interrupt,
                "ctrl-c must escape while claiming {claiming:?}"
            );
            // ANTI-VACUOUS TWIN #2: idle, nothing escapes. Esc with no
            // claimant is `Unbound` (the editor's no-op), and Ctrl-C is
            // `Unbound` too so the draft-clear keeps owning it. A ladder that
            // ignored `work_running` would pass the first assertion and fail
            // both of these.
            assert_eq!(
                ESC_LADDER.resolve("ctrl-c", &situation(&set, false)),
                Verdict::Unbound,
                "idle ctrl-c is the editor's draft-clear, not an escape"
            );
        }
        let none = ClaimSet::default();
        assert_eq!(
            ESC_LADDER.resolve("esc", &situation(&none, false)),
            Verdict::Unbound,
            "idle Esc in vi NORMAL stays the harmless no-op vim defines"
        );
    }

    #[test]
    fn only_esc_and_ctrl_c_are_named() {
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(trigger_name(&esc), Some("esc"));
        assert_eq!(trigger_name(&ctrl_c), Some("ctrl-c"));
        // A bare `c` is a character the editor must receive. If this ever
        // returns `Some`, typing "cancel" mid-turn kills the turn.
        assert_eq!(
            trigger_name(&KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            trigger_name(&KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            None,
            "Ctrl-D is Eof / half-page-down, not a hatch trigger (ADR §8 Q4)"
        );
        assert_eq!(
            trigger_name(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            None
        );
    }

    /// Every trigger the table mentions must be nameable, and every name the
    /// mapping produces must be in the table. Without the second half a typo
    /// in `trigger_name` is a silent no-op; without the first, a trigger added
    /// to the table can never arrive.
    #[test]
    fn the_table_and_the_key_naming_agree() {
        let named = ["esc", "ctrl-c"];
        let mut in_table: Vec<&str> = ESC_LADDER.hatch().reserved().collect();
        in_table.extend(ESC_LADDER.fallthrough());
        for rung in ESC_LADDER.rungs() {
            in_table.extend(rung.triggers.iter().map(String::as_str));
        }
        in_table.sort_unstable();
        in_table.dedup();
        for trigger in &in_table {
            assert!(
                named.contains(trigger),
                "`{trigger}` is in the table but `trigger_name` never produces it"
            );
        }
        for name in named {
            assert!(
                in_table.contains(&name),
                "`trigger_name` produces `{name}`, which no row mentions"
            );
        }
    }
}
