use super::*;

#[test]
fn the_shipped_table_parses_and_binds_every_operation() {
    let bound: Vec<MetaAction> = BINDINGS.0.iter().map(|(_, action)| *action).collect();
    for action in [
        MetaAction::Zoom,
        MetaAction::Resize,
        MetaAction::Redraw,
        MetaAction::Help,
    ] {
        assert!(
            bound.contains(&action),
            "{action:?} has no key in prefix_keys.toml"
        );
    }
    assert_eq!(BINDINGS.action('z'), Some(MetaAction::Zoom));
    assert_eq!(BINDINGS.action('r'), Some(MetaAction::Resize));
    assert_eq!(BINDINGS.action('?'), Some(MetaAction::Help));
    assert_eq!(
        BINDINGS.action('/'),
        Some(MetaAction::Help),
        "no-shift help key"
    );
}

#[test]
fn a_malformed_table_is_an_error_not_a_panic() {
    assert!(
        Bindings::from_toml("[bindings]\nzz = \"zoom\"\n").is_err(),
        "two keys"
    );
    assert!(
        Bindings::from_toml("[bindings]\nz = \"explode\"\n").is_err(),
        "unknown op"
    );
    assert!(Bindings::from_toml("not toml").is_err());
}

#[test]
fn chords_parse_as_the_operator_writes_them() {
    assert_eq!(parse_chord("ctrl+space"), Some(Key::Ctrl(' ')));
    assert_eq!(parse_chord("Ctrl+A"), Some(Key::Ctrl('a')));
    assert_eq!(parse_chord("ctrl-n"), Some(Key::Ctrl('n')));
    for bad in ["space", "a", "ctrl+", "ctrl+ab", "ctrl+1", "alt+a"] {
        assert_eq!(parse_chord(bad), None, "{bad} must not be a prefix");
    }
    assert_eq!(chord_label(Key::Ctrl(' ')), "ctrl+space");
    assert_eq!(chord_label(Key::Ctrl('a')), "ctrl+a");
    assert_eq!(parse_chord(DEFAULT_PREFIX), Some(Key::Ctrl(' ')));
}

/// tmux's contract: prefix arms; the next key acts or is swallowed; the
/// prefix twice sends the prefix itself; nothing else is ever eaten.
#[test]
fn the_sequencer_arms_acts_cancels_and_passes_a_doubled_prefix_through() {
    let prefix = Key::Ctrl(' ');
    let mut seq = Sequencer::new(prefix);
    assert_eq!(
        seq.feed(Key::Char('z'), &BINDINGS),
        Step::Pass(Key::Char('z')),
        "idle: z is text"
    );
    assert_eq!(seq.feed(prefix, &BINDINGS), Step::Armed);
    assert!(seq.armed());
    assert_eq!(
        seq.feed(Key::Char('z'), &BINDINGS),
        Step::Act(MetaAction::Zoom)
    );
    assert!(!seq.armed(), "one key, then idle again");
    assert_eq!(seq.feed(prefix, &BINDINGS), Step::Armed);
    assert_eq!(
        seq.feed(Key::Char('q'), &BINDINGS),
        Step::Cancelled,
        "unbound: swallowed"
    );
    assert_eq!(
        seq.feed(Key::Down, &BINDINGS),
        Step::Pass(Key::Down),
        "disarmed after cancel"
    );
    assert_eq!(seq.feed(prefix, &BINDINGS), Step::Armed);
    assert_eq!(
        seq.feed(prefix, &BINDINGS),
        Step::Pass(prefix),
        "doubled prefix passes through"
    );
    assert!(!seq.armed());
    // A different configured chord: ctrl+space is then ordinary input.
    let mut screen_style = Sequencer::new(Key::Ctrl('a'));
    assert_eq!(screen_style.feed(prefix, &BINDINGS), Step::Pass(prefix));
    assert_eq!(screen_style.feed(Key::Ctrl('a'), &BINDINGS), Step::Armed);
}
