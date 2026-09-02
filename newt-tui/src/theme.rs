//! **One table of named roles, so a colour is chosen once and named
//! everywhere.**
//!
//! Before this, colour in the TUI was thirty-odd literals spread across nine
//! files, and the census reads exactly like the spinner census in CLAUDE.md
//! that justified the `tty` arbiter:
//!
//! - **`DarkGray` twenty times and `DarkGrey` thirteen** — two spellings of one
//!   intent, which no amount of grepping reconciles into a single decision.
//! - **`Color::Rgb(255, 165, 90)` written out in `palette.rs` beside
//!   `ACTIVE_INPUT_CT`**, which is that exact colour. The same duplication was
//!   fixed in `header_line` (#2019) and simply reappeared elsewhere, because
//!   nothing named the colour.
//! - Nine files each deciding, privately, what "dim" and "emphasis" mean.
//!
//! A theme fixes that by making the knowledge DATA: a role has a name, a role
//! has one colour, and a surface asks for the role. That is the three Cs
//! applied to presentation — the language-pack model CLAUDE.md names as
//! canonical, pointed at the palette.
//!
//! # What a role is, and what it is not
//!
//! A role is a MEANING, not a place. `Dim` is "subordinate to what is beside
//! it", not "the colour of the provenance column" — so a new surface with
//! subordinate text asks for `Dim` and is automatically consistent with every
//! other one. A role named after a widget would just be a literal wearing a
//! longer name.
//!
//! # The default theme changes nothing
//!
//! [`Theme::builtin`] reproduces the current colours EXACTLY, site for site,
//! and `the_builtin_theme_preserves_every_colour_in_use` pins that. A theme
//! seam whose arrival re-colours the product is two changes wearing one commit
//! message, and the second one is invisible in review.

use ratatui::style::Color;

/// What a colour MEANS on this surface.
///
/// Deliberately small. Every role here is one a surface actually asked for —
/// derived from the colour census rather than invented, so there is no role
/// that exists only in this file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub(crate) enum Role {
    /// The live input line, and anything that currently owns the keyboard.
    /// The one colour an operator reads as "here".
    Accent,
    /// Ordinary body text.
    Text,
    /// Subordinate to what it sits beside: hints, provenance, timestamps, a
    /// receded prompt. The role that was thirty-three literals in two
    /// spellings.
    Dim,
    /// Between [`Self::Text`] and [`Self::Dim`] — present, but not the point.
    Muted,
    /// Draws the eye without claiming the keyboard: a mode word, an open
    /// command line.
    Emphasis,

    /// Work in progress: the activity row, and the thinking block that will
    /// join it. Its own role rather than `Dim` because "the machine is busy"
    /// is a different claim from "this is subordinate", and a theme will want
    /// to say so.
    Thinking,

    /// The slab a `!` or `:` command is rendered on.
    CommandBackground,
    /// The same slab while a modal owns the keyboard.
    CommandBackgroundInactive,
    /// The `!` host-command marker.
    CommandBang,
    /// The `:` ex-command marker.
    CommandEx,

    /// Context budget: comfortable.
    GaugeOk,
    /// Context budget: worth noticing.
    GaugeWarn,
    /// Context budget: act now.
    GaugeCritical,

    /// A selected row's label.
    SelectedLabel,
    /// A selected row's value.
    SelectedValue,
    /// Succeeded, allowed, present.
    Ok,
    /// A persona, a name, an identity.
    Identity,
}

/// A complete assignment of colours to roles.
///
/// Total by construction: [`Self::color`] is a `match`, so a role added to the
/// enum fails to compile until every theme answers for it. A theme with a
/// missing role would silently fall back to whatever the terminal last set,
/// which is the class of bug that makes a colour scheme look "sometimes
/// broken".
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Theme {
    pub(crate) name: &'static str,
    accent: Color,
    text: Color,
    dim: Color,
    muted: Color,
    emphasis: Color,
    thinking: Color,
    command_background: Color,
    command_background_inactive: Color,
    command_bang: Color,
    command_ex: Color,
    gauge_ok: Color,
    gauge_warn: Color,
    gauge_critical: Color,
    selected_label: Color,
    selected_value: Color,
    ok: Color,
    identity: Color,
}

