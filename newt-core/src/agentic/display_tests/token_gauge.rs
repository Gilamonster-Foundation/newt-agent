use super::*;

#[test]
fn fmt_tokens_inserts_thousands_separators() {
    assert_eq!(fmt_tokens(0), "0");
    assert_eq!(fmt_tokens(999), "999");
    assert_eq!(fmt_tokens(1_000), "1,000");
    assert_eq!(fmt_tokens(1_234_567), "1,234,567");
}

#[test]
fn gauge_formatting_k_compact_fraction_and_level() {
    use super::{fmt_token_gauge, fmt_tokens_compact, fmt_tokens_k, gauge_level, GaugeLevel};
    // k (rounded thousands)
    assert_eq!(fmt_tokens_k(899_000), "899k");
    assert_eq!(fmt_tokens_k(1_024_000), "1024k");
    assert_eq!(fmt_tokens_k(512_400), "512k");
    // compact: 1M = 1024k; round M drops the .0
    assert_eq!(fmt_tokens_compact(899_000), "899k");
    assert_eq!(fmt_tokens_compact(1_024_000), "1M");
    assert_eq!(fmt_tokens_compact(2_048_000), "2M");
    assert_eq!(fmt_tokens_compact(1_536_000), "1.5M");
    // fraction
    assert_eq!(fmt_token_gauge(899_000, 1_024_000), "899k/1024k");
    // level bands: <75 Ok, 75–90 Warn, ≥90 Critical
    assert_eq!(gauge_level(100, 1000), GaugeLevel::Ok);
    assert_eq!(gauge_level(740, 1000), GaugeLevel::Ok);
    assert_eq!(gauge_level(750, 1000), GaugeLevel::Warn);
    assert_eq!(gauge_level(890, 1000), GaugeLevel::Warn);
    assert_eq!(gauge_level(900, 1000), GaugeLevel::Critical);
    assert_eq!(gauge_level(0, 0), GaugeLevel::Ok); // no budget → no panic
}

/// Visual preview for UX review (run with `--nocapture`). Not an assertion —
/// prints the gauge at several fills with colors so the format/thresholds
/// can be eyeballed. Ignored by default so it never adds CI noise.
#[test]
#[ignore = "visual preview; run with --ignored --nocapture"]
fn gauge_visual_preview() {
    use super::{fmt_token_gauge, fmt_tokens_compact, gauge_level, GaugeLevel};
    use crossterm::style::Color;
    let color = |lvl: GaugeLevel| match lvl {
        GaugeLevel::Ok => Color::Green,
        GaugeLevel::Warn => Color::DarkYellow,
        GaugeLevel::Critical => Color::Red,
    };
    let paint = |c: Color, s: &str| match c {
        Color::Green => format!("\x1b[32m{s}\x1b[0m"),
        Color::DarkYellow => format!("\x1b[33m{s}\x1b[0m"),
        Color::Red => format!("\x1b[31m{s}\x1b[0m"),
        _ => s.to_string(),
    };
    let budget = 1_024_000;
    println!("\n  context-budget gauge — fraction form (live header):");
    for used in [102_000u32, 512_000, 800_000, 972_000, 1_010_000] {
        let lvl = gauge_level(used, budget);
        let g = fmt_token_gauge(used, budget);
        println!("    {:<14} {:?}", paint(color(lvl), &g), lvl);
    }
    println!("\n  compact budget form (1M = 1024k):");
    for n in [899_000u32, 1_024_000, 1_536_000, 2_048_000] {
        println!("    {n:>9} → {}", fmt_tokens_compact(n));
    }
    println!(
        "\n  mock header:\n    [2026-06-22 14:32:01] vi --INSERT-- nemotron @ REDACTED-HOST   {}\n",
        paint(
            color(gauge_level(972_000, budget)),
            &fmt_token_gauge(972_000, budget)
        ),
    );
}
