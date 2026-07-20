//! §6.5 — **the regression proof**: when the harness blocks on a human, the
//! question must be the most recent thing on the terminal.
//!
//! # The bug this pins
//!
//! A turn parks a spinner on the cursor line, then — 736 lines later, deep
//! inside the same turn — a capability denial reaches
//! `PromptPermissionGate::ask`, which calls `prompt_permission_choice`, which
//! `print!`s the question with no `\r`, no `ESC[K` and no leading newline, and
//! blocks in `read_line`. The question lands *appended to spinner chrome*, and
//! then the spinner's own ticker redraws over it ~10x/second for as long as the
//! operator is looking at the screen. The process is correctly blocked on a
//! question the operator cannot see. There is no timeout. It waits forever.
//!
//! `gate.ask` has six call sites, so a per-site fix is whack-a-mole. This test
//! drives the ONE seam they all funnel through
//! (`newt-tui/src/permissions.rs`, `(self.ask_human)(...)`) with the
//! *production* `prompt_permission_choice` wired in exactly as `chat.rs` wires
//! it, and with a *production* `newt_core::tty::Spinner` live — the same
//! spinner `stream_response` and the probe/compression waits construct.
//!
//! # Why a PTY, and why a child process
//!
//! The property under test is "what a human sees on a terminal". It is
//! unobservable without one: the spinner refuses to paint at all unless it can
//! own a line — correct behavior, and precisely why this bug never showed up in
//! a piped test.
//!
//! The scenario runs in a **child process** (this same test binary, re-invoked
//! with `--nocapture`) whose stdin and stdout are the pty. That is not
//! ceremony: `cargo test` installs a thread-local capture that swallows
//! `print!`, and `prompt_permission_choice` prints the question with `print!`.
//! Run in-process, the question would vanish into the harness buffer and the
//! test would "fail" for a reason that has nothing to do with the bug. In a
//! child with `--nocapture`, every byte the production code writes lands on the
//! pty, which is the only way this proves anything.
//!
//! No filesystem, no network, no real service — a pty pair and a re-exec of the
//! test binary itself.
//!
//! # The assertion
//!
//! Everything is judged on the window between the question's first byte and the
//! operator's keystroke — i.e. exactly while `read_line` is blocked:
//!
//! 1. the full multi-line question survives **contiguously** (not truncated to
//!    its last row, which is all a single-line `ESC[K` can preserve);
//! 2. **no braille glyph appears after the question starts** — nothing redrew
//!    over it;
//! 3. the `> ` menu is the **last thing written** before the human typed.

use std::os::unix::io::{FromRawFd as _, RawFd};
use std::time::Duration;

use crate::danger;
use crate::permissions::{
    permission_prompt_text, prompt_permission_choice, PermissionPromptState, PromptPermissionGate,
};
use newt_core::caveats::{Caveats, CountBound, Scope};
use newt_core::tty::{LineCaps, Sink, Spinner};
use newt_core::{DenialKind, PermissionGate as _, PermissionRequest};

/// The child test's fully-qualified name, used to re-invoke this binary.
const CHILD_TEST: &str = "prompt_visibility_test::prompt_scenario_child";

/// How long the "operator" takes to answer. Long enough that a 100 ms ticker
/// gets several chances to redraw over the question — which is what it did.
const HUMAN_THINKING_TIME: Duration = Duration::from_millis(600);

/// The session's enforced authority: no net at all, so `example.com` denies and
/// the gate prompts — the `tools.rs` `web_fetch` arm's precondition.
fn no_net_caveats() -> Caveats {
    Caveats {
        fs_read: Scope::only(["/ws".to_string()]),
        fs_write: Scope::only(["/ws".to_string()]),
        exec: Scope::only(["cargo".to_string()]),
        net: Scope::none(),
        max_calls: CountBound::Unlimited,
        valid_for_generation: Scope::All,
    }
}

