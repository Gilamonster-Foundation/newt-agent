//! Real terminal grounding for TUI migration-notice host delivery. The existing
//! cockpit driver owns all fds and input; no second presentation loop is added.

use super::*;
use crate::cockpit::test_tty::{modes_equal, termios_of, TestTty};

#[test]
#[serial_test::serial(tty_arbiter, prompt_stdin)]
fn migration_notices_preserve_the_cockpit_editor_and_active_modal() {
    crate::interaction_view_pty_test::drive_cockpit_migration();
}

pub(crate) fn cockpit_migration_case() {
    let mut tty = TestTty::install();
    tty.capture_stderr();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "# current config\n").unwrap();
    // Isolated child; discovery is pinned before its UI starts any workers.
    unsafe {
        std::env::set_var("NEWT_CONFIG_DIR", dir.path());
        std::env::set_var("NEWT_CONFIG", &path);
    }
    std::env::set_current_dir(dir.path()).unwrap();
    let before = termios_of(0);
    let surface = crate::rich_input::RichSurface::new(None).unwrap();
    {
        let mut cockpit = Presenter::open(surface).unwrap();
        {
            let (editor, screen) = (&mut cockpit.editor, &mut cockpit.screen);
            editor
                .on_event(Event::Paste("migration draft survives".into()), screen)
                .unwrap();
        }
        cockpit.draw().unwrap();
        let draft = cockpit.editor.draft();
        let marker = "migrated config";
        let old = "[tenacity]\ndefault = \"standard\"\n";
        std::fs::write(&path, old).unwrap();
        let start = tty.painted().len();
        let (reply, answer) = std::sync::mpsc::sync_channel(1);
        let interaction = newt_core::interaction_surface::SurfaceInteraction::blocking(
            crate::permissions::free_text_form("Migration modal visible?"),
        );
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                assert!(tty.wait_for_painted_after(start, "visible?", Duration::from_secs(3)));
                assert!(tty.wait_for_painted_after(start, "╯", Duration::from_secs(3)));
                let result =
                    crate::migration_notices::read(|report| newt_core::Config::load(&path, report));
                result.unwrap();
                assert_eq!(
                    std::fs::read_to_string(&path).unwrap(),
                    newt_core::psyche_import::migrate_config_text(old)
                        .unwrap()
                        .text
                );
                // Stderr is captured while the presenter lends the real tty to
                // this modal. The report must not be painted over its question.
                assert!(!tty.painted()[start..].contains(marker));
                tty.type_bytes(b"yes\r");
            });
            cockpit
                .handle_request(SurfaceRequest::Interact {
                    interaction: Box::new(interaction),
                    reply,
                })
                .unwrap();
            worker.join().unwrap();
        });
        assert_eq!(
            answer.recv().unwrap(),
            newt_core::HumanQuestionOutcome::Answer("yes".into())
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            cockpit.drain_pty().unwrap();
            if tty.wait_for_painted_after(start, marker, Duration::from_millis(5)) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "migration notice did not reach cockpit transcript"
            );
        }
        assert_eq!(cockpit.editor.draft(), draft);
        assert!(
            !cockpit.chat_inactive,
            "editor regains focus after notice delivery"
        );
    }
    assert!(
        modes_equal(&before, &termios_of(0)),
        "exact termios restoration"
    );
}

#[test]
#[serial_test::serial(tty_arbiter, prompt_stdin)]
fn migration_persona_list_keeps_decode_skip_notices_and_later_reads_are_quiet() {
    crate::interaction_view_pty_test::drive_persona_migration();
}

