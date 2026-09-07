//! Real-resource grounding for the mocked terminal-guard/drop-order tests.
//!
//! Run: `cargo test -p newt-agent --test terminal_exit_pty`.
//! Also run with `--no-default-features` to cover the compiled lean morphology.
//! `NEWT_TERMINAL_EXIT_TEST_BIN` optionally selects an already-built binary for
//! focused before/after diagnosis; requested surface checks still apply.
//!
//! Unlike guard-only tests, these run actual CLI startup, input, an optional
//! mocked provider turn, and the operator's `/exit`. The PTY/process are real;
//! only inference is mocked. Full termios equality grounds the save/restore
//! contract: canonical input plus echo alone cannot prove that signals or
//! newline/output processing were restored. A nondefault baseline must be
//! preserved too, not silently replaced with "sane" terminal defaults.

#![cfg(unix)]

mod common;

use std::path::PathBuf;
use std::time::Duration;

use serde_json::json;
use tests_pty::Pty;
use tokio::process::Command;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const BUDGET: Duration = Duration::from_secs(30);
const MODEL: &str = "terminal-exit-fixture";
const PROMPT: &str = "What is two plus two? Give a short answer.";
const ANSWER: &str = "The answer is four. TERM_EXIT_ANSWER.";
const LEAN_READY: &str = "TERM_EXIT_READY> ";
// TurnMetrics::display_line renders this suffix for the unpriced fixture model
// with reported usage. It does not depend on elapsed time or probe/replay totals.
const TURN_METRICS: &str = " out · free (local)";

#[derive(Clone, Copy, Debug)]
enum Surface {
    Lean,
    #[cfg(feature = "rich-tui")]
    RichPerTurn,
    #[cfg(feature = "rich-tui")]
    Cockpit,
}

const SURFACES: &[Surface] = &[
    Surface::Lean,
    #[cfg(feature = "rich-tui")]
    Surface::RichPerTurn,
    #[cfg(feature = "rich-tui")]
    Surface::Cockpit,
];

impl Surface {
    fn configure(self, cmd: &mut Command) {
        match self {
            Self::Lean => {
                cmd.env("NEWT_FOOTER", "off").env("NEWT_NO_COCKPIT", "1");
            }
            #[cfg(feature = "rich-tui")]
            Self::RichPerTurn => {
                cmd.env("NEWT_FOOTER", "on").env("NEWT_NO_COCKPIT", "1");
            }
            #[cfg(feature = "rich-tui")]
            Self::Cockpit => {
                cmd.env("NEWT_FOOTER", "on").env_remove("NEWT_NO_COCKPIT");
            }
        }
    }

    fn ready_marker(self) -> &'static str {
        match self {
            Self::Lean => LEAN_READY,
            #[cfg(feature = "rich-tui")]
            // The classic editor exists only between turns. In the cockpit
            // this hint follows the turn flag, not whether ReadLine is pending:
            // it can coexist with pre/post-turn work and is not an idle receipt.
            Self::RichPerTurn | Self::Cockpit => "^D exit",
        }
    }

    fn assert_selected(self, screen: &str) {
        let cockpit = screen.contains("\x1b[?7l"); // Presenter::open disables wrap.
        assert!(!screen.contains("cockpit unavailable"), "{screen:?}");
        match self {
            Self::Lean => {
                assert!(
                    screen.contains(LEAN_READY),
                    "lean editor missing: {screen:?}"
                );
                assert!(!cockpit, "lean case selected the cockpit");
            }
            #[cfg(feature = "rich-tui")]
            Self::RichPerTurn => {
                assert!(
                    !screen.contains(LEAN_READY),
                    "rich case selected lean input"
                );
                assert!(!cockpit, "per-turn case selected the cockpit");
            }
            #[cfg(feature = "rich-tui")]
            Self::Cockpit => assert!(cockpit, "cockpit never activated: {screen:?}"),
        }
    }
}

struct Answer;

