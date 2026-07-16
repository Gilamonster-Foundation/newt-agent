use std::io::{self, IsTerminal};

/// Returns `true` when stdout supports ANSI color.
///
/// Priority:
/// 1. `NO_COLOR` set (any value) → false  (<https://no-color.org/>)
/// 2. `TERM=dumb`                → false
/// 3. stdout is not a TTY        → false
/// 4. otherwise                  → true
pub fn color_supported() -> bool {
    color_supported_with(&|k| std::env::var(k).ok())
}

/// Back-compat/test shim: the effective on/off decision with no config layer
/// (`[tui] color` defaults to `Auto`). Honors `NEWT_COLOR` (set by `--color` /
/// `--mono`), then `NO_COLOR` / `TERM=dumb`, else `Auto` → `is_terminal()`.
fn color_supported_with(get_env: &dyn Fn(&str) -> Option<String>) -> bool {
    color_enabled_for(
        resolve_color_mode(get_env, newt_core::ColorMode::Auto),
        io::stdout().is_terminal(),
    )
}

/// Resolve the effective [`ColorMode`](newt_core::ColorMode) (issue #527) from,
/// in precedence order: `NEWT_COLOR` env (set by `--color` / `--mono`, the
/// explicit user request) > `NO_COLOR` > `TERM=dumb` > the `[tui] color` config
/// (`cfg_color`) > `Auto`.
///
/// An explicit `NEWT_COLOR` overrides `NO_COLOR` — the documented deviation (see
/// `docs/decisions/plain_scroller_tui.md`): if you ask for color on the command
/// line you get it. `NO_COLOR` / `TERM=dumb` still win over a *persisted* config
/// choice.
pub(crate) fn resolve_color_mode(
    get_env: &dyn Fn(&str) -> Option<String>,
    cfg_color: newt_core::ColorMode,
) -> newt_core::ColorMode {
    if let Some(kw) = get_env("NEWT_COLOR") {
        if let Some(mode) = newt_core::ColorMode::from_keyword(&kw) {
            return mode;
        }
    }
    if get_env("NO_COLOR").is_some() {
        return newt_core::ColorMode::Never;
    }
    if get_env("TERM").as_deref() == Some("dumb") {
        return newt_core::ColorMode::Never;
    }
    cfg_color
}

/// Apply a resolved mode against the terminal: [`ColorMode::forced`]
/// short-circuits (`always`/`never`/`mono`/theme variants); `Auto` defers to
/// `is_tty`.
pub(crate) fn color_enabled_for(mode: newt_core::ColorMode, is_tty: bool) -> bool {
    mode.forced().unwrap_or(is_tty)
}

#[cfg(test)]
mod tests {
    use super::*;
    use newt_core::ColorMode;

    fn mock_env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        |k| {
            pairs
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn no_color_env_disables_color() {
        assert!(!color_supported_with(&mock_env(&[("NO_COLOR", "1")])));
    }

    #[test]
    fn no_color_empty_value_still_disables() {
        assert!(!color_supported_with(&mock_env(&[("NO_COLOR", "")])));
    }

    #[test]
    fn dumb_term_disables_color() {
        assert!(!color_supported_with(&mock_env(&[("TERM", "dumb")])));
    }

    #[test]
    fn non_dumb_term_passes_env_check() {
        let get_env = mock_env(&[("TERM", "xterm-256color")]);
        let _ = color_supported_with(&get_env);
    }

    #[test]
    fn newt_color_env_wins_over_config() {
        // The flag (threaded as NEWT_COLOR) beats a persisted [tui] color.
        let env = mock_env(&[("NEWT_COLOR", "mono")]);
        assert_eq!(resolve_color_mode(&env, ColorMode::Always), ColorMode::Mono);
    }

    #[test]
    fn explicit_color_flag_overrides_no_color() {
        // Documented deviation: an explicit --color=always (NEWT_COLOR) beats
        // NO_COLOR. If you ask for color on the command line, you get it.
        let env = mock_env(&[("NEWT_COLOR", "always"), ("NO_COLOR", "1")]);
        assert_eq!(resolve_color_mode(&env, ColorMode::Auto), ColorMode::Always);
        assert!(color_enabled_for(ColorMode::Always, false));
    }

    #[test]
    fn no_color_beats_persisted_config_but_not_the_flag() {
        // Runtime NO_COLOR wins over a persisted [tui] color = "dark"…
        let env = mock_env(&[("NO_COLOR", "1")]);
        assert_eq!(resolve_color_mode(&env, ColorMode::Dark), ColorMode::Never);
        // …and TERM=dumb does too.
        let env = mock_env(&[("TERM", "dumb")]);
        assert_eq!(resolve_color_mode(&env, ColorMode::Dark), ColorMode::Never);
    }

    #[test]
    fn config_color_used_when_no_env_signal() {
        let env = mock_env(&[]);
        assert_eq!(resolve_color_mode(&env, ColorMode::Light), ColorMode::Light);
        // Default config (Auto) falls through to Auto.
        assert_eq!(resolve_color_mode(&env, ColorMode::Auto), ColorMode::Auto);
    }

    #[test]
    fn invalid_newt_color_is_ignored_and_falls_through() {
        // A garbage NEWT_COLOR doesn't hijack the decision; the chain continues.
        let env = mock_env(&[("NEWT_COLOR", "rainbow"), ("NO_COLOR", "1")]);
        assert_eq!(resolve_color_mode(&env, ColorMode::Auto), ColorMode::Never);
    }

    #[test]
    fn color_enabled_for_applies_mode_against_tty() {
        assert!(color_enabled_for(ColorMode::Always, false));
        assert!(!color_enabled_for(ColorMode::Never, true));
        assert!(!color_enabled_for(ColorMode::Mono, true));
        assert!(color_enabled_for(ColorMode::Dark, false));
        // Auto defers to the terminal.
        assert!(color_enabled_for(ColorMode::Auto, true));
        assert!(!color_enabled_for(ColorMode::Auto, false));
    }
}
