/// **The policy (#1866)**, in the shape `progress_sink`'s
/// `protocol_mode_vetoes_rendering_from_every_capability` uses, and for the
/// same reason: `enter_protocol_mode` is documented as one-way, so a test
/// that set the real flag would veto every sibling test in this binary for
/// the rest of the run.
#[test]
fn protocol_mode_vetoes_every_prompt() {
    assert!(
        !super::prompts_permitted(true),
        "fd 1 may be a JSON-RPC wire; no prompt may reach it"
    );
}

/// The anti-vacuous twin: without it `prompts_permitted` could be `false`
/// always and the test above would still pass. Exactly one shape prompts,
/// and this names it.
#[test]
fn and_a_process_outside_protocol_mode_may_prompt() {
    assert!(
        super::prompts_permitted(false),
        "an ordinary terminal session is the ONE shape that prompts — if \
         this fails the veto test above is vacuous"
    );
}

/// The veto as the CALLER meets it: a vetoed window refuses, and refuses in
/// the two different ways the three methods deliberately chose.
///
/// Reaches `PromptWindow::vetoed(..)` because this module is the seal's
/// inside. Nothing here widens it — the constructor is private, and the
/// trybuild proofs under `tests/ui/` still pin that no public constructor
/// exists, the struct cannot be literaled, and `test_stub` is unreachable
/// from outside.
#[test]
fn a_vetoed_window_refuses_to_ask_or_read_and_drops_notices() {
    let window = super::PromptWindow::vetoed(super::PromptOutput::Stdout);

    assert!(
        window.ask("question > ").is_err(),
        "ask must refuse rather than report a question it never wrote"
    );

    let mut buf = String::new();
    let read = window.read_line_into(&mut buf);
    assert!(
        read.is_err(),
        "read must ERROR, not return Ok(0) — EOF is a deliberate empty \
         answer from a human, and forging one is not failing closed: {read:?}"
    );
    assert!(buf.is_empty(), "nothing may land in the caller's buffer");

    assert!(
        window.notice("fyi").is_ok(),
        "a notice is informational and is dropped silently, which is why \
         it differs from ask"
    );
}

/// …and the twin for THAT: an unvetoed window is not refused, so the
/// assertions above are about protocol mode rather than about every
/// window being inert.
#[test]
fn and_an_unvetoed_window_is_not_refused() {
    let window = super::PromptWindow::test_stub();
    assert!(window.ask("").is_ok(), "an ordinary window may ask");
    assert!(window.notice("").is_ok(), "an ordinary window may narrate");
}

/// The explicit-output seam writes both prompt byte families to the file
/// it owns. A regular file is deliberately non-terminal, which also pins
/// the capability probe modal input uses instead of process stdout.
#[serial_test::serial(tty_arbiter)]
#[test]
fn an_explicit_prompt_output_routes_ask_and_notice_to_that_file() {
    let output = tempfile::NamedTempFile::new().expect("prompt output file");
    let c = counter();
    let dynamic: Arc<dyn Ephemeral> = c.clone();
    Terminal::register(9_002, &dynamic);
    let window = Terminal::suspend_for_prompt_to(
        output.reopen().expect("independent prompt output handle"),
        TerminalTaker::PlainCliConfirm,
    );

    assert_eq!(
        c.erased.load(Ordering::SeqCst),
        1,
        "the alternate output must not bypass prompt arbitration"
    );
    assert!(suspended(), "the alternate output quiesces other writers");
    assert!(
        prompt_stdin_active(),
        "the alternate output still owns prompt stdin"
    );
    assert!(
        !window.output_is_terminal(),
        "a regular-file destination must select the non-TTY modal path"
    );
    window.ask("question > ").expect("write the question");
    window.notice("narration").expect("write the notice");
    drop(window);

    assert!(!suspended(), "dropping the window resumes other writers");
    assert!(!prompt_stdin_active(), "dropping the window releases stdin");
    assert_eq!(c.restored.load(Ordering::SeqCst), 1);
    assert_eq!(
        std::fs::read_to_string(output.path()).expect("read routed prompt bytes"),
        "question > narration\n"
    );
}

