use super::*;
use test_support::production;

/// **The structural half of #1898.** The PTY test proves the guard
/// restores; it cannot prove `read_turn` uses it, because driving the
/// event loop needs a real interactive turn. A guard that is correct and
/// unused is exactly the state this file was in — the restore existed, it
/// just was not owed from anywhere a panic respects.
#[test]
fn raw_and_paste_are_owned_by_one_guard() {
    let src = production();
    // #1905 subsumed the raw half onto `RawModeGuard`, so this file
    // reaches crossterm's process-global not at all. The count that used
    // to be "exactly one" is now "none": the ONE nesting-aware owner is in
    // newt-core, and a bare call reappearing here would be a second owner
    // restoring to a fixed state instead of to what it found.
    assert_eq!(
        src.matches("enable_raw_mode()").count(),
        0,
        "raw mode comes from RawModeGuard, never from crossterm directly"
    );
    assert_eq!(
        src.matches("disable_raw_mode();").count(),
        0,
        "…and is released by the field, never by a statement here"
    );
    assert!(
        src.contains("_raw: RawModeGuard"),
        "RawPasteGuard must HOLD a RawModeGuard — composition, not a \
             reimplementation"
    );
    assert_eq!(
        src.matches("EnableBracketedPaste)").count(),
        1,
        "bracketed paste is enabled in exactly one place"
    );
    assert_eq!(
        src.matches("DisableBracketedPaste)").count(),
        1,
        "…and disabled in exactly one place"
    );
    assert!(
        src.contains("impl Drop for RawPasteGuard"),
        "the restore must be a Drop obligation, not a method someone has \
             to remember to call — which is what #1411 cleared this site for"
    );
}

/// **The ordering the issue asks to settle explicitly.** Teardown mirrors
/// setup: paste off, then raw off. Bracketed paste is a terminal INPUT
/// mode, so releasing raw mode first would hand line discipline back with
/// paste markers still armed. Pinned here because a Drop body is exactly
/// the kind of two-line block someone reorders while tidying.
#[test]
fn the_guard_releases_paste_before_raw_mode() {
    let src = production();
    let drop_impl = src
        .split_once("impl Drop for RawPasteGuard")
        .expect("the guard must restore from Drop")
        .1;
    let body = &drop_impl[..drop_impl.find("\n}").unwrap_or(drop_impl.len())];
    // THE MECHANISM CHANGED, THE CONTRACT DID NOT (#1905). Raw mode is no
    // longer released by a statement in this body; it is released by the
    // `_raw: RawModeGuard` field, which Rust drops AFTER this body runs.
    // So the assertion is structural: paste here, raw as a field, and NO
    // raw release in the body — a `disable_raw_mode()` back in here would
    // run FIRST and invert the order.
    assert!(
        body.contains("DisableBracketedPaste"),
        "Drop must release bracketed paste in its own body"
    );
    assert!(
        !body.contains("disable_raw_mode();"),
        "releasing raw mode in the body would run BEFORE the field drops, \
             handing line discipline back with paste markers still armed"
    );
    assert!(
        src.contains("_raw: RawModeGuard"),
        "raw mode must be a FIELD, so it drops after the body"
    );
}

/// **The language rule the ordering now rests on** (#1905).
///
/// The test above asserts a STRUCTURE — paste in the Drop body, raw mode
/// in a field — and that only implies the right order if a struct's own
/// `Drop::drop` runs before its fields drop. It does; this pins it here
/// rather than leaving the contract resting on a fact nobody in this repo
/// has checked.
#[test]
fn a_drop_body_runs_before_its_fields_drop() {
    use std::cell::RefCell;
    thread_local! {
        static ORDER: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
    }
    struct Field;
    impl Drop for Field {
        fn drop(&mut self) {
            ORDER.with(|o| o.borrow_mut().push("field"));
        }
    }
    struct Outer {
        _f: Field,
    }
    impl Drop for Outer {
        fn drop(&mut self) {
            ORDER.with(|o| o.borrow_mut().push("body"));
        }
    }
    drop(Outer { _f: Field });
    ORDER.with(|o| assert_eq!(*o.borrow(), ["body", "field"]));
}