/// The request `web_fetch` builds when `!caveats.permits_net(&host)` — copied
/// from the tool arm so this test tracks the real shape.
fn web_fetch_request(host: &str) -> PermissionRequest {
    PermissionRequest {
        tool: "web_fetch".to_string(),
        kind: DenialKind::Net,
        target: host.to_string(),
        reason: format!("net does not permit '{host}'"),
    }
}

// ---------------------------------------------------------------------------
// The child: the scenario itself, run with fd 0/1 on a pty.
// ---------------------------------------------------------------------------

/// Not a test in its own right — the body of the scenario, invoked as a child
/// process by [`a_permission_prompt_is_visible_and_survives_a_live_spinner`].
/// `#[ignore]` keeps it out of a normal run; it does nothing unless the parent's
/// env marker is set.
#[test]
#[ignore = "child process of the prompt-visibility regression test"]
fn prompt_scenario_child() {
    if std::env::var_os("NEWT_PROMPT_VISIBILITY_CHILD").is_none() {
        return;
    }

    // A live production spinner — the one `stream_response` and the probe /
    // compression waits construct during a turn. It owns the bottom line and
    // its shared ticker redraws it every 100 ms.
    let spinner = Spinner::start_with_caps(LineCaps::Own, "thinking…", Sink::Stdout, true)
        .expect("the pty is a real terminal, so the spinner takes the line");
    // Let it paint a few frames, exactly as it would mid-turn.
    std::thread::sleep(Duration::from_millis(250));

    let request = web_fetch_request("example.com");
    let mut state = PermissionPromptState::default();
    {
        // Wired exactly as `chat.rs` wires the production gate.
        let mut gate = PromptPermissionGate {
            state: &mut state,
            base: no_net_caveats(),
            key_path: None,
            conversation_id: "conv-prompt-visibility".to_string(),
            log_path: None,
            denials_path: None,
            config_path: None,
            preset_clamp: None,
            danger: danger::DangerTable::builtin(),
            color: true,
            verbose: false,
            ask_human: prompt_permission_choice,
        };
        let _decision = gate.ask(std::slice::from_ref(&request));
    }
    // Stop the animation before exiting, so the capture is not extended by
    // frames drawn after the operator already answered.
    drop(spinner);
}

// ---------------------------------------------------------------------------
// The parent: allocate the pty, drive the child, judge the capture.
// ---------------------------------------------------------------------------

/// A pty pair. The slave becomes the child's stdin+stdout; the master is what
/// the "operator" types on and what we read the screen from.
struct Pty {
    master: RawFd,
    slave: RawFd,
}

impl Pty {
    fn open() -> Self {
        unsafe {
            let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
            assert!(master >= 0, "posix_openpt failed");
            assert_eq!(libc::grantpt(master), 0, "grantpt failed");
            assert_eq!(libc::unlockpt(master), 0, "unlockpt failed");
            let name = libc::ptsname(master);
            assert!(!name.is_null(), "ptsname failed");
            let slave = libc::open(name, libc::O_RDWR | libc::O_NOCTTY);
            assert!(slave >= 0, "opening the pty slave failed");

            // A realistic geometry. A 0x0 pty would drive `term_cols()` to its
            // 8-column floor and truncate the spinner into something the
            // assertions could not recognize.
            let ws = libc::winsize {
                ws_row: 50,
                ws_col: 200,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            libc::ioctl(slave, libc::TIOCSWINSZ, &ws);

            Self { master, slave }
        }
    }

    /// Send bytes as if the operator typed them.
    fn type_in(&self, s: &str) {
        let n = unsafe { libc::write(self.master, s.as_ptr().cast::<libc::c_void>(), s.len()) };
        assert!(n > 0, "writing the operator's keystrokes to the pty failed");
    }

    /// Everything the terminal has been shown so far.
    fn screen(&self) -> String {
        unsafe {
            let flags = libc::fcntl(self.master, libc::F_GETFL);
            libc::fcntl(self.master, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = unsafe {
                libc::read(
                    self.master,
                    buf.as_mut_ptr().cast::<libc::c_void>(),
                    buf.len(),
                )
            };
            if n <= 0 {
                break;
            }
            out.extend_from_slice(&buf[..n as usize]);
        }
        String::from_utf8_lossy(&out).into_owned()
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.slave);
            libc::close(self.master);
        }
    }
}

