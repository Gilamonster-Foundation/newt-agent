//! #2002 — PTY acceptance for the `/settings` form: open it on a real
//! terminal, pick a field the way an operator does, apply a value, and prove
//! the receipt landed. Same self-re-exec tier as `panel_raw_mode_pty_test`
//! (which this is modelled on), and it grounds the fully-mocked
//! `settings_form` unit tests the way `prompt_visibility_test` grounds the
//! line arbiter's: no mock can observe what actually reached the screen.
//!
//! The assertions are the honest kind (#1986 bar): the field menu is checked
//! on the rendered GRID, not in the emitted byte stream, and the receipt is
//! read back from the file the child's `apply_and_record` wrote — so a pass
//! cannot be explained by bytes that were emitted and then painted over, nor
//! by an apply that never recorded.

use std::time::{Duration, Instant};

use tests_pty::{screen_grid, Pty};

use crate::prompt_visibility_test::wait_for_child;

const CHILD_TEST: &str = "settings_form_pty_test::settings_form_child";
const REACH_TIMEOUT: Duration = Duration::from_secs(60);
const EXIT_TIMEOUT: Duration = Duration::from_secs(60);

/// The child half: runs the EXACT ask wiring `dispatch_slash`'s `"settings"`
/// arm uses — `suspend_for_prompt` + `present_on_terminal` — so this proves
/// the production path, not a test-only twin of it.
#[test]
#[ignore = "child process of the settings-form PTY acceptance test"]
fn settings_form_child() {
    if std::env::var_os("NEWT_SETTINGS_PTY_CHILD").is_none() {
        return;
    }
    let ask = |interaction: &newt_core::interaction_surface::SurfaceInteraction| {
        let window = newt_core::tty::Terminal::suspend_for_prompt();
        crate::permissions::present_on_terminal(&window, interaction)
    };
    for line in crate::settings_form::run(&ask, "") {
        println!("{line}");
    }
}

/// Every field label the bare-form menu must show. Asserted on the grid one
/// by one so a failure names the missing row instead of "menu absent".
const FIELD_LABELS: &[&str] = &[
    "line-editor key bindings",
    "tenacity",
    "cognition",
    "thinking spinner",
    "action-pressure nudges",
    "tool-call round limit",
];

#[test]
fn the_settings_form_renders_picks_applies_and_receipts() {
    // Real fs, deliberately: this is the expensive tier, and the receipt on
    // disk is half of what it exists to prove. Isolated per-run directory;
    // the child inherits it as NEWT_CONFIG_DIR so nothing touches ~/.newt.
    let home = std::env::temp_dir().join(format!(
        "newt-settings-pty-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    std::fs::create_dir_all(&home).expect("create the isolated config dir");

    let pty = Pty::open();
    let mut child = std::process::Command::new(
        std::env::current_exe().expect("the test binary re-invokes itself"),
    )
    .args(["--exact", CHILD_TEST, "--ignored", "--nocapture"])
    .env("NEWT_SETTINGS_PTY_CHILD", "form")
    .env("NEWT_CONFIG_DIR", &home)
    .stdin(pty.slave_stdio())
    .stdout(pty.slave_stdio())
    .stderr(std::process::Stdio::null())
    .spawn()
    .expect("spawn the pty child");

    // Drive the two menu levels. `wait_on_grid` plays the terminal (answers
    // DSR) while polling the RENDERED screen for a marker.
    let mut transcript = String::new();
    let mut answered_dsr = false;
    let mut wait_on_grid = |marker: &str| -> String {
        let deadline = Instant::now() + REACH_TIMEOUT;
        loop {
            let screen = pty.screen();
            transcript.push_str(&screen);
            if !answered_dsr && transcript.contains("\u{1b}[6n") {
                pty.type_in("\u{1b}[3;1R");
                answered_dsr = true;
            }
            let grid = screen_grid(&screen).join("\n");
            if grid.contains(marker) {
                return grid;
            }
            assert!(
                Instant::now() < deadline,
                "`{marker}` never RENDERED; last grid:\n{grid}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    };

    // 1. The bare form: every field label visible on the grid.
    let menu = wait_on_grid(FIELD_LABELS[FIELD_LABELS.len() - 1]);
    for label in FIELD_LABELS {
        assert!(
            menu.contains(label),
            "field `{label}` not rendered:\n{menu}"
        );
    }

    // 2. Pick field [1] (edit-mode) by number, exactly as an operator types.
    pty.type_in("1\r");
    let values = wait_on_grid("nano");

    // 3. Pick `nano` by ITS number, read off the rendered menu rather than
    //    assumed — the menu's order is the form's business, not this test's.
    let nano_row = values
        .lines()
        .find(|l| l.contains("nano"))
        .expect("a nano row was on the grid");
    let number: String = nano_row
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(char::is_ascii_digit)
        .collect();
    assert!(!number.is_empty(), "no option number on: {nano_row}");
    pty.type_in(&format!("{number}\r"));

    let status =
        wait_for_child(&mut child, EXIT_TIMEOUT).expect("the form child exited within the timeout");
    assert!(status.success(), "the form child failed: {status:?}");

    // 4. The receipt is the point (#1965 closed this class): the change is on
    //    disk, content-addressed, with the verb bound in.
    let receipts = std::fs::read_to_string(home.join("receipts.jsonl"))
        .expect("apply_and_record wrote receipts.jsonl beside settings.toml");
    assert!(
        receipts.contains("nano"),
        "the applied value is not in the receipt: {receipts}"
    );
    assert!(
        receipts.contains("/settings"),
        "the verb is not bound into the receipt: {receipts}"
    );
    let parsed = newt_core::settings_receipt::read_jsonl(&receipts);
    assert!(
        !parsed.is_empty(),
        "the receipt line did not parse back: {receipts}"
    );

    let _ = std::fs::remove_dir_all(&home);
}
