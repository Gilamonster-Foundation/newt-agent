//! Verdict-bound challenges (#1373).
//!
//! A gesture authorizes exactly one (request, verdict) pair — so a Deny
//! gesture cannot be replayed as an AllowSession.
//!
//! The answer-authorization half of this file moved out with
//! `newt_core::answer_authz`, deleted in A3 (#1837): it had zero
//! production callers, and `newt_interaction::binding` now expresses the
//! same refusal (`Refusal::AssertionRequired`) against the records A2
//! froze. The one property it encoded that A3 does NOT yet express —
//! that an ENROLLED operator answering without a usable assertion is a
//! hard deny rather than a downgrade to header trust — is recorded as a
//! residual for B0, which owns responder policy.

use newt_core::permission_challenge::{requires_terminal_echo, verdict_tag, PermissionChallenge};
use newt_core::Verdict;

const TTL: i64 = 5 * 60 * 1_000_000_000;

fn challenge() -> PermissionChallenge {
    PermissionChallenge {
        request_id: "req-1".into(),
        conversation_id: "conv-1".into(),
        requests_json: r#"[{"tool":"run_command"}]"#.into(),
        danger_json: r#"["low"]"#.into(),
        created_tick: 1_000,
    }
}

/// The replay this binding exists to stop.
#[test]
fn a_gesture_for_one_verdict_does_not_authorize_another() {
    let c = challenge();
    let deny = c.challenge_for(Verdict::Deny);
    let allow_once = c.challenge_for(Verdict::AllowOnce);
    let allow_session = c.challenge_for(Verdict::AllowSession);

    assert_ne!(deny, allow_once);
    assert_ne!(deny, allow_session);
    assert_ne!(allow_once, allow_session);
}

/// And a gesture for one request does not authorize another, even for the same
/// verdict — otherwise a low-danger approval could be replayed onto whatever
/// the agent asks for next.
#[test]
fn a_gesture_for_one_request_does_not_authorize_another() {
    let a = challenge();
    for mutate in [
        (|c: &mut PermissionChallenge| c.request_id = "req-2".into())
            as fn(&mut PermissionChallenge),
        |c: &mut PermissionChallenge| c.conversation_id = "conv-2".into(),
        |c: &mut PermissionChallenge| c.requests_json = r#"[{"tool":"web_fetch"}]"#.into(),
        |c: &mut PermissionChallenge| c.danger_json = r#"["high"]"#.into(),
        |c: &mut PermissionChallenge| c.created_tick = 1_001,
    ] {
        let mut b = challenge();
        mutate(&mut b);
        assert_ne!(
            a.challenge_for(Verdict::AllowOnce),
            b.challenge_for(Verdict::AllowOnce),
            "every field must be bound into the challenge"
        );
        assert_ne!(a.digest(), b.digest());
    }
}

#[test]
fn the_same_decision_always_yields_the_same_challenge() {
    assert_eq!(
        challenge().challenge_for(Verdict::AllowOnce),
        challenge().challenge_for(Verdict::AllowOnce),
        "the browser and the server must derive one challenge, not two"
    );
}

/// Tags are spelled out so a rename or reorder of `Verdict` cannot silently
/// change what a signature covers.
#[test]
fn verdict_tags_are_stable_and_distinct() {
    assert_eq!(verdict_tag(Verdict::AllowOnce), b"allow_once");
    assert_eq!(verdict_tag(Verdict::AllowSession), b"allow_session");
    assert_eq!(verdict_tag(Verdict::Deny), b"deny");
}

#[test]
fn expiry_is_evaluated_against_the_supplied_clock() {
    let c = challenge();
    assert!(!c.is_expired(c.created_tick, TTL));
    assert!(!c.is_expired(c.created_tick + TTL - 1, TTL));
    assert!(c.is_expired(c.created_tick + TTL, TTL));
    // A clock that ran backwards must not resurrect an expired request.
    assert!(!c.is_expired(0, TTL));
}

// --- danger tiers ---

#[test]
fn high_danger_demands_a_terminal_echo_and_unparseable_counts_as_high() {
    assert!(requires_terminal_echo(r#"["high"]"#));
    assert!(requires_terminal_echo(r#"["low","high"]"#));
    assert!(
        requires_terminal_echo(r#"["HIGH"]"#),
        "tier match is case-insensitive"
    );
    assert!(!requires_terminal_echo(r#"["low"]"#));
    // #1836: the two forms production ACTUALLY wrote. Before the fix the
    // reader accepted only a JSON array, so a JSON string — which is what
    // the gate wrote — always fell through to the fail-closed default and
    // this function could never return `false` for a real value.
    assert!(!requires_terminal_echo(r#""low""#), "the JSON-string form");
    assert!(requires_terminal_echo(r#""high""#));
    // ...and the plain tier B0b-2's transport stores.
    assert!(!requires_terminal_echo("low"), "the plain form");
    assert!(requires_terminal_echo("high"));
    assert!(!requires_terminal_echo("  low  "));
    assert!(!requires_terminal_echo("[]"));

    // Fail toward asking a human.
    assert!(requires_terminal_echo("not json"));
    assert!(requires_terminal_echo("{}"));
    assert!(requires_terminal_echo(""));
}
