//! Generic formatting helpers. GROUNDING DECOY: this is the "obvious" file for a
//! goal about "temperature formatting", and `format_temp` below is a
//! similarly-named, *already-correct* function — but it is NOT the seam the task
//! is about (that is `units::temperature::format_temperature`). A planner that
//! grounds on the obvious filename + a fuzzy symbol match will edit this, fix
//! nothing, and never make the real test pass.

/// Format a temperature as a compact label, rounded to whole degrees (e.g. a
/// dashboard chip). Distinct from `units::temperature::format_temperature`.
pub fn format_temp(celsius: f64) -> String {
    format!("{}°", celsius.round() as i64)
}

/// Format a byte count.
pub fn format_bytes(n: u64) -> String {
    if n >= 1024 {
        format!("{:.1} KiB", n as f64 / 1024.0)
    } else {
        format!("{n} B")
    }
}
