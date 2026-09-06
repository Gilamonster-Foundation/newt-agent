use super::{is_ctrl_c, is_lone_esc, watch_for_interrupt_fd, SpillInput, TurnKey, TurnKeyDecoder};

#[test]
fn lone_esc_interrupts_but_sequences_and_chords_do_not() {
    assert!(is_lone_esc(&[0x1b]), "a bare Esc press interrupts");
    // Arrow / function keys arrive as a CSI/SS3 burst — not an interrupt.
    assert!(!is_lone_esc(&[0x1b, b'[', b'A']), "Up arrow");
    assert!(!is_lone_esc(&[0x1b, b'O', b'P']), "F1 (SS3)");
    // Alt-chord (Esc + char) and plain typed-ahead text never interrupt.
    assert!(!is_lone_esc(&[0x1b, b'x']), "Alt-x");
    assert!(!is_lone_esc(b"hello"), "typed text");
    assert!(!is_lone_esc(&[]), "nothing");
}

#[test]
fn ctrl_c_detected_anywhere_in_the_read() {
    assert!(is_ctrl_c(&[0x03]), "a bare Ctrl-C press interrupts");
    // #1303 FIX A: a `0x03` coalesced with other bytes in ONE read must
    // still interrupt — the old exact-match dropped these.
    assert!(
        is_ctrl_c(&[0x03, b'x']),
        "Ctrl-C coalesced with typed-ahead still interrupts"
    );
    assert!(
        is_ctrl_c(b"\x1b[<35;120;40M\x03"),
        "Ctrl-C coalesced after a mouse-motion event still interrupts"
    );
    assert!(
        is_ctrl_c(&[b'a', b'b', 0x03]),
        "a trailing Ctrl-C interrupts"
    );
    assert!(!is_ctrl_c(&[0x1b]), "Esc is not Ctrl-C");
    assert!(!is_ctrl_c(b"c"), "the letter c is not Ctrl-C");
    assert!(!is_ctrl_c(&[]), "nothing");
}

#[test]
fn arrow_decoder_preserves_fragmented_csi_and_ss3_sequences() {
    let mut decoder = TurnKeyDecoder::default();
    assert!(decoder.feed(&[0x1b]).is_empty());
    assert!(decoder.feed(b"[").is_empty());
    assert_eq!(decoder.feed(b"A"), [TurnKey::Up]);

    assert!(decoder.feed(&[0x1b, b'O']).is_empty());
    assert_eq!(decoder.feed(b"B"), [TurnKey::Down]);
    assert_eq!(
        decoder.feed(&[0x1b, b'[', b'1', b';', b'2', b'A']),
        [TurnKey::Up]
    );
    assert!(decoder.feed(&[0x1b, b'x']).is_empty(), "Alt chord");
    assert_eq!(decoder.feed(b" "), [TurnKey::ToggleExpanded]);
    assert_eq!(decoder.feed(b"\r"), [TurnKey::ToggleExpanded]);
}

/// The type-ahead drain happens at watcher EXIT, not per read: interactive
/// typing arrives one byte per read(2), and a per-read drain would empty
/// the decoder buffer between keystrokes — defeating the Space "typing has
/// begun" latch, so mid-sentence spaces would toggle the viewport and
/// vanish from the prefill ("fix the bug" → "fixthebug").
#[serial_test::serial(prompt_stdin, type_ahead)]
#[test]
fn per_keystroke_reads_preserve_spaces_in_type_ahead() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    let _ = crate::type_ahead::take();
    let mut pipe = [0; 2];
    assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
    let cancel = AtomicBool::new(false);
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            watch_for_interrupt_fd(
                pipe[0],
                &cancel,
                &stop,
                None,
                newt_core::EditMode::Nano,
                false,
                10,
                50,
            );
        });
        // One byte per write = one byte per read, the human typing shape.
        for byte in *b"hi !" {
            assert_eq!(
                unsafe { libc::write(pipe[1], [byte].as_ptr().cast(), 1) },
                1
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        // No wake byte: the 10 ms poll timeout notices `stop` on its own,
        // so nothing can race into the decoder after this store.
        stop.store(true, Ordering::Relaxed);
    });
    unsafe {
        libc::close(pipe[0]);
        libc::close(pipe[1]);
    }
    assert!(!cancel.load(Ordering::Relaxed), "typing never interrupts");
    assert_eq!(
        crate::type_ahead::take(),
        "hi !",
        "the mid-sentence space survives per-keystroke reads"
    );
}