pub(crate) fn persona_migration_case() {
    use std::io::Write as _;
    let mut tty = TestTty::install();
    tty.capture_stderr();
    let dir = tempfile::tempdir().unwrap();
    let store = crate::PersonaStore::new(dir.path());
    let good = dir.path().join("migration-good.md");
    let bad = dir.path().join("migration-bad.md");
    let bad_load = dir.path().join("migration-load.md");
    let old = "+++\ncognition = \"pondering\"\n+++\nPersona body.\n";
    let invalid = old.replace("+++\nPersona", "tools = 7\n+++\nPersona");
    std::fs::write(&good, old).unwrap();
    std::fs::write(&bad, &invalid).unwrap();
    let listed = store.list().unwrap();
    assert!(listed.iter().any(|p| p.name == "migration-good"));
    assert!(!listed.iter().any(|p| p.name == "migration-bad"));
    std::fs::write(&bad_load, &invalid).unwrap();
    assert!(store.load("migration-load").is_err());
    // Repeated successful physical rewrites are quiet, including a persona
    // that remains invalid and is skipped on each later listing.
    store.list().unwrap();
    assert!(store.load("migration-load").is_err());
    writeln!(std::io::stderr(), "MIGRATION_PERSONAS_DONE").unwrap();
    assert!(tty.wait_for_painted_after(0, "MIGRATION_PERSONAS_DONE", Duration::from_secs(3)));
    let painted = tty.painted();
    assert_eq!(
        painted.matches("newt: migrated persona").count(),
        3,
        "{painted:?}"
    );
    for path in [&good, &bad, &bad_load] {
        assert_eq!(
            painted.matches(path.to_str().unwrap()).count(),
            1,
            "{painted:?}"
        );
        let original = if path == &good { old } else { &invalid };
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            newt_core::psyche_import::migrate_persona_text(original)
                .unwrap()
                .text
        );
    }
}

#[test]
#[serial_test::serial(tty_arbiter, prompt_stdin)]
fn migration_startup_flushes_after_splash_on_success_and_error() {
    crate::interaction_view_pty_test::drive_startup_migration();
}

pub(crate) fn startup_migration_case() {
    use std::io::Write as _;
    let mut tty = TestTty::install();
    tty.capture_stderr();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_current_dir(dir.path()).unwrap();
    unsafe {
        std::env::set_var("NEWT_CONFIG_DIR", dir.path());
    }
    let before = termios_of(0);
    for invalid in [false, true] {
        let path = dir
            .path()
            .join(if invalid { "bad.toml" } else { "good.toml" });
        let old = "[tenacity]\ndefault = \"standard\"\n";
        std::fs::write(
            &path,
            format!(
                "{}{old}",
                if invalid { "default_backend = 7\n" } else { "" }
            ),
        )
        .unwrap();
        let start = tty.painted().len();
        // The actual startup owners and declaration order; no new screen or
        // diagnostic writer is introduced by this acceptance fixture.
        let result = (|| -> anyhow::Result<()> {
            let mut pending = crate::migration_notices::Pending::default();
            let screen = crate::SplashScreenGuard::enter()?;
            newt_core::Config::load(&path, &mut |n| pending.report(n))?;
            assert!(!tty.painted()[start..].contains("migrated config"));
            drop(screen);
            pending.flush();
            Ok(())
        })();
        assert_eq!(result.is_err(), invalid);
        writeln!(std::io::stderr(), "MIGRATION_STARTUP_DONE").unwrap();
        assert!(tty.wait_for_painted_after(
            start,
            "MIGRATION_STARTUP_DONE",
            Duration::from_secs(3)
        ));
        let painted = tty.painted();
        let after = &painted[start..];
        let restored = after
            .find("\x1b[?1049l")
            .expect("splash leaves alternate screen");
        let report = after
            .find("newt: migrated config")
            .expect("startup report retained");
        assert!(
            restored < report,
            "report must follow splash restoration: {after:?}"
        );
        assert_eq!(
            after.matches("newt: migrated config").count(),
            1,
            "{after:?}"
        );
        assert!(modes_equal(&before, &termios_of(0)));
    }
    startup_run_code_paths(&tty, dir.path());
}