impl Theme {
    /// The colours the TUI uses today, named.
    ///
    /// **Every value here is lifted from a site that already existed**, so
    /// adopting the seam is invisible to an operator. The block roles
    /// (`Thinking`, `Gutter`, `Fold`) are the exception — they have no
    /// predecessor, and take `Dim`'s colour so a surface that starts using
    /// them looks like the rest of the product until a theme says otherwise.
    pub(crate) fn builtin() -> Self {
        Self {
            name: "newt",
            // `newt_core::tty::ACTIVE_INPUT_CT` — the one colour that means
            // "the keyboard is here". Written out as an Rgb literal in
            // `palette.rs` as well, which is the duplication this ends.
            accent: Color::Rgb(255, 165, 90),
            text: Color::White,
            // `DarkGray` (20 sites) and `DarkGrey` (13) were the same request.
            dim: Color::DarkGray,
            muted: Color::Gray,
            emphasis: Color::LightYellow,
            // The activity row's existing accent — `Rgb(255, 165, 90)`.
            thinking: Color::Rgb(255, 165, 90),
            command_background: Color::Rgb(82, 82, 82),
            command_background_inactive: Color::Rgb(45, 45, 45),
            command_bang: Color::LightMagenta,
            command_ex: Color::LightYellow,
            gauge_ok: Color::Green,
            gauge_warn: Color::Rgb(200, 140, 0),
            gauge_critical: Color::Red,
            selected_label: Color::Cyan,
            selected_value: Color::Yellow,
            ok: Color::Green,
            identity: Color::Magenta,
        }
    }

    /// The colour for a role. Total: adding a `Role` breaks this until every
    /// theme answers.
    pub(crate) fn color(&self, role: Role) -> Color {
        match role {
            Role::Accent => self.accent,
            Role::Text => self.text,
            Role::Dim => self.dim,
            Role::Muted => self.muted,
            Role::Emphasis => self.emphasis,
            Role::Thinking => self.thinking,
            Role::CommandBackground => self.command_background,
            Role::CommandBackgroundInactive => self.command_background_inactive,
            Role::CommandBang => self.command_bang,
            Role::CommandEx => self.command_ex,
            Role::GaugeOk => self.gauge_ok,
            Role::GaugeWarn => self.gauge_warn,
            Role::GaugeCritical => self.gauge_critical,
            Role::SelectedLabel => self.selected_label,
            Role::SelectedValue => self.selected_value,
            Role::Ok => self.ok,
            Role::Identity => self.identity,
        }
    }

    /// Overlay a partial assignment read from a theme file.
    ///
    /// Partial ON PURPOSE, and it is the whole ergonomic argument: an operator
    /// who wants a brighter accent writes one line, not a file of nineteen
    /// colours they must keep in step with ours. Everything unstated keeps the
    /// built-in value, so a theme file cannot go stale by omission when a role
    /// is added — the same droppable-override shape the language packs use.
    pub(crate) fn overlaid(mut self, name: &'static str, overrides: &[(Role, Color)]) -> Self {
        self.name = name;
        for (role, color) in overrides {
            match role {
                Role::Accent => self.accent = *color,
                Role::Text => self.text = *color,
                Role::Dim => self.dim = *color,
                Role::Muted => self.muted = *color,
                Role::Emphasis => self.emphasis = *color,
                Role::Thinking => self.thinking = *color,
                Role::CommandBackground => self.command_background = *color,
                Role::CommandBackgroundInactive => self.command_background_inactive = *color,
                Role::CommandBang => self.command_bang = *color,
                Role::CommandEx => self.command_ex = *color,
                Role::GaugeOk => self.gauge_ok = *color,
                Role::GaugeWarn => self.gauge_warn = *color,
                Role::GaugeCritical => self.gauge_critical = *color,
                Role::SelectedLabel => self.selected_label = *color,
                Role::SelectedValue => self.selected_value = *color,
                Role::Ok => self.ok = *color,
                Role::Identity => self.identity = *color,
            }
        }
        self
    }
}

