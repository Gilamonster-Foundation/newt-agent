//! Real-PTY regression coverage for nested prompt controls.
//!
//! The unit-level parsers cannot observe either property that matters here:
//! Escape must return without an Enter in raw per-keystroke mode, and Ctrl-D
//! must leave the *actual terminal* usable even though `abort` skips `Drop`.
//! Each scenario therefore runs in a child of this test binary with fd 0/1/2
//! attached to a fresh pseudo-terminal.

use std::io::Write as _;
use std::os::unix::io::{FromRawFd as _, RawFd};
use std::os::unix::process::ExitStatusExt as _;
use std::process::{Child, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use crate::permissions::{permission_prompt_text, prompt_permission_choice, prompt_user_input};
use crate::setup_tui::{run_setup_inline, SetupHandle};
use newt_core::tty::Terminal;
use newt_core::{DenialKind, PermissionRequest};

const CHILD_ENV: &str = "NEWT_PROMPT_CONTROLS_PTY_CHILD";
const PERMISSION_CHILD: &str = "prompt_controls_pty_test::permission_prompt_child";
const USER_INPUT_CHILD: &str = "prompt_controls_pty_test::user_input_prompt_child";
const ACTIVE_TURN_CHILD: &str = "prompt_controls_pty_test::active_turn_child";
const INLINE_SETUP_CHILD: &str = "prompt_controls_pty_test::inline_setup_child";
const NON_TTY_CHILD: &str = "prompt_controls_pty_test::non_tty_eof_child";
const READY: &str = "PROMPT-CONTROL-READY > ";

fn disable_core_dumps() {
    // SIGABRT is the behavior under test; a core image is not.
    unsafe {
        let no_core = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        let _ = libc::setrlimit(libc::RLIMIT_CORE, &no_core);
    }
}

#[test]
#[ignore = "child process of the prompt-controls PTY regression test"]
fn permission_prompt_child() {
    let Some(scenario) = std::env::var_os(CHILD_ENV) else {
        return;
    };
    let scenario = scenario.to_string_lossy();
    if scenario != "permission-escape" && scenario != "permission-ctrl-d" {
        return;
    }
    if scenario == "permission-ctrl-d" {
        disable_core_dumps();
    }

    let window = Terminal::suspend_for_prompt();
    let prompt = permission_prompt_text(
        &PermissionRequest {
            tool: "PROMPT-CONTROL-READY".to_string(),
            kind: DenialKind::Exec,
            target: "cargo".to_string(),
            reason: "PTY control test".to_string(),
        },
        &crate::danger::DangerTable::builtin(),
    );
    let result = prompt_permission_choice(&window, &prompt);
    drop(window);
    println!("PERMISSION-RESULT:{result:?}");
}

#[test]
#[ignore = "child process of the prompt-controls PTY regression test"]
fn user_input_prompt_child() {
    if std::env::var_os(CHILD_ENV).as_deref() != Some("user-input-escape".as_ref()) {
        return;
    }

    let window = Terminal::suspend_for_prompt();
    let result = prompt_user_input(&window, "PROMPT-CONTROL-READY");
    drop(window);
    println!("USER-INPUT-RESULT:{result:?}");
}

#[test]
#[ignore = "child process of the active-turn prompt-control PTY regression test"]
fn active_turn_child() {
    let Some(scenario) = std::env::var_os(CHILD_ENV) else {
        return;
    };
    let scenario = scenario.to_string_lossy();
    if scenario != "active-turn-escape" && scenario != "active-turn-ctrl-d" {
        return;
    }
    if scenario == "active-turn-ctrl-d" {
        disable_core_dumps();
    }

    let cancel = std::sync::atomic::AtomicBool::new(false);
    let hard = std::sync::atomic::AtomicBool::new(false);
    crate::with_interrupt_watch(true, &cancel, &hard, || {
        print!("{READY}");
        std::io::stdout().flush().expect("flush readiness marker");
        while !cancel.load(std::sync::atomic::Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(10));
        }
    });
    println!(
        "ACTIVE-TURN-RESULT:cancel={}:hard={}",
        cancel.load(std::sync::atomic::Ordering::Relaxed),
        hard.load(std::sync::atomic::Ordering::Relaxed)
    );
}

