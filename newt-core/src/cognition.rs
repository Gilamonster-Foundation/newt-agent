//! The cognition **session dial** — the operator's live `/cognition` override,
//! layered over the active persona's `cognition:` front-matter.
//!
//! This is the cognition analogue of [`crate::tenacity`]'s override, kept as a
//! separate, deliberately tiny module because the two psyche dials act in
//! different places: **tenacity steers the harness LOOP** (nudge timing, read at
//! decision points via [`crate::tenacity::effective_tenacity`]), while
//! **cognition rides the wire REQUEST** (projected to `reasoning.effort` at the
//! Responses `build_body` via `agentic::responses_reasoning_field`). So cognition
//! resolves to a value carried on `ChatCtx.cognition`, not read from a global at
//! the loop — this module owns only the *resolution* (override vs persona), and
//! the wire projection stays a single owner on the request path.

use crate::role_profile::Cognition;
use std::sync::Mutex;

/// A session `/cognition` override, layered over the persona's `cognition:`.
///
/// Three states because, unlike tenacity, cognition can be genuinely *absent*
/// (no `reasoning.effort` field at all): the operator must be able to force it
/// off even when a persona sets a level, and to step back to following the
/// persona.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CognitionOverride {
    /// No operator override — the active persona's cognition (if any) applies.
    #[default]
    Unset,
    /// Force cognition OFF for the session: emit no `reasoning.effort`, even if
    /// the persona sets a level.
    Off,
    /// Force a specific level, overriding the persona.
    Set(Cognition),
}

// The operator dial can't be threaded through every ChatCtx construction site,
// so — exactly like `tenacity`'s `CLI_TENACITY` — it is stashed process-global
// and combined lazily by [`effective_cognition`] with the persona layer below.
static CLI_COGNITION: Mutex<CognitionOverride> = Mutex::new(CognitionOverride::Unset);
// The active persona's declared `cognition:` — the layer BELOW the `/cognition`
// override, set when a persona activates (symmetric with `tenacity`'s
// `PERSONA_TENACITY`). `None` when no persona / the persona declares none. Having
// it as a global means status surfaces (`/psyche`, the config panel) can report
// the EFFECTIVE cognition without threading the persona to every call site.
static PERSONA_COGNITION: Mutex<Option<Cognition>> = Mutex::new(None);

/// Install the session `/cognition` override (highest priority). Set by the
/// `/cognition` command; call is order-free w.r.t. persona selection.
pub fn set_cli_cognition(o: CognitionOverride) {
    if let Ok(mut slot) = CLI_COGNITION.lock() {
        *slot = o;
    }
}

/// The current session override (for the `/cognition` status view). `Unset` when
/// the operator has not set one — resolution then follows the persona.
#[must_use]
pub fn cli_cognition() -> CognitionOverride {
    CLI_COGNITION.lock().ok().map(|s| *s).unwrap_or_default()
}

/// Install the active persona's declared `cognition:` (call on persona activation
/// / clear, alongside `tenacity::set_persona_tenacity`). `None` clears it.
pub fn set_persona_cognition(level: Option<Cognition>) {
    if let Ok(mut slot) = PERSONA_COGNITION.lock() {
        *slot = level;
    }
}

/// The active persona's declared cognition, if any (for status rendering).
#[must_use]
pub fn persona_cognition() -> Option<Cognition> {
    PERSONA_COGNITION.lock().ok().and_then(|s| *s)
}

/// The cognition in EFFECT, most-specific first: the `/cognition` override wins
/// (`Off` forces no field, `Set` forces a level); else the active persona's
/// declared cognition; else `None` (no `reasoning.effort`). This is the single
/// definition of the precedence — read by the loop, `/psyche`, and the panel.
#[must_use]
pub fn effective_cognition() -> Option<Cognition> {
    match cli_cognition() {
        CognitionOverride::Unset => persona_cognition(),
        CognitionOverride::Off => None,
        CognitionOverride::Set(c) => Some(c),
    }
}

/// The cognition in effect given an EXPLICIT persona level (rather than the
/// process-global). Retained for headless / eval callers that pass the persona
/// directly; the interactive path uses [`effective_cognition`].
#[must_use]
pub fn resolve_cognition(persona: Option<Cognition>) -> Option<Cognition> {
    match cli_cognition() {
        CognitionOverride::Unset => persona,
        CognitionOverride::Off => None,
        CognitionOverride::Set(c) => Some(c),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_guard::GlobalSettingsGuard;

    #[test]
    fn unset_override_follows_the_persona() {
        let _g = GlobalSettingsGuard::acquire();
        set_cli_cognition(CognitionOverride::Unset);
        // No persona → no field; persona level → that level.
        assert_eq!(resolve_cognition(None), None);
        assert_eq!(
            resolve_cognition(Some(Cognition::Pondering)),
            Some(Cognition::Pondering)
        );
    }

    #[test]
    fn set_override_wins_over_the_persona() {
        let _g = GlobalSettingsGuard::acquire();
        set_cli_cognition(CognitionOverride::Set(Cognition::Glancing));
        assert_eq!(
            resolve_cognition(Some(Cognition::Contemplating)),
            Some(Cognition::Glancing),
            "the session /cognition override must beat the persona"
        );
        set_cli_cognition(CognitionOverride::Unset); // restore for other tests
    }

    #[test]
    fn off_override_forces_no_field_even_with_a_persona() {
        let _g = GlobalSettingsGuard::acquire();
        set_cli_cognition(CognitionOverride::Off);
        assert_eq!(
            resolve_cognition(Some(Cognition::Contemplating)),
            None,
            "/cognition off must suppress reasoning.effort despite the persona"
        );
        set_cli_cognition(CognitionOverride::Unset); // restore
    }

    #[test]
    fn override_reaches_the_headless_path_with_no_persona() {
        // The headless driver (solve / worker) has no persona, so it resolves
        // `resolve_cognition(None)`. A `--cognition` / `--obsessive` override
        // installed via `set_cli_cognition` must therefore reach the wire headless
        // (default `Unset` → `None`, unchanged).
        let _g = GlobalSettingsGuard::acquire();
        set_cli_cognition(CognitionOverride::Unset);
        assert_eq!(
            resolve_cognition(None),
            None,
            "no override ⇒ no effort headless"
        );
        set_cli_cognition(CognitionOverride::Set(Cognition::Contemplating));
        assert_eq!(
            resolve_cognition(None),
            Some(Cognition::Contemplating),
            "--cognition must reach the persona-less headless driver"
        );
        set_cli_cognition(CognitionOverride::Unset); // restore
    }

    #[test]
    fn effective_cognition_layers_override_over_persona_global() {
        // review-2 #1/#6: the persona's declared cognition is a real layer via the
        // PERSONA_COGNITION global, so status/save see the EFFECTIVE value.
        let _g = GlobalSettingsGuard::acquire();
        set_cli_cognition(CognitionOverride::Unset);
        set_persona_cognition(None);
        assert_eq!(
            effective_cognition(),
            None,
            "no override, no persona ⇒ none"
        );
        set_persona_cognition(Some(Cognition::Contemplating));
        assert_eq!(
            effective_cognition(),
            Some(Cognition::Contemplating),
            "persona cognition applies when no override"
        );
        set_cli_cognition(CognitionOverride::Set(Cognition::Glancing));
        assert_eq!(
            effective_cognition(),
            Some(Cognition::Glancing),
            "the /cognition override beats the persona"
        );
        set_cli_cognition(CognitionOverride::Off);
        assert_eq!(
            effective_cognition(),
            None,
            "/cognition off suppresses even the persona"
        );
    }
}