/// A stale leaked flag is healed on ENTRY — including on the watcherless
/// (`enabled == false`) path, which has no other opportunity to clear it.
#[serial_test::serial(interrupt_pending)]
#[test]
fn a_stale_interrupt_flag_is_cleared_on_entry() {
    use std::sync::atomic::AtomicBool;
    newt_core::tty::set_interrupt_pending(true);
    let cancel = AtomicBool::new(false);
    let ran = super::with_interrupt_watch(false, &cancel, || {
        assert!(
            !newt_core::tty::interrupt_pending(),
            "cleared before the turn body runs"
        );
        42
    });
    assert_eq!(ran, 42);
    assert!(!newt_core::tty::interrupt_pending());
}

/// Persistent-prompt phase 1: ground bytes that are neither interrupts nor
/// nav keys accumulate as type-ahead text instead of vanishing; backspace
/// edits, and escape-sequence bytes never leak in.
#[test]
fn typed_ground_bytes_accumulate_as_type_ahead_text() {
    let mut d = TurnKeyDecoder::default();
    assert!(d.feed(b"fix").is_empty(), "text produces no keys");
    // Space after typing has begun is text, not ToggleExpanded…
    assert!(d.feed(b" it").is_empty());
    // …and an arrow key mid-typing is still nav, never text.
    assert_eq!(d.feed(b"\x1b[A"), vec![TurnKey::Up]);
    // Backspace removes the last char; UTF-8 is popped whole.
    d.feed("é".as_bytes());
    d.feed(&[0x7f]);
    assert_eq!(d.take_text(), b"fix it");
    // Drained: Space on an empty buffer toggles again (the base contract).
    assert_eq!(d.feed(b" "), vec![TurnKey::ToggleExpanded]);
    assert!(d.take_text().is_empty());
}

/// Enter with a non-empty buffer is a newline (a queued message), and CR
/// normalizes to `\n`.
#[test]
fn enter_mid_typing_is_a_newline_not_a_toggle() {
    let mut d = TurnKeyDecoder::default();
    assert!(d.feed(b"run the tests\r").is_empty());
    assert_eq!(d.take_text(), b"run the tests\n");
}

/// The first Ctrl-C both trips the graceful cancel AND raises the
/// process-wide acknowledgment flag the spinner reads — the press is
/// visible on screen within a frame instead of feeling ignored.
#[serial_test::serial(prompt_stdin, interrupt_pending)]
#[test]
fn first_ctrl_c_raises_the_interrupt_acknowledgment() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    newt_core::tty::set_interrupt_pending(false);
    let mut pipe = [0; 2];
    assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
    let cancel = AtomicBool::new(false);
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            watch_for_interrupt_fd(
                pipe[0],
                &cancel,
                &stop,
                None,
                newt_core::EditMode::Nano,
                false,
                10,
                100,
            );
        });
        assert_eq!(
            unsafe { libc::write(pipe[1], [0x03u8].as_ptr().cast(), 1) },
            1
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !cancel.load(Ordering::Relaxed) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        stop.store(true, Ordering::Relaxed);
        assert_eq!(unsafe { libc::write(pipe[1], b"x".as_ptr().cast(), 1) }, 1);
    });
    unsafe {
        libc::close(pipe[0]);
        libc::close(pipe[1]);
    }
    assert!(cancel.load(Ordering::Relaxed), "graceful cancel tripped");
    assert_eq!(
        newt_core::tty::interrupt_presses(),
        1,
        "the acknowledgment count is raised for the spinner"
    );
    newt_core::tty::set_interrupt_pending(false);
}

