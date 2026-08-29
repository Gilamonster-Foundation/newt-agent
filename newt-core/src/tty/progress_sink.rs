//! The terminal renderer for [`crate::progress`] — **not yet wired to
//! anything** (D2a, #1864).
//!
//! # Why this exists but is not connected
//!
//! D2a dual-publishes: the new contract runs beside the existing
//! stdout/`TurnDriver` paths, and *those stay authoritative*. A renderer that
//! actually emitted here today would print every notice twice — once from the
//! old path, once from this one — which is not dual-publishing, it is a
//! regression wearing the word. So the production wiring publishes into
//! [`Scrollback`], which records and emits nothing, and this type is the
//! renderer each family's **cutover** PR will switch onto, one family at a
//! time, each with its own deletion gate.
//!
//! `no_production_path_renders_progress_yet` in this module holds that line
//! mechanically, the way A2.2 held it for the interaction adapter.
//!
//! # The veto, and why it is a pure predicate
//!
//! [`may_render`] takes `protocol` as a *parameter* rather than reading
//! [`protocol_mode`]. That is deliberate, and it is not merely for tidiness:
//! `enter_protocol_mode()` is documented irreversible — "there is no leaving
//! it" — so a test that entered protocol mode to prove the veto would poison
//! every later test in the same binary, including the anti-vacuous twin that
//! has to show rendering happens when the veto is absent. `caps::probe`
//! already established this shape for exactly this reason; this mirrors it.
//!
//! The predicate is the same one [`Notice::emit`] applies, promoted rather
//! than re-derived: protocol mode is an absolute veto, and a process that may
//! not own a terminal line does not narrate into someone's captured log.

use crate::progress::{Commit, Durable, Frame, ProgressSink, Scrollback, TaskId};
use crate::tty::caps::{protocol_mode, LineCaps};
use crate::tty::widgets::{Level, Notice};
use crate::tty::Sink;

/// May a progress renderer put bytes on the terminal?
///
/// Pure, so the whole cartesian product is table-testable without a terminal
/// and without tripping the irreversible global. See the module docs.
#[must_use]
pub(crate) fn may_render(caps: LineCaps, protocol: bool) -> bool {
    // A protocol channel on fd 1 vetoes everything, on every platform — an
    // explicitly-supplied `Own` cannot pierce it, because fd 1 may be a
    // JSON-RPC wire.
    if protocol {
        return false;
    }
    caps.can_own()
}

/// Renders committed progress as permanent lines, through the ONE [`Notice`]
/// seam.
///
/// Transient frames are dropped rather than painted: the ephemeral row belongs
/// to whoever holds the [`LineLease`](crate::tty::LineLease) — today the
/// existing `Spinner` — and a second writer to that row is the exact defect
/// `tty` exists to prevent. This renderer commits; it does not animate.
#[derive(Debug)]
pub struct TerminalProgressSink {
    caps: LineCaps,
    sink: Sink,
    color: bool,
    scrollback: Scrollback,
}

impl TerminalProgressSink {
    /// A renderer that writes to `sink` when `caps` permits.
    ///
    /// `sink` is explicit and defaulted nowhere: relocating a stderr notice to
    /// stdout would break someone's `2>/dev/null`.
    #[must_use]
    pub fn new(caps: LineCaps, sink: Sink, color: bool) -> Self {
        Self {
            caps,
            sink,
            color,
            scrollback: Scrollback::new(),
        }
    }

    /// What has committed so far.
    #[must_use]
    pub fn committed(&self) -> &[Commit] {
        self.scrollback.committed()
    }

    /// The line a commit renders as — pure, no ANSI, no I/O.
    ///
    /// Split out for the same reason [`Notice::line`] is: the text contract is
    /// testable off the terminal, and the emitting half stays a thin adapter.
    #[must_use]
    pub fn line_of(commit: &Commit) -> String {
        Self::notice_of(commit).line()
    }

    /// The [`Notice`] a commit becomes. One mapping, so the semantics table in
    /// the module docs of [`crate::progress`] has exactly one implementation.
    fn notice_of(commit: &Commit) -> Notice<'_> {
        match commit {
            Commit::Started { label } => Notice::new(Level::Info, "▸", label.clone()).gap(2),
            Commit::Advanced(m) => {
                let text = match m.total {
                    Some(total) => format!("{}/{total}", m.done),
                    None => m.done.to_string(),
                };
                Notice::new(Level::Dim, "·", text).gap(2)
            }
            Commit::Note(n) => Notice::new(n.level, n.glyph, n.text.clone()).gap(2),
            Commit::Finished(outcome) => {
                let (level, glyph, text) = match outcome {
                    crate::progress::Outcome::Completed => (Level::Ok, "✓", "done"),
                    crate::progress::Outcome::Failed => (Level::Warn, "⚠", "failed"),
                    crate::progress::Outcome::Cancelled => (Level::Dim, "⧉", "cancelled"),
                };
                Notice::new(level, glyph, text).gap(2)
            }
        }
    }
}

impl ProgressSink for TerminalProgressSink {
    /// Frames are dropped, not painted.
    ///
    /// The ephemeral row belongs to whoever holds the
    /// [`LineLease`](crate::tty::LineLease) — today the existing `Spinner` —
    /// and a second writer to that row is the exact defect `tty` exists to
    /// prevent. This renderer commits; it does not animate. Note there is no
    /// code path here that *could* commit a frame: `Frame` has no conversion
    /// to [`Commit`].
    fn frame(&mut self, _task: TaskId, _at_ms: u64, _frame: &Frame) {}