/// Exercise the actual startup entry point with its Pending declaration and
/// splash guard ordering. The existing child owns all descriptors; no global
/// parent state or alternate terminal harness is involved.
fn startup_run_code_paths(tty: &TestTty, dir: &std::path::Path) {
    use std::io::Write as _;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    // The plain startup splash includes its quit footer below the 24-row logo.
    // Resize only the terminal owned by this isolated child.
    let size = libc::winsize {
        ws_row: 40,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    assert_eq!(unsafe { libc::ioctl(0, libc::TIOCSWINSZ, &size) }, 0);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _runtime = runtime.enter();
    let cases: &[bool] = if cfg!(target_os = "linux") {
        &[false, true]
    } else {
        &[false]
    };
    for &fail_splash in cases {
        let path = dir.join(if fail_splash {
            "entry-error.toml"
        } else {
            "entry-quit.toml"
        });
        // An explicit inert registry keeps startup out of ambient discovery.
        let old = concat!(
            "[[providers]]\nname = \"fixture\"\n",
            "command = \"unused-provider-fixture\"\n",
            "model = \"fixture\"\ntiers = [\"complex\"]\n",
            "[tenacity]\ndefault = \"standard\"\n",
        );
        std::fs::write(&path, old).unwrap();
        // This is the process-isolated PTY child, before any startup worker.
        unsafe {
            std::env::set_var("NEWT_CONFIG", &path);
            std::env::set_var("NO_COLOR", "1");
            std::env::set_var("NEWT_COLOR", "never");
        }
        let before = termios_of(0);
        let start = tty.painted().len();
        let result = if fail_splash {
            // Linux's real failing device grounds the production `?` at
            // SplashScreenGuard::enter after raw mode was acquired. Stderr
            // stays on the owned PTY so the deferred report remains visible.
            let full = std::fs::OpenOptions::new()
                .write(true)
                .open("/dev/full")
                .unwrap();
            let saved = unsafe { libc::dup(libc::STDOUT_FILENO) };
            assert!(saved >= 0);
            let saved = unsafe { OwnedFd::from_raw_fd(saved) };
            assert!(unsafe { libc::dup2(full.as_raw_fd(), libc::STDOUT_FILENO) } >= 0);
            let result = crate::run_code(Some(dir), false, None, None, None, None);
            let restored = modes_equal(&before, &termios_of(0));
            assert!(unsafe { libc::dup2(saved.as_raw_fd(), libc::STDOUT_FILENO) } >= 0);
            drop(saved);
            assert!(
                restored,
                "raw mode must be restored on splash-entry failure"
            );
            let error = result.as_ref().unwrap_err();
            assert_eq!(
                error
                    .downcast_ref::<std::io::Error>()
                    .unwrap()
                    .raw_os_error(),
                Some(libc::ENOSPC)
            );
            result
        } else {
            std::thread::scope(|scope| {
                let input = scope.spawn(|| {
                    assert!(tty.wait_for_painted_after(start, "q quit", Duration::from_secs(3)));
                    assert!(
                        !tty.painted()[start..].contains("newt: migrated config"),
                        "report must remain pending while splash owns the screen"
                    );
                    tty.type_bytes(b"q");
                });
                let result = crate::run_code(Some(dir), false, None, None, None, None);
                input.join().unwrap();
                result
            })
        };
        assert_eq!(result.is_err(), fail_splash);
        assert!(modes_equal(&before, &termios_of(0)));
        writeln!(std::io::stderr(), "MIGRATION_RUN_CODE_DONE").unwrap();
        assert!(tty.wait_for_painted_after(
            start,
            "MIGRATION_RUN_CODE_DONE",
            Duration::from_secs(3)
        ));
        let painted = tty.painted();
        let after = &painted[start..];
        let report = after
            .find("newt: migrated config")
            .expect("actual startup retained report");
        assert_eq!(
            after.matches("newt: migrated config").count(),
            1,
            "{after:?}"
        );
        if !fail_splash {
            let restored = after
                .find("\x1b[?1049l")
                .expect("actual splash exits alternate screen");
            assert!(
                restored < report,
                "startup must restore before reporting: {after:?}"
            );
        }
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            newt_core::psyche_import::migrate_config_text(old)
                .unwrap()
                .text
        );
    }
}
