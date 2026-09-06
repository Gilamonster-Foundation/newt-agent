//! Width policy for automatic and configured rich-input gutters.

// Opt-in wide-gutter width (`NEWT_GUTTER=auto`/`tui.gutter=N`): a fixed left
// column for the input-row indicator. Since #527 the clock/mode/model live on the
// status header row, so the gutter only carries `❯`/`:` — this stays a generous
// fixed width for the opt-in aligned layout; the default is the 1-col overhang.
pub(super) const GUTTER_W: u16 = 19;
/// Auto-gutter threshold: use the left gutter only while it stays under this
/// fraction of the terminal width; on a squished terminal, drop it and stack
/// the prompt on its own line. (A `[tui] gutter = auto|on|off` setting later.)
const GUTTER_MAX_FRACTION: f32 = 0.33;

pub(super) fn use_gutter(width: u16) -> bool {
    width > 0 && (GUTTER_W as f32) <= GUTTER_MAX_FRACTION * width as f32
}

/// Resolve the effective gutter / input-indent width (columns) from the
/// `[tui] gutter` setting and the terminal width:
/// - `None` (auto): a prompt-width gutter (`GUTTER_W`) when it stays under ~1/3
///   of the width, else `0` (stacked prompt).
/// - `Some(n)`: exactly `n`, clamped so it can't consume the whole line.
///
/// A result `>= GUTTER_W` is wide enough to hold the inline prompt; a smaller
/// result (including `0`) stacks the prompt on its own row and indents the input
/// that many columns.
pub(super) fn resolve_gutter(setting: Option<u16>, width: u16) -> u16 {
    match setting {
        None => {
            if use_gutter(width) {
                GUTTER_W
            } else {
                0
            }
        }
        Some(n) => n.min(width.saturating_sub(1)),
    }
}
