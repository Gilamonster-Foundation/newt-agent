//! `requires_terminal_echo` — the danger tier that forces WYSIWYS at the
//! terminal before a permission answer is honoured.
//!
//! Carried over from `tests/permission_challenge.rs`, which was deleted with
//! `PermissionChallenge` in #1839. The challenge machinery had zero production
//! callers and is superseded by the definition's `ContentId`; this function is
//! live, sits on the authorization path, and was the subject of #1836 — so its
//! coverage moves rather than going down with the module's old name.
//!
//! The A3 residual that has NO home yet is recorded on #1839 part 2 and is not
//! expressed here: an ENROLLED operator answering without a usable assertion
//! must be a hard deny rather than a downgrade to header trust ("enrolling must
//! never make you less safe than not enrolling", #1366/#1373).
//! `ResponderPolicy.requires_assertion` is enrollment-blind, so it can refuse
//! an unauthenticated responder but cannot refuse a DOWNGRADE. B0 owns
//! responder policy and should express it.

use newt_core::permission_challenge::requires_terminal_echo;

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
