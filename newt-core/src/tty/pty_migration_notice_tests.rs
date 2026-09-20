//! Real-PTY grounding for migration values, host stderr fallback and modal
//! refusal. Reuses the existing bounded notice fixture and terminal owner.

use super::*;

const CHILD_NAME: &str = "tty::pty_notice_test::migration::migration_notice_child";
const OLD: &str = "[tenacity]\ndefault = \"standard\"\n";
const MARKER: &str = "newt: migrated config";

#[test]
#[ignore = "re-executed by the bounded migration notice PTY parents"]
fn migration_notice_child() {
    let Ok(scenario) = std::env::var("NEWT_NOTICE_PTY_CHILD") else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let own = dir.path().join("user");
    std::fs::create_dir(&own).unwrap();
    let path = own.join("config.toml");
    std::fs::write(&path, OLD).unwrap();
    // This child owns its process and sets discovery before starting a ticker.
    unsafe {
        std::env::set_var("NEWT_CONFIG_DIR", &own);
        std::env::set_var("NEWT_CONFIG", &path);
    }
    std::env::set_current_dir(dir.path()).unwrap();
    match scenario.as_str() {
        "migration-spinner" => {
            let spinner =
                Spinner::start_with_caps(LineCaps::Own, "migration spinner", Sink::Stdout, true)
                    .unwrap();
            std::thread::sleep(DWELL);
            let mut notices = Vec::new();
            let result = crate::Config::load(&path, &mut |notice| notices.push(notice));
            for notice in notices {
                notice
                    .diagnostic(LineCaps::Own, false, std::io::stderr())
                    .unwrap();
            }
            result.unwrap();
            std::thread::sleep(DWELL);
            drop(spinner);
        }
        "migration-modal" | "migration-modal-redirected" => {
            let workspace = dir.path().join("workspace");
            std::fs::create_dir_all(workspace.join(".newt")).unwrap();
            std::fs::write(workspace.join(".newt/config.toml"), OLD).unwrap();
            std::env::set_current_dir(&workspace).unwrap();
            let stderr_path = dir.path().join("modal-stderr.log");
            let saved_stderr = if scenario == "migration-modal-redirected" {
                use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
                let redirected = std::fs::File::create(&stderr_path).unwrap();
                // SAFETY: this isolated child owns its descriptors. The saved
                // duplicate is restored and then closed below.
                let saved = unsafe { libc::dup(libc::STDERR_FILENO) };
                assert!(saved >= 0);
                assert!(unsafe { libc::dup2(redirected.as_raw_fd(), libc::STDERR_FILENO) } >= 0);
                Some(unsafe { OwnedFd::from_raw_fd(saved) })
            } else {
                None
            };
            let window = Terminal::suspend_for_prompt(crate::tty::TerminalTaker::PlainCliConfirm);
            let answer =
                crate::tty::read_prompt_window_line(&window, QUESTION, crate::tty::Echo::Chars)
                    .unwrap();
            assert!(matches!(answer, crate::tty::PromptLine::Line(text) if text == "yes"));
            drop(window);
            if let Some(saved) = saved_stderr {
                use std::os::fd::AsRawFd;
                assert!(unsafe { libc::dup2(saved.as_raw_fd(), libc::STDERR_FILENO) } >= 0);
                drop(saved);
                let reports = std::fs::read_to_string(stderr_path).unwrap();
                assert_eq!(
                    reports.matches("newt: migrated config").count(),
                    1,
                    "{reports}"
                );
                assert_eq!(reports.matches("old psyche labels").count(), 1, "{reports}");
                assert!(
                    !reports.contains('\u{1b}'),
                    "redirected reports must be plain: {reports:?}"
                );
            }
        }
        "migration-prompt" => {
            let window = Terminal::suspend_for_prompt(crate::tty::TerminalTaker::PlainCliConfirm);
            window.ask(QUESTION).unwrap();
            let mut notices = Vec::new();
            let result = crate::Config::load(&path, &mut |notice| notices.push(notice));
            for notice in notices {
                // The current owner delivers without re-entering Terminal.
                window.notice(&notice.line()).unwrap();
            }
            result.unwrap();
            drop(window);
        }
        "migration-protocol" | "migration-refused" => {
            let _held = if scenario == "migration-protocol" {
                crate::tty::enter_protocol_mode();
                None
            } else {
                Some(
                    Terminal::lease_region(
                        crate::tty::Region::WholeScreen,
                        crate::tty::OnCollision::Refuse,
                    )
                    .unwrap(),
                )
            };
            let window = Terminal::suspend_for_prompt(crate::tty::TerminalTaker::PlainCliConfirm);
            let result =
                crate::tty::read_prompt_window_line(&window, QUESTION, crate::tty::Echo::Chars);
            assert!(result.is_err(), "a refused window must not wait");
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                OLD,
                "a refused prompt must not consume a migration"
            );
            drop(window);
            drop(_held);
            let mut notices = Vec::new();
            let result = crate::Config::load(&path, &mut |notice| notices.push(notice));
            for notice in notices {
                notice
                    .diagnostic(LineCaps::Own, false, std::io::stderr())
                    .unwrap();
            }
            result.unwrap();
        }
        _ => panic!("unexpected migration scenario"),
    }
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        crate::psyche_import::migrate_config_text(OLD).unwrap().text
    );
    println!("MIGRATION_TERMINAL_RETURNED");
}