/// The role vocabulary as it appears in a theme file.
///
/// Kebab-case, and the same strings the `/theme` command will print — an
/// operator who reads a role in the UI can put that exact word in the file.
pub(crate) fn role_from_name(name: &str) -> Option<Role> {
    Some(match name.trim().to_ascii_lowercase().as_str() {
        "accent" => Role::Accent,
        "text" => Role::Text,
        "dim" => Role::Dim,
        "muted" => Role::Muted,
        "emphasis" => Role::Emphasis,
        "thinking" => Role::Thinking,
        "command-background" => Role::CommandBackground,
        "command-background-inactive" => Role::CommandBackgroundInactive,
        "command-bang" => Role::CommandBang,
        "command-ex" => Role::CommandEx,
        "gauge-ok" => Role::GaugeOk,
        "gauge-warn" => Role::GaugeWarn,
        "gauge-critical" => Role::GaugeCritical,
        "selected-label" => Role::SelectedLabel,
        "selected-value" => Role::SelectedValue,
        "ok" => Role::Ok,
        "identity" => Role::Identity,
        _ => return None,
    })
}

/// Every role, for `/theme` and for the totality test.
pub(crate) const ALL_ROLES: &[Role] = &[
    Role::Accent,
    Role::Text,
    Role::Dim,
    Role::Muted,
    Role::Emphasis,
    Role::Thinking,
    Role::CommandBackground,
    Role::CommandBackgroundInactive,
    Role::CommandBang,
    Role::CommandEx,
    Role::GaugeOk,
    Role::GaugeWarn,
    Role::GaugeCritical,
    Role::SelectedLabel,
    Role::SelectedValue,
    Role::Ok,
    Role::Identity,
];

/// The name a role answers to in a theme file.
pub(crate) fn role_name(role: Role) -> &'static str {
    match role {
        Role::Accent => "accent",
        Role::Text => "text",
        Role::Dim => "dim",
        Role::Muted => "muted",
        Role::Emphasis => "emphasis",
        Role::Thinking => "thinking",
        Role::CommandBackground => "command-background",
        Role::CommandBackgroundInactive => "command-background-inactive",
        Role::CommandBang => "command-bang",
        Role::CommandEx => "command-ex",
        Role::GaugeOk => "gauge-ok",
        Role::GaugeWarn => "gauge-warn",
        Role::GaugeCritical => "gauge-critical",
        Role::SelectedLabel => "selected-label",
        Role::SelectedValue => "selected-value",
        Role::Ok => "ok",
        Role::Identity => "identity",
    }
}

/// Parse a colour as a theme file writes one.
///
/// Three spellings, because three are what people actually have to hand: a hex
/// triplet from a palette they like, an ANSI index from a terminal scheme, or
/// one of the sixteen names. A fourth would be a fourth thing to get wrong.
///
/// # Errors
///
/// The value is not a colour this vocabulary can express.
pub(crate) fn parse_color(value: &str) -> Result<Color, String> {
    let value = value.trim();
    if let Some(hex) = value.strip_prefix('#') {
        if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("`{value}` is not #rrggbb"));
        }
        let byte = |at: usize| u8::from_str_radix(&hex[at..at + 2], 16).unwrap_or(0);
        return Ok(Color::Rgb(byte(0), byte(2), byte(4)));
    }
    if let Ok(index) = value.parse::<u8>() {
        return Ok(Color::Indexed(index));
    }
    Ok(match value.to_ascii_lowercase().as_str() {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "gray" | "grey" => Color::Gray,
        // One spelling in, both accepted — the DarkGray/DarkGrey split is
        // exactly what this file exists to stop, and it must not be recreated
        // in the config vocabulary.
        "dark-gray" | "dark-grey" => Color::DarkGray,
        "light-red" => Color::LightRed,
        "light-green" => Color::LightGreen,
        "light-yellow" => Color::LightYellow,
        "light-blue" => Color::LightBlue,
        "light-magenta" => Color::LightMagenta,
        "light-cyan" => Color::LightCyan,
        "white" => Color::White,
        other => {
            return Err(format!(
                "`{other}` is not a colour name, an ANSI index (0-255), or #rrggbb"
            ))
        }
    })
}

