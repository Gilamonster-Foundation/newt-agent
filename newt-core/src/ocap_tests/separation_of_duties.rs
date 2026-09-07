use super::*;

#[test]
fn distinctness_primitive() {
    assert!(proposer_distinct("SHA256:proposer", "SHA256:worker"));
    assert!(!proposer_distinct("SHA256:same", "SHA256:same"));
    assert!(
        !proposer_distinct("", "SHA256:worker"),
        "empty proposer is not distinct"
    );
}

#[test]
fn verify_sod_is_absent_until_taint_aware() {
    // Even with distinct keys, sod stays open (taint-aware half unbuilt).
    let v = verify_sod("SHA256:proposer", "SHA256:worker");
    assert!(!v.is_verified());
    assert_eq!(v.deviation(), Some("sod-proposer-not-worker"));
    // Self-proposal reports the distinctness failure specifically.
    let self_prop = verify_sod("SHA256:same", "SHA256:same");
    assert!(
        matches!(self_prop, Verification::Absent { reason, .. } if reason.contains("self-proposal"))
    );
}

#[test]
fn auto_apply_fails_closed_both_ways() {
    assert!(matches!(
        auto_apply_policy("SHA256:p", "SHA256:w"),
        Err(FailClosed {
            deviation: "sod-proposer-not-worker",
            ..
        })
    ));
    assert!(auto_apply_policy("SHA256:same", "SHA256:same").is_err());
}