/// #2010: EVERY press is acknowledged, not just the first. The watcher
/// bumps the process-wide press count the spinner renders, so a 2nd
/// Ctrl-C changes the label within a tick instead of being absorbed into
/// a flag nothing read until the turn returned.
#[serial_test::serial(prompt_stdin, interrupt_pending)]
#[test]
fn every_ctrl_c_press_is_counted_for_the_spinner() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    newt_core::tty::set_interrupt_pending(false);
    let mut pipe = [0; 2];
    assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
    let cancel = AtomicBool::new(false);
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            watch_for_interrupt_fd(
                pipe[0],
                &cancel,
                &stop,
                None,
                newt_core::EditMode::Nano,
                false,
                10,
                100,
            );
        });
        let press = || {
            assert_eq!(
                unsafe { libc::write(pipe[1], [0x03u8].as_ptr().cast(), 1) },
                1
            );
        };
        let wait_for = |presses: u32| {
            let deadline = std::time::Instant::now() + Duration::from_secs(1);
            while newt_core::tty::interrupt_presses() < presses
                && std::time::Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        press();
        wait_for(1);
        press();
        wait_for(2);
        press();
        wait_for(3);
        stop.store(true, Ordering::Relaxed);
        assert_eq!(unsafe { libc::write(pipe[1], b"x".as_ptr().cast(), 1) }, 1);
    });
    unsafe {
        libc::close(pipe[0]);
        libc::close(pipe[1]);
    }
    assert!(cancel.load(Ordering::Relaxed), "graceful cancel tripped");
    assert_eq!(
        newt_core::tty::interrupt_presses(),
        3,
        "the 2nd and 3rd presses are heard, not absorbed"
    );
    newt_core::tty::set_interrupt_pending(false);
}

#[serial_test::serial(prompt_stdin)]
#[test]
fn watcher_routes_a_fragmented_arrow_and_activation_without_cancelling() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    #[derive(Default)]
    struct RecordingSpill {
        up: AtomicUsize,
        toggled: AtomicUsize,
    }
    impl SpillInput for RecordingSpill {
        fn scroll_up(&self) -> bool {
            self.up.fetch_add(1, Ordering::Relaxed);
            true
        }
        fn scroll_down(&self) -> bool {
            true
        }
        fn toggle_expanded(&self) -> bool {
            self.toggled.fetch_add(1, Ordering::Relaxed);
            true
        }
        fn expand_half(&self) -> bool {
            true
        }
        fn is_exploring(&self) -> bool {
            false
        }
        fn exit_explore(&self) -> bool {
            true
        }
        fn refresh_geometry(&self) -> bool {
            true
        }
        #[cfg(feature = "live-spill")]
        fn scroll_to_top(&self) -> bool {
            true
        }
        #[cfg(feature = "live-spill")]
        fn scroll_to_bottom(&self) -> bool {
            true
        }
        #[cfg(feature = "live-spill")]
        fn half_page_up(&self) -> bool {
            true
        }
        #[cfg(feature = "live-spill")]
        fn half_page_down(&self) -> bool {
            true
        }
    }

    let mut pipe = [0; 2];
    assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
    let cancel = AtomicBool::new(false);
    let stop = AtomicBool::new(false);
    let spill = RecordingSpill::default();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            watch_for_interrupt_fd(
                pipe[0],
                &cancel,
                &stop,
                Some(&spill),
                newt_core::EditMode::Nano,
                false, // mode_nav: base keys (arrow + space) only
                10,
                100,
            );
        });
        let write = |bytes: &[u8]| {
            assert_eq!(
                unsafe { libc::write(pipe[1], bytes.as_ptr().cast(), bytes.len()) },
                bytes.len() as isize
            );
        };
        write(&[0x1b]);
        std::thread::sleep(Duration::from_millis(10));
        write(b"[");
        std::thread::sleep(Duration::from_millis(10));
        write(b"A ");

        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while (spill.up.load(Ordering::Relaxed) == 0 || spill.toggled.load(Ordering::Relaxed) == 0)
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        stop.store(true, Ordering::Relaxed);
        write(b"x");
    });
    unsafe {
        libc::close(pipe[0]);
        libc::close(pipe[1]);
    }

    assert_eq!(spill.up.load(Ordering::Relaxed), 1);
    assert_eq!(spill.toggled.load(Ordering::Relaxed), 1);
    assert!(!cancel.load(Ordering::Relaxed));
}

