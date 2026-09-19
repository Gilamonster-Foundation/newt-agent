//! Psyche **posture macros** — named acts that set several psyche dials at once.
//!
//! Today there is one: **obsessive**, newt's answer to codex's "ultra" — the
//! max-effort posture (deepest [`Cognition`], [`Tenacity::Relentless`], crew
//! on). A posture is a *named act*, not a dial value: the operator asks for it by
//! name and it moves several orthogonal dials together. It leaves
//! [initiative](crate::initiative) alone: the deepest thinking and acting after
//! one round pull against each other (`docs/design/psyche-effort-dials.md`).
//!
//! This module is the single owner of *what obsessive means* for the two dials
//! it moves (cognition + tenacity), so the CLI `--obsessive` flag and the in-session
//! `/psyche obsessive` don't each hardcode the same three values. Crew is the odd
//! one out: it is a **startup gate** (`NEWT_TEAM`, read once when newt-cli builds
//! the crew runner), not a live dial, so it can't be engaged from here — the
//! caller applies it (the launch path sets `NEWT_TEAM` for full effect; the
//! in-session path defers it to the next launch).

use crate::cognition::{set_cli_cognition, CognitionOverride};
use crate::role_profile::Cognition;
use crate::tenacity::{set_cli_tenacity, Tenacity};

/// The obsessive posture's cognition: the deepest backend-specific reasoning level.
pub const OBSESSIVE_COGNITION: Cognition = Cognition::Meticulous;
/// The obsessive posture's tenacity: pursue with the round limit lifted.
pub const OBSESSIVE_TENACITY: Tenacity = Tenacity::Relentless;

/// Engage the obsessive posture's two **live** dials — install a cognition
/// session override at [`OBSESSIVE_COGNITION`] and a tenacity override at
/// [`OBSESSIVE_TENACITY`]. Initiative is deliberately NOT touched. Crew is NOT
/// touched here either (it is a `NEWT_TEAM` startup gate); the caller engages
/// it — at launch for full effect, or deferred with a note in-session. Returns
/// the pair it set, for the caller's confirmation line.
pub fn engage_obsessive_dials() -> (Cognition, Tenacity) {
    set_cli_cognition(CognitionOverride::Set(OBSESSIVE_COGNITION));
    set_cli_tenacity(OBSESSIVE_TENACITY);
    (OBSESSIVE_COGNITION, OBSESSIVE_TENACITY)
}

// ---------------------------------------------------------------------------
// Per-turn capture (#1669)
// ---------------------------------------------------------------------------

/// All three live dials, pinned for one turn on one thread.
///
/// Holding this is what makes a turn's psyche *immutable for its duration*.
/// Drop it and the thread resolves from the process dials again.
#[must_use]
pub struct TurnPsyche {
    _verification: crate::agentic::self_verify::ScopedVerificationSettings,
    _cognition: crate::cognition::ScopedEffectiveCognition,
    _tenacity: crate::tenacity::ScopedEffectiveTenacity,
    _initiative: crate::initiative::ScopedEffectiveInitiative,
}

