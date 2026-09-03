//! newt-tui's view of the ONE role table.
//!
//! The table itself lives in [`newt_core::tty::theme`], and this file is the
//! adapter that lets a ratatui surface read it.
//!
//! # Why it moved
//!
//! It started here, and being here made it a half-theme. `theme::Role` was
//! `pub(crate)` in newt-tui while the lean modal, every dim notice and every
//! tool header render in newt-core — and the dependency runs newt-tui →
//! newt-core, so those could never ask. `NEWT_THEME='dim=gray'` retinted the
//! footer clock and left tool output alone, which reads as a rendering bug
//! rather than as a theme.
//!
//! Worse, the split forced a second source of truth: the table wrote out
//! `Rgb(255, 165, 90)` as a literal because it could not name
//! `newt_core::tty::ACTIVE_INPUT_CT`, which is that exact colour. A colour
//! table whose own existence is justified by "thirty-odd literals in nine
//! files" cannot itself hold a copy of one.
//!
//! # The seam
//!
//! The table is typed over `crossterm::style::Color`, because that is the
//! colour vocabulary newt-core already speaks and the one the lean/wyvern tier
//! already links — newt-core has no ratatui dependency and must not grow one.
//! [`color`] is the single place the two libraries meet.

use ratatui::style::Color as RtColor;

pub(crate) use newt_core::tty::theme::{active, complaints, Role};

// The table's OWN tests moved with it to newt-core; what stays here tests the
// adapter, which needs only these two.
#[cfg(test)]
use newt_core::tty::theme::{role_name, ALL_ROLES};

/// The colour for a role, as ratatui wants it.
///
/// The whole adapter, and the ONLY place a crossterm colour becomes a ratatui
/// one. Call sites read `theme::color(Role::Dim)` rather than
/// `theme::active().color(Role::Dim)`, so adopting the shared table made them
/// shorter rather than longer.
pub(crate) fn color(role: Role) -> RtColor {
    to_ratatui(active().color(role))
}

/// crossterm → ratatui.
///
/// **Not a spelling exercise.** The two libraries swap the dark and light
/// halves of the ANSI 16: crossterm's `Green` is ANSI 10 (bright) while
/// ratatui's `Green` is ANSI 2 (dark), and each names the other's green
/// `DarkGreen` / `LightGreen`. Mapping by name would silently recolour every
/// surface, which is the failure `the_builtin_theme_preserves_every_colour_in_use`
/// exists to catch.
fn to_ratatui(color: crossterm::style::Color) -> RtColor {
    use crossterm::style::Color as Ct;
    match color {
        Ct::Reset => RtColor::Reset,
        Ct::Black => RtColor::Black,
        Ct::DarkRed => RtColor::Red,
        Ct::Red => RtColor::LightRed,
        Ct::DarkGreen => RtColor::Green,
        Ct::Green => RtColor::LightGreen,
        Ct::DarkYellow => RtColor::Yellow,
        Ct::Yellow => RtColor::LightYellow,
        Ct::DarkBlue => RtColor::Blue,
        Ct::Blue => RtColor::LightBlue,
        Ct::DarkMagenta => RtColor::Magenta,
        Ct::Magenta => RtColor::LightMagenta,
        Ct::DarkCyan => RtColor::Cyan,
        Ct::Cyan => RtColor::LightCyan,
        Ct::Grey => RtColor::Gray,
        Ct::DarkGrey => RtColor::DarkGray,
        Ct::White => RtColor::White,
        Ct::Rgb { r, g, b } => RtColor::Rgb(r, g, b),
        Ct::AnsiValue(i) => RtColor::Indexed(i),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The adapter must survive the round trip for every name, or a theme file
    /// says one colour and the surface paints another.
    ///
    /// The dark/light swap is the whole hazard: crossterm `Green` is ANSI 10
    /// and ratatui `Green` is ANSI 2. Both directions are asserted so a
    /// name-for-name "simplification" of this table fails loudly.
    #[test]
    fn the_adapter_crosses_the_dark_light_swap_rather_than_matching_names() {
        use crossterm::style::Color as Ct;

        // The trap, stated directly.
        assert_eq!(to_ratatui(Ct::Green), RtColor::LightGreen);
        assert_ne!(to_ratatui(Ct::Green), RtColor::Green);
        assert_eq!(to_ratatui(Ct::DarkGreen), RtColor::Green);
        assert_eq!(to_ratatui(Ct::Red), RtColor::LightRed);
        assert_eq!(to_ratatui(Ct::DarkRed), RtColor::Red);
        assert_eq!(to_ratatui(Ct::Yellow), RtColor::LightYellow);
        assert_eq!(to_ratatui(Ct::DarkYellow), RtColor::Yellow);

        // The two greys, which are the same word with different spellings and
        // are the reason this module exists at all.
        assert_eq!(to_ratatui(Ct::Grey), RtColor::Gray);
        assert_eq!(to_ratatui(Ct::DarkGrey), RtColor::DarkGray);

        // The shapes that differ structurally, not just by name.
        assert_eq!(
            to_ratatui(Ct::Rgb { r: 1, g: 2, b: 3 }),
            RtColor::Rgb(1, 2, 3)
        );
        assert_eq!(to_ratatui(Ct::AnsiValue(42)), RtColor::Indexed(42));
    }

    /// The colours a ratatui surface actually paints are unchanged by the move
    /// across the crate boundary. Site for site, this is the same assertion
    /// `newt_core::tty::theme` makes about its own table — made again on THIS
    /// side of the adapter, because a correct table plus a wrong adapter still
    /// recolours the product.
    #[test]
    fn the_adapter_preserves_every_colour_a_surface_asks_for() {
        assert_eq!(color(Role::Accent), RtColor::Rgb(255, 165, 90));
        assert_eq!(color(Role::Thinking), RtColor::Rgb(255, 165, 90));
        assert_eq!(color(Role::Dim), RtColor::DarkGray);
        assert_eq!(color(Role::Muted), RtColor::Gray);
        assert_eq!(color(Role::Emphasis), RtColor::LightYellow);
        assert_eq!(color(Role::Text), RtColor::White);
        assert_eq!(color(Role::CommandBang), RtColor::LightMagenta);
        assert_eq!(color(Role::CommandEx), RtColor::LightYellow);
        assert_eq!(color(Role::GaugeOk), RtColor::Green);
        assert_eq!(color(Role::GaugeCritical), RtColor::Red);
        assert_eq!(color(Role::SelectedLabel), RtColor::Cyan);
        assert_eq!(color(Role::SelectedValue), RtColor::Yellow);
        assert_eq!(color(Role::Ok), RtColor::Green);
        assert_eq!(color(Role::Identity), RtColor::Magenta);
        assert_eq!(color(Role::ModalBorder), RtColor::Rgb(255, 165, 90));
        assert_eq!(color(Role::ModalTitle), RtColor::LightYellow);
    }

    /// Every role crosses the adapter without falling into a hole. A role that
    /// mapped to `Reset` would render as "whatever the terminal last set",
    /// which is the un-styled bug the theme exists to end.
    #[test]
    fn no_role_crosses_the_adapter_into_reset() {
        for role in ALL_ROLES {
            assert_ne!(
                color(*role),
                RtColor::Reset,
                "`{}` lost its colour crossing into ratatui",
                role_name(*role)
            );
        }
    }
}
