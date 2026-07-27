//! Verdict-bound challenges and answer authorization (#1373).
//!
//! Two properties. A gesture authorizes exactly one (request, verdict) pair —
//! so a Deny gesture cannot be replayed as an AllowSession. And an enrolled
//! session that cannot produce a valid assertion is **denied**, never
//! downgraded to header trust: enrolling must not make an operator less safe
//! than not enrolling.

use newt_core::answer_authz::{authorize, AnswerAuthz, AnswerContext, DenyReason};
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
    assert!(!requires_terminal_echo("[]"));

    // Fail toward asking a human.
    assert!(requires_terminal_echo("not json"));
    assert!(requires_terminal_echo("{}"));
    assert!(requires_terminal_echo(""));
}

// --- answer authorization ---

fn ctx() -> AnswerContext {
    AnswerContext {
        enrolled: true,
        assertion_present: true,
        assertion_valid: true,
        challenge_matches: true,
        credential_known: true,
        expired: false,
        high_danger: false,
        terminal_echoed: false,
    }
}

#[test]
fn a_well_signed_answer_is_honoured() {
    let out = authorize(ctx(), Verdict::AllowSession);
    assert_eq!(out, AnswerAuthz::Signed(Verdict::AllowSession));
    assert!(out.is_signed());
    assert_eq!(out.verdict(), Verdict::AllowSession);
}

/// The pivot: header-only answers on an enrolled session are DENIED, not
/// downgraded. If this ever became a fallback, stripping the assertion would
/// defeat enrollment entirely.
#[test]
fn an_enrolled_session_without_an_assertion_is_hard_denied() {
    let out = authorize(
        AnswerContext {
            assertion_present: false,
            ..ctx()
        },
        Verdict::AllowSession,
    );
    assert_eq!(out, AnswerAuthz::Denied(DenyReason::MissingAssertion));
    assert!(!out.is_signed());
    assert_eq!(
        out.verdict(),
        Verdict::Deny,
        "a denied answer records Deny, never the verdict that was requested"
    );
}

/// Every way an assertion can fail must land on Deny, and none may fall back
/// to the requested verdict.
#[test]
fn every_assertion_failure_denies() {
    for (context, expected) in [
        (
            AnswerContext {
                assertion_valid: false,
                ..ctx()
            },
            DenyReason::BadAssertion,
        ),
        (
            AnswerContext {
                challenge_matches: false,
                ..ctx()
            },
            DenyReason::WrongChallenge,
        ),
        (
            AnswerContext {
                credential_known: false,
                ..ctx()
            },
            DenyReason::UnknownCredential,
        ),
        (
            AnswerContext {
                expired: true,
                ..ctx()
            },
            DenyReason::Expired,
        ),
        (
            AnswerContext {
                high_danger: true,
                terminal_echoed: false,
                ..ctx()
            },
            DenyReason::TerminalEchoRequired,
        ),
    ] {
        let out = authorize(context, Verdict::AllowSession);
        assert_eq!(out, AnswerAuthz::Denied(expected));
        assert_eq!(out.verdict(), Verdict::Deny);
        assert!(!expected.message().is_empty());
    }
}

/// An operator with no enrolled credential keeps the pre-passkey path — there
/// is nothing to downgrade from, and breaking them would make enrollment a
/// prerequisite for using the cockpit at all.
#[test]
fn an_unenrolled_session_keeps_the_header_path() {
    let out = authorize(
        AnswerContext {
            enrolled: false,
            assertion_present: false,
            assertion_valid: false,
            challenge_matches: false,
            credential_known: false,
            ..ctx()
        },
        Verdict::AllowOnce,
    );
    assert_eq!(out, AnswerAuthz::Unenrolled(Verdict::AllowOnce));
    assert!(!out.is_signed(), "unenrolled is not the same as signed");
    assert_eq!(out.verdict(), Verdict::AllowOnce);
}

/// Expiry outranks everything: re-signing cannot rescue an aged-out request, so
/// that is the reason worth reporting.
#[test]
fn expiry_is_reported_ahead_of_a_missing_assertion() {
    let out = authorize(
        AnswerContext {
            expired: true,
            assertion_present: false,
            enrolled: false,
            ..ctx()
        },
        Verdict::AllowOnce,
    );
    assert_eq!(
        out,
        AnswerAuthz::Denied(DenyReason::Expired),
        "an expired request denies even an unenrolled operator"
    );
}

#[test]
fn a_high_danger_answer_passes_once_the_terminal_echoes() {
    let out = authorize(
        AnswerContext {
            high_danger: true,
            terminal_echoed: true,
            ..ctx()
        },
        Verdict::AllowOnce,
    );
    assert_eq!(out, AnswerAuthz::Signed(Verdict::AllowOnce));
}

// --- slice 9 (#1374): the identity header is a nameplate, not a credential ---

/// The supersession, stated as a test rather than as prose in a doc.
///
/// #1365 proposed an OIDC stamp as the authorization signal. It is not one: a
/// forward-auth header is only as trustworthy as everything upstream of it, and
/// it is replayable by anything already inside the network. So for an ENROLLED
/// operator the header authorizes nothing at all — `authorize` never consults
/// it, and no combination of header-derived state yields anything but a denial
/// without an assertion.
///
/// This is a regression test in the strict sense: if someone later adds a
/// "trusted header" bypass, every case below flips and this fails.
#[test]
fn a_header_only_answer_on_an_enrolled_session_is_denied() {
    // Whatever the header says, and whatever verdict is requested, an enrolled
    // session with no assertion is denied.
    for requested in [Verdict::AllowOnce, Verdict::AllowSession, Verdict::Deny] {
        let out = authorize(
            AnswerContext {
                enrolled: true,
                assertion_present: false,
                assertion_valid: false,
                challenge_matches: false,
                credential_known: false,
                expired: false,
                high_danger: false,
                terminal_echoed: false,
            },
            requested,
        );
        assert_eq!(
            out,
            AnswerAuthz::Denied(DenyReason::MissingAssertion),
            "the identity header must never authorize {requested:?}"
        );
        assert_eq!(out.verdict(), Verdict::Deny);
    }
}

/// `AnswerContext` carries no field derived from the identity header — the type
/// itself is the proof that the header cannot influence the decision. If a
/// field like `header_email_matches` ever appears, this comment is the place to
/// argue why it is not an authorization signal.
#[test]
fn the_decision_inputs_contain_no_identity_header() {
    let a = AnswerContext {
        enrolled: true,
        assertion_present: true,
        assertion_valid: true,
        challenge_matches: true,
        credential_known: true,
        expired: false,
        high_danger: false,
        terminal_echoed: false,
    };
    // Two answers that differ only in who the header claims to be are the same
    // value, because that claim is not represented here at all.
    assert_eq!(a, a.clone());
    assert_eq!(
        authorize(a, Verdict::AllowOnce),
        AnswerAuthz::Signed(Verdict::AllowOnce)
    );
}
