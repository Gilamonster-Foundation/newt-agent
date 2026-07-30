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

/// The obsessive posture's cognition: the deepest level (→ `reasoning.effort=high`).
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cognition::{cli_cognition, set_cli_cognition, CognitionOverride};
    use crate::tenacity::{effective_tenacity, set_cli_tenacity, Tenacity};

    #[test]
    fn obsessive_sets_max_cognition_and_max_tenacity() {
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
}