/// Grounds diagnostic-value delivery while the arbiter has a live spinner.
#[test]
#[serial_test::serial(tty_arbiter)]
fn migration_notice_clears_the_live_spinner_and_returns_the_terminal() {
    let (screen, exited) =
        run_scenario_bounded("migration-spinner", CHILD_NAME, Duration::from_secs(20));
    assert!(exited, "{screen:?}");
    let start = screen
        .find(MARKER)
        .expect("migration reached terminal stderr");
    let before = &screen[..start];
    let row_start = before.rfind('\r').expect("spinner owns an ephemeral row");
    assert!(
        before[row_start..].contains("\x1b[K"),
        "notice must erase the spinner before committing its row: {before:?}"
    );
    assert!(screen.contains("MIGRATION_TERMINAL_RETURNED"));
    assert!(
        !frames(&screen[start..]).is_empty(),
        "spinner did not resume"
    );
}

/// Grounds the caller-owned PromptWindow path with a question already visible.
#[test]
#[serial_test::serial(tty_arbiter)]
fn migration_notice_preserves_an_owned_permission_question() {
    let (screen, exited) =
        run_scenario_bounded("migration-prompt", CHILD_NAME, Duration::from_secs(20));
    assert!(exited, "{screen:?}");
    let start = screen.find(QUESTION).unwrap() + QUESTION.len();
    let notice = screen[start..].find(MARKER).unwrap() + start;
    assert!(!screen[start..notice].contains("\x1b[K"), "{screen:?}");
    assert!(screen.contains("MIGRATION_TERMINAL_RETURNED"));
}

/// Protocol/held-row refusal must occur before config migration. A later real
/// host read still reports on stderr, including a terminal-shaped protocol fd.
#[test]
#[serial_test::serial(tty_arbiter)]
fn migration_refused_windows_do_not_consume_or_silence_reports() {
    for scenario in ["migration-protocol", "migration-refused"] {
        let (screen, exited) = run_scenario_bounded(scenario, CHILD_NAME, Duration::from_secs(20));
        assert!(exited, "{scenario}: {screen:?}");
        assert!(!screen.contains(QUESTION), "{scenario}: {screen:?}");
        assert_eq!(screen.matches(MARKER).count(), 1, "{scenario}: {screen:?}");
        assert!(screen.contains("MIGRATION_TERMINAL_RETURNED"));
    }
}

/// Unlike the manually-owned question case, this enters the actual raw modal
/// editor. Both layered reports must finish at column zero before its first
/// prompt render, and the prompt must still accept an answer.
#[test]
#[serial_test::serial(tty_arbiter)]
fn migration_actual_modal_keeps_columns_after_layered_notices() {
    let (screen, exited) =
        run_scenario_bounded("migration-modal", CHILD_NAME, Duration::from_secs(20));
    assert!(exited, "{screen:?}");
    let prompt = screen.find(QUESTION).expect("actual modal prompt");
    let before = &screen[..prompt];
    assert!(
        before.contains("migrated config"),
        "base migration missing: {screen:?}"
    );
    assert!(
        before.contains("old psyche labels"),
        "overlay migration missing: {screen:?}"
    );
    let newline = before.rfind('\n').unwrap();
    assert!(
        before[..=newline].ends_with("\r\n"),
        "raw LF leaves the prompt in the previous column: {before:?}"
    );
    assert!(screen.contains("MIGRATION_TERMINAL_RETURNED"));
}

/// Grounds the actual modal's nonterminal-stderr branch while its stdin and
/// prompt output remain a real tty. Redirection must not steal the prompt or
/// leak migration diagnostics back onto its terminal.
#[test]
#[serial_test::serial(tty_arbiter)]
fn migration_actual_modal_keeps_redirected_stderr_separate() {
    let (screen, exited) = run_scenario_bounded(
        "migration-modal-redirected",
        CHILD_NAME,
        Duration::from_secs(20),
    );
    assert!(exited, "{screen:?}");
    assert!(screen.contains(QUESTION), "{screen:?}");
    assert!(!screen.contains(MARKER), "{screen:?}");
    assert!(!screen.contains("old psyche labels"), "{screen:?}");
    assert!(screen.contains("MIGRATION_TERMINAL_RETURNED"), "{screen:?}");
}