    fn record(&mut self, _task: TaskId, _at_ms: u64, event: &Durable) {
        // The commit rule decides first, so a non-advancing snapshot cannot
        // reach the terminal even where the veto would have allowed bytes.
        let Some(commit) = self.scrollback.offer(event) else {
            return;
        };
        if !may_render(self.caps, protocol_mode()) {
            return;
        }
        Self::notice_of(&commit).emit(self.caps, self.sink, self.color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::{Measure, Note, Outcome};

    const TASK: TaskId = "compaction";

    /// **The veto**, over the whole product. Protocol mode refuses even a
    /// perfect terminal — that is the Windows JSON-RPC protection, where the
    /// fd-redirect is absent and this flag is the only thing between a
    /// progress line and a corrupted wire.
    #[test]
    fn protocol_mode_vetoes_rendering_from_every_capability() {
        let cases: &[(LineCaps, bool, bool)] = &[
            // (caps, protocol, may_render)
            (LineCaps::Own, false, true),
            (LineCaps::Own, true, false),
            (LineCaps::None, false, false),
            (LineCaps::None, true, false),
        ];
        for &(caps, protocol, want) in cases {
            assert_eq!(
                may_render(caps, protocol),
                want,
                "caps={caps:?} protocol={protocol}"
            );
        }
    }

    /// The anti-vacuous twin for the veto: without it, the predicate could be
    /// `false` always and the test above would still pass. Exactly one shape
    /// renders, and this names it.
    #[test]
    fn and_a_real_terminal_outside_protocol_mode_does_render() {
        assert!(
            may_render(LineCaps::Own, false),
            "a real terminal with no protocol guard is the ONE shape that renders — \
             if this fails the veto test above is vacuous"
        );
    }

    /// A sink that cannot own the line emits nothing, and — the part worth
    /// pinning — still *records*, so the contract is exercised identically on
    /// a headless tier. Observation does not depend on rendering.
    #[test]
    fn a_vetoed_sink_still_records_what_would_have_committed() {
        let mut s = TerminalProgressSink::new(LineCaps::None, Sink::Stderr, false);
        s.record(
            TASK,
            0,
            &Durable::Started {
                label: "compacting".into(),
            },
        );
        s.record(TASK, 1, &Durable::Finished(Outcome::Completed));
        assert_eq!(
            s.committed().len(),
            2,
            "the commit rule runs regardless of whether bytes are allowed"
        );
    }

    /// Frames are dropped before the veto is even consulted: the commit rule
    /// is the first gate, so animation cannot reach a terminal that would
    /// otherwise have accepted bytes.
    #[test]
    fn frames_never_reach_the_renderer_even_when_rendering_is_allowed() {
        let mut s = TerminalProgressSink::new(LineCaps::Own, Sink::Stderr, false);
        for t in 0..100 {
            s.frame(TASK, t, &Frame::new(format!("f{t}")));
        }
        assert!(
            s.committed().is_empty(),
            "frames committed: {:?}",
            s.committed()
        );
    }

    /// The rendered text is plain — no ANSI in the line contract. `newt-cli`'s
    /// `stdout_purity` asserts zero `\u{1b}` on a protocol wire and every
    /// widget is upstream of it.
    #[test]
    fn rendered_lines_carry_no_ansi_and_no_newline() {
        let commits = [
            Commit::Started {
                label: "compacting".into(),
            },
            Commit::Advanced(Measure::out_of(3, 10)),
            Commit::Advanced(Measure::of(7)),
            Commit::Note(Note {
                level: Level::Warn,
                glyph: "⚠",
                text: "trimmed".into(),
            }),
            Commit::Finished(Outcome::Completed),
            Commit::Finished(Outcome::Failed),
            Commit::Finished(Outcome::Cancelled),
        ];
        for c in &commits {
            let line = TerminalProgressSink::line_of(c);
            assert!(!line.contains('\u{1b}'), "ANSI in {line:?}");
            assert!(!line.contains('\n'), "newline in {line:?}");
            assert!(!line.is_empty(), "empty line for {c:?}");
        }
    }

    /// A measured advance renders its total when it has one, and does not
    /// invent one when it does not.
    #[test]
    fn an_advance_renders_its_total_only_when_it_has_one() {
        assert_eq!(
            TerminalProgressSink::line_of(&Commit::Advanced(Measure::out_of(3, 10))),
            "·  3/10"
        );
        assert_eq!(
            TerminalProgressSink::line_of(&Commit::Advanced(Measure::of(3))),
            "·  3"
        );
    }

    /// **No silent cutover** (constraint 5). This renderer must have no
    /// production caller in D2a — the cutover is a separate, named, per-family
    /// PR. Scans this crate's and `newt-tui`'s sources at COMPILE time, so the
    /// guard does no filesystem I/O; the needle is assembled with `concat!` so
    /// this file's own source cannot match it.
    #[test]
    fn no_production_path_renders_progress_yet() {
        const NEEDLE: &str = concat!("TerminalProgress", "Sink::new(");
        for (name, src) in [
            (
                "newt-core/src/agentic/driver.rs",
                include_str!("../agentic/driver.rs"),
            ),
            (
                "newt-core/src/agentic/display.rs",
                include_str!("../agentic/display.rs"),
            ),
            (
                "newt-core/src/agentic/note_sink.rs",
                include_str!("../agentic/note_sink.rs"),
            ),
            (
                "newt-core/src/tty/spinner.rs",
                include_str!("../tty/spinner.rs"),
            ),
        ] {
            assert!(
                !src.contains(NEEDLE),
                "{name} constructs the terminal progress renderer — D2a dual-publishes \
                 and the OLD path stays authoritative. Cutover is its own per-family PR \
                 with its own deletion gate (#1864 constraint 5)."
            );
        }
    }
}