/// Resolve all three live dials NOW and pin them for the rest of this turn, on
/// this thread.
///
/// Two sessions can run at once, and the dials they resolve from
/// (`CLI_COGNITION`, `PERSONA_COGNITION`, `CLI_TENACITY`, `CLI_INITIATIVE`,
/// `PERSONA_INITIATIVE`, `INITIATIVE_CONFIG`, `ACTIVE_FAMILY`) are all
/// process-global. Without a
/// capture, a `/cognition` typed in tab B — or a persona activating there —
/// would change what tab A's already-running turn resolves on its *next*
/// round, so one turn could straddle two postures and no evidence would say
/// which one produced which request.
///
/// Capture at the turn boundary and the answer is fixed for the whole turn.
/// Operator changes still take effect — on that session's next turn, which is
/// where the operator expects them.
///
/// This composite exists so the dials cannot be captured *separately*: a turn
/// pinned for cognition but not initiative is a bug with no symptom until two
/// sessions overlap (initiative is read every round, by the action nudge). It
/// is the only intended entry point; the parts are public for tests and for
/// callers that genuinely need one.
pub fn capture_turn_psyche() -> TurnPsyche {
    // Resolve through the SAME accessors every other reader uses, so a capture
    // can never disagree with what an uncaptured read would have returned at
    // this instant.
    let cognition = crate::cognition::effective_cognition();
    let tenacity = crate::tenacity::effective_tenacity();
    let initiative = crate::initiative::effective_initiative();
    TurnPsyche {
        _verification: crate::agentic::self_verify::scoped_verification_settings(
            crate::agentic::self_verify::VerificationSettings::capture(),
        ),
        _cognition: crate::cognition::scoped_effective_cognition(cognition),
        _tenacity: crate::tenacity::scoped_effective_tenacity(tenacity),
        _initiative: crate::initiative::scoped_effective_initiative(initiative),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cognition::{cli_cognition, set_cli_cognition, CognitionOverride};
    use crate::initiative::{effective_initiative, set_cli_initiative, Initiative};
    use crate::tenacity::{effective_tenacity, set_cli_tenacity, Tenacity};

    #[test]
    fn obsessive_sets_max_cognition_and_relentless_tenacity() {
        let _g = crate::test_guard::GlobalSettingsGuard::acquire();
        // Start from a non-obsessive state so the assertions mean something.
        set_cli_cognition(CognitionOverride::Unset);
        set_cli_tenacity(Tenacity::Normal);

        let (cog, ten) = engage_obsessive_dials();
        assert_eq!(cog, Cognition::Meticulous);
        assert_eq!(ten, Tenacity::Relentless);
        // The overrides are actually installed (not just returned).
        assert_eq!(
            cli_cognition(),
            CognitionOverride::Set(Cognition::Meticulous)
        );
        assert_eq!(effective_tenacity(), Tenacity::Relentless);
    }

    /// Regression (slice 1b, a deliberate behaviour change): obsessive leaves
    /// initiative where it was. Before the split its relentless tenacity also
    /// nudged after a single read-only round and forced an edit on plan exit.
    #[test]
    fn obsessive_leaves_initiative_alone() {
        let _g = crate::test_guard::GlobalSettingsGuard::acquire();
        crate::initiative::clear_cli_initiative();
        crate::initiative::set_persona_initiative(None);
        crate::initiative::set_initiative_config(crate::initiative::InitiativeConfig::default());
        crate::initiative::set_active_model_family(None);
        let _ = engage_obsessive_dials();
        assert_eq!(crate::initiative::cli_initiative(), None);
        assert_eq!(effective_initiative(), Initiative::Measured);
        assert_eq!(effective_initiative().read_only_nudge_after(), 3);

        set_cli_initiative(Initiative::Patient);
        let _ = engage_obsessive_dials();
        assert_eq!(effective_initiative(), Initiative::Patient, "held value");
    }

    // ── #1669: per-turn capture ────────────────────────────────────────────

    /// THE property: a captured turn is immune to a dial changed after it
    /// started — which is what lets two sessions run at once without one
    /// operator's `/cognition` rewriting the other's in-flight turn.
    ///
    /// Non-vacuous by construction: the same mutation is applied twice, once
    /// with a capture held and once without, and the two must disagree. If
    /// `capture_turn_psyche` did nothing, both halves would observe the new
    /// value and the assertions would collide.
    #[test]
    fn a_captured_turn_does_not_see_a_dial_changed_after_it_started() {
        let _g = crate::test_guard::GlobalSettingsGuard::acquire();
        set_cli_cognition(CognitionOverride::Set(Cognition::Rational));
        set_cli_tenacity(Tenacity::Normal);
        set_cli_initiative(Initiative::Patient);

        {
            let _turn = capture_turn_psyche();
            // The operator moves every dial mid-turn.
            set_cli_cognition(CognitionOverride::Set(Cognition::Meticulous));
            set_cli_tenacity(Tenacity::Relentless);
            set_cli_initiative(Initiative::Eager);

            assert_eq!(
                crate::cognition::effective_cognition(),
                Some(Cognition::Rational),
                "the running turn keeps the cognition it started with"
            );
            assert_eq!(
                effective_tenacity(),
                Tenacity::Normal,
                "and the tenacity it started with"
            );
            assert_eq!(
                effective_initiative(),
                Initiative::Patient,
                "and the initiative, which the per-round nudge reads (slice 1b)"
            );
        }

        // Control: with no capture, the very same mutation IS visible — so the
        // assertions above are measuring the capture, not a frozen global.
        assert_eq!(
            crate::cognition::effective_cognition(),
            Some(Cognition::Meticulous)
        );
        assert_eq!(effective_tenacity(), Tenacity::Relentless);
        assert_eq!(effective_initiative(), Initiative::Eager);
    }

    /// A capture is per-THREAD: one session's pinned turn must not pin another
    /// session's. This is the property that makes concurrent turns honest.
    #[test]
    fn a_capture_on_one_thread_does_not_pin_another() {
        let _g = crate::test_guard::GlobalSettingsGuard::acquire();
        set_cli_initiative(Initiative::Patient);
        let _turn = capture_turn_psyche();
        set_cli_initiative(Initiative::Eager);

        assert_eq!(effective_initiative(), Initiative::Patient, "pinned here");
        let elsewhere = std::thread::spawn(effective_initiative)
            .join()
            .expect("probe thread");
        assert_eq!(
            elsewhere,
            Initiative::Eager,
            "an unpinned thread resolves live — the capture did not leak"
        );
    }

    /// `/cognition off` means "no reasoning.effort field", and that is a real
    /// captured value — not the absence of a capture. Collapsing the two would
    /// silently fall through to the process dial.
    #[test]
    fn capturing_cognition_off_pins_off_rather_than_falling_through() {
        let _g = crate::test_guard::GlobalSettingsGuard::acquire();
        set_cli_cognition(CognitionOverride::Off);
        {
            let _turn = capture_turn_psyche();
            set_cli_cognition(CognitionOverride::Set(Cognition::Meticulous));
            assert_eq!(
                crate::cognition::effective_cognition(),
                None,
                "an explicitly-off turn stays off"
            );
        }
    }

    /// Captures restore in LIFO order and leave the thread clean.
    #[test]
    fn captures_nest_and_restore() {
        let _g = crate::test_guard::GlobalSettingsGuard::acquire();
        set_cli_tenacity(Tenacity::Normal);
        {
            let _outer = capture_turn_psyche();
            set_cli_tenacity(Tenacity::Relentless);
            {
                // A nested capture resolves through `effective_tenacity`, which
                // already honours the outer pin — so it inherits the turn's
                // value rather than reaching past it to the mutated global.
                // That is the point: a capture is not a window back to the
                // process dials.
                let _inner = capture_turn_psyche();
                assert_eq!(
                    effective_tenacity(),
                    Tenacity::Normal,
                    "an inner capture inherits the pinned value, not the global"
                );
            }
            assert_eq!(
                effective_tenacity(),
                Tenacity::Normal,
                "dropping the inner capture restores the outer one, not the global"
            );
        }
        assert_eq!(
            effective_tenacity(),
            Tenacity::Relentless,
            "thread is clean"
        );
    }
}
