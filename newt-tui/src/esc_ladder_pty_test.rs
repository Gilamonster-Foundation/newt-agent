//! #2005 — PTY acceptance for the Esc rung: on a real terminal, with a real
//! turn running, Esc from vi NORMAL interrupts, and Esc from vi INSERT or from
//! a half-typed operator does not.
//!
//! **Not `#[ignore]`d, on purpose.** This is the primary per-PR guard for the
//! rung (`docs/decisions/key_ladder_crate.md` §5 G2). Everything else that
//! watches this behaviour is weaker and says so: the dead-code lint on
//! `escape_during_turn` is voided by making it `pub`, and the behaviour-map's
//! `refs.production` resolver counts `fn <symbol>` DEFINITIONS, so deleting
//! the presenter's match arm leaves every registry row green. Only this file
//! fails when the arm goes away. Same module gate as
//! `settings_form_pty_test` — `#[cfg(all(test, unix, feature = "rich-tui"))]`,
//! which workspace feature unification turns on — so it rides
//! `cargo test --workspace`.
//!
//! **What it grounds.** The mocked ladder tests (`esc_ladder.rs`) and the
//! registration conformance test (`rich_input_tests/esc_ladder.rs`) both reason about a
//! `ClaimSet` this crate constructs from state it also owns. Neither can tell
//! you that a lone `0x1b` byte, arriving on a terminal in raw mode, becomes a
//! `KeyCode::Esc` press event at all — crossterm's split-escape
//! disambiguation is the thing the classic watcher needed a 200 ms grace
//! window to do by hand, and the ADR's decision to drop that window rests
//! entirely on crossterm doing it. This test is the ground truth for that
//! belief.
//!
//! # Anchoring
//!
//! Every wait is on a full, tagged, mechanically-generated line — never on a
//! bare word. That is the lesson `settings_form_pty_test` records the hard
//! way: it waited on `"nano"`, the vi mode hint advertises `/nano /emacs`, and
//! the wait returned while the WRONG menu was up. Here the hazard is sharper
//! still — the INSERT mode hint reads *"vi INSERT — Esc: NORMAL …"*, so a wait
//! on `"NORMAL"` matches the screen that proves the opposite of what is being
//! asserted. Two rules follow:
//!
//! * the child prints `[esc-ladder] <phase> …` lines that carry the ASSERTION
//!   in the anchor (`cancel=false`, not just "phase done"), so a wait that
//!   returns is a wait that passed; and
//! * the mode indicator is checked as `vi --NORMAL--` / `vi --INSERT--`, the
//!   doubly-dashed `header_mode` spelling, which no hint line contains.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tests_pty::{screen_grid, Pty};

use crate::prompt_visibility_test::wait_for_child;

const CHILD_TEST: &str = "esc_ladder_pty_test::esc_ladder_child";
/// Prefix on every line the child prints. Distinctive enough that no editor
/// hint, header, or cargo-harness line can collide with a wait.
const TAG: &str = "[esc-ladder]";
const REACH_TIMEOUT: Duration = Duration::from_secs(60);
const EXIT_TIMEOUT: Duration = Duration::from_secs(60);
/// The child's own per-phase deadline, shorter than the parent's so a stuck
/// child reports which phase it died in instead of being killed anonymously.
const CHILD_PHASE_TIMEOUT: Duration = Duration::from_secs(30);

/// Which half of the terminal a marker is expected in.
#[derive(Clone, Copy)]
enum Where {
    /// The byte stream: a scrollback row, written once, then scrolled.
    Committed,
    /// The rendered grid: the mounted block, repainted in place every frame.
    OnScreen,
}

