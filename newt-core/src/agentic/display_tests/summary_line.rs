use super::*;

/// #1640 Layer 1: a spilled result collapses to ONE line in summary mode —
/// total count, the tail (where errors live), and the #1433 recovery hint.
/// Fitting results and `/spill 0` (unbounded) return `None` (normal render).
#[test]
fn summary_line_collapses_only_spilled_results() {
    // Spilled: one line with count + tail + hint.
    let line = spill_summary_line("l1\nl2\nl3\nl4\nl5", 3, 80).expect("5 > 3 collapses");
    assert_eq!(line, "▲ 5 lines hidden  [/spill N raises this view] · l5");

    // Tail skips trailing blank lines — the last NON-EMPTY line informs.
    let line = spill_summary_line("l1\nl2\nl3\nerror: boom\n\n", 3, 80).unwrap();
    assert!(line.contains("error: boom"), "{line}");

    // Fits the budget → None (normal render, no pointless indirection).
    assert_eq!(spill_summary_line("a\nb\nc", 3, 80), None);
    // `/spill 0` = unbounded → None (full text always wins).
    assert_eq!(spill_summary_line("a\nb\nc\nd\ne", 0, 80), None);
}

/// The one-line promise holds on narrow terminals: the tail is truncated
/// (with `…`) to keep the marker within the column budget, and dropped
/// entirely when almost no room remains — but the hint always survives.
#[test]
fn summary_line_fits_narrow_terminals() {
    let wide = format!("l1\nl2\nl3\n{}", "x".repeat(300));
    let line = spill_summary_line(&wide, 3, 60).unwrap();
    assert!(
        line.chars().count() <= 60,
        "one visual row on an 60-col terminal: {} chars",
        line.chars().count()
    );
    assert!(line.contains('…'), "a cut tail is marked: {line}");
    assert!(line.contains("/spill N raises this view"), "{line}");

    // Pathologically narrow: tail dropped, count + hint intact.
    let line = spill_summary_line(&wide, 3, 20).unwrap();
    assert_eq!(line, "▲ 4 lines hidden  [/spill N raises this view]");
}

/// `ToolDisplay` in summary mode commits the collapse marker INSTEAD of the
/// excerpt for a spilled result — and keeps the excerpt for a fitting one.
/// (Excerpt mode `false` is pinned by every other test in this module.)
#[test]
fn summary_mode_commits_the_marker_not_the_excerpt() {
    let mut display = super::ToolDisplay::new(Vec::new(), false, 80, 3, true);
    display.result("l1\nl2\nl3\nl4\nl5");
    let out = String::from_utf8(display.writer).unwrap();
    assert!(
        out.contains("▲ 5 lines hidden  [/spill N raises this view] · l5"),
        "the marker committed: {out:?}"
    );
    assert!(
        !out.contains("▒ l3"),
        "no excerpt rows in summary mode: {out:?}"
    );

    // A fitting result renders exactly as excerpt mode would.
    let mut display = super::ToolDisplay::new(Vec::new(), false, 80, 3, true);
    display.result("a\nb");
    let out = String::from_utf8(display.writer).unwrap();
    assert!(
        out.contains("▒ a"),
        "fitting results keep the full render: {out:?}"
    );
}

/// #1663 review F4: the collapse predicate spends WRAPPED rows exactly like
/// the excerpt path (#1433) — a result of few logical lines but heavy
/// wrapping collapses in summary mode instead of falling back to a
/// truncated excerpt.
#[test]
fn collapse_uses_wrapped_row_accounting_like_the_excerpt() {
    // 2 logical lines, but the first wraps to many rows at 20 columns.
    let long = format!("{}\nshort tail\n", "x".repeat(200));
    assert!(super::spills_past(&long, 3, 20), "wrapped rows spill");
    assert!(
        super::spill_summary_line(&long, 3, 20).is_some(),
        "summary engages on wrapped spill (logical-line count would say no)"
    );
    // Parity with the excerpt: what the excerpt truncates, summary collapses.
    let excerpt = super::spill_view_lines(&long, 3, 20);
    assert!(
        excerpt[0].starts_with('▲'),
        "excerpt path truncates the same input: {excerpt:?}"
    );
    // And a genuinely fitting result engages neither.
    let fits = "a\nb\n";
    assert!(!super::spills_past(fits, 3, 80));
    assert!(super::spill_summary_line(fits, 3, 80).is_none());
}
