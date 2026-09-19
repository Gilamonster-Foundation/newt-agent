use super::tenacity_indicator;
use newt_core::Tenacity;

#[test]
fn shows_only_when_raised_above_normal() {
    // The default adds no clutter to the ready line.
    assert_eq!(tenacity_indicator(Tenacity::Normal), "");
    assert_eq!(
        tenacity_indicator(Tenacity::Relentless),
        " · tenacity: relentless"
    );
}