/// The child half: a real cockpit on the inherited pty, with a real turn
/// running, driven one loop-turn at a time.
///
/// It reports by PRINTING rather than by asserting. A panic message would go
/// to fd 2, which the cockpit's `PtyCapture` owns for the duration, so it
/// would be swallowed exactly when it is needed; a printed line goes through
/// the capture and is relayed into the parent's scrollback as a real rendered
/// row. Each line carries its own verdict, so the parent's wait IS the
/// assertion.
#[test]
#[ignore = "child process of the Esc-ladder PTY acceptance test"]
fn esc_ladder_child() {
    if std::env::var_os("NEWT_ESC_LADDER_PTY_CHILD").is_none() {
        return;
    }
    // The parent's per-run directory, not one of our own: it already isolates
    // the child from ~/.newt, and the parent removes it at the end — a second
    // directory here would be one the parent cannot name and so would never
    // clean up.
    let dir = std::path::PathBuf::from(
        std::env::var_os("NEWT_CONFIG_DIR").expect("the parent supplies the isolated state dir"),
    );
    let surface =
        crate::rich_input::RichSurface::new(Some(dir.join("history"))).expect("rich surface");
    let mut cockpit = crate::cockpit::Presenter::open(surface).expect("the cockpit takes the pty");

    // The real flag the session races against — the same `Arc<AtomicBool>`
    // `chat.rs` hands the surface on `TurnStarted`.
    let cancel = Arc::new(AtomicBool::new(false));
    cockpit
        .handle_request(crate::session_worker::SurfaceRequest::TurnStarted {
            cancel: Arc::clone(&cancel),
        })
        .expect("a turn starts");

    let insert = |c: &crate::cockpit::Presenter| c.claims().is_live("vi-insert");
    let pending = |c: &crate::cockpit::Presenter| c.claims().is_live("vi-pending");
    let mode = |c: &crate::cockpit::Presenter| if insert(c) { "INSERT" } else { "NORMAL" };

    // A fresh vi mount starts in INSERT (`Vi::new`), so this is the operator
    // opening a session and sending a line: rung 5 is live before anything is
    // typed.
    let line = format!(
        "ready mode={} cancel={}",
        mode(&cockpit),
        cancel.load(Ordering::SeqCst)
    );
    say(&mut cockpit, &line);

    // ---- rung 5: vi INSERT owns Esc. It is an editing transition. ----
    settle(&mut cockpit, "insert-esc", |c| !insert(c));
    let line = format!(
        "insert-esc mode={} cancel={}",
        mode(&cockpit),
        cancel.load(Ordering::SeqCst)
    );
    say(&mut cockpit, &line);

    // ---- rung 6: a pending operator outranks the interrupt. ----
    // This is the rung codex does not have: mid-turn `d` then Esc kills the
    // turn there, because only INSERT-mode escape is subtracted from its
    // predicate.
    settle(&mut cockpit, "pending-armed", pending);
    let line = format!(
        "pending-armed mode={} cancel={}",
        mode(&cockpit),
        cancel.load(Ordering::SeqCst)
    );
    say(&mut cockpit, &line);
    settle(&mut cockpit, "pending-esc", |c| !pending(c));
    let line = format!(
        "pending-esc mode={} cancel={}",
        mode(&cockpit),
        cancel.load(Ordering::SeqCst)
    );
    say(&mut cockpit, &line);

    // ---- rung 7: everything above declined, so Esc reaches the hatch. ----
    settle(&mut cockpit, "normal-esc", |_| {
        cancel.load(Ordering::SeqCst)
    });
    let line = format!(
        "normal-esc cancel={} presses={}",
        cancel.load(Ordering::SeqCst),
        newt_core::tty::interrupt_presses()
    );
    say(&mut cockpit, &line);

    // ---- the second press is heard, exactly as Ctrl-C's second press is. ----
    settle(&mut cockpit, "second-esc", |_| {
        newt_core::tty::interrupt_presses() >= 2
    });
    let line = format!("second-esc presses={}", newt_core::tty::interrupt_presses());
    say(&mut cockpit, &line);
}

/// Print one tagged line and turn the loop enough times for the capture to
/// relay it onto the real terminal.
fn say(cockpit: &mut crate::cockpit::Presenter, line: &str) {
    println!("{TAG} {line}");
    for _ in 0..8 {
        cockpit.pump().expect("pump the cockpit");
    }
}

/// Turn the cockpit's loop until `done`, or report which phase hung.
fn settle(
    cockpit: &mut crate::cockpit::Presenter,
    phase: &str,
    done: impl Fn(&crate::cockpit::Presenter) -> bool,
) {
    let deadline = Instant::now() + CHILD_PHASE_TIMEOUT;
    while !done(cockpit) {
        cockpit.pump().expect("pump the cockpit");
        if Instant::now() >= deadline {
            println!("{TAG} TIMEOUT {phase}");
            for _ in 0..8 {
                let _ = cockpit.pump();
            }
            panic!("the Esc-ladder child hung waiting for `{phase}`");
        }
    }
}

