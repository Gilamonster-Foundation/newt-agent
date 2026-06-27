/// Format a Celsius temperature to one decimal place with a `°C` suffix
/// (e.g. `21.05` → `"21.1°C"`). This is the seam the task is about — NOT
/// `crate::format::format_temp`, which is a different (whole-degree) helper.
pub fn format_temperature(celsius: f64) -> String {
    format!("{}°C", celsius as i64)
}