impl Respond for Answer {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: serde_json::Value = serde_json::from_slice(&request.body).expect("chat JSON");
        if body["stream"].as_bool().unwrap_or(false) {
            let frame =
                json!({"choices": [{"delta": {"content": ANSWER}, "finish_reason": "stop"}]});
            ResponseTemplate::new(200).set_body_raw(
                format!("data: {frame}\n\ndata: [DONE]\n\n"),
                "text/event-stream",
            )
        } else {
            ResponseTemplate::new(200).set_body_json(json!({
                "model": MODEL,
                "choices": [{
                    "message": {"role": "assistant", "content": ANSWER},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 32, "completion_tokens": 12, "total_tokens": 44}
            }))
        }
    }
}

fn seed_baseline(pty: &Pty, customized: bool) -> tests_pty::TermiosSnapshot {
    let mut baseline = pty.termios_snapshot();
    baseline.local_flags |= libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN;
    baseline.input_flags |= libc::ICRNL;
    baseline.output_flags |= libc::OPOST;
    if customized {
        // The partial mode a prior force-terminated raw-mode owner can leave.
        // An orderly successor must restore what it inherited, not "repair"
        // the operator's settings. Also change a control character explicitly.
        baseline.local_flags &= !(libc::ISIG | libc::IEXTEN);
        baseline.input_flags &= !(libc::ICRNL | libc::IXON);
        baseline.output_flags &= !libc::OPOST;
        baseline.control_chars[libc::VERASE] = 8;
    }
    pty.set_termios_snapshot(&baseline);
    assert_eq!(
        pty.termios_snapshot(),
        baseline,
        "fixture baseline installed"
    );
    baseline
}

fn exercise_rich_resizes(pty: &Pty, pid: u32, screen: &mut String) -> Result<String, String> {
    let mut expected = String::new();
    for cycle in 0..2 {
        let first = format!("TERM_RESIZE_{cycle}_FIRST");
        let middle = format!("TERM_RESIZE_{cycle}_MIDDLE");
        let last = format!("TERM_RESIZE_{cycle}_LAST");
        // Bracketed paste grows the draft without submitting any of its lines.
        expected = format!("{first}\n{middle}\n{last}");
        pty.type_in(&format!("\x1b[200~{expected}\x1b[201~"));
        let grew = pty.wait_for_screen(&last, BUDGET);
        screen.push_str(&pty.screen());
        if !grew {
            return Err(format!(
                "cycle {cycle}: multiline input marker never appeared"
            ));
        }

        // Each resize gets a NEW input marker, so output from a prior frame
        // cannot satisfy this barrier. Raw bytes prove input responsiveness,
        // not clipping or erasure; the backend later proves draft preservation.
        for (step, (rows, cols)) in [(3, 80), (50, 200)].into_iter().enumerate() {
            pty.resize(rows, cols);
            tests_pty::signal_winch(pid);
            let suffix = format!("__C{cycle}R{step}__");
            pty.type_in(&format!("\x1b[200~{suffix}\x1b[201~"));
            expected.push_str(&suffix);
            let accepted = pty.wait_for_screen(&suffix, BUDGET);
            screen.push_str(&pty.screen());
            if !accepted {
                return Err(format!(
                    "cycle {cycle}: fresh input marker missing after resize to {cols}x{rows}"
                ));
            }
        }

        // Clear only the first draft. The second remains intact for Enter;
        // backend capture must contain it exactly and never contain cycle 0.
        if cycle == 0 {
            pty.type_in("\x03");
            let cleared = pty.wait_for_screen_after("Ctrl-C to interrupt", "^D exit", BUDGET);
            screen.push_str(&pty.screen());
            if !cleared {
                return Err("cleared draft never returned to the input hint".to_string());
            }
        }
        if !pty.is_raw() {
            return Err(format!(
                "cycle {cycle}: editor released raw mode while still mounted"
            ));
        }
    }
    Ok(expected)
}

