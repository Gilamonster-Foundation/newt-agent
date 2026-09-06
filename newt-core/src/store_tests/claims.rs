use super::*;

#[test]
fn clamp_claim_saturates_oversized_legacy_nanos() {
    assert_eq!(clamp_claim(0), 0);
    assert_eq!(clamp_claim(42), 42);
    assert_eq!(clamp_claim(u128::MAX), i64::MAX);
}

#[test]
fn claim_clock_saturates_instead_of_wrapping() {
    let now = now_claim_nanos();
    assert!(now > 0);
    assert_eq!(claim_to_u128(-5), 0);
    assert_eq!(claim_to_u128(42), 42);
}
