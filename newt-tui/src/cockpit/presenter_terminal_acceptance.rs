use super::*;
use crate::cockpit::test_tty::{
    echoes, is_canonical, mode_diff, modes_equal, set_canonical_echo, termios_of, TestTty,
};

const CTRL_C: &[u8] = &[0x03];

/// The three properties #1744 turns on, proven on one real terminal with
/// one cockpit: Ctrl-C's two tiers, a modal opening underneath it, and the
/// terminal handed back exactly as it was found.
///
/// #1959: also serialized on `prompt_stdin` — this test constructs a real
/// `PromptWindow` via `Terminal::suspend_for_prompt`, which bumps the same
/// process-global counter
/// `permission_prompt_tests::headless_and_piped_sessions_never_construct_a_prompt_window`
/// asserts is untouched.
#[serial_test::serial(tty_arbiter, prompt_stdin)]
#[test]
fn the_cockpit_owns_the_terminal_correctly_and_gives_it_back() {
    let tty = TestTty::install();
    newt_core::tty::set_interrupt_pending(false);

    // A shell's terminal: canonical, echoing.
    set_canonical_echo(0);
    let before = termios_of(0);
    assert!(
        is_canonical(0) && echoes(0),
        "precondition: the terminal starts as a shell hands it over"
    );

    let dir = std::env::temp_dir().join(format!("newt-cockpit-acceptance-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let surface =
        crate::rich_input::RichSurface::new(Some(dir.join("history"))).expect("rich surface");

    {
        let mut cockpit = match Presenter::open(surface) {
            Ok(p) => p,
            Err(e) => panic!(
                "cockpit failed to open: {e}; master saw {:?}",
                tty.painted()
            ),
        };

        // ---- the terminal is genuinely taken ----
        assert!(!is_canonical(0), "the cockpit runs the terminal raw");
        assert!(!echoes(0), "and the kernel is not echoing over the editor");

        // ---- Ctrl-C: every press trips the cancel and is counted ----
        let cancel = Arc::new(AtomicBool::new(false));
        cockpit
            .handle_request(SurfaceRequest::TurnStarted {
                cancel: Arc::clone(&cancel),
            })
            .expect("a turn starts");

        tty.type_bytes(CTRL_C);
        cockpit.poll_keys().expect("first Ctrl-C");
        assert!(
            cancel.load(Ordering::SeqCst),
            "first press trips the cancel the session races against"
        );
        // Operator-observable: this is the count the spinner reads to swap
        // its stage label, so the press is acknowledged on screen.
        assert_eq!(
            newt_core::tty::interrupt_presses(),
            1,
            "first press raises the acknowledgment the operator sees"
        );

        tty.type_bytes(CTRL_C);
        cockpit.poll_keys().expect("second Ctrl-C");
        // #2010: the second press is HEARD — it bumps the count the
        // spinner renders, so it is visibly different from the first
        // instead of being absorbed into a flag read after the turn.
        assert_eq!(
            newt_core::tty::interrupt_presses(),
            2,
            "the second press is acknowledged at press time"
        );

        cockpit
            .handle_request(SurfaceRequest::TurnEnded)
            .expect("the turn ends");
        assert!(
            !newt_core::tty::interrupt_pending(),
            "ending the turn clears the acknowledgment, so the next turn \
                 does not open already showing it"
        );

        // ---- a modal is visible before an answer while the cockpit owns
        //      the terminal, then focus and the draft return to chat ----
        // Push the mounted block to the bottom, then queue a chrome change
        // that grows it by one row without drawing in between. `run`
        // drains requests in exactly that order; the interaction must
        // synchronize the pending layout before reserving its own rows.
        cockpit
            .screen
            .insert_rows(vec![b"transcript row".to_vec(); 24])
            .expect("push the cockpit block to the bottom");
        cockpit.draw().expect("seat the bottom-anchored block");
        let stale_top = cockpit.screen.top;
        {
            let (editor, screen) = (&mut cockpit.editor, &mut cockpit.screen);
            editor
                .on_event(Event::Paste("draft survives".into()), screen)
                .expect("prefill the mounted draft");
        }
        let draft = cockpit.editor.draft();
        cockpit
            .handle_request(SurfaceRequest::SetBackgroundJobs(vec![
                crate::chat::BackgroundJob::start("indexing repository"),
            ]))
            .expect("queue a chrome row before the modal");
        assert!(cockpit.dirty, "the queued chrome has not drawn yet");
        let expected_status_rows = cockpit.status_rows();
        let expected_editor_rows = cockpit.editor.wanted_rows(
            cockpit.screen.cols,
            cockpit.screen.rows,
            &cockpit.surface.chrome(),
        );
        let expected_block_h =
            (expected_editor_rows + expected_status_rows).clamp(1, cockpit.screen.rows.max(1));
        let expected_top = if stale_top + expected_block_h > cockpit.screen.rows {
            cockpit.screen.rows - expected_block_h
        } else {
            stale_top
        };
        assert!(
            expected_top < stale_top,
            "the regression needs the pending chrome row to move the block: \
                 stale={stale_top}, expected={expected_top}"
        );
        let definition = crate::permissions::free_text_form("Cockpit modal visible?");
        let plain_body = newt_core::markup::plain::render(&definition);
        let requested_rows = modal_requested_rows(&plain_body, cockpit.screen.cols);
        let expected_reservation =
            plan_modal_reservation(expected_top, cockpit.screen.rows, requested_rows);
        assert!(
                expected_reservation.chat_visible,
                "acceptance must exercise reserved rows with the inactive chat still visible: {expected_reservation:?}"
            );
        // Where THIS round's bytes begin. The assertions below ask what
        // the modal wrote, and the buffer also holds what everything
        // before it wrote — including, when the harness orders the two
        // serialized cockpit tests the other way round (which coverage
        // instrumentation does), the previous test's terminal RESTORE.
        // That restore legitimately re-enables line wrap, so a whole-buffer
        // scan for `EnableLineWrap` was reading another test's teardown as
        // this modal's behavior.
        let before_modal_round = tty.painted().len();
        let typer = tty.type_when_painted("Prompt — Cockpit modal visible?", b"yes\r");
        let (reply, answer) = std::sync::mpsc::sync_channel(1);
        cockpit
            .handle_request(SurfaceRequest::Interact {
                interaction: Box::new(
                    newt_core::interaction_surface::SurfaceInteraction::blocking(definition),
                ),
                reply,
            })
            .expect("present the interaction");
        assert!(
            typer.join().expect("prompt watcher"),
            "the modal must reach the real terminal before input is sent; painted: {:?}",
            tty.painted()
        );
        assert_eq!(
            answer.recv().expect("interaction answer"),
            newt_core::HumanQuestionOutcome::Answer("yes".into())
        );
        assert_eq!(cockpit.editor.draft(), draft, "the chat draft survives");
        assert!(!cockpit.chat_inactive, "keyboard focus returns to chat");
        assert_eq!(
            cockpit.screen.top, expected_top,
            "the pending layout is applied before modal reservation"
        );
        let provisional = tty.painted();
        let prompt_at = provisional
            .find("Prompt — Cockpit modal visible?")
            .expect("modal body reached the terminal");
        let mut show = Vec::new();
        queue!(show, crossterm::cursor::Show).expect("show-cursor bytes");
        let show = String::from_utf8(show).expect("show bytes are UTF-8");
        // #1959 (post-rebase flake): the modal's OPENING is already
        // synchronized (`type_when_painted` above waits for its prompt
        // text before this point), but the repaint that restores the
        // chat cursor when it CLOSES is the last thing this round
        // writes, with no synchronization point before the snapshot
        // below — unlike input, `painted()` has no way to know the
        // responder thread (which drains the pty master on its own
        // thread) has caught up with a write that already returned.
        // Proved directly, not assumed: 15 concurrent runs of this exact
        // test, 6 failed here, each truncated at a DIFFERENT byte
        // offset — the signature of a drain race, not a fixed defect.
        // Waiting for the exact evidence the assertion below checks for
        // changes WHEN it is safe to read, not WHAT is asserted.
        assert!(
            tty.wait_for_painted_after(prompt_at, &show, std::time::Duration::from_secs(2)),
            "the chat cursor was not restored after the modal closed (waited 2s): {:?}",
            tty.painted()
        );
        let painted = tty.painted();
        let mut expected_move = Vec::new();
        queue!(expected_move, MoveTo(0, expected_reservation.start))
            .expect("cursor-placement bytes");
        let expected_move = String::from_utf8(expected_move).expect("cursor bytes are UTF-8");
        assert!(
                painted[..prompt_at].contains(&expected_move),
                "the modal was not placed in its reserved rows above chat; expected {expected_move:?}, painted: {painted:?}"
            );
        let mut hide = Vec::new();
        queue!(hide, crossterm::cursor::Hide).expect("hide-cursor bytes");
        let hide = String::from_utf8(hide).expect("hide bytes are UTF-8");
        let before_modal = &painted[..prompt_at];
        assert!(
            before_modal.rfind(&hide) > before_modal.rfind(&show),
            "the mounted chat cursor must recede before the modal takes focus: {painted:?}"
        );
        assert!(
            painted[prompt_at..].contains(&show),
            "the chat cursor was not restored after the modal closed: {painted:?}"
        );
        let mut enable_wrap = Vec::new();
        queue!(enable_wrap, EnableLineWrap).expect("enable-wrap bytes");
        let enable_wrap = String::from_utf8(enable_wrap).expect("wrap bytes are UTF-8");
        assert!(
                !painted[before_modal_round..].contains(&enable_wrap),
                "a modal must keep terminal autowrap disabled so a long answer cannot spill into chat: {:?}",
                &painted[before_modal_round..]
            );

        // A slash command entered in a modal backs out to chat and leaves
        // corrective guidance in durable transcript space. The old path
        // wrote this notice through PromptWindow on the first chat row;
        // the immediate cockpit repaint then erased it.
        let slash_definition = crate::permissions::free_text_form("Slash commands stay in chat?");
        let slash_top = cockpit.screen.top;
        let slash_block_h = cockpit.screen.block_h;
        let slash_rows = cockpit.screen.rows;
        let slash_cols = cockpit.screen.cols;
        let before_slash = tty.painted().len();
        let slash_typer =
            tty.type_when_painted("Prompt — Slash commands stay in chat?", b"/help\r");
        let (slash_reply, slash_answer) = std::sync::mpsc::sync_channel(1);
        cockpit
            .handle_request(SurfaceRequest::Interact {
                interaction: Box::new(
                    newt_core::interaction_surface::SurfaceInteraction::blocking(slash_definition),
                ),
                reply: slash_reply,
            })
            .expect("present the slash-command interaction");
        assert!(
            slash_typer.join().expect("slash prompt watcher"),
            "the slash-command prompt must be visible before input"
        );
        assert_eq!(
            slash_answer.recv().expect("slash-command outcome"),
            newt_core::HumanQuestionOutcome::Cancelled,
            "a slash command is chat intent, not a modal answer"
        );

        let notice_row = crate::permissions::SLASH_COMMAND_PROMPT_NOTICE
            .as_bytes()
            .to_vec();
        let notice_rows = wrap_row(&notice_row, usize::from(slash_cols));
        let (committed_notice, _) =
            render_insert(slash_top, slash_block_h, slash_rows, &notice_rows)
                .expect("durable notice insertion plan");
        let committed_notice = String::from_utf8(committed_notice).expect("notice bytes are UTF-8");
        // #1959 (post-rebase flake, same shape as the cursor-restore wait
        // above): the notice-commit repaint is the last thing this round
        // writes, with no synchronization point before a snapshot taken
        // right after `handle_request` returns. Wait for the exact
        // evidence the assertion below checks for.
        assert!(
            tty.wait_for_painted_after(
                before_slash,
                &committed_notice,
                std::time::Duration::from_secs(2)
            ),
            "slash guidance was not committed above the cockpit viewport (waited 2s): {:?}",
            &tty.painted()[before_slash..]
        );
        let painted_after_slash = tty.painted();
        let slash_delta = &painted_after_slash[before_slash..];
        assert!(
            slash_delta.contains(&committed_notice),
            "slash guidance was not committed above the cockpit viewport: {slash_delta:?}"
        );
        assert_eq!(
            cockpit.editor.draft(),
            draft,
            "backing out of a modal keeps the mounted chat draft"
        );

        // The modal's raw reader still composes with the cockpit's own
        // raw-mode guard. This checks termios directly, independently of
        // the visibility assertion above.
        // The integration #1770 could not prove alone: that fix made the
        // modal take raw mode from the real termios instead of crossterm's
        // global, and the cockpit is exactly the second raw-mode owner
        // that broke it. Here the cockpit genuinely holds fd 0/1.
        let window = newt_core::tty::Terminal::suspend_for_prompt(
            newt_core::tty::TerminalTaker::RichSurfaceModal,
        );
        {
            let _reader = newt_core::tty::modal_prompt_controls(&window)
                .expect("the modal takes the terminal from under the cockpit");
            assert!(
                !is_canonical(0),
                "the modal's read must be non-canonical, or a keypress \
                     waits for Enter the operator does not know to press"
            );
            assert!(
                !echoes(0),
                "the kernel must not echo the answer over the prompt"
            );
        }
        drop(window);
        // The modal restored what it found: the cockpit still has a raw
        // terminal to go on painting into.
        assert!(
            !is_canonical(0),
            "the cockpit's raw mode survives the modal"
        );

        // ---- and a PANEL is lent rows on the REAL terminal ----
        //
        // The `/backends` defect, at the level only a real terminal can
        // show it: a panel that draws to fd 1 under a mounted cockpit
        // paints into the pty CAPTURE, not onto the screen, and comes back
        // as flattened transcript rows. Nothing mocked can observe that,
        // because the mock IS the capture. Here the panel draws through the
        // window the presenter lends it, and the bytes must appear on the
        // terminal the operator is looking at.
        //
        // The painter runs on its own thread because `handle_request` PARKS
        // until the window drops — that parking is the mechanism keeping
        // two writers off one terminal, so the test has to exercise it
        // rather than sidestep it.
        let (panel_reply, panel_window) =
            std::sync::mpsc::sync_channel::<Option<crate::session_worker::PanelWindow>>(1);
        let painter = std::thread::spawn(move || {
            let window = panel_window
                .recv()
                .expect("the presenter answers the panel request")
                .expect("a mounted cockpit has rows to lend");
            let mut term = window.terminal().expect("a terminal over the lent rows");
            term.draw(|f| {
                f.render_widget(
                    ratatui::widgets::Paragraph::new("PANEL BODY ON THE REAL TERMINAL"),
                    f.area(),
                );
            })
            .expect("the panel paints");
            drop(term);
            // Dropping the window is the release; the presenter is parked
            // on it.
            drop(window);
        });
        let panel_plan = plan_modal_reservation(cockpit.screen.top, cockpit.screen.rows, 6);
        assert!(
            panel_plan.chat_visible,
            "acceptance must exercise a panel with the inactive chat still \
                 visible: {panel_plan:?}"
        );
        let before_panel = tty.painted().len();
        cockpit
            .handle_request(SurfaceRequest::Panel {
                rows: 6,
                reply: panel_reply,
            })
            .expect("the presenter lends the panel its rows");
        painter.join().expect("panel painter");
        // #1959 (same shape as the cursor-restore and notice-commit waits
        // above): `join` proves the painter's `write` returned, not that
        // the responder thread — the pty master's sole reader — has drained
        // those bytes into `painted`. This is what failed on #2069, a PR
        // that touched a command parser and the slash registry and has no
        // path to cockpit terminal ownership: the delta held the
        // reservation scroll and the chat repaint that precedes the panel,
        // and simply stopped before the panel body.
        //
        // `TERMINAL` is the LAST word the panel paints — ratatui emits
        // cells in row-major order — so once it has landed, every earlier
        // byte of this round is present by stream ordering: the other three
        // words and the presenter's reservation `MoveTo` alike. Waiting on
        // the last evidence changes WHEN the snapshot is safe to read, not
        // WHAT any assertion below demands of it.
        assert!(
            tty.wait_for_painted_after(before_panel, "TERMINAL", std::time::Duration::from_secs(2)),
            "the panel's bytes never reached the real terminal (waited 2s): {:?}",
            &tty.painted()[before_panel..]
        );
        let panel_delta = tty.painted()[before_panel..].to_string();
        // Ratatui emits a cursor move per painted run, so the body arrives
        // as words rather than one string. What matters is that they
        // arrive AT ALL (they would be swallowed by the capture) and that
        // they land in the rows the presenter reserved.
        let mut panel_move = Vec::new();
        queue!(panel_move, MoveTo(0, panel_plan.start)).expect("panel cursor bytes");
        let panel_move = String::from_utf8(panel_move).expect("panel cursor bytes are UTF-8");
        assert!(
            panel_delta.contains(&panel_move),
            "the panel must be placed in its reserved rows above chat; \
                 expected {panel_move:?}, painted: {panel_delta:?}"
        );
        for word in ["PANEL", "BODY", "REAL", "TERMINAL"] {
            assert!(
                panel_delta.contains(word),
                "the panel's bytes must reach the real terminal, not the \
                     cockpit's fd 1 capture; missing {word:?}: {panel_delta:?}"
            );
        }
        assert!(
            !cockpit.chat_inactive,
            "keyboard focus returns to chat when the panel window drops"
        );
        assert_eq!(
            cockpit.editor.draft(),
            draft,
            "the chat draft survives a panel"
        );
        assert!(
            !is_canonical(0),
            "the cockpit's raw mode survives the panel"
        );

        // ---- the rows come back on the paths nobody plans for ----
        //
        // Release-on-drop is the whole synchronization mechanism, so the
        // paths worth proving are the ones with no drawing in them at all:
        // a panel that fails before its first frame, and a session that
        // vanishes between asking for rows and taking them. Either would
        // strand the presenter parked on rows nobody will release, which
        // presents as a frozen cockpit rather than as an error.
        let (early_reply, early_window) =
            std::sync::mpsc::sync_channel::<Option<crate::session_worker::PanelWindow>>(1);
        let early = std::thread::spawn(move || {
            // Took the rows, drew nothing, gave them straight back — the
            // shape of a panel whose seed was empty or whose terminal
            // failed to build.
            drop(early_window.recv().expect("the presenter answers"));
        });
        cockpit
            .handle_request(SurfaceRequest::Panel {
                rows: 6,
                reply: early_reply,
            })
            .expect("an undrawn panel still returns");
        early.join().expect("early-drop panel");
        assert!(
            !cockpit.chat_inactive,
            "focus returns when a panel drops its window without drawing"
        );

        // And the session that disappears mid-ask: the reply send fails,
        // which must undo the reservation rather than park on it.
        let (orphan_reply, orphan_window) =
            std::sync::mpsc::sync_channel::<Option<crate::session_worker::PanelWindow>>(1);
        drop(orphan_window);
        cockpit
            .handle_request(SurfaceRequest::Panel {
                rows: 6,
                reply: orphan_reply,
            })
            .expect("a vanished session is not a presenter error");
        assert!(
            !cockpit.chat_inactive,
            "a panel request nobody received must not leave chat dimmed"
        );
        assert_eq!(
            cockpit.editor.draft(),
            draft,
            "the chat draft survives both panel failure paths"
        );
        // **The block is still LIVE after both failure paths** — it takes
        // input and paints it, rather than being stuck in the geometry the
        // failed panel reserved.
        //
        // Proven by typing something new, not by `dirty = true` + a length
        // comparison. Ratatui diffs its buffer: a redraw of unchanged
        // content emits a handful of trailing bytes and nothing else, so
        // the length check was asserting almost nothing — and what little
        // it did assert raced the responder thread's drain, which is how
        // it failed under coverage instrumentation. New text can only
        // reach the terminal through a live block, and
        // `wait_for_painted_after` reads at a point where the drain has
        // caught up.
        let before_repaint = tty.painted().len();
        {
            let (editor, screen) = (&mut cockpit.editor, &mut cockpit.screen);
            editor
                .on_event(Event::Paste(" and still alive".into()), screen)
                .expect("the mounted editor still takes input");
        }
        cockpit
            .draw()
            .expect("the cockpit still paints after a failed panel");
        assert!(
            tty.wait_for_painted_after(before_repaint, "alive", std::time::Duration::from_secs(2)),
            "the cockpit block is still live after the panel failure paths \
                 (waited 2s): {:?}",
            &tty.painted()[before_repaint..]
        );
    }

    // ---- and the terminal comes back, exactly ----
    assert!(is_canonical(0), "canonical mode restored on teardown");
    assert!(echoes(0), "echo restored on teardown");
    assert!(
        modes_equal(&before, &termios_of(0)),
        "every termios mode field restored exactly, not approximately — \
             this is the assertion an emitted-escape-bytes check cannot make; \
             differs: {}",
        mode_diff(&before, &termios_of(0))
    );
}

/// Acceptance (#1744): the same restoration through an ABNORMAL exit.
///
/// Proven against the modes guard itself rather than a second cockpit: the
/// guard is the mechanism (`Presenter::open` binds it before the fallible
/// capture install precisely so a `?` or a panic cannot strand the
/// terminal), and one cockpit per process is a harness limit, not a reason
/// to leave the unwind path unproven.
#[serial_test::serial(tty_arbiter)]
#[test]
fn a_panic_restores_the_real_termios_through_the_modes_guard() {
    let _tty = TestTty::install();
    // Clear crossterm's saved-mode static FIRST. It is process-global, so
    // an earlier test in this binary may have populated it.
    //
    // #1925 is what makes this belt and braces rather than load-bearing:
    // the presenter takes raw through `RawModeGuard`, which captures the
    // real termios and never consults that static. The hazard this line
    // guards against — restoring an OLDER baseline than the one this test
    // set — is the very defect the swap removes. Kept because other tests
    // in this binary still populate the static.
    let _ = crossterm::terminal::disable_raw_mode();
    set_canonical_echo(0);
    let before = termios_of(0);

    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let result = std::panic::catch_unwind(|| {
        // Model the PRESENTER'S OWN FIELD PAIR, in its declaration order
        // (#1925). A struct, not two locals, and deliberately: struct
        // fields drop in declaration order while locals drop in REVERSE,
        // so two `let`s here would exercise the opposite order to the one
        // the presenter has and prove nothing about it.
        struct Held {
            _restore: crate::RestoreOnDrop<fn()>,
            _raw: newt_core::tty::raw_mode::RawModeGuard,
        }
        let _held = Held {
            _restore: crate::RestoreOnDrop {
                restore: restore_terminal_modes,
            },
            _raw: newt_core::tty::raw_mode::RawModeGuard::enter().expect("raw"),
        };
        assert!(!is_canonical(0), "precondition: raw mode taken");
        panic!("turn exploded while the terminal was raw");
    });
    std::panic::set_hook(hook);

    assert!(
        result.is_err(),
        "the panic must propagate, not be swallowed"
    );
    assert!(
        is_canonical(0),
        "canonical mode restored through the unwind"
    );
    assert!(echoes(0), "echo restored through the unwind");
    assert!(
        modes_equal(&before, &termios_of(0)),
        "every termios mode field restored exactly after a panic; differs: {}",
        mode_diff(&before, &termios_of(0))
    );
}