/// #1704: while the spill viewport is in explore mode (scrolled back off the
/// tail), a lone Esc must LEAVE explore mode — not cancel the turn. A second
/// Esc, now that the view follows the tail again, is the real interrupt.
#[serial_test::serial(prompt_stdin)]
#[test]
fn watcher_esc_exits_explore_before_interrupting() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    #[derive(Default)]
    struct ExploringSpill {
        exploring: AtomicBool,
        exited: AtomicUsize,
    }
    impl SpillInput for ExploringSpill {
        fn scroll_up(&self) -> bool {
            true
        }
        fn scroll_down(&self) -> bool {
            true
        }
        fn toggle_expanded(&self) -> bool {
            true
        }
        fn expand_half(&self) -> bool {
            true
        }
        fn is_exploring(&self) -> bool {
            self.exploring.load(Ordering::Relaxed)
        }
        fn exit_explore(&self) -> bool {
            self.exited.fetch_add(1, Ordering::Relaxed);
            // Leaving explore mode → the view now follows the tail again.
            self.exploring.store(false, Ordering::Relaxed);
            true
        }
        fn refresh_geometry(&self) -> bool {
            true
        }
        #[cfg(feature = "live-spill")]
        fn scroll_to_top(&self) -> bool {
            true
        }
        #[cfg(feature = "live-spill")]
        fn scroll_to_bottom(&self) -> bool {
            true
        }
        #[cfg(feature = "live-spill")]
        fn half_page_up(&self) -> bool {
            true
        }
        #[cfg(feature = "live-spill")]
        fn half_page_down(&self) -> bool {
            true
        }
    }

    let mut pipe = [0; 2];
    assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
    let cancel = AtomicBool::new(false);
    let stop = AtomicBool::new(false);
    let spill = ExploringSpill {
        exploring: AtomicBool::new(true),
        ..ExploringSpill::default()
    };
    std::thread::scope(|scope| {
        scope.spawn(|| {
            watch_for_interrupt_fd(
                pipe[0],
                &cancel,
                &stop,
                Some(&spill),
                newt_core::EditMode::Nano,
                false,
                10,
                // Short grace so a lone Esc resolves quickly in the test.
                30,
            );
        });
        let write = |bytes: &[u8]| {
            assert_eq!(
                unsafe { libc::write(pipe[1], bytes.as_ptr().cast(), bytes.len()) },
                bytes.len() as isize
            );
        };
        let wait_for = |cond: &dyn Fn() -> bool| {
            let deadline = std::time::Instant::now() + Duration::from_secs(1);
            while !cond() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
        };

        // 1st lone Esc while exploring → exits explore, NO cancel.
        write(&[0x1b]);
        wait_for(&|| spill.exited.load(Ordering::Relaxed) == 1);
        assert_eq!(spill.exited.load(Ordering::Relaxed), 1);
        assert!(
            !cancel.load(Ordering::Relaxed),
            "exploring Esc must not cancel the turn"
        );

        // 2nd lone Esc, now following the tail → the real interrupt.
        write(&[0x1b]);
        wait_for(&|| cancel.load(Ordering::Relaxed));
        assert!(
            cancel.load(Ordering::Relaxed),
            "Esc after leaving explore cancels"
        );
        assert_eq!(spill.exited.load(Ordering::Relaxed), 1);

        stop.store(true, Ordering::Relaxed);
        write(b"x");
    });
    unsafe {
        libc::close(pipe[0]);
        libc::close(pipe[1]);
    }
}

/// #1704: Ctrl-t (0x14) decodes to the half-expand key.
#[test]
fn ctrl_t_decodes_to_expand_half() {
    let mut d = TurnKeyDecoder::default();
    assert_eq!(d.feed(b"\x14"), vec![TurnKey::ExpandHalf]);
    // And it does not leak into the type-ahead text buffer.
    assert!(d.take_text().is_empty());
}
