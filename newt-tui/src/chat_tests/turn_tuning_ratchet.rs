use super::*;

/// Replays the incident's exact numbers (#1967's evidence): `num_ctx`
/// 209,715 (the session's `safe_context`, standing in for an explicit
/// `[backends] num_ctx` the config never set), the poisoned round's
/// real 205,189 input tokens (97.8% of that window — inside the 95%
/// suspect zone), and a turn that DID see an Accepted round elsewhere.
/// A suspect turn-max at 97.8% of the window, following a turn that
/// otherwise completed normally, must not move `max_ok_input` — this
/// fails on current (pre-fix) `main` by construction, since that code
/// path checks only `turn_saw_accepted`.
#[test]
fn a_suspect_turn_max_does_not_ratchet_even_with_an_earlier_accept() {
    assert!(
        !turn_tuning_ratchet_is_trustworthy(true, 205_189, Some(209_715)),
        "turn_saw_accepted alone must not license a suspect turn-max"
    );
}

/// The anti-vacuous twin: a genuinely clean turn — accepted, and its
/// max input nowhere near the window — still ratchets. Proves the fix
/// is a real exclusion, not a change that silently disables the
/// turn-level ratchet altogether.
#[test]
fn a_genuinely_clean_accepted_turn_still_ratchets() {
    assert!(turn_tuning_ratchet_is_trustworthy(
        true,
        4_136,
        Some(209_715)
    ));
    // And the untouched half of the existing gate: no acceptance at all
    // still means no ratchet, suspect or not.
    assert!(!turn_tuning_ratchet_is_trustworthy(
        false,
        4_136,
        Some(209_715)
    ));
}

/// No known `num_ctx` (e.g. a provider that never reports one): nothing
/// to compare against, so `is_truncation_suspect` is never true and an
/// accepted turn ratchets exactly as it always has.
#[test]
fn unknown_num_ctx_never_blocks_the_ratchet() {
    assert!(turn_tuning_ratchet_is_trustworthy(true, 205_189, None));
}
