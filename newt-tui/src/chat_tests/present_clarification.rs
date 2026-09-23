//! #2524 item 7: the trait DEFAULT for `present_clarification` must
//! reproduce today's lean/piped/headless behavior byte-for-byte — the same
//! `print_newt(batch)` call every clarification call site in `chat.rs` used
//! to make directly, followed by the SAME `read_line(prompt)` call the
//! chat loop's top-of-loop read has always made.
//!
//! A fake `InputSurface` implementing only the required methods (never
//! overriding `present_clarification`) is the right test double for this:
//! anything it observes came through the DEFAULT body, not a surface-specific
//! override, so a regression here is a regression in the shared default
//! every non-RichTUI surface depends on.

use super::*;

/// Records what it was asked to do, so a test can assert on the exact
/// arguments the default body forwarded — never overrides
/// `present_clarification`, so every call to it below exercises the trait
/// default.
#[derive(Default)]
struct RecordingFake {
    read_line_calls: Vec<String>,
    next: Option<ReadOutcome>,
}

impl InputSurface for RecordingFake {
    fn read_line(&mut self, prompt: &str) -> anyhow::Result<ReadOutcome> {
        self.read_line_calls.push(prompt.to_string());
        Ok(self.next.take().unwrap_or(ReadOutcome::Eof))
    }
    fn add_history(&mut self, _entry: &str) {}
    fn save_history(&mut self) {}
    fn reload(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    fn present_interaction(
        &mut self,
        _interaction: &newt_core::interaction_surface::SurfaceInteraction,
    ) -> newt_core::HumanQuestionOutcome {
        newt_core::HumanQuestionOutcome::Unavailable
    }
}

/// **The non-rich path is byte-identical to today.**
///
/// The default body forwards `prompt` to `read_line` UNCHANGED — the same
/// per-turn prompt string the top-of-loop read has always built and passed,
/// not an empty string or a rendering of the batch/hint. This is the
/// property that keeps the lean/session-worker/plain-scroller surfaces
/// reading exactly as they did before this trait method existed.
#[test]
fn the_default_forwards_the_unmodified_prompt_to_read_line() {
    let mut fake = RecordingFake {
        next: Some(ReadOutcome::Line("2: ship it".to_string())),
        ..Default::default()
    };
    let outcome = fake
        .present_clarification(
            "1: which lane?\n   a) build\n   b) ship",
            "reply with an ordinal — or /discuss",
            "› ",
            true,
            false,
        )
        .expect("default body never errors on a fake surface");
    assert_eq!(fake.read_line_calls, vec!["› ".to_string()]);
    assert!(matches!(outcome, ReadOutcome::Line(a) if a == "2: ship it"));
}

/// The default reads EXACTLY once per call — it does not loop, retry, or
/// otherwise diverge from `read_line`'s own single-read contract.
#[test]
fn the_default_reads_exactly_once() {
    let mut fake = RecordingFake::default();
    let _ = fake.present_clarification("batch", "hint", "prompt", false, false);
    assert_eq!(fake.read_line_calls.len(), 1);
}

/// Every `ReadOutcome` variant `read_line` can produce passes straight
/// through the default unchanged — the default is a pure forward, not a
/// filter over some outcomes and not others (the shape a per-surface
/// `read_line` override could otherwise diverge on: Interrupted/Eof/Fatal
/// all end a turn the same way regardless of which read call produced them).
#[test]
fn every_read_outcome_variant_passes_through_unchanged() {
    for outcome in [
        ReadOutcome::Interrupted,
        ReadOutcome::Eof,
        ReadOutcome::Fatal("degraded".to_string()),
    ] {
        let expect_debug = format!("{outcome:?}");
        let mut fake = RecordingFake {
            next: Some(outcome),
            ..Default::default()
        };
        let got = fake
            .present_clarification("batch", "hint", "prompt", false, false)
            .expect("default body never errors on a fake surface");
        assert_eq!(format!("{got:?}"), expect_debug);
    }
}
