//! Psyche **posture macros** — named acts that set several psyche dials at once.
//!
//! Today there is one: **obsessive**, newt's answer to codex's "ultra" — the
//! max-everything posture (deepest [`Cognition`], most forcing [`Tenacity`], crew
//! on). A posture is a *named act*, not a dial value: the operator asks for it by
//! name and it moves several orthogonal dials together.
//!
//! This module is the single owner of *what obsessive means* for the two LIVE
//! dials (cognition + tenacity), so the CLI `--obsessive` flag and the in-session
//! `/psyche obsessive` don't each hardcode the same three values. Crew is the odd
//! one out: it is a **startup gate** (`NEWT_TEAM`, read once when newt-cli builds
//! the crew runner), not a live dial, so it can't be engaged from here — the
//! caller applies it (the launch path sets `NEWT_TEAM` for full effect; the
//! in-session path defers it to the next launch).

use crate::cognition::{set_cli_cognition, CognitionOverride};
use crate::role_profile::Cognition;
use crate::tenacity::{set_cli_tenacity, Tenacity};

/// The obsessive posture's cognition: the deepest backend-specific reasoning level.
pub const OBSESSIVE_COGNITION: Cognition = Cognition::Contemplating;
/// The obsessive posture's tenacity: the most forcing level.
pub const OBSESSIVE_TENACITY: Tenacity = Tenacity::Relentless;

/// Engage the obsessive posture's two **live** dials — install a cognition
/// session override at [`OBSESSIVE_COGNITION`] and a tenacity override at
/// [`OBSESSIVE_TENACITY`]. Crew is NOT touched here (it is a `NEWT_TEAM` startup
/// gate); the caller engages it — at launch for full effect, or deferred with a
/// note in-session. Returns the pair it set, for the caller's confirmation line.
pub fn engage_obsessive_dials() -> (Cognition, Tenacity) {
    set_cli_cognition(CognitionOverride::Set(OBSESSIVE_COGNITION));
    set_cli_tenacity(OBSESSIVE_TENACITY);
    (OBSESSIVE_COGNITION, OBSESSIVE_TENACITY)
}

// ---------------------------------------------------------------------------
// Per-turn capture (#1669)
// ---------------------------------------------------------------------------

/// Both live dials, pinned for one turn on one thread.
///
/// Holding this is what makes a turn's psyche *immutable for its duration*.
/// Drop it and the thread resolves from the process dials again.
#[must_use]
pub struct TurnPsyche {
    _cognition: crate::cognition::ScopedEffectiveCognition,
    _tenacity: crate::tenacity::ScopedEffectiveTenacity,
}

