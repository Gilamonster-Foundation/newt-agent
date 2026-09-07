use super::*;
use newt_core::tty::{LineCaps, Sink, Terminal};

fn row_is_free() -> bool {
    Terminal::lease_with_caps(LineCaps::Own, Sink::Stdout).is_some()
}

#[test]
#[serial_test::serial(tty_arbiter, prompt_stdin)]
fn summary_progress_spans_success_and_error() {
    for fail in [false, true] {
        let summarizer: newt_core::Summarizer = Box::new(move |_| {
            Box::pin(async move {
                assert!(!row_is_free(), "pending summary must own the progress row");
                if fail {
                    anyhow::bail!("summary failed");
                }
                Ok("summary".into())
            })
        });
        let summary = with_summary_progress(summarizer, LineCaps::Own, false);
        let out = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(summary("input".into()));
        assert_eq!(out.is_err(), fail);
        assert!(row_is_free(), "completed summary must release its row");
    }
}

#[test]
#[serial_test::serial(tty_arbiter, prompt_stdin)]
fn summary_progress_releases_on_cancel_and_respects_no_terminal() {
    for caps in [LineCaps::Own, LineCaps::None] {
        let pending: newt_core::Summarizer = Box::new(|_| Box::pin(std::future::pending()));
        let summary = with_summary_progress(pending, caps, true);
        let mut future = summary("input".into());
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(std::future::Future::poll(future.as_mut(), &mut cx).is_pending());
        assert_eq!(row_is_free(), caps == LineCaps::None);
        drop(future);
        assert!(row_is_free(), "cancelled summary must release its row");
    }
}

#[test]
#[serial_test::serial(tty_arbiter, prompt_stdin)]
fn summary_progress_preserves_an_existing_spinner() {
    let outer = newt_core::tty::Spinner::start_with_caps(
        LineCaps::Own,
        "compressing context…",
        Sink::Stdout,
        false,
    )
    .unwrap();
    let summarizer: newt_core::Summarizer = Box::new(|_| Box::pin(async { Ok("summary".into()) }));
    let summary = with_summary_progress(summarizer, LineCaps::Own, false);
    let out = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(summary("input".into()));
    assert_eq!(out.unwrap(), "summary");
    assert!(
        !row_is_free(),
        "summary must not release the outer spinner's row"
    );
    drop(outer);
    assert!(row_is_free());
}

#[cfg(unix)]
const CHILD: &str = "compaction_progress_tests::summary_progress_child";

#[cfg(unix)]
#[test]
#[ignore = "child process of the compaction progress PTY test"]
fn summary_progress_child() {
    use std::io::IsTerminal as _;
    if std::env::var_os("NEWT_SUMMARY_PROGRESS_CHILD").is_none() {
        return;
    }
    // The release input must not echo a newline over the spinner's live row.
    let raw = std::io::stdin()
        .is_terminal()
        .then(|| newt_core::tty::raw_mode::RawModeGuard::enter().unwrap());
    let release = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let input_release = release.clone();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        input_release.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(wiremock::matchers::method("POST"))
            .respond_with(move |_: &wiremock::Request| {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
                while !release.load(std::sync::atomic::Ordering::SeqCst) {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "parent did not release summary"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{"message": {"content": "SUMMARY_COMPLETE"}}]
                }))
            })
            .mount(&server)
            .await;
        let cfg = newt_core::SummarizerConfig {
            kind: Some(newt_core::BackendKind::Openai),
            endpoint: Some(server.uri()),
            model: Some("test-model".into()),
            retries: 0,
            ..Default::default()
        };
        let summary = build_session_summarizer(
            &cfg,
            &newt_core::Config::default(),
            &server.uri(),
            "test-model",
            newt_core::BackendKind::Openai,
            &None,
            None,
            true,
        );
        assert_eq!(
            summary("summarize".into()).await.unwrap(),
            "SUMMARY_COMPLETE"
        );
    });
    drop(raw);
    println!("SUMMARY_COMPLETE");
}

/// Grounds the mocked future/arbiter lifecycle tests above: the production
/// summarizer factory must paint while an HTTP response is still pending, then
/// erase its row. The parent releases the response only after seeing the grid.
#[cfg(unix)]
#[test]
fn summary_progress_is_visible_until_the_http_response_arrives() {
    use std::time::Duration;
    let pty = tests_pty::Pty::open();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", CHILD, "--ignored", "--nocapture"])
        .env("NEWT_SUMMARY_PROGRESS_CHILD", "1")
        .env("TERM", "xterm-256color")
        .stdin(pty.slave_stdio())
        .stdout(pty.slave_stdio())
        .stderr(pty.slave_stdio())
        .spawn()
        .unwrap();
    let visible = pty.wait_for_screen("summarizing context…", Duration::from_secs(10));
    let mut transcript = pty.screen();
    let active_grid = tests_pty::screen_grid(&transcript).join("\n");
    pty.type_in("release\n");
    let status = crate::prompt_visibility_test::wait_for_child(&mut child, Duration::from_secs(10));
    transcript.push_str(&pty.screen_when_finished(status.is_some()));
    assert!(
        visible && active_grid.contains("summarizing context…"),
        "{transcript}"
    );
    assert!(
        !active_grid.contains("SUMMARY_COMPLETE"),
        "summary completed before progress appeared"
    );
    assert!(
        status.is_some_and(|status| status.success()),
        "{transcript}"
    );
    let final_grid = tests_pty::screen_grid(&transcript).join("\n");
    assert!(final_grid.contains("SUMMARY_COMPLETE"), "{final_grid}");
    assert!(!final_grid.contains("summarizing context…"), "{final_grid}");
}

#[cfg(unix)]
#[test]
fn summary_progress_is_silent_on_a_pipe_even_with_color() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", CHILD, "--ignored", "--nocapture"])
        .env("NEWT_SUMMARY_PROGRESS_CHILD", "1")
        .env("TERM", "xterm-256color")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("SUMMARY_COMPLETE"), "{stdout}");
    assert!(!stdout.contains("summarizing context…"), "{stdout}");
    assert!(!stdout.contains('\x1b'), "{stdout}");
}
