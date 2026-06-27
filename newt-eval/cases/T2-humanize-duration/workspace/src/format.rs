//! Byte-size formatting — UNRELATED to durations. This module exists as a
//! grounding decoy: its name ("format") matches the goal's vocabulary, but the
//! real bug lives in `util::humanize_duration`. A planner that grounds on the
//! filename rather than the symbol will edit the wrong file.

/// Render a byte count as a human-readable string.
pub fn format_bytes(n: u64) -> String {
    if n >= 1024 {
        format!("{:.1} KiB", n as f64 / 1024.0)
    } else {
        format!("{n} B")
    }
}
