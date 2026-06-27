//! Ungameable external behavioral spec for T3 (dropped into the produced tree at
//! grading time — the agent never sees it). Grades the REAL seam
//! `units::temperature::format_temperature`, not the `format::format_temp` decoy.
use t3_format_temperature::format_temperature;

#[test]
fn format_temperature_rounds_to_one_decimal() {
    assert_eq!(format_temperature(21.05), "21.1°C");
    assert_eq!(format_temperature(0.0), "0.0°C");
    assert_eq!(format_temperature(100.0), "100.0°C");
    assert_eq!(format_temperature(-3.27), "-3.3°C");
}