/// The whole ladder, on a terminal, in one child: rungs 5, 6 and 7 in the
/// order an operator meets them.
///
/// One cockpit per process is a hard constraint of the harness — crossterm
/// caches the first terminal it resolves, so a second `Presenter::open` in the
/// same binary times out on its cursor query — which is why this is a child
/// process at all, and why all four phases run in sequence against one
/// cockpit rather than as four tests.
#[test]
fn esc_interrupts_a_running_turn_from_vi_normal_but_never_from_vi_insert() {
    let home = std::env::temp_dir().join(format!(
        "newt-esc-ladder-home-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    std::fs::create_dir_all(&home).expect("create the isolated config dir");

    let pty = Pty::open();
    let mut child = std::process::Command::new(
        std::env::current_exe().expect("the test binary re-invokes itself"),
    )
    .args(["--exact", CHILD_TEST, "--ignored", "--nocapture"])
    .env("NEWT_ESC_LADDER_PTY_CHILD", "1")
    // Pinned, not inherited: the ladder's rungs 3-6 are vi's, and a host whose
    // config says `emacs` would leave every one of them dead and assertion 2
    // vacuous.
    .env("NEWT_EDIT_MODE", "vi")
    .env("NEWT_CONFIG_DIR", &home)
    .stdin(pty.slave_stdio())
    .stdout(pty.slave_stdio())
    // fd 2 to the same pty: anything that fails BEFORE the cockpit takes the
    // terminal (a surface that will not build, a missing dir) then shows up in
    // the transcript this test prints on failure.
    .stderr(pty.slave_stdio())
    .spawn()
    .expect("spawn the pty child");

    let mut transcript = String::new();
    let mut answered_dsr = false;
    // Wait until `marker` shows up, playing the terminal's own half (answering
    // the presenter's `ESC[6n` cursor query, which nothing else is on the
    // other end of) while we wait.
    //
    // `Where::Committed` searches the byte stream — right for a scrollback row,
    // which is written once and then scrolls. `Where::OnScreen` searches the
    // GRID, which replays cursor positioning — the only honest way to read the
    // mounted block, whose rows are repainted in place. Asserting the mode
    // indicator on the raw stream would match a stale `vi --INSERT--` from
    // before the Esc; asserting a scrollback row on the grid would miss a line
    // that has since scrolled off.
    let mut wait_for = |marker: &str, whence: Where| -> String {
        let deadline = Instant::now() + REACH_TIMEOUT;
        loop {
            transcript.push_str(&pty.screen());
            if !answered_dsr && transcript.contains("\u{1b}[6n") {
                pty.type_in("\u{1b}[3;1R");
                answered_dsr = true;
            }
            let grid = screen_grid(&transcript).join("\n");
            let hit = match whence {
                Where::Committed => transcript.contains(marker),
                Where::OnScreen => grid.contains(marker),
            };
            if hit {
                return grid;
            }
            assert!(
                Instant::now() < deadline,
                "`{marker}` never {}.\nThe line the child DID print is the \
                 failure — read it, do not rerun.\ngrid:\n{grid}",
                match whence {
                    Where::Committed => "reached the terminal",
                    Where::OnScreen => "RENDERED",
                }
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    };

    // 0. The turn is running and the editor is in INSERT — and the mode
    //    indicator SAYS so on the real screen. Without this the next
    //    assertion proves nothing: a mount that was never in INSERT would
    //    satisfy "Esc did not interrupt" for the wrong reason.
    wait_for(
        &format!("{TAG} ready mode=INSERT cancel=false"),
        Where::Committed,
    );
    wait_for("vi --INSERT--", Where::OnScreen);

    // 1. Esc from vi INSERT is a mode transition, NOT an interrupt.
    //
    //    This is the assertion that makes the rest mean something. Against a
    //    stub that always escapes, `cancel=false` never appears and this fails
    //    — which is why a one-sided "Esc interrupts" test would have been the
    //    wrong contract: it passes on an implementation that silently deletes
    //    rungs 2-6.
    pty.type_in("\u{1b}");
    wait_for(
        &format!("{TAG} insert-esc mode=NORMAL cancel=false"),
        Where::Committed,
    );
    // #2006 made the mode indicator load-bearing for #2005: the operator has
    // to be able to SEE which rung their next Esc will land on.
    wait_for("vi --NORMAL--", Where::OnScreen);

    // 2. A half-typed operator outranks the interrupt (rung 6 over rung 7).
    pty.type_in("d");
    wait_for(
        &format!("{TAG} pending-armed mode=NORMAL cancel=false"),
        Where::Committed,
    );
    pty.type_in("\u{1b}");
    wait_for(
        &format!("{TAG} pending-esc mode=NORMAL cancel=false"),
        Where::Committed,
    );

    // 3. vi NORMAL, nothing pending, turn running: Esc reaches the hatch.
    //    `presses=1` is the operator-visible half — the count the spinner
    //    reads to swap its stage label, so the press is acknowledged on
    //    screen rather than only in a bool.
    pty.type_in("\u{1b}");
    wait_for(
        &format!("{TAG} normal-esc cancel=true presses=1"),
        Where::Committed,
    );

    // 4. The second press is HEARD (#2010): the count the spinner renders
    //    is 2, so the operator sees a different label, not the same one.
    //    Same as Ctrl-C and as the classic watcher. `presses=1` above is
    //    what makes this non-trivial.
    pty.type_in("\u{1b}");
    wait_for(&format!("{TAG} second-esc presses=2"), Where::Committed);

    let status =
        wait_for_child(&mut child, EXIT_TIMEOUT).expect("the child exited within the timeout");
    assert!(
        status.success(),
        "the Esc-ladder child failed: {status:?}\ngrid:\n{}",
        screen_grid(&transcript).join("\n")
    );

    let _ = std::fs::remove_dir_all(&home);
}