/// Real-resource grounding for the modal branch predicate: a duplicated
/// PTY slave is a `File` just like a presenter's saved terminal, and the
/// window both recognizes it as interactive and writes to that device.
#[cfg(unix)]
#[serial_test::serial(tty_arbiter)]
#[test]
fn an_explicit_pty_output_is_detected_and_written_as_a_terminal() {
    use std::io::Read as _;
    use std::os::fd::FromRawFd as _;

    let mut master_fd = -1;
    let mut slave_fd = -1;
    // SAFETY: `openpty` initializes both owned descriptors on success. Each
    // is immediately transferred into exactly one `File` below.
    let opened = unsafe {
        libc::openpty(
            &mut master_fd,
            &mut slave_fd,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(opened, 0, "open the grounding PTY");
    // SAFETY: successful `openpty` returned fresh descriptors, and these
    // `File`s become their sole owners.
    let (mut master, output) =
        unsafe { (File::from_raw_fd(master_fd), File::from_raw_fd(slave_fd)) };
    let window = Terminal::suspend_for_prompt_to(output, TerminalTaker::PlainCliConfirm);

    assert!(
        window.output_is_terminal(),
        "the saved terminal file must select the interactive modal path"
    );
    window.ask("direct prompt").expect("write to the PTY slave");

    let mut painted = [0_u8; "direct prompt".len()];
    master
        .read_exact(&mut painted)
        .expect("read the explicitly routed terminal bytes");
    assert_eq!(&painted, b"direct prompt");
    drop(window);
}

/// Protocol veto semantics belong to the window, not to stdout. Supplying
/// another file must not create a side door that can emit a question.
#[test]
fn a_vetoed_explicit_output_still_emits_zero_bytes() {
    let output = tempfile::NamedTempFile::new().expect("vetoed prompt output file");
    let window = PromptWindow::vetoed(PromptOutput::File(
        output.reopen().expect("independent prompt output handle"),
    ));

    assert!(window.ask("question").is_err());
    assert!(window.notice("narration").is_ok());
    drop(window);

    assert_eq!(
        std::fs::read_to_string(output.path()).expect("read vetoed prompt output"),
        ""
    );
}

/// The original entry point remains process-stdout-backed. Keeping this
/// as a distinct assertion prevents a future refactor from silently
/// making every prompt require an explicit file.
#[test]
fn the_default_prompt_window_keeps_the_process_stdout_route() {
    let window = PromptWindow::test_stub();
    assert!(matches!(&window.output, PromptOutput::Stdout));
    assert_eq!(
        window.output_is_terminal(),
        io::stdout().is_terminal(),
        "the legacy window must keep probing the stream it writes"
    );
}

use super::*;
use std::sync::atomic::AtomicUsize;

struct Counter {
    erased: AtomicUsize,
    restored: AtomicUsize,
}

impl Ephemeral for Counter {
    fn erase(&self) {
        self.erased.fetch_add(1, Ordering::SeqCst);
    }
    fn restore(&self) {
        self.restored.fetch_add(1, Ordering::SeqCst);
    }
}

fn counter() -> Arc<Counter> {
    Arc::new(Counter {
        erased: AtomicUsize::new(0),
        restored: AtomicUsize::new(0),
    })
}

/// §6.4(a): at most ONE ephemeral lease exists at a time. The second
/// acquirer is refused rather than becoming a second writer on the same row.
#[serial_test::serial(tty_arbiter)]
#[test]
fn the_line_admits_exactly_one_writer() {
    let first = Terminal::lease_with_caps(LineCaps::Own, Sink::Stdout)
        .expect("an Own capability yields the line");
    assert!(
        Terminal::lease_with_caps(LineCaps::Own, Sink::Stdout).is_none(),
        "a second writer must NOT get the line while the first holds it"
    );
    drop(first);
    let third = Terminal::lease_with_caps(LineCaps::Own, Sink::Stdout);
    assert!(third.is_some(), "the line is reusable once released");
}

/// The gate is honored: `LineCaps::None` yields no lease, so a caller emits
/// zero bytes rather than painting into a pipe.
#[serial_test::serial(tty_arbiter)]
#[test]
fn no_capability_means_no_lease_and_no_bytes() {
    assert!(Terminal::lease_with_caps(LineCaps::None, Sink::Stdout).is_none());
}

/// §6.5's mechanism, at the unit tier: suspending for a prompt erases every
/// registered ephemeral BEFORE the window exists, and restores on drop.
#[serial_test::serial(tty_arbiter)]
#[test]
fn suspending_erases_every_ephemeral_then_restores() {
    let c = counter();
    let dynamic: Arc<dyn Ephemeral> = c.clone();
    Terminal::register(9_001, &dynamic);

    assert_eq!(c.erased.load(Ordering::SeqCst), 0);
    {
        let w = Terminal::suspend_for_prompt(TerminalTaker::PlainCliConfirm);
        assert_eq!(
            c.erased.load(Ordering::SeqCst),
            1,
            "the ephemeral must be erased before the window is handed out"
        );
        assert!(suspended(), "the ticker must see the suspend flag");
        // Painting is inert while a question is on screen — this is the
        // property that stops a 100ms ticker overwriting the prompt.
        assert_eq!(c.restored.load(Ordering::SeqCst), 0);
        drop(w);
    }
    assert!(!suspended(), "the flag clears when the window drops");
    assert_eq!(c.restored.load(Ordering::SeqCst), 1);
}

/// A lease held while a window is alive paints nothing, so the question
/// stays the most recent thing on the terminal.
#[serial_test::serial(tty_arbiter)]
#[test]
fn a_live_prompt_window_makes_painting_a_no_op() {
    let lease = Terminal::lease_with_caps(LineCaps::Own, Sink::Stdout).expect("lease");
    let w = Terminal::suspend_for_prompt(TerminalTaker::PlainCliConfirm);
    lease.paint(|_w| Ok(()));
    assert!(
        !lease.painted.load(Ordering::SeqCst),
        "a paint during a prompt must be dropped, not deferred onto the question"
    );
    drop(w);
    lease.paint(|_w| Ok(()));
    assert!(
        lease.painted.load(Ordering::SeqCst),
        "painting resumes once the window is gone"
    );
}

/// Erase is idempotent and flag-guarded, so a `Drop` after an explicit
/// teardown cannot clear a row someone else has since taken.
#[serial_test::serial(tty_arbiter)]
#[test]
fn erase_is_idempotent() {
    let lease = Terminal::lease_with_caps(LineCaps::Own, Sink::Stdout).expect("lease");
    lease.paint(|_w| Ok(()));
    assert!(lease.painted.load(Ordering::SeqCst));
    lease.erase();
    assert!(!lease.painted.load(Ordering::SeqCst));
    lease.erase(); // no-op, no panic, no stray escape
    assert!(!lease.painted.load(Ordering::SeqCst));
}

/// Nested prompts keep ownership until the LAST one releases — dropping an
/// inner guard must not hand stdin back to the turn watcher mid-question.
/// (Moved here from `newt-tui/src/permissions.rs` with the mechanism.)
#[serial_test::serial(tty_arbiter)]
#[test]
fn nested_prompts_hold_stdin_until_the_outermost_releases() {
    assert!(
        !prompt_stdin_active(),
        "test starts with no active prompt stdin owner"
    );
    {
        let _outer = StdinToken::acquire();
        assert!(
            try_watch_stdin().is_none(),
            "the watcher cannot read while a prompt owns stdin"
        );
        assert!(prompt_stdin_active());
        {
            let _nested = StdinToken::acquire();
            assert!(prompt_stdin_active(), "nested prompts keep ownership");
        }
        assert!(
            prompt_stdin_active(),
            "dropping one nested guard must not release stdin early"
        );
    }
    assert!(
        !prompt_stdin_active(),
        "prompt stdin ownership must clear when the last guard drops"
    );
}

/// The watcher's protected read BLOCKS a prompt from entering, closing the
/// check-then-read race at permission transitions. (Also moved here.)
#[serial_test::serial(tty_arbiter)]
#[test]
fn watcher_read_token_blocks_prompt_entry_until_the_read_finishes() {
    let watcher = try_watch_stdin().expect("watcher acquires idle stdin");
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let prompt = std::thread::spawn(move || {
        let _prompt = StdinToken::acquire();
        let _ = entered_tx.send(());
    });

    assert!(
        entered_rx.recv_timeout(Duration::from_millis(50)).is_err(),
        "prompt entered during the watcher's protected read"
    );
    drop(watcher);
    entered_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("prompt enters once the watcher releases stdin");
    prompt.join().unwrap();
}

/// The stdin half still interlocks: the watcher's read token blocks a
/// prompt, and a prompt blocks the watcher. (The behavior the TUI's
/// `PromptStdinGuard` / `try_watch_stdin` pair had, now under one arbiter.)
#[serial_test::serial(tty_arbiter)]
#[test]
fn watcher_and_prompt_exclude_each_other_on_stdin() {
    assert!(!prompt_stdin_active());
    let watcher = try_watch_stdin().expect("the watcher acquires idle stdin");
    assert!(
        try_watch_stdin().is_none(),
        "the watcher's read token is exclusive"
    );
    drop(watcher);

    let token = StdinToken::acquire();
    assert!(prompt_stdin_active());
    assert!(
        try_watch_stdin().is_none(),
        "the watcher must not read while a prompt owns stdin"
    );
    drop(token);
    assert!(!prompt_stdin_active());
    assert!(try_watch_stdin().is_some(), "released on drop");
}

/// **#1959: the seal has exactly two doors, and both gates sit BELOW the
/// fork.**
///
/// The seal's value is that its doors are enumerated and each is proven.
/// A second public constructor is fine; a second constructor that skipped
/// the protocol veto or the stdin token would be a hole, and the thing
/// that keeps both honest is that they delegate to ONE private builder
/// with the gates inside it.
///
/// Stated as a source scan rather than a behaviour, because the property
/// is structural: "no future door can be added above the gates". A
/// behavioural test can only cover the doors that exist today.
///
/// Production code only, and cut at the test module — the lesson
/// `config_panel::enter_panel_raw_mode_is_the_only_way_in` records the
/// hard way: this test lives IN the file it scans, so its own needles
/// would otherwise be counted.
#[test]
fn the_seal_has_exactly_two_doors_and_both_gates_sit_below_the_fork() {
    let src = include_str!("arbiter.rs");
    let production = src.split("\n#[cfg(test)]").next().unwrap_or("");
    assert!(
        production.len() > 1000,
        "the production cut read nothing; every count below would be vacuous"
    );

    assert_eq!(
        production.matches("pub fn suspend_for_prompt(").count(),
        1,
        "the stdout door"
    );
    assert_eq!(
        production.matches("pub fn suspend_for_prompt_to(").count(),
        1,
        "the File door (#1959)"
    );
    assert_eq!(
        production
            .matches("Self::suspend_for_prompt_with_output(")
            .count(),
        2,
        "BOTH public doors must delegate to the one private builder — a \
         third door, or a door that built a PromptWindow itself, would \
         bypass the gates below"
    );

    // Call forms, not names: `prompts_permitted` is also DEFINED here and
    // discussed in prose, and counting mentions would move whenever
    // someone edited a comment.
    let veto = "prompts_permitted(super::caps::protocol_mode())";
    let acquire = "StdinToken::acquire()";
    assert_eq!(production.matches(veto).count(), 1, "one veto, one place");
    assert_eq!(
        production.matches(acquire).count(),
        1,
        "one acquire, one place"
    );

    let fork = production
        .find("fn suspend_for_prompt_with_output")
        .expect("the private builder");
    assert!(
        production.find(veto).is_some_and(|at| at > fork),
        "the protocol veto was hoisted ABOVE the fork — it would then \
         cover only the door it sits in, and the other would prompt on a \
         JSON-RPC wire"
    );
    assert!(
        production.find(acquire).is_some_and(|at| at > fork),
        "the stdin token was hoisted ABOVE the fork — one door would then \
         ask without exclusive stdin"
    );
}

/// **#1959: the File door takes the same exclusive stdin token.**
///
/// The scan above proves the acquire is shared; this proves what sharing
/// it buys, through the same `prompt_stdin_active` observable
/// `watcher_and_prompt_exclude_each_other_on_stdin` uses for door one.
///
/// `/dev/null` is a real fd, which the unit tier otherwise avoids —
/// `PromptOutput::File` takes a `std::fs::File` and offers no seam. It is
/// never written to here: the window is constructed and dropped, and the
/// assertions are all about stdin.
#[cfg(unix)]
#[serial_test::serial(tty_arbiter)]
#[test]
fn the_file_door_takes_the_same_exclusive_stdin_token() {
    assert!(!prompt_stdin_active(), "stdin must start idle");
    let sink = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/null")
        .expect("/dev/null");

    let window = Terminal::suspend_for_prompt_to(sink, TerminalTaker::PlainCliConfirm);
    assert!(
        prompt_stdin_active(),
        "the File door must take the prompt's stdin token, like the stdout door"
    );
    assert!(
        try_watch_stdin().is_none(),
        "and hold it EXCLUSIVELY — the turn watcher must not read underneath it"
    );

    drop(window);
    assert!(!prompt_stdin_active(), "released on drop");
    assert!(try_watch_stdin().is_some(), "the watcher may read again");
}

/// The test stub is inert: it arbitrates nothing, so it cannot leave the
/// process suspended for every later test.
#[serial_test::serial(tty_arbiter)]
#[test]
fn the_test_stub_arbitrates_nothing() {
    let w = PromptWindow::test_stub();
    assert!(!suspended());
    drop(w);
    assert!(!suspended());
}

/// Blocked/Unblocked describe reality, not intent: `Blocked` is emitted
/// only once stdin ownership and suspension have succeeded (observable
/// here as stdin already being prompt-owned when the observer runs),
/// `Unblocked` on drop, and the inert test stub emits neither.
#[serial_test::serial(tty_arbiter)]
#[test]
fn blocked_is_emitted_after_stdin_acquisition_and_unblocked_on_drop() {
    use crate::lifecycle::LifecycleEvent;

    // (event, stdin_owned_at_callback), recorded only for this thread —
    // the lifecycle registry is process-global and sibling tests emit
    // concurrently.
    let log: Arc<Mutex<Vec<(LifecycleEvent, bool)>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&log);
    let me = std::thread::current().id();
    let sub = crate::lifecycle::subscribe(move |event| {
        if std::thread::current().id() == me {
            sink.lock()
                .unwrap()
                .push((event.event.clone(), prompt_stdin_active()));
        }
    });

    {
        let _stub = PromptWindow::test_stub();
    }
    assert!(
        log.lock().unwrap().is_empty(),
        "the stub must not emit lifecycle events"
    );

    let w = Terminal::suspend_for_prompt(TerminalTaker::PlainCliConfirm);
    drop(w);
    drop(sub);
    assert_eq!(
        *log.lock().unwrap(),
        vec![
            (LifecycleEvent::Blocked, true),
            (LifecycleEvent::Unblocked, false)
        ],
        "Blocked is emitted exactly once, with stdin already owned \
         (post-acquire, not intent); Unblocked on drop, after stdin is released"
    );
}

// ---- #1979: region ownership --------------------------------------

fn rows(top: u16, height: u16) -> Region {
    Region::Rows { top, height }
}

#[test]
fn regions_intersect_only_when_they_share_a_row() {
    assert!(rows(10, 3).intersects(rows(12, 2)), "overlapping");
    assert!(rows(12, 2).intersects(rows(10, 3)), "and symmetrically");
    assert!(
        !rows(10, 3).intersects(rows(13, 2)),
        "adjacent is not overlapping"
    );
    assert!(!rows(13, 2).intersects(rows(10, 3)), "and symmetrically");
    // The alternate screen is every row, including against itself.
    assert!(Region::WholeScreen.intersects(rows(0, 1)));
    assert!(rows(40, 1).intersects(Region::WholeScreen));
    assert!(Region::WholeScreen.intersects(Region::WholeScreen));
    // A zero-height region owns nothing.
    assert!(!rows(10, 0).intersects(rows(10, 3)));
}

#[serial_test::serial(tty_arbiter)]
#[test]
fn the_mint_refuses_rows_another_writer_holds() {
    let held = Terminal::lease_region(rows(18, 6), OnCollision::Refuse).expect("first take");
    assert_eq!(held.region(), rows(18, 6));
    assert!(
        Terminal::lease_region(rows(20, 4), OnCollision::Refuse).is_none(),
        "two writers were granted the same rows — this is #1977"
    );
    // TWIN: the refusal is about the OVERLAP, not about refusing always.
    let elsewhere =
        Terminal::lease_region(rows(2, 4), OnCollision::Refuse).expect("clear rows are granted");
    assert_eq!(elsewhere.region(), rows(2, 4));
    // And dropping returns them.
    drop(held);
    drop(elsewhere);
    let reclaimed =
        Terminal::lease_region(rows(18, 6), OnCollision::Refuse).expect("released rows return");
    assert_eq!(reclaimed.region(), rows(18, 6));
}

#[serial_test::serial(tty_arbiter)]
#[test]
fn shift_opens_above_the_holder_rather_than_through_it() {
    let prompt = Terminal::lease_region(rows(18, 6), OnCollision::Refuse).expect("prompt");
    let panel = Terminal::lease_region(rows(18, 6), OnCollision::Shift).expect("panel shifts");
    assert_eq!(
        panel.region(),
        rows(12, 6),
        "#1977: the panel must open ABOVE the prompt's rows, not over them"
    );
    assert!(
        !panel.region().intersects(prompt.region()),
        "the shifted region still overlaps"
    );
}

#[serial_test::serial(tty_arbiter)]
#[test]
fn shift_refuses_when_there_is_no_room_above() {
    let _floor = Terminal::lease_region(rows(0, 4), OnCollision::Refuse).expect("floor");
    assert!(
        Terminal::lease_region(rows(2, 6), OnCollision::Shift).is_none(),
        "shifting off the top of the screen must refuse, not wrap"
    );
}

#[serial_test::serial(tty_arbiter)]
#[test]
fn the_whole_screen_collides_with_everything_and_shifts_nowhere() {
    let _rowsy = Terminal::lease_region(rows(10, 2), OnCollision::Refuse).expect("some rows");
    assert!(
        Terminal::lease_region(Region::WholeScreen, OnCollision::Refuse).is_none(),
        "the alternate screen takes every row and cannot share"
    );
    assert!(
        Terminal::lease_region(Region::WholeScreen, OnCollision::Shift).is_none(),
        "whole-screen has nowhere to shift to"
    );
}

#[serial_test::serial(tty_arbiter)]
#[test]
fn suspend_holder_takes_the_rows_the_caller_already_quiesced() {
    let _held = Terminal::lease_region(rows(18, 6), OnCollision::Refuse).expect("holder");
    let taken = Terminal::lease_region(rows(18, 6), OnCollision::SuspendHolder)
        .expect("a caller that quiesced the holder takes the rows");
    assert_eq!(taken.region(), rows(18, 6));
}

/// The cockpit presenter's block moves and is clamped on resize, so the
/// lease has to move WITHOUT a release-and-retake window.
#[serial_test::serial(tty_arbiter)]
#[test]
fn a_lease_relocates_in_place_and_still_respects_other_holders() {
    let _other = Terminal::lease_region(rows(0, 4), OnCollision::Refuse).expect("other");
    let mut moving = Terminal::lease_region(rows(18, 6), OnCollision::Refuse).expect("moving");
    assert!(
        moving.relocate(rows(10, 6), OnCollision::Refuse),
        "a clear move must succeed"
    );
    assert_eq!(moving.region(), rows(10, 6));
    // TWIN: relocation is checked, not merely recorded.
    assert!(
        !moving.relocate(rows(2, 4), OnCollision::Refuse),
        "relocating onto another holder was allowed"
    );
    assert_eq!(
        moving.region(),
        rows(10, 6),
        "a refused move must leave the lease holding what it held"
    );
    // Moving onto its OWN rows is not a self-collision.
    assert!(
        moving.relocate(rows(10, 8), OnCollision::Refuse),
        "a lease may resize in place"
    );
}

/// **Compose proof.** Panel over prompt, close, both restored in order —
/// the nested-modal property (`a_nested_frame_does_not_restore_the_
/// terminal_early`) at region scale. The inner holder returning its rows
/// must not disturb the outer one, which is what makes nesting safe.
#[serial_test::serial(tty_arbiter)]
#[test]
fn an_inner_region_returns_without_disturbing_the_outer_one() {
    let prompt = Terminal::lease_region(rows(18, 6), OnCollision::Refuse).expect("prompt");
    {
        let panel = Terminal::lease_region(rows(18, 6), OnCollision::Shift).expect("panel");
        assert_eq!(panel.region(), rows(12, 6));
        // While both are up, the rows they hold are BOTH unavailable.
        assert!(
            Terminal::lease_region(rows(12, 6), OnCollision::Refuse).is_none(),
            "the panel's rows were re-let while it held them"
        );
        assert!(
            Terminal::lease_region(rows(18, 6), OnCollision::Refuse).is_none(),
            "the prompt's rows were re-let while it held them"
        );
    }
    // The panel closed. ITS rows are free; the prompt's are NOT — the
    // inner drop must not have released the outer holder's claim.
    let reclaimed =
        Terminal::lease_region(rows(12, 6), OnCollision::Refuse).expect("the panel's rows return");
    assert_eq!(reclaimed.region(), rows(12, 6));
    assert!(
        Terminal::lease_region(rows(18, 6), OnCollision::Refuse).is_none(),
        "closing the panel released the PROMPT's rows — restoring more than \
         it took is the nested-modal defect"
    );
    drop(prompt);
    drop(reclaimed);
}

/// **#2027 (red-first): #2019's shape, at the arbiter.**
///
/// `/settings` acquired its own prompt window while the cockpit had an
/// editor mounted below it — two live chevrons, a modal with no rows
/// reserved, and a header repainting through the question every 250 ms.
/// Every one of those follows from the same fact: this file decides who
/// owns ROWS and, separately, hands out the TERMINAL, and the second half
/// never asked the first.
///
/// So: a surface holds rows, and somebody who did not declare that it
/// would take them asks for a prompt window. Nothing may have been taken.
/// Asserted on stdin ownership and the suspend flag rather than on bytes,
/// so the failing run emits nothing onto a sibling test's terminal.
#[serial_test::serial(tty_arbiter)]
#[test]
fn a_bare_acquisition_is_refused_while_a_surface_holds_rows() {
    let _held = Terminal::lease_region(rows(18, 6), OnCollision::Refuse).expect("holder");
    let window = Terminal::suspend_for_prompt(TerminalTaker::SlashForm);
    assert!(
        !prompt_stdin_active(),
        "a refused acquisition must not have taken stdin"
    );
    assert!(
        !suspended(),
        "a refused acquisition must not have quiesced the holder"
    );
    assert!(
        window.ask("question > ").is_err(),
        "a refused window must not report a question it never wrote"
    );
    assert!(
        window.read_line_into(&mut String::new()).is_err(),
        "and must ERROR rather than synthesise an EOF nobody typed"
    );
    assert!(
        window.notice("fyi").is_ok(),
        "a notice is informational and is dropped silently, as under the veto"
    );
    drop(window);
    assert!(
        !suspended(),
        "dropping a refused window must not clear a flag it never set"
    );
}

/// **ANTI-VACUOUS TWIN (rows).** The refusal above is about the OVERLAP,
/// not about `SlashForm` never getting a window. With no holder the same
/// taker acquires for real — stdin taken, ephemerals erased, the window
/// able to speak. Without this, `on_held_rows` could return `Refuse`
/// unconditionally and every prompt in the process would be silently dead.
#[serial_test::serial(tty_arbiter)]
#[test]
fn and_the_same_taker_acquires_when_nobody_holds_rows() {
    let counter = counter();
    let dynamic: Arc<dyn Ephemeral> = counter.clone();
    Terminal::register(9_027, &dynamic);

    let window = Terminal::suspend_for_prompt(TerminalTaker::SlashForm);
    assert!(prompt_stdin_active(), "an uncontested taker owns stdin");
    assert!(suspended(), "and quiesces every registered ephemeral");
    assert_eq!(counter.erased.load(Ordering::SeqCst), 1);
    assert!(window.ask("").is_ok(), "and may speak");
    drop(window);
    assert!(!prompt_stdin_active());
}

/// **ANTI-VACUOUS TWIN (declaration).** `SuspendHolder` is the OTHER legal
/// move: a caller that has already quiesced the holder — the cockpit
/// presenter, which holds the rows itself — takes them deliberately. If
/// the region check ignored the declaration, this would refuse too and the
/// cockpit's modal (and every mid-turn permission prompt under it) would
/// stop reaching the operator.
#[serial_test::serial(tty_arbiter)]
#[test]
fn a_declared_row_taker_acquires_while_a_surface_holds_rows() {
    let _held = Terminal::lease_region(rows(18, 6), OnCollision::Refuse).expect("holder");
    let window = Terminal::suspend_for_prompt(TerminalTaker::CockpitModal);
    assert!(
        prompt_stdin_active(),
        "a declared SuspendHolder taker must still get the terminal"
    );
    assert!(suspended(), "and must still quiesce the holder");
    assert!(window.ask("").is_ok(), "and must be able to speak");
    drop(window);
}

/// A refused acquisition is still COUNTED. §6.10's default-deny witness is
/// "no prompt was ever constructed"; hiding the attempts of a caller that
/// is reaching past a surface would hide exactly the caller this guard
/// exists to name — the same reasoning the protocol veto records.
#[serial_test::serial(tty_arbiter)]
#[test]
fn a_refused_acquisition_is_still_counted() {
    let _held = Terminal::lease_region(rows(18, 6), OnCollision::Refuse).expect("holder");
    let before = prompt_windows_constructed();
    drop(Terminal::suspend_for_prompt(TerminalTaker::SlashForm));
    assert_eq!(
        prompt_windows_constructed(),
        before + 1,
        "a refused attempt must remain visible in the counter"
    );
}

/// The registry is complete, distinct, and each row says what it does.
///
/// `name` is an exhaustive match, so a new variant cannot be added without
/// visiting it — and the count below is what makes the author visit
/// [`TerminalTaker::ALL`] in the same edit.
#[test]
fn the_taker_table_is_complete_and_distinct() {
    assert_eq!(
        TerminalTaker::ALL.len(),
        9,
        "a variant was added without reaching ALL, which the registry test walks"
    );
    let mut names: Vec<&str> = TerminalTaker::ALL.iter().map(|t| t.name()).collect();
    names.sort_unstable();
    let listed = names.len();
    names.dedup();
    assert_eq!(names.len(), listed, "two takers share a name: {names:?}");
}

/// **`Shift` is not a terminal policy.** A prompt window is the whole
/// terminal; there is nowhere above it to move to. The type cannot say so
/// (it reuses `OnCollision`, which is right — a second two-variant enum
/// for the same two intents is the sprawl the reuse discipline forbids),
/// so the table says it here.
#[test]
fn no_taker_declares_a_shift() {
    for taker in TerminalTaker::ALL {
        assert_ne!(
            taker.on_held_rows(),
            OnCollision::Shift,
            "`{}` declared Shift, which a whole-terminal take cannot honour",
            taker.name()
        );
    }
}

/// **The ratchet, and it is not a call-site count.**
///
/// #2027 records why counting acquisitions is the wrong guard: it would
/// have passed #2019 unchanged, because the call site already existed.
/// What is worth counting is the ESCAPE HATCH — takers that take rows
/// another surface owns. Two are justified today, each documented on its
/// variant. **This number may only go DOWN.** A third is how the
/// declaration turns back into a formality.
#[test]
fn exactly_two_takers_take_rows_they_do_not_own() {
    let deliberate: Vec<&str> = TerminalTaker::ALL
        .iter()
        .filter(|t| t.on_held_rows() == OnCollision::SuspendHolder)
        .map(|t| t.name())
        .collect();
    assert_eq!(
        deliberate,
        vec!["CockpitModal", "PermissionAuthorization"],
        "the set of takers that take rows they do not own may only shrink"
    );
}

/// A relocation is sometimes a REPORT, not a request (#1980).
///
/// The cockpit presenter recomputes its top from the terminal's new size
/// on a resize. Refusing that move would not un-resize the terminal — it
/// would leave the lease naming rows the block has already left.
#[serial_test::serial(tty_arbiter)]
#[test]
fn a_forced_relocation_lands_where_a_checked_one_is_refused() {
    let _other = Terminal::lease_region(rows(0, 4), OnCollision::Refuse).expect("other");
    let mut block = Terminal::lease_region(rows(18, 6), OnCollision::Refuse).expect("block");

    // Checked: refused, and the lease is unchanged.
    assert!(!block.relocate(rows(2, 4), OnCollision::Refuse));
    assert_eq!(block.region(), rows(18, 6));

    // Forced: lands, because the move already happened on the terminal.
    assert!(
        block.relocate(rows(2, 4), OnCollision::SuspendHolder),
        "a forced relocation must land, or the lease describes rows the \
         writer has already left"
    );
    assert_eq!(block.region(), rows(2, 4));

    // TWIN: `Shift` is rejected outright rather than quietly landing
    // somewhere else, which would make the lease disagree with the
    // caller's own bookkeeping.
    assert!(
        !block.relocate(rows(0, 4), OnCollision::Shift),
        "a shifting relocation would move the lease somewhere the caller \
         did not ask for"
    );
    assert_eq!(block.region(), rows(2, 4));
}
