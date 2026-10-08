#[test]
fn compression_notice_text_registers_per_outcome() {
    use super::compression_notice_text;
    use crate::agentic::compress::CompressAction;
    // The static-marker last resort is LOUD and degraded-sounding (24.7).
    let (msg, loud) = compression_notice_text(CompressAction::StaticFallback, 10_000, 6_000, "");
    assert!(loud, "static marker is the loud last resort");
    assert!(msg.starts_with("⛔"), "{msg}");
    assert!(msg.contains("summary unavailable"), "{msg}");
    assert!(msg.contains("Re-read files"), "{msg}");
    // Success and prune are calm, distinct glyphs.
    let (msg, loud) = compression_notice_text(CompressAction::Summarized, 10_000, 6_000, "");
    assert!(!loud);
    assert!(msg.starts_with("✓") && msg.contains("summarized"), "{msg}");
    let (msg, loud) = compression_notice_text(CompressAction::Pruned, 10_000, 6_000, "");
    assert!(!loud);
    assert!(
        msg.starts_with("⧉") && msg.contains("structural prune"),
        "{msg}"
    );
    // The over-budget suffix rides along.
    let (msg, _) = compression_notice_text(
        CompressAction::Summarized,
        10_000,
        6_000,
        ", still over budget",
    );
    assert!(msg.contains(", still over budget"), "{msg}");
}

/// `docs/decisions/tty_widget_suite.md` §5 row 3: `Notice` must reproduce
/// this builder's bytes **exactly** before any call site is migrated onto
/// it. Every one of the three registers is a glyph + the two-space gutter +
/// text, so the widget's composition is the right shape and the migration
/// in step 5 is a deletion rather than a rewrite.
///
/// Byte-for-byte, not "starts with" — the assertions above are the loose
/// ones this deliberately is not.
#[test]
fn notice_reproduces_the_compression_notice_bytes() {
    use super::compression_notice_text;
    use crate::agentic::compress::CompressAction;
    use crate::tty::{Level, Notice};

    let cases = [
        (CompressAction::StaticFallback, Level::Loud, "⛔"),
        (CompressAction::Summarized, Level::Ok, "✓"),
        (CompressAction::Pruned, Level::Info, "⧉"),
    ];
    for (action, level, glyph) in cases {
        let (msg, _) = compression_notice_text(action, 10_000, 6_000, "");
        let body = msg
            .strip_prefix(glyph)
            .and_then(|r| r.strip_prefix("  "))
            .unwrap_or_else(|| panic!("{action:?} is not `{glyph}` + two spaces: {msg:?}"));
        assert_eq!(
            Notice::new(level, glyph, body).gap(2).line(),
            msg,
            "Notice must reproduce {action:?}'s bytes exactly"
        );
    }
}

/// Visual preview for UX review (run with `--ignored --nocapture`): the three
/// compression-notice registers with their colors.
#[test]
#[ignore = "visual preview; run with --ignored --nocapture"]
fn compression_notice_visual_preview() {
    use super::compression_notice_text;
    use crate::agentic::compress::CompressAction;
    let paint = |loud: bool, s: &str| {
        if loud {
            format!("\x1b[31m{s}\x1b[0m") // red, loud
        } else {
            format!("\x1b[33m{s}\x1b[0m") // amber
        }
    };
    println!("\n  compression-notice registers (24.7):");
    for action in [
        CompressAction::Pruned,
        CompressAction::Summarized,
        CompressAction::StaticFallback,
    ] {
        let (msg, loud) = compression_notice_text(action, 1_024_000, 600_000, "");
        println!("    {}", paint(loud, &msg));
    }
    println!();
}

/// Every number in the overflow notice is one the decision used, and no bare
/// comparison can read as false.
#[test]
fn overflow_notice_prints_the_numbers_the_decision_used() {
    use super::{overflow_notice_text, OverflowReason};
    let empty = overflow_notice_text(
        &OverflowReason::EmptyReplyNearWindow {
            prompt_tokens: 74_256,
            window: 83_885,
            trigger_pct: 85,
        },
        "ornith",
        1,
    );
    assert!(
        empty.contains("returned nothing at 74,256 prompt tokens"),
        "{empty}"
    );
    assert!(
        empty.contains("past 85% of its 83,885-token window"),
        "{empty}"
    );
    assert!(!empty.contains('>'), "no bare comparison: {empty}");
    assert!(empty.contains("attempt 1/2"), "{empty}");

    let refused = overflow_notice_text(
        &OverflowReason::Refused {
            request_estimate: Some(91_204),
            window: Some(83_885),
            trim_to: 74_000,
        },
        "ornith",
        2,
    );
    assert!(
        refused.contains(
            "refused a request of about 91,204 tokens (window 83,885); trimming to 74,000"
        ),
        "{refused}"
    );
    assert!(refused.contains("attempt 2/2"), "{refused}");

    let blind = overflow_notice_text(
        &OverflowReason::Refused {
            request_estimate: None,
            window: None,
            trim_to: 60_000,
        },
        "ornith",
        1,
    );
    assert!(
        blind.contains("refused the request as too large; trimming to 60,000"),
        "{blind}"
    );
}