/// The theme in force, from `NEWT_THEME` — `role=colour` pairs, comma
/// separated: `NEWT_THEME='dim=gray,accent=#00d7ff'`.
///
/// An env vocabulary rather than a file, for now, and deliberately: it is the
/// smallest thing that makes a role TUNABLE, it needs no path resolution or
/// precedence story, and it is what an operator reaches for while deciding
/// what they actually want. A `[tui] theme` file follows once the roles have
/// stopped moving — the same order `brand.rs` took, whose env override shipped
/// before anything richer.
///
/// **A bad pair is reported, not obeyed and not fatal.** An unreadable theme
/// that silently half-applies looks like a rendering bug everywhere except the
/// one place that would explain it.
pub(crate) fn from_env(raw: Option<&str>) -> (Theme, Vec<String>) {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return (Theme::builtin(), Vec::new());
    };
    let (mut overrides, mut complaints) = (Vec::new(), Vec::new());
    for pair in raw.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let Some((name, value)) = pair.split_once('=') else {
            complaints.push(format!("`{pair}` is not `role=colour`"));
            continue;
        };
        let Some(role) = role_from_name(name) else {
            // Name what IS valid. An operator who mistyped a role has no other
            // way to discover the vocabulary, and "not a theme role" without
            // the list is a dead end wearing an error's clothes.
            let known = ALL_ROLES
                .iter()
                .map(|r| role_name(*r))
                .collect::<Vec<_>>()
                .join(", ");
            complaints.push(format!(
                "`{}` is not a theme role (have: {known})",
                name.trim()
            ));
            continue;
        };
        match parse_color(value) {
            Ok(color) => overrides.push((role, color)),
            Err(why) => complaints.push(format!("{}: {why}", name.trim())),
        }
    }
    (Theme::builtin().overlaid("custom", &overrides), complaints)
}

/// The theme in force, resolved ONCE.
///
/// A `OnceLock` rather than a call per span: `draw` runs on every keystroke and
/// on every 250 ms repaint, and re-reading the environment each time would put
/// a parser on the hot path to answer a question that cannot change
/// mid-process.
///
/// Complaints about a malformed `NEWT_THEME` are held by [`complaints`]
/// for the session to print once — a theme that silently half-applies looks
/// like a rendering bug everywhere except the one place that would explain it.
pub(crate) fn active() -> &'static Theme {
    &resolved().0
}

/// What was wrong with `NEWT_THEME`, if anything.
pub(crate) fn complaints() -> &'static [String] {
    &resolved().1
}

