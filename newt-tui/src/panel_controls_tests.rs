use super::*;
use crossterm::event::KeyModifiers;

const PREFIX: Key = Key::Ctrl(' ');

fn mouse(kind: MouseEventKind, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column: 10,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

#[test]
fn prefix_z_zooms_and_plain_keys_reach_the_panel() {
    let mut c = Controls::new(PREFIX);
    assert_eq!(c.key(Key::Char('z')), Effect::Forward(Key::Char('z')));
    assert_eq!(c.key(PREFIX), Effect::Nothing);
    assert!(c
        .overlay()
        .is_some_and(|o| o.starts_with("ctrl+space:") && o.contains("z zoom")));
    assert_eq!(c.key(Key::Char('z')), Effect::Size(SizeKey::Zoom));
    assert_eq!(c.overlay(), None);
    assert_eq!(c.key(PREFIX), Effect::Nothing);
    assert_eq!(c.key(Key::Char('l')), Effect::Redraw);
    assert_eq!(c.key(PREFIX), Effect::Nothing);
    assert_eq!(
        c.key(PREFIX),
        Effect::Forward(PREFIX),
        "doubled prefix reaches the panel"
    );
}

/// herdr's resize mode: prefix r, then arrows until Enter or Esc — and Esc
/// there ends the mode, it does not close the panel.
#[test]
fn resize_mode_owns_the_arrows_until_enter_or_esc() {
    let mut c = Controls::new(PREFIX);
    c.key(PREFIX);
    assert_eq!(c.key(Key::Char('r')), Effect::Nothing);
    assert!(c.overlay().is_some_and(|o| o.starts_with("resize:")));
    assert_eq!(c.key(Key::Up), Effect::Size(SizeKey::Grow));
    assert_eq!(c.key(Key::Down), Effect::Size(SizeKey::Shrink));
    assert_eq!(
        c.key(Key::Char('j')),
        Effect::Nothing,
        "the panel does not scroll mid-resize"
    );
    assert_eq!(
        c.key(Key::Esc),
        Effect::Nothing,
        "Esc leaves resize mode, not the panel"
    );
    assert_eq!(c.overlay(), None);
    assert_eq!(
        c.key(Key::Esc),
        Effect::Forward(Key::Esc),
        "the next Esc is the panel's"
    );
}

#[test]
fn prefix_question_mark_shows_the_bindings_until_the_next_key() {
    let mut c = Controls::new(PREFIX);
    c.key(PREFIX);
    assert_eq!(c.key(Key::Char('?')), Effect::Nothing);
    assert!(c.overlay().is_some_and(|o| o.contains("r resize")));
    assert_eq!(c.key(Key::Down), Effect::Forward(Key::Down));
    assert_eq!(c.overlay(), None);
}

/// herdr's border drag: press on the top border, drag, release. The height
/// runs from the pointer to the panel's bottom; a press elsewhere is not a
/// drag.
#[test]
fn dragging_the_top_border_sets_the_height_from_the_pointer() {
    let area = Rect::new(0, 13, 100, 18); // rows 13..31
    let mut c = Controls::new(PREFIX);
    assert_eq!(
        c.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 20), area),
        Effect::Nothing
    );
    assert_eq!(
        c.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 18), area),
        Effect::Nothing,
        "a press inside the panel is not a border drag"
    );
    c.mouse(mouse(MouseEventKind::Up(MouseButton::Left), 18), area);
    assert_eq!(
        c.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 13), area),
        Effect::Nothing
    );
    assert_eq!(
        c.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 5), area),
        Effect::Size(SizeKey::To(26)),
        "border dragged up to row 5: rows 5..31"
    );
    assert_eq!(
        c.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 25), area),
        Effect::Size(SizeKey::To(6))
    );
    c.mouse(mouse(MouseEventKind::Up(MouseButton::Left), 25), area);
    assert_eq!(
        c.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 2), area),
        Effect::Nothing,
        "released: no drag"
    );
}

/// `/` after the prefix is help; without the prefix it is the panel's (its
/// filter key), untouched.
#[test]
fn slash_is_help_only_after_the_prefix() {
    let mut c = Controls::new(PREFIX);
    assert_eq!(c.key(Key::Char('/')), Effect::Forward(Key::Char('/')));
    c.key(PREFIX);
    assert_eq!(c.key(Key::Char('/')), Effect::Nothing);
    assert!(c.overlay().is_some_and(|o| o.contains("/ help")));
}