/// Resolve both live dials NOW and pin them for the rest of this turn, on this
/// thread.
///
/// Two sessions can run at once, and the dials they resolve from
/// (`CLI_COGNITION`, `PERSONA_COGNITION`, `CLI_TENACITY`, `PERSONA_TENACITY`,
/// `TENACITY_CONFIG`, `ACTIVE_FAMILY`) are all process-global. Without a
/// capture, a `/cognition` typed in tab B — or a persona activating there —
/// would change what tab A's already-running turn resolves on its *next*
/// round, so one turn could straddle two postures and no evidence would say
/// which one produced which request.
///
/// Capture at the turn boundary and the answer is fixed for the whole turn.
/// Operator changes still take effect — on that session's next turn, which is
/// where the operator expects them.
///
/// This composite exists so the two dials cannot be captured *separately*: a
/// turn pinned for cognition but not tenacity is a bug with no symptom until
/// two sessions overlap. It is the only intended entry point; the two halves
/// are public for tests and for callers that genuinely need one.
pub fn capture_turn_psyche() -> TurnPsyche {
    // Resolve through the SAME accessors every other reader uses, so a capture
    // can never disagree with what an uncaptured read would have returned at
    // this instant.
    let cognition = crate::cognition::effective_cognition();
    let tenacity = crate::tenacity::effective_tenacity();
    TurnPsyche {
        _cognition: crate::cognition::scoped_effective_cognition(cognition),
        _tenacity: crate::tenacity::scoped_effective_tenacity(tenacity),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cognition::{cli_cognition, set_cli_cognition, CognitionOverride};
    use crate::tenacity::{effective_tenacity, set_cli_tenacity, Tenacity};

    #[test]
    fn obsessive_sets_max_cognition_and_max_tenacity() {
        let _g = crate::test_guard::GlobalSettingsGuard::acquire();
        // Start from a non-obsessive state so the assertions mean something.
        set_cli_cognition(CognitionOverride::Unset);
        set_cli_tenacity(Tenacity::Standard);

        let (cog, ten) = engage_obsessive_dials();
        assert_eq!(cog, Cognition::Contemplating);
        assert_eq!(ten, Tenacity::Relentless);
        // The overrides are actually installed (not just returned).
        assert_eq!(
            cli_cognition(),
            CognitionOverride::Set(Cognition::Contemplating)
        );
        assert_eq!(effective_tenacity(), Tenacity::Relentless);

        // Restore so the process-globals don't leak into sibling tests.
        set_cli_cognition(CognitionOverride::Unset);
        set_cli_tenacity(Tenacity::Standard);
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
        set_cli_cognition(CognitionOverride::Set(Cognition::Pondering));
        set_cli_tenacity(Tenacity::Relaxed);

        {
            let _turn = capture_turn_psyche();
            // The operator moves both dials mid-turn.
            set_cli_cognition(CognitionOverride::Set(Cognition::Contemplating));
            set_cli_tenacity(Tenacity::Relentless);

            assert_eq!(
                crate::cognition::effective_cognition(),
                Some(Cognition::Pondering),
                "the running turn keeps the cognition it started with"
            );
            assert_eq!(
                effective_tenacity(),
                Tenacity::Relaxed,
                "and the tenacity it started with"
            );
        }

        // Control: with no capture, the very same mutation IS visible — so the
        // assertions above are measuring the capture, not a frozen global.
        assert_eq!(
            crate::cognition::effective_cognition(),
            Some(Cognition::Contemplating)
        );
        assert_eq!(effective_tenacity(), Tenacity::Relentless);

        set_cli_cognition(CognitionOverride::Unset);
        set_cli_tenacity(Tenacity::Standard);
    }

    /// A capture is per-THREAD: one session's pinned turn must not pin another
    /// session's. This is the property that makes concurrent turns honest.
    #[test]
    fn a_capture_on_one_thread_does_not_pin_another() {
        let _g = crate::test_guard::GlobalSettingsGuard::acquire();
        set_cli_tenacity(Tenacity::Relaxed);
        let _turn = capture_turn_psyche();
        set_cli_tenacity(Tenacity::Relentless);

        assert_eq!(effective_tenacity(), Tenacity::Relaxed, "pinned here");
        let elsewhere = std::thread::spawn(effective_tenacity)
            .join()
            .expect("probe thread");
        assert_eq!(
            elsewhere,
            Tenacity::Relentless,
            "an unpinned thread resolves live — the capture did not leak"
        );

        set_cli_tenacity(Tenacity::Standard);
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
            set_cli_cognition(CognitionOverride::Set(Cognition::Contemplating));
            assert_eq!(
                crate::cognition::effective_cognition(),
                None,
                "an explicitly-off turn stays off"
            );
        }
        set_cli_cognition(CognitionOverride::Unset);
    }

    /// Captures restore in LIFO order and leave the thread clean.
    #[test]
    fn captures_nest_and_restore() {
        let _g = crate::test_guard::GlobalSettingsGuard::acquire();
        set_cli_tenacity(Tenacity::Relaxed);
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
                    Tenacity::Relaxed,
                    "an inner capture inherits the pinned value, not the global"
                );
            }
            assert_eq!(
                effective_tenacity(),
                Tenacity::Relaxed,
                "dropping the inner capture restores the outer one, not the global"
            );
        }
        assert_eq!(
            effective_tenacity(),
            Tenacity::Relentless,
            "thread is clean"
        );
        set_cli_tenacity(Tenacity::Standard);
    }
}