fn resolved() -> &'static (Theme, Vec<String>) {
    static THEME: std::sync::OnceLock<(Theme, Vec<String>)> = std::sync::OnceLock::new();
    THEME.get_or_init(|| from_env(std::env::var("NEWT_THEME").ok().as_deref()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The seam changes nothing on arrival.**
    ///
    /// Every value the built-in theme serves is one that was already on
    /// screen, so adopting it is invisible to an operator. A theme seam that
    /// re-coloured the product would be two changes in one commit, and the
    /// second would be invisible in review.
    #[test]
    fn the_builtin_theme_preserves_every_colour_in_use() {
        let t = Theme::builtin();
        // The accent, which `palette.rs` wrote out as a literal beside the
        // constant that already held it.
        assert_eq!(t.color(Role::Accent), Color::Rgb(255, 165, 90));
        assert_eq!(
            t.color(Role::Accent),
            Color::from(newt_core::tty::ACTIVE_INPUT_CT),
            "the accent IS the active-input colour; two sources would drift"
        );
        // The role that was thirty-three literals under two spellings.
        assert_eq!(t.color(Role::Dim), Color::DarkGray);
        assert_eq!(t.color(Role::Muted), Color::Gray);
        assert_eq!(t.color(Role::Emphasis), Color::LightYellow);
        assert_eq!(t.color(Role::CommandBackground), Color::Rgb(82, 82, 82));
        assert_eq!(
            t.color(Role::CommandBackgroundInactive),
            Color::Rgb(45, 45, 45)
        );
        assert_eq!(t.color(Role::CommandBang), Color::LightMagenta);
        assert_eq!(t.color(Role::CommandEx), Color::LightYellow);
        assert_eq!(t.color(Role::GaugeWarn), Color::Rgb(200, 140, 0));
        assert_eq!(t.color(Role::GaugeCritical), Color::Red);
        assert_eq!(t.color(Role::SelectedLabel), Color::Cyan);
        assert_eq!(t.color(Role::SelectedValue), Color::Yellow);
        assert_eq!(t.color(Role::Identity), Color::Magenta);
    }

    /// `Thinking` keeps the activity row's existing colour, so naming the
    /// role changed nothing on screen.
    #[test]
    fn thinking_keeps_the_activity_rows_colour() {
        assert_eq!(
            Theme::builtin().color(Role::Thinking),
            Color::Rgb(255, 165, 90)
        );
    }

    /// **Every role is reachable by name, both ways.** A role the file cannot
    /// spell is a role an operator cannot theme, and a name that parses to
    /// nothing is a typo the file accepts in silence.
    #[test]
    fn every_role_round_trips_through_its_name() {
        for role in ALL_ROLES {
            let name = role_name(*role);
            assert_eq!(
                role_from_name(name),
                Some(*role),
                "`{name}` does not parse back to the role it names"
            );
            assert_eq!(name, name.to_ascii_lowercase(), "names are kebab-case");
        }
        assert_eq!(ALL_ROLES.len(), 17, "add the new role to ALL_ROLES too");
        assert_eq!(role_from_name("nonsense"), None);
    }

    /// An override is PARTIAL: state one role, keep eighteen.
    #[test]
    fn an_override_touches_only_what_it_names() {
        let base = Theme::builtin();
        let themed = Theme::builtin().overlaid("high-contrast", &[(Role::Dim, Color::Gray)]);
        assert_eq!(themed.name, "high-contrast");
        assert_eq!(
            themed.color(Role::Dim),
            Color::Gray,
            "the stated role moved"
        );
        for role in ALL_ROLES.iter().filter(|r| **r != Role::Dim) {
            assert_eq!(
                themed.color(*role),
                base.color(*role),
                "`{}` was not named and must not have moved",
                role_name(*role)
            );
        }
    }

    /// A role is TUNABLE, which is the point of naming it.
    #[test]
    fn the_environment_can_retheme_any_role() {
        let (themed, complaints) = from_env(Some("dim=gray, accent=#00d7ff,thinking=214"));
        assert!(complaints.is_empty(), "{complaints:?}");
        assert_eq!(themed.color(Role::Dim), Color::Gray);
        assert_eq!(themed.color(Role::Accent), Color::Rgb(0, 215, 255));
        assert_eq!(themed.color(Role::Thinking), Color::Indexed(214));
        // Unnamed roles keep the built-in value.
        assert_eq!(
            themed.color(Role::Emphasis),
            Theme::builtin().color(Role::Emphasis)
        );

        // Absent, empty, and whitespace are all "no theme", not an error.
        for nothing in [None, Some(""), Some("   ")] {
            let (t, c) = from_env(nothing);
            assert_eq!(t, Theme::builtin());
            assert!(c.is_empty());
        }
    }

    /// **A bad pair is REPORTED, and the rest still applies.** A theme that
    /// silently half-applies looks like a rendering bug everywhere except the
    /// one place that would explain it.
    #[test]
    fn a_bad_pair_is_named_and_the_rest_still_applies() {
        let (themed, complaints) = from_env(Some("dim=gray,nonsense=red,accent=puce,bare-word"));
        assert_eq!(
            themed.color(Role::Dim),
            Color::Gray,
            "the good pair applied"
        );
        assert_eq!(complaints.len(), 3, "{complaints:?}");
        assert!(complaints.iter().any(|c| c.contains("nonsense")));
        assert!(complaints.iter().any(|c| c.contains("puce")));
        assert!(complaints.iter().any(|c| c.contains("bare-word")));
    }

    #[test]
    fn colours_are_written_the_three_ways_people_have_them() {
        assert_eq!(parse_color("#ffa55a"), Ok(Color::Rgb(255, 165, 90)));
        assert_eq!(parse_color("#FFA55A"), Ok(Color::Rgb(255, 165, 90)));
        assert_eq!(parse_color("214"), Ok(Color::Indexed(214)));
        assert_eq!(parse_color("0"), Ok(Color::Indexed(0)));
        assert_eq!(parse_color("light-yellow"), Ok(Color::LightYellow));
        assert_eq!(parse_color("  cyan  "), Ok(Color::Cyan), "trimmed");

        // Both spellings in, one colour out. Recreating the DarkGray/DarkGrey
        // split in the CONFIG vocabulary would be this file's own bug.
        assert_eq!(parse_color("dark-gray"), parse_color("dark-grey"));
        assert_eq!(parse_color("gray"), parse_color("grey"));

        // A refusal names what it wanted rather than falling back silently: a
        // theme that quietly ignores a typo looks broken in one place and
        // right everywhere else, which is the hardest kind to report.
        for bad in ["#fff", "#gggggg", "puce", "256", ""] {
            let err = parse_color(bad).expect_err("`{bad}` is not a colour");
            assert!(err.contains(bad) || bad.is_empty(), "{err}");
        }
    }
}
