use super::{TurnKey, TurnKeyDecoder};

#[test]
fn sgr_wheel_up_and_down_map_to_scroll_keys() {
    let mut d = TurnKeyDecoder::default();
    // SGR wheel up: ESC [ < 64 ; col ; row M
    assert_eq!(d.feed(b"\x1b[<64;10;5M"), vec![TurnKey::Up]);
    // SGR wheel down: button 65
    assert_eq!(d.feed(b"\x1b[<65;10;5M"), vec![TurnKey::Down]);
}

#[test]
fn sgr_non_wheel_events_are_ignored_by_the_wheel_tier() {
    let mut d = TurnKeyDecoder::default();
    // A left-button RELEASE emits no key; a right/middle click is ignored.
    assert_eq!(d.feed(b"\x1b[<0;3;3m"), vec![]);
    assert_eq!(d.feed(b"\x1b[<2;3;3M"), vec![]);
}

#[test]
fn left_click_press_toggles_expand() {
    let mut d = TurnKeyDecoder::default();
    // SGR left-button PRESS toggles expand/collapse; the release is a no-op,
    // so one click = one toggle.
    assert_eq!(d.feed(b"\x1b[<0;3;3M"), vec![TurnKey::ToggleExpanded]);
    assert_eq!(d.feed(b"\x1b[<0;3;3m"), vec![]);
}

#[test]
fn wheel_sequence_split_across_reads_still_decodes() {
    let mut d = TurnKeyDecoder::default();
    assert_eq!(d.feed(b"\x1b[<64;"), vec![]);
    assert_eq!(d.feed(b"10;5M"), vec![TurnKey::Up]);
}

#[test]
fn arrow_and_space_still_decode_alongside_mouse_params() {
    let mut d = TurnKeyDecoder::default();
    assert_eq!(d.feed(b"\x1b[A"), vec![TurnKey::Up]);
    assert_eq!(d.feed(b"\x1b[B"), vec![TurnKey::Down]);
    assert_eq!(d.feed(b" "), vec![TurnKey::ToggleExpanded]);
}

#[test]
fn vi_mode_maps_jk_gg_g_and_halfpage() {
    let mut d = TurnKeyDecoder::with_mode(newt_core::EditMode::Vi);
    assert_eq!(d.feed(b"j"), vec![TurnKey::Down]);
    assert_eq!(d.feed(b"k"), vec![TurnKey::Up]);
    assert_eq!(d.feed(b"gg"), vec![TurnKey::Top]);
    assert_eq!(d.feed(b"G"), vec![TurnKey::Bottom]);
    assert_eq!(d.feed(b"\x04"), vec![TurnKey::HalfPageDown]); // C-d
    assert_eq!(d.feed(b"\x15"), vec![TurnKey::HalfPageUp]); // C-u
}

#[test]
fn vi_single_g_waits_for_the_second_g() {
    let mut d = TurnKeyDecoder::with_mode(newt_core::EditMode::Vi);
    assert_eq!(d.feed(b"g"), vec![]); // pending, no key yet
    assert_eq!(d.feed(b"g"), vec![TurnKey::Top]);
}

#[test]
fn emacs_mode_maps_ctrl_np_not_vi_letters() {
    let mut d = TurnKeyDecoder::with_mode(newt_core::EditMode::Emacs);
    assert_eq!(d.feed(b"\x0e"), vec![TurnKey::Down]); // C-n
    assert_eq!(d.feed(b"\x10"), vec![TurnKey::Up]); // C-p
                                                    // Bare j/k are vi motions, inert in emacs mode.
    assert_eq!(d.feed(b"j"), vec![]);
}

#[test]
fn base_arrows_space_and_enter_work_in_every_mode() {
    for mode in [
        newt_core::EditMode::Vi,
        newt_core::EditMode::Emacs,
        newt_core::EditMode::Nano,
    ] {
        let label = format!("{mode:?}");
        let mut d = TurnKeyDecoder::with_mode(mode);
        assert_eq!(d.feed(b"\x1b[A"), vec![TurnKey::Up], "{label} up-arrow");
        assert_eq!(d.feed(b"\x1b[B"), vec![TurnKey::Down], "{label} down-arrow");
        assert_eq!(d.feed(b" "), vec![TurnKey::ToggleExpanded], "{label} space");
        assert_eq!(
            d.feed(b"\r"),
            vec![TurnKey::ToggleExpanded],
            "{label} enter"
        );
    }
}