#[test]
#[ignore = "child process of the inline-setup controls PTY regression test"]
fn inline_setup_child() {
    let Some(scenario) = std::env::var_os(CHILD_ENV) else {
        return;
    };
    let scenario = scenario.to_string_lossy();
    if scenario != "inline-setup-escape" && scenario != "inline-setup-ctrl-d" {
        return;
    }
    if scenario == "inline-setup-ctrl-d" {
        disable_core_dumps();
    }

    // Keep the sender alive and silent so provisioning remains pending until
    // the synthetic operator key decides the scenario.
    let (_tx, rx) = std::sync::mpsc::channel();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let setup = SetupHandle {
        what: "PROMPT-CONTROL-READY".to_string(),
        rx,
        cancel: cancel.clone(),
    };
    run_setup_inline(&setup);
    println!(
        "INLINE-SETUP-RESULT:{}",
        cancel.load(std::sync::atomic::Ordering::SeqCst)
    );
}

#[test]
#[ignore = "child process of the prompt-controls non-TTY EOF regression test"]
fn non_tty_eof_child() {
    if std::env::var_os(CHILD_ENV).as_deref() != Some("non-tty-eof".as_ref()) {
        return;
    }

    let permission = {
        let window = Terminal::suspend_for_prompt();
        prompt_permission_choice(&window, READY)
    };
    let user_input = {
        let window = Terminal::suspend_for_prompt();
        prompt_user_input(&window, "closed stdin")
    };
    println!("NON-TTY-RESULT:{permission:?}:{user_input:?}");
}

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

            let ws = libc::winsize {
                ws_row: 40,
                ws_col: 160,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            libc::ioctl(slave, libc::TIOCSWINSZ, &ws);

            // Start from an explicitly usable line discipline, so the
            // post-abort assertion has a meaningful baseline on every host.
            let mut term: libc::termios = std::mem::zeroed();
            assert_eq!(libc::tcgetattr(slave, &mut term), 0, "tcgetattr");
            term.c_lflag |= libc::ICANON | libc::ECHO | libc::ISIG;
            term.c_cc[libc::VMIN] = 1;
            term.c_cc[libc::VTIME] = 0;
            assert_eq!(libc::tcsetattr(slave, libc::TCSANOW, &term), 0, "tcsetattr");

            let flags = libc::fcntl(master, libc::F_GETFL);
            assert!(flags >= 0, "fcntl(F_GETFL)");
            assert_eq!(
                libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK),
                0,
                "fcntl(F_SETFL)"
            );
            Self { master, slave }
        }
    }

    fn dup_slave(&self) -> std::fs::File {
        unsafe { std::fs::File::from_raw_fd(libc::dup(self.slave)) }
    }

    fn type_bytes(&self, bytes: &[u8]) {
        let written = unsafe {
            libc::write(
                self.master,
                bytes.as_ptr().cast::<libc::c_void>(),
                bytes.len(),
            )
        };
        assert_eq!(written, bytes.len() as isize, "write operator keystroke");
    }

    fn read_available(&self, capture: &mut Vec<u8>) {
        let mut buf = [0u8; 4096];
        loop {
            let n = unsafe {
                libc::read(
                    self.master,
                    buf.as_mut_ptr().cast::<libc::c_void>(),
                    buf.len(),
                )
            };
            if n > 0 {
                capture.extend_from_slice(&buf[..n as usize]);
            } else {
                break;
            }
        }
    }

    fn wait_for_screen(&self, needle: &str, capture: &mut Vec<u8>) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            self.read_available(capture);
            if String::from_utf8_lossy(capture).contains(needle) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!(
            "child never displayed {needle:?}; screen={:?}",
            String::from_utf8_lossy(capture)
        );
    }

    fn wait_for_raw_mode(&self) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            let term = self.termios();
            if term.c_lflag & libc::ICANON == 0 {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("child displayed its prompt but never entered per-keystroke mode");
    }

    fn termios(&self) -> libc::termios {
        unsafe {
            let mut term: libc::termios = std::mem::zeroed();
            assert_eq!(libc::tcgetattr(self.slave, &mut term), 0, "tcgetattr");
            term
        }
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

fn spawn_pty_child(pty: &Pty, scenario: &str, child_test: &str) -> Child {
    std::process::Command::new(std::env::current_exe().expect("re-invoke test binary"))
        .args(["--exact", child_test, "--ignored", "--nocapture"])
        .env(CHILD_ENV, scenario)
        .env("TERM", "xterm-256color")
        .stdin(Stdio::from(pty.dup_slave()))
        .stdout(Stdio::from(pty.dup_slave()))
        .stderr(Stdio::from(pty.dup_slave()))
        .spawn()
        .expect("spawn PTY child")
}

fn wait_for_child(child: &mut Child, timeout: Duration) -> ExitStatus {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("poll child") {
            return status;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("prompt child did not react before the timeout");
}

fn run_key_scenario(
    scenario: &str,
    child_test: &str,
    key: &[u8],
) -> (ExitStatus, String, libc::termios) {
    let pty = Pty::open();
    let mut child = spawn_pty_child(&pty, scenario, child_test);
    let mut capture = Vec::new();
    pty.wait_for_screen("PROMPT-CONTROL-READY", &mut capture);
    // Inspect the real slave as well as the screen: this grounds the claim that
    // Escape/Ctrl-D are being handled per keystroke rather than line-buffered.
    pty.wait_for_raw_mode();
    pty.type_bytes(key);
    let status = wait_for_child(&mut child, Duration::from_secs(2));
    pty.read_available(&mut capture);
    (
        status,
        String::from_utf8_lossy(&capture).into_owned(),
        pty.termios(),
    )
}

#[serial_test::serial(prompt_control_pty)]
#[test]
fn bare_escape_immediately_backs_out_of_permission_prompt() {
    let (status, screen, _) = run_key_scenario("permission-escape", PERMISSION_CHILD, b"\x1b");
    assert!(status.success(), "child failed; screen={screen:?}");
    assert!(
        screen.contains("PERMISSION-RESULT:Deny"),
        "Escape must take the safe Back/cancel path; screen={screen:?}"
    );
    assert!(
        screen.contains("Esc=Back") && screen.contains("Ctrl+D=Emergency brake"),
        "permission prompt did not advertise Back and the emergency brake; screen={screen:?}"
    );
}

#[serial_test::serial(prompt_control_pty)]
#[test]
fn bare_escape_immediately_backs_out_of_request_user_input() {
    let (status, screen, _) = run_key_scenario("user-input-escape", USER_INPUT_CHILD, b"\x1b");
    assert!(status.success(), "child failed; screen={screen:?}");
    assert!(
        screen.contains("USER-INPUT-RESULT:None"),
        "Escape must cancel instead of synthesizing a model answer; screen={screen:?}"
    );
    assert!(
        screen.contains("Esc=Back") && screen.contains("Ctrl+D=Emergency brake"),
        "user-input prompt did not advertise Back and the emergency brake; screen={screen:?}"
    );
}

#[serial_test::serial(prompt_control_pty)]
#[test]
fn ctrl_d_aborts_and_repairs_the_real_terminal_before_dying() {
    let (status, screen, term) = run_key_scenario("permission-ctrl-d", PERMISSION_CHILD, b"\x04");
    assert_eq!(
        status.signal(),
        Some(libc::SIGABRT),
        "Ctrl-D must terminate with SIGABRT; status={status:?}, screen={screen:?}"
    );
    for (name, flag) in [
        ("ICANON", libc::ICANON),
        ("ECHO", libc::ECHO),
        ("ISIG", libc::ISIG),
    ] {
        assert_ne!(
            term.c_lflag & flag,
            0,
            "{name} was left disabled after emergency abort; screen={screen:?}"
        );
    }
    assert_eq!(term.c_cc[libc::VMIN], 1, "VMIN was not restored");
    assert_eq!(term.c_cc[libc::VTIME], 0, "VTIME was not restored");
    assert!(
        screen.contains("Emergency brake — STOP RIGHT NOW"),
        "operator-facing emergency label missing; screen={screen:?}"
    );
    for reset in [
        "\u{1b}[?2004l",
        "\u{1b}[?1000l",
        "\u{1b}[?25h",
        "\u{1b}[?1049l",
    ] {
        assert!(
            screen.contains(reset),
            "terminal reset {reset:?} missing; screen={screen:?}"
        );
    }
}

#[serial_test::serial(prompt_control_pty)]
#[test]
fn active_turn_ctrl_d_repairs_custom_cbreak_before_abort() {
    let (status, screen, term) = run_key_scenario("active-turn-ctrl-d", ACTIVE_TURN_CHILD, b"\x04");
    assert_eq!(
        status.signal(),
        Some(libc::SIGABRT),
        "active-turn Ctrl-D must terminate with SIGABRT; status={status:?}, screen={screen:?}"
    );
    assert_eq!(
        term.c_lflag & (libc::ICANON | libc::ECHO | libc::ISIG),
        libc::ICANON | libc::ECHO | libc::ISIG,
        "active-turn emergency did not restore every interactive flag; screen={screen:?}"
    );
}

#[serial_test::serial(prompt_control_pty)]
#[test]
fn active_turn_escape_is_back_without_aborting_the_process() {
    let (status, screen, term) = run_key_scenario("active-turn-escape", ACTIVE_TURN_CHILD, b"\x1b");
    assert!(
        status.success(),
        "active-turn Escape must return normally; status={status:?}, screen={screen:?}"
    );
    assert!(
        screen.contains("ACTIVE-TURN-RESULT:cancel=true:hard=false"),
        "Escape must take the graceful Back/cancel path; screen={screen:?}"
    );
    assert!(
        !screen.contains("Emergency brake — STOP RIGHT NOW"),
        "Escape must never invoke the emergency brake; screen={screen:?}"
    );
    assert_eq!(
        term.c_lflag & (libc::ICANON | libc::ECHO | libc::ISIG),
        libc::ICANON | libc::ECHO | libc::ISIG,
        "active-turn Escape did not restore cooked terminal mode; screen={screen:?}"
    );
}

#[serial_test::serial(prompt_control_pty)]
#[test]
fn inline_setup_escape_skips_and_restores_cooked_mode() {
    let (status, screen, term) =
        run_key_scenario("inline-setup-escape", INLINE_SETUP_CHILD, b"\x1b");
    assert!(
        status.success(),
        "inline setup child failed; screen={screen:?}"
    );
    assert!(
        screen.contains("INLINE-SETUP-RESULT:true"),
        "one Escape must cancel provisioning and return; screen={screen:?}"
    );
    assert_eq!(
        term.c_lflag & (libc::ICANON | libc::ECHO | libc::ISIG),
        libc::ICANON | libc::ECHO | libc::ISIG,
        "inline setup left the terminal raw after Escape; screen={screen:?}"
    );
}

#[serial_test::serial(prompt_control_pty)]
#[test]
fn inline_setup_ctrl_d_is_the_emergency_brake_and_repairs_terminal() {
    let (status, screen, term) =
        run_key_scenario("inline-setup-ctrl-d", INLINE_SETUP_CHILD, b"\x04");
    assert_eq!(
        status.signal(),
        Some(libc::SIGABRT),
        "inline setup Ctrl-D must abort; status={status:?}, screen={screen:?}"
    );
    assert_eq!(
        term.c_lflag & (libc::ICANON | libc::ECHO | libc::ISIG),
        libc::ICANON | libc::ECHO | libc::ISIG,
        "inline setup emergency left the terminal raw; screen={screen:?}"
    );
}

#[serial_test::serial(prompt_control_pty)]
#[test]
fn closed_non_tty_stdin_remains_clean_eof_not_emergency_brake() {
    let output =
        std::process::Command::new(std::env::current_exe().expect("re-invoke test binary"))
            .args(["--exact", NON_TTY_CHILD, "--ignored", "--nocapture"])
            .env(CHILD_ENV, "non-tty-eof")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("run non-TTY child");
    assert!(
        output.status.success(),
        "closed stdin must not abort: status={:?}, stderr={:?}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("NON-TTY-RESULT:Deny:Some(\"\")"),
        "permission EOF must deny and request_user_input EOF must remain an empty answer: {stdout:?}"
    );
}
