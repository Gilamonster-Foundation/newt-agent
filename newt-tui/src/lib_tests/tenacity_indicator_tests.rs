use super::tenacity_indicator;
use newt_core::Tenacity;

#[test]
fn shows_only_when_elevated_above_standard() {
    // Behaviour-preserving default: no clutter on the ready line.
    assert_eq!(tenacity_indicator(Tenacity::Standard), "");
    // Every elevated level (either direction from Standard) is announced.
    assert_eq!(
        tenacity_indicator(Tenacity::Relentless),
        " · tenacity: relentless"
    );
    assert_eq!(
        tenacity_indicator(Tenacity::Insistent),
        " · tenacity: insistent"
    );
    assert_eq!(
        tenacity_indicator(Tenacity::Relaxed),
        " · tenacity: relaxed"
    );
}
