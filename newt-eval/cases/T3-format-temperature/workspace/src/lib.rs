pub mod format;
pub mod units;

pub use units::temperature::format_temperature;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_temperature_to_one_decimal() {
        assert_eq!(format_temperature(21.05), "21.1°C");
    }
}
