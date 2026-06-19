//! crew_attest.rs — the human-presence (`attest`) authority surface for the
//! crew/team tools (#479, ROADMAP 23.2).
//!
//! Wraps agent-bridle's `step_up` (the merged `attest` primitive, agent-bridle#24)
//! for the crew. A crew **dispatch** is an *amplify* — it grants a sub-agent the
//! authority to act — which §7.5 (knowledge#40) says must ride a **live human
//! gesture**: the `{allow, attest, deny}` surface, not a bare env toggle.
//! `compose_roster` only *proposes* (no effects), so it needs nothing.
//!
//! This is the **structure** — the decision surface + the policy. The real Passkey
//! enforcement (a WebAuthn `DischargeVerifier` + the ceremony) lands with **BOOT**
//! (#472). Today the established presence is `Prompt` (the operator's `/team`
//! enable — a soft human affirmation, exactly `Presence::Prompt`); a
//! `Passkey`-required action surfaces `NeedsAttest`, awaiting BOOT's verifier.
//!
//! Pure (no I/O); the caller surfaces `NeedsAttest` to the overseer.

use agent_bridle::{AttestRequirement, CallRequest, Rule, StepUpPolicy};

// Re-export the presence ladder so callers (e.g. the CLI's LocalCrewRunner) can
// name the established presence without a direct agent-bridle dependency.
pub use agent_bridle::Presence;

/// The default crew/team step-up policy. `crew`/`team` dispatch requires a human
/// gesture (`Prompt` today; raise to `Passkey` for amplifying crews once BOOT
/// lands); `compose_roster` only proposes, so it needs none. Unknown crew ops
/// fail *toward* attestation (the default demands a gesture).
#[must_use]
pub fn crew_step_up_policy() -> StepUpPolicy {
    StepUpPolicy::new(
        vec![
            Rule {
                selector: "compose_roster".to_string(),
                requirement: AttestRequirement::NONE,
            },
            Rule {
                selector: "crew".to_string(),
                requirement: AttestRequirement::presence(Presence::Prompt),
            },
            Rule {
                selector: "team".to_string(),
                requirement: AttestRequirement::presence(Presence::Prompt),
            },
        ],
        AttestRequirement::presence(Presence::Prompt),
    )
}

/// The crew authority decision under an established human presence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrewAuthz {
    /// The established presence satisfies the policy — run it.
    Allow,
    /// Hold: needs a human gesture of at least this strength (the §7.5 `attest`).
    /// The overseer surfaces it; post-BOOT it drives the discharge ceremony.
    NeedsAttest(Presence),
}

/// Decide whether `op` (over `resource`) may run given the presence the session
/// has already established. `Prompt` = the operator's `/team` enable; `Passkey` =
/// a hardware gesture. Pure — the caller acts on `NeedsAttest`.
#[must_use]
pub fn crew_authz(
    policy: &StepUpPolicy,
    op: &str,
    resource: &str,
    established: Presence,
) -> CrewAuthz {
    let req = policy.required_for(&CallRequest::new(op, serde_json::Value::Null, resource));
    if req.demands_gesture() && established < req.presence {
        CrewAuthz::NeedsAttest(req.presence)
    } else {
        CrewAuthz::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_roster_never_needs_a_gesture() {
        let p = crew_step_up_policy();
        // Even with NO established presence, proposing a roster is allowed (no effects).
        assert_eq!(
            crew_authz(&p, "compose_roster", "", Presence::None),
            CrewAuthz::Allow
        );
    }

    #[test]
    fn crew_dispatch_needs_at_least_prompt() {
        let p = crew_step_up_policy();
        // No human presence -> hold for an attest.
        assert_eq!(
            crew_authz(&p, "crew", "add a flag", Presence::None),
            CrewAuthz::NeedsAttest(Presence::Prompt)
        );
        // The operator's /team enable (Prompt) satisfies it.
        assert_eq!(
            crew_authz(&p, "crew", "add a flag", Presence::Prompt),
            CrewAuthz::Allow
        );
        // A stronger gesture also satisfies it (presence is ordered).
        assert_eq!(
            crew_authz(&p, "crew", "add a flag", Presence::Passkey),
            CrewAuthz::Allow
        );
    }

    #[test]
    fn team_and_unknown_ops_default_to_attest() {
        let p = crew_step_up_policy();
        assert_eq!(
            crew_authz(&p, "team", "build X", Presence::None),
            CrewAuthz::NeedsAttest(Presence::Prompt)
        );
        // An op no rule matches falls to the policy default (gate it).
        assert_eq!(
            crew_authz(&p, "mystery_op", "", Presence::None),
            CrewAuthz::NeedsAttest(Presence::Prompt)
        );
    }

    #[test]
    fn a_passkey_required_action_is_not_satisfied_by_prompt() {
        // Forward-looking: an amplifying crew that demands a hardware gesture is NOT
        // satisfied by the soft /team enable — this is the seam BOOT's verifier fills.
        let p = StepUpPolicy::new(
            vec![Rule {
                selector: "crew:secrets/*".to_string(),
                requirement: AttestRequirement::presence(Presence::Passkey),
            }],
            AttestRequirement::NONE,
        );
        assert_eq!(
            crew_authz(&p, "crew", "secrets/rotate", Presence::Prompt),
            CrewAuthz::NeedsAttest(Presence::Passkey)
        );
        assert_eq!(
            crew_authz(&p, "crew", "secrets/rotate", Presence::Passkey),
            CrewAuthz::Allow
        );
    }
}