async fn run_case(surface: Surface, customized: bool, provider_turn: bool, resize_editor: bool) {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list", "data": [{"id": MODEL, "object": "model"}]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(Answer)
        .mount(&server)
        .await;

    let root = common::isolated_root();
    let config_dir = root.path().join(".newt");
    std::fs::create_dir(&config_dir).expect("isolated config dir");
    let config = config_dir.join("config.toml");
    std::fs::write(
        &config,
        format!(
            r#"default_backend = "terminal-fixture"

[[backends]]
name = "terminal-fixture"
endpoint = "{}"
model = "{MODEL}"
kind = "openai"

[tui.permissions]
preset = "read_only"
prompt = false
extra_exec = []
net = []
"#,
            server.uri()
        ),
    )
    .expect("isolated config");

    let pty = Pty::open();
    let baseline = seed_baseline(&pty, customized);
    let binary = std::env::var_os("NEWT_TERMINAL_EXIT_TEST_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_newt")));
    let mut cmd = Command::new(binary);
    common::isolate(&mut cmd, root.path());
    // No ambient helper/reporting connection, permission bypass or input mode.
    // These are child-only env changes; sibling tests keep their environment.
    for (key, _) in std::env::vars_os() {
        if key.to_str().is_some_and(|key| key.starts_with("HERDR_")) {
            cmd.env_remove(key);
        }
    }
    for key in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        cmd.env_remove(key);
    }
    cmd.args(["--no-splash", "--ephemeral", "--no-prompt-for-permissions"])
        .arg("--config")
        .arg(&config)
        .env("NEWT_DISABLE_OCAP", "0")
        .env("NEWT_FULL_ACCESS", "0")
        .env("NEWT_EDIT_MODE", "emacs")
        .env("NEWT_NO_MODEL_PULL", "1")
        .env("NEWT_PROMPT", LEAN_READY)
        .env("TERM", "xterm-256color")
        .env_remove("NEWT_METRICS_PORT")
        .env_remove("NEWT_DEBUG")
        .env_remove("NEWT_TRACE")
        .env_remove("NEWT_GUTTER")
        .stdin(pty.slave_stdio())
        .stdout(pty.slave_stdio())
        .stderr(pty.slave_stdio())
        .kill_on_drop(true);
    surface.configure(&mut cmd);
    let mut child = cmd.spawn().expect("spawn real CLI on the owned PTY");
    // Command retains its reusable Stdio handles after spawn. Drop those
    // parent-side slave duplicates so child exit can produce real PTY EOF.
    drop(cmd);

    // Wait for painted input, not a guessed startup delay. The quiet PTY also
    // exercises the existing bounded cursor-query fallback on rich surfaces.
    let ready = pty.wait_for_screen(surface.ready_marker(), BUDGET);
    let mut screen = pty.screen();
    let resized = if ready && resize_editor {
        exercise_rich_resizes(
            &pty,
            child.id().expect("running CLI has a pid"),
            &mut screen,
        )
    } else {
        Ok(PROMPT.to_string())
    };
    let mut answered = !provider_turn;
    let mut metrics_complete = !provider_turn;
    let mut ready_after_answer = !provider_turn;
    if ready && resized.is_ok() {
        if provider_turn {
            if resize_editor {
                pty.type_in("\r");
            } else {
                pty.type_in(&format!("{PROMPT}\r"));
            }
            answered = pty.wait_for_screen("TERM_EXIT_ANSWER.", BUDGET);
            if answered {
                // Streamed ANSWER can precede provider completion and memory
                // sync. The flushed final metrics follow that post-turn work.
                // Keep peeking so same/split-drain markers are never discarded.
                metrics_complete =
                    pty.wait_for_screen_after("TERM_EXIT_ANSWER.", TURN_METRICS, BUDGET);
                if metrics_complete {
                    ready_after_answer = match surface {
                        #[cfg(feature = "rich-tui")]
                        // A complete Line is queued until ReadLine if needed.
                        // Unlike Ctrl-D, literal /exit cannot be dropped here.
                        Surface::Cockpit => true,
                        _ => {
                            pty.wait_for_screen_after(TURN_METRICS, surface.ready_marker(), BUDGET)
                        }
                    };
                }
            }
            screen.push_str(&pty.screen());
        }
        // Classic surfaces require a fresh editor after final metrics. The
        // cockpit stays mounted and queues this complete Line until ReadLine;
        // neither a mode hint nor Ctrl-D is used as its idle synchronization.
        if ready_after_answer {
            // Typing a leading slash opens the rich palette, where the first
            // Enter completes rather than submits. Paste the complete command
            // so Enter is a submission, just as an operator's full-line paste.
            match surface {
                Surface::Lean => pty.type_in("/exit\r"),
                #[cfg(feature = "rich-tui")]
                Surface::RichPerTurn | Surface::Cockpit => {
                    pty.type_in("\x1b[200~/exit\x1b[201~\r");
                }
            }
        }
    }

    // Always reap before asserting, including startup/answer timeout cases.
    // A killed child is diagnostic failure, never evidence of clean /exit.
    let status = if ready && resized.is_ok() && ready_after_answer {
        match tokio::time::timeout(BUDGET, child.wait()).await {
            Ok(Ok(status)) => Some(status),
            _ => None,
        }
    } else {
        None
    };
    if status.is_none() {
        let _ = child.start_kill();
        tokio::time::timeout(BUDGET, child.wait())
            .await
            .expect("stalled fixture child did not reap within its budget")
            .expect("reap stalled fixture child");
    }
    let restored = pty.termios_snapshot();
    screen.push_str(&pty.screen_to_eof());
    let case = format!(
        "{surface:?}, customized={customized}, provider_turn={provider_turn}, resize_editor={resize_editor}"
    );
    assert!(ready, "{case}: editor never became ready: {screen:?}");
    assert!(resized.is_ok(), "{case}: {resized:?}: {screen:?}");
    assert!(answered, "{case}: mocked answer never appeared: {screen:?}");
    assert!(
        metrics_complete,
        "{case}: final turn metrics never followed the answer: {screen:?}"
    );
    assert!(
        ready_after_answer,
        "{case}: no post-turn exit boundary (fresh editor for classic surfaces): {screen:?}"
    );
    let status = status.unwrap_or_else(|| {
        panic!("{case}: CLI did not exit within its bounded budget: {screen:?}")
    });
    assert!(status.success(), "{case}: CLI exited {status}: {screen:?}");
    surface.assert_selected(&screen);
    assert_eq!(
        restored, baseline,
        "{case}: /exit changed inherited termios"
    );

    let requests = server.received_requests().await.expect("captured requests");
    let expected_prompt = resized.expect("resize result checked after reaping the CLI");
    let saw_prompt = requests.iter().any(|request| {
        serde_json::from_slice::<serde_json::Value>(&request.body)
            .ok()
            .and_then(|body| body["messages"].as_array().cloned())
            .is_some_and(|messages| {
                messages.iter().any(|message| {
                    message["role"] == "user"
                        && message["content"]
                            .as_str()
                            .is_some_and(|text| text == expected_prompt)
                })
            })
    });
    assert_eq!(
        saw_prompt, provider_turn,
        "{case}: backend must receive the exact submitted draft"
    );
    assert!(
        !requests
            .iter()
            .any(|request| String::from_utf8_lossy(&request.body).contains("TERM_RESIZE_0_")),
        "{case}: the cleared draft reached inference"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(terminal_exit_pty)]
async fn immediate_operator_exit_preserves_full_termios() {
    for &surface in SURFACES {
        for customized in [false, true] {
            run_case(surface, customized, false, false).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(terminal_exit_pty)]
async fn operator_exit_after_a_provider_turn_preserves_full_termios() {
    for &surface in SURFACES {
        for customized in [false, true] {
            run_case(surface, customized, true, false).await;
        }
    }
}

/// Grounds the mocked multiline-editor and idle Ctrl-C tests in the classic
/// driver's actual lease lifecycle. Fresh markers after resizes prove input
/// responsiveness, not visible geometry; backend capture proves the final
/// draft survives exactly and the cleared draft was never submitted.
#[cfg(feature = "rich-tui")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(terminal_exit_pty)]
async fn rich_editor_preserves_draft_through_resizes_and_clear_then_restores_termios() {
    for customized in [false, true] {
        run_case(Surface::RichPerTurn, customized, true, true).await;
    }
}