// #1303 FIX F: the mode-aware nav keys activate ONLY with the mouse opt-in
// (`mode_nav`). A decoder built WITHOUT the opt-in must never emit nav keys
// for vi `j`/`k`/`gg` — since the persistent-prompt work those bytes are
// type-ahead TEXT (not dropped) — while the base keys stay unconditional.
#[test]
fn mode_nav_off_ignores_editor_keys_even_in_vi_mode() {
    let mut d = TurnKeyDecoder {
        mode: newt_core::EditMode::Vi,
        mode_nav: false,
        ..Default::default()
    };
    assert_eq!(d.feed(b"j"), vec![], "opt-in off: vi `j` is not nav");
    assert_eq!(d.feed(b"k"), vec![], "opt-in off: vi `k` is not nav");
    assert_eq!(d.feed(b"gg"), vec![], "opt-in off: `gg` is not nav");
    assert_eq!(d.feed(b"\x04"), vec![], "opt-in off: C-d does nothing");
    // The letters became type-ahead text (C-d, a C0 control, is dropped)…
    assert_eq!(d.take_text(), b"jkgg");
    // …and with the buffer drained, base keys remain unconditional.
    assert_eq!(d.feed(b" "), vec![TurnKey::ToggleExpanded], "space");
    assert_eq!(d.feed(b"\r"), vec![TurnKey::ToggleExpanded], "enter");
    assert_eq!(d.feed(b"\x1b[A"), vec![TurnKey::Up], "up-arrow");
    assert_eq!(d.feed(b"\x1b[B"), vec![TurnKey::Down], "down-arrow");
}

#[test]
fn mode_nav_on_activates_vi_scroll() {
    // Opt-in ON (mouse tier): the same vi `j` now scrolls.
    let mut d = TurnKeyDecoder::with_mode(newt_core::EditMode::Vi);
    assert_eq!(d.feed(b"j"), vec![TurnKey::Down]);
}

// #1303 FIX D: the vi `gg` latch must not survive an intervening escape /
// CSI / SS3 / mouse event. A stray `g` then an arrow (or wheel) then a `g`
// must NOT mis-fire `Top`.
#[test]
fn pending_g_cleared_by_intervening_arrow() {
    let mut d = TurnKeyDecoder::with_mode(newt_core::EditMode::Vi);
    assert_eq!(d.feed(b"g"), vec![], "arms pending_g");
    assert_eq!(
        d.feed(b"\x1b[A"),
        vec![TurnKey::Up],
        "arrow clears the latch"
    );
    assert_eq!(d.feed(b"g"), vec![], "lone `g` again — pending, NOT Top");
    assert_eq!(
        d.feed(b"g"),
        vec![TurnKey::Top],
        "a real `gg` still fires Top"
    );
}

#[test]
fn pending_g_cleared_by_intervening_mouse_wheel() {
    let mut d = TurnKeyDecoder::with_mode(newt_core::EditMode::Vi);
    assert_eq!(d.feed(b"g"), vec![]);
    // The feature's headline interaction: a wheel scroll (SGR mouse).
    assert_eq!(d.feed(b"\x1b[<64;10;5M"), vec![TurnKey::Up]);
    assert_eq!(d.feed(b"g"), vec![], "wheel cleared the latch — no misfire");
}

// #1303 FIX C: the legacy X10 mouse form (`ESC[M` + 3 raw bytes) must be
// recognized and its 3 bytes CONSUMED, never leaked as ground keys.
#[test]
fn x10_mouse_left_press_consumes_three_bytes() {
    let mut d = TurnKeyDecoder::with_mode(newt_core::EditMode::Vi);
    // Cb=0x20 (button 0 = left press) => ToggleExpanded; Cx=Cy=0x21 consumed.
    assert_eq!(d.feed(b"\x1b[M\x20\x21\x21"), vec![TurnKey::ToggleExpanded]);
    // Decoder is back in Ground: a plain space toggles as normal.
    assert_eq!(d.feed(b" "), vec![TurnKey::ToggleExpanded]);
}

