use super::*;

const KEYS: [SizeKey; 3] = [SizeKey::Grow, SizeKey::Shrink, SizeKey::Zoom];

/// A host with a `screen`-row terminal: it grants the request, clamped.
fn grant(request: u16, screen: u16) -> u16 {
    request.min(screen)
}

/// Every key sequence up to `depth` from `start`, run against a host.
fn walk(start: u16, screen: u16, depth: usize, check: &mut dyn FnMut(&[SizeKey], ModalSize, u16)) {
    fn go(
        seq: &mut Vec<SizeKey>,
        size: ModalSize,
        screen: u16,
        depth: usize,
        check: &mut dyn FnMut(&[SizeKey], ModalSize, u16),
    ) {
        let granted = grant(size.requested(), screen);
        check(seq, size, granted);
        if seq.len() == depth {
            return;
        }
        for key in KEYS {
            let mut next = size;
            next.apply(key, granted);
            seq.push(key);
            go(seq, next, screen, depth, check);
            seq.pop();
        }
    }
    go(&mut Vec::new(), ModalSize::new(start), screen, depth, check);
}

/// Exhaustive over every sequence of up to five keys, every starting height
/// 1..=14 and screens 4..=12 — a small domain, so enumerating it proves more
/// than sampling would.
#[test]
fn never_below_the_minimum_and_never_granted_past_the_screen() {
    for screen in MIN_ROWS..=12 {
        for start in 1..=14 {
            walk(start, screen, 5, &mut |seq, size, granted| {
                assert!(
                    size.requested() >= MIN_ROWS,
                    "{seq:?} from {start}: {size:?}"
                );
                assert!(granted <= screen, "{seq:?}: granted {granted} on {screen}");
            });
        }
    }
}

#[test]
fn zoom_fills_and_a_second_zoom_restores_the_prior_height() {
    for screen in MIN_ROWS..=12 {
        for start in MIN_ROWS..=12 {
            let mut size = ModalSize::new(start);
            let before = grant(size.requested(), screen);
            assert_eq!(size.apply(SizeKey::Zoom, before), Some(FILL));
            assert!(size.zoomed());
            assert_eq!(
                grant(size.requested(), screen),
                screen,
                "zoom fills the screen"
            );
            size.apply(SizeKey::Zoom, screen);
            assert!(!size.zoomed());
            assert_eq!(grant(size.requested(), screen), before, "zoom round-trips");
        }
    }
}

#[test]
fn grow_and_shrink_step_one_row_from_what_is_on_screen_and_leave_zoom() {
    let mut size = ModalSize::new(8);
    assert_eq!(size.apply(SizeKey::Grow, 8), Some(9));
    assert_eq!(size.apply(SizeKey::Shrink, 9), Some(8));
    // Held at full height, Grow does not bank rows past what is granted.
    let mut full = ModalSize::new(8);
    full.apply(SizeKey::Grow, 12);
    full.apply(SizeKey::Grow, 12);
    assert_eq!(
        full.requested(),
        13,
        "steps from the granted 12, not the request"
    );
    assert_eq!(
        full.apply(SizeKey::Shrink, 12),
        Some(11),
        "one press shrinks one row"
    );
    // Sizing while zoomed leaves zoom.
    let mut zoomed = ModalSize::new(8);
    zoomed.apply(SizeKey::Zoom, 8);
    zoomed.apply(SizeKey::Shrink, 30);
    assert!(!zoomed.zoomed());
    assert_eq!(zoomed.requested(), 29);
}

#[test]
fn a_shrink_at_the_minimum_changes_nothing() {
    let mut size = ModalSize::new(MIN_ROWS);
    assert_eq!(size.apply(SizeKey::Shrink, MIN_ROWS), None);
    assert_eq!(size.requested(), MIN_ROWS);
}
