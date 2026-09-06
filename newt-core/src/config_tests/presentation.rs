use super::*;

// Display/footer/markdown/thinking modes and spill height.

#[test]
fn footer_mode_defaults_to_auto_and_round_trips() {
    // Absent key → Auto (the amphibious default).
    let cfg: TuiConfig = toml::from_str("").unwrap();
    assert_eq!(cfg.footer, FooterMode::Auto);
    // Each variant parses from its snake_case key.
    for (key, want) in [
        ("auto", FooterMode::Auto),
        ("on", FooterMode::On),
        ("off", FooterMode::Off),
    ] {
        let cfg: TuiConfig = toml::from_str(&format!("footer = \"{key}\"")).unwrap();
        assert_eq!(cfg.footer, want, "footer = {key}");
    }
}

// ── color / theme mode (issue #527) ─────────────────────────────────

#[test]
fn color_mode_defaults_to_auto_and_round_trips() {
    // Absent key → Auto (color on a TTY, none off one).
    let cfg: TuiConfig = toml::from_str("").unwrap();
    assert_eq!(cfg.color, ColorMode::Auto);
    // Every keyword parses from its serde (lowercase) key.
    for (key, want) in [
        ("auto", ColorMode::Auto),
        ("always", ColorMode::Always),
        ("never", ColorMode::Never),
        ("minimal", ColorMode::Minimal),
        ("inverted", ColorMode::Inverted),
        ("dark", ColorMode::Dark),
        ("light", ColorMode::Light),
        ("mono", ColorMode::Mono),
    ] {
        let cfg: TuiConfig = toml::from_str(&format!("color = \"{key}\"")).unwrap();
        assert_eq!(cfg.color, want, "color = {key}");
    }
}

#[test]
fn color_mode_keyword_round_trips_and_aliases_parse() {
    // keyword() is the inverse of from_keyword() for every canonical variant.
    for m in [
        ColorMode::Auto,
        ColorMode::Always,
        ColorMode::Never,
        ColorMode::Minimal,
        ColorMode::Inverted,
        ColorMode::Dark,
        ColorMode::Light,
        ColorMode::Mono,
    ] {
        assert_eq!(ColorMode::from_keyword(m.keyword()), Some(m));
    }
    // Case-insensitive + aliases.
    assert_eq!(ColorMode::from_keyword("ALWAYS"), Some(ColorMode::Always));
    assert_eq!(ColorMode::from_keyword(" on "), Some(ColorMode::Always));
    assert_eq!(ColorMode::from_keyword("off"), Some(ColorMode::Never));
    assert_eq!(ColorMode::from_keyword("monochrome"), Some(ColorMode::Mono));
    // Unknown keyword is rejected (the CLI value_parser surfaces this).
    assert_eq!(ColorMode::from_keyword("rainbow"), None);
}

#[test]
fn color_mode_forced_and_is_mono() {
    // forced(): Some(true) = color on, Some(false) = off, None = defer to TTY.
    assert_eq!(ColorMode::Always.forced(), Some(true));
    assert_eq!(ColorMode::Dark.forced(), Some(true));
    assert_eq!(ColorMode::Light.forced(), Some(true));
    assert_eq!(ColorMode::Inverted.forced(), Some(true));
    assert_eq!(ColorMode::Minimal.forced(), Some(true));
    assert_eq!(ColorMode::Never.forced(), Some(false));
    assert_eq!(ColorMode::Mono.forced(), Some(false));
    assert_eq!(ColorMode::Auto.forced(), None);
    // is_mono distinguishes the ASCII-fallback mode from plain Never.
    assert!(ColorMode::Mono.is_mono());
    assert!(!ColorMode::Never.is_mono());
    assert!(!ColorMode::Auto.is_mono());
}

#[test]
fn markdown_mode_defaults_to_auto_round_trips_and_forces() {
    assert_eq!(MarkdownMode::default(), MarkdownMode::Auto);
    for m in [MarkdownMode::Auto, MarkdownMode::On, MarkdownMode::Off] {
        assert_eq!(MarkdownMode::from_keyword(m.keyword()), Some(m));
    }
    // Case-insensitive + always/never aliases.
    assert_eq!(MarkdownMode::from_keyword("ON"), Some(MarkdownMode::On));
    assert_eq!(
        MarkdownMode::from_keyword(" always "),
        Some(MarkdownMode::On)
    );
    assert_eq!(MarkdownMode::from_keyword("never"), Some(MarkdownMode::Off));
    assert_eq!(MarkdownMode::from_keyword("rainbow"), None);
    // forced(): On = Some(true), Off = Some(false), Auto = defer.
    assert_eq!(MarkdownMode::On.forced(), Some(true));
    assert_eq!(MarkdownMode::Off.forced(), Some(false));
    assert_eq!(MarkdownMode::Auto.forced(), None);
}

#[test]
fn tui_markdown_parses_from_toml_and_defaults_to_auto() {
    let cfg: TuiConfig = toml::from_str("markdown = \"off\"").unwrap();
    assert_eq!(cfg.markdown, MarkdownMode::Off);
    let default: TuiConfig = toml::from_str("").unwrap();
    assert_eq!(default.markdown, MarkdownMode::Auto);
}

#[test]
fn thinking_mode_defaults_to_fold_and_round_trips() {
    // The default is BOUNDED reasoning. `stream` still names the old unbounded
    // trickle, so an operator who wants every line back writes one word.
    let cfg: TuiConfig = toml::from_str("").unwrap();
    assert_eq!(cfg.thinking, ThinkingMode::Fold);
    let cfg: TuiConfig = toml::from_str("thinking = \"off\"").unwrap();
    assert_eq!(cfg.thinking, ThinkingMode::Off);
    let cfg: TuiConfig = toml::from_str("thinking = \"stream\"").unwrap();
    assert_eq!(cfg.thinking, ThinkingMode::Stream);
    let cfg: TuiConfig = toml::from_str("thinking = \"fold\"").unwrap();
    assert_eq!(cfg.thinking, ThinkingMode::Fold);
}

/// #1235: the spill-view height defaults to 3, parses when absent, and
/// overrides from `[tui]`.
#[test]
fn spill_lines_defaults_to_3_and_overrides() {
    assert_eq!(TuiConfig::default().spill_lines, 3);
    let empty: TuiConfig = toml::from_str("").unwrap();
    assert_eq!(empty.spill_lines, 3);
    let set: TuiConfig = toml::from_str("spill_lines = 7").unwrap();
    assert_eq!(set.spill_lines, 7);
}