#[test]
fn x10_mouse_coordinate_bytes_never_leak_as_nav() {
    let mut d = TurnKeyDecoder::with_mode(newt_core::EditMode::Vi);
    // X10 wheel-up: Cb = 64 + 32 = 0x60. The coord bytes are `j` and `g` —
    // which WOULD scroll / arm-`gg` if leaked. They must be swallowed.
    assert_eq!(d.feed(b"\x1b[M\x60jg"), vec![TurnKey::Up]);
    // No stray Down (from `j`) and no armed `gg`: a lone `g` now is pending,
    // and only a second `g` fires Top.
    assert_eq!(d.feed(b"g"), vec![]);
    assert_eq!(d.feed(b"g"), vec![TurnKey::Top]);
}

#[test]
fn x10_mouse_bytes_split_across_reads_are_consumed() {
    let mut d = TurnKeyDecoder::with_mode(newt_core::EditMode::Vi);
    assert_eq!(d.feed(b"\x1b[M"), vec![], "header only");
    assert_eq!(d.feed(b"\x20"), vec![TurnKey::ToggleExpanded], "Cb (btn 0)");
    assert_eq!(d.feed(b"j"), vec![], "Cx consumed, not a Down");
    assert_eq!(d.feed(b"j"), vec![], "Cy consumed — sequence complete");
    // Back in Ground: base space still toggles.
    assert_eq!(d.feed(b" "), vec![TurnKey::ToggleExpanded]);
}

// #1303 FIX E: the CSI params accumulator is length-capped so a
// non-terminating CSI stream can't grow it without bound; the decoder
// swallows the malformed sequence's tail and a following well-formed
// sequence still decodes.
#[test]
fn csi_params_are_length_capped_and_resync() {
    let mut d = TurnKeyDecoder::default();
    d.feed(b"\x1b[");
    for _ in 0..1000 {
        d.feed(b";");
    }
    assert!(
        d.params.len() <= super::MAX_CSI_PARAM_BYTES,
        "params bounded at the cap, was {}",
        d.params.len()
    );
    // After the overflow resync, a fresh arrow decodes normally.
    assert_eq!(d.feed(b"\x1b[A"), vec![TurnKey::Up]);
    // The overflowed sequence's tail was swallowed, never captured as
    // type-ahead text (prefill garbage).
    assert!(d.take_text().is_empty(), "no CSI tail leaks into text");
}

/// An overflowed CSI's remaining param bytes and terminator are swallowed
/// to the terminator — not resynced to Ground where type-ahead capture
/// would turn them into visible prefill garbage.
#[test]
fn csi_overflow_tail_never_becomes_type_ahead_text() {
    let mut d = TurnKeyDecoder::default();
    let mut seq = b"\x1b[".to_vec();
    seq.extend(std::iter::repeat_n(b'1', 40));
    seq.push(b'~'); // terminator of the malformed sequence
    seq.extend_from_slice(b"hi"); // real typing after it
    assert!(d.feed(&seq).is_empty());
    assert_eq!(d.take_text(), b"hi");
}

/// A broken vi `gg` latch flushes the armed `g` as text — "great" must
/// prefill as "great", not "reat"; an escape sequence breaking the latch
/// keeps the `g` too. A real `gg` still navigates.
#[test]
fn broken_gg_latch_keeps_the_typed_g() {
    let mut d = TurnKeyDecoder::with_mode(newt_core::EditMode::Vi);
    assert!(d.feed(b"great").is_empty());
    assert_eq!(d.take_text(), b"great");
    assert_eq!(d.feed(b"g\x1b[A"), vec![TurnKey::Up]);
    assert_eq!(d.take_text(), b"g");
    assert_eq!(d.feed(b"gg"), vec![TurnKey::Top]);
    assert!(d.take_text().is_empty());
}