#[serial_test::serial(prompt_stdin)]
#[test]
fn a_permission_prompt_is_visible_and_survives_a_live_spinner() {
    let pty = Pty::open();

    // Hand the child its own duplicates of the slave: `Stdio::from(File)`
    // consumes the fd, and we need three of them plus our own copy.
    let dup = |fd: RawFd| unsafe { std::fs::File::from_raw_fd(libc::dup(fd)) };
    let mut child = std::process::Command::new(
        std::env::current_exe().expect("the test binary re-invokes itself"),
    )
    .args(["--exact", CHILD_TEST, "--ignored", "--nocapture"])
    .env("NEWT_PROMPT_VISIBILITY_CHILD", "1")
    .stdin(std::process::Stdio::from(dup(pty.slave)))
    .stdout(std::process::Stdio::from(dup(pty.slave)))
    .stderr(std::process::Stdio::null())
    .spawn()
    .expect("spawn the pty child");

    // The operator reads the question, thinks, then denies.
    std::thread::sleep(HUMAN_THINKING_TIME);
    pty.type_in("d\n");

    let status = child.wait().expect("wait for the pty child");

    let screen = pty.screen();
    assert!(
        status.success(),
        "the scenario child failed.\n\nscreen:\n{screen:?}"
    );

    // --- the window under test: question's first byte .. operator's keystroke
    let prompt_start = screen.find('⊘').unwrap_or_else(|| {
        panic!("the question was never written to the terminal at all.\n\nscreen:\n{screen:?}")
    });
    let echo_rel = screen[prompt_start..]
        .find("d\r\n")
        .or_else(|| screen[prompt_start..].find("d\n"))
        .unwrap_or_else(|| {
            panic!(
                "never saw the operator's keystroke echoed — the prompt did not \
                 reach a blocking read.\n\nscreen:\n{screen:?}"
            )
        });
    let window = &screen[prompt_start..prompt_start + echo_rel];

    let expected_prompt = permission_prompt_text(
        &web_fetch_request("example.com"),
        &danger::DangerTable::builtin(),
    );

    // (1) The FULL multi-line question survives, contiguously. A single-line
    // `ESC[K` can only preserve the final menu row; the header rows would be
    // stranded in scrollback, leaving a truncated question with no menu.
    // The pty's ONLCR translates every `\n` the process wrote into `\r\n` on
    // the wire, so compare against the same normalization.
    let window_lf = window.replace("\r\n", "\n");
    assert!(
        window_lf.contains(expected_prompt.trim_end()),
        "the question did not survive intact.\n\n  expected:\n{expected_prompt:?}\n\n  \
         on screen between the question and the keystroke:\n{window:?}"
    );

    // (2) NOTHING redrew after the question started. This is the assertion that
    // fails before the fix: the spinner's ticker paints a braille frame over
    // the menu roughly ten times a second for as long as the operator reads.
    let intruders: Vec<char> = window
        .chars()
        .filter(|c| ('\u{2800}'..='\u{28FF}').contains(c))
        .collect();
    assert!(
        intruders.is_empty(),
        "an ephemeral writer painted {} spinner frame(s) {:?} OVER the question \
         while the operator was reading it — this is the reported hang: newt is \
         correctly blocked on a question that has been scribbled out.\n\n  \
         on screen:\n{window:?}",
        intruders.len(),
        intruders,
    );

    // (3) The menu is the last thing written before the human typed.
    let visible_tail = window.trim_end_matches(|c: char| c.is_whitespace());
    assert!(
        visible_tail.ends_with('>'),
        "the choice menu was not the last thing on screen when the read \
         blocked.\n\n  on screen:\n{window:?}"
    );
}
