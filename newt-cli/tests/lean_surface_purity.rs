//! #1409 (epic #1408) — the lean/plain surface's **byte** contract.
//!
//! # What this pins, and why it is shaped this way
//!
//! The epic's safety claim is that LeanTUI stays byte-identical while the rich
//! surface is reorganized underneath it. PR #1420 made the lean *configuration*
//! run in CI; this pins what it may put on the wire.
//!
//! **These are purity assertions, not transcript goldens, and that is
//! deliberate.** A captured full transcript of a newt session is not stable
//! enough to be a regression signal: it embeds the crate version, the absolute
//! workspace path, the configured backend URL, and a reqwest error string whose
//! wording tracks a dependency. A golden carrying those would fail on a version
//! bump and pass on a real regression — and committing one would also put an
//! operator's paths and hostnames into a public repo. What is *invariant* about
//! the lean surface is the shape of the bytes: **no cursor control, no erase, no
//! alternate screen, no mouse tracking**, on every non-rich configuration.
//!
//! Structural claims that ARE deterministic (wrapping math, prompt-token
//! expansion, footer-mode resolution) are pinned as pure unit tests in
//! `newt_tui::lean_input` and `newt_tui::prompt`, where they need no subprocess.
//! This file covers only what those cannot see: what actually reaches the fd.
//!
//! # Isolation
//!
//! Each case runs with its own `HOME` **and its own working directory**. The cwd
//! matters: config resolution walks up from the cwd looking for
//! `.newt/config.toml`, so a test run from the repo would silently pick up the
//! developer's real backend and model. That is not hypothetical — it is how the
//! output that motivated this file was first captured.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

/// Escape sequences the lean surface must never emit.
///
/// Grouped so a failure names the capability that leaked rather than a raw
/// byte string. `ESC[?1049` is the alternate screen; `ESC[?100x`/`101x` are
/// mouse tracking modes; the cursor and erase families are what a redraw
/// surface uses and a scroller must not.
const FORBIDDEN: &[(&str, &[&[u8]])] = &[
    ("alternate screen", &[b"\x1b[?1049", b"\x1b[?47"]),
    (
        "mouse tracking",
        &[
            b"\x1b[?1000",
            b"\x1b[?1002",
            b"\x1b[?1003",
            b"\x1b[?1006",
            b"\x1b[?1015",
        ],
    ),
    (
        "cursor movement",
        &[
            b"\x1b[A", b"\x1b[B", b"\x1b[H", b"\x1b[s", b"\x1b[u", b"\x1b7", b"\x1b8",
        ],
    ),
    ("scroll region", &[b"\x1b[r", b"\x1bM", b"\x1bD"]),
];

/// One lean configuration: how it is invoked, and under what environment.
struct Case {
    name: &'static str,
    args: &'static [&'static str],
    env: &'static [(&'static str, &'static str)],
}

const CASES: &[Case] = &[
    Case {
        name: "piped stdin, defaults",
        args: &["--no-splash"],
        env: &[],
    },
    Case {
        name: "--plain",
        args: &["--no-splash", "--plain"],
        env: &[],
    },
    Case {
        name: "TERM=dumb",
        args: &["--no-splash"],
        env: &[("TERM", "dumb")],
    },
    Case {
        name: "footer off",
        args: &["--no-splash"],
        env: &[("NEWT_FOOTER", "off")],
    },
    Case {
        name: "footer off + TERM=dumb",
        args: &["--no-splash", "--plain"],
        env: &[("NEWT_FOOTER", "off"), ("TERM", "dumb")],
    },
    Case {
        name: "custom prompt template",
        args: &["--no-splash", "--plain"],
        env: &[("NEWT_PROMPT", "test> ")],
    },
];

/// Every lean configuration must reach the fd with a plain scroller's bytes.
///
/// `docs/decisions/plain_scroller_tui.md` states the rule this enforces: no
/// alternate screen, no scroll regions, no mouse handling, no persistent status
/// bars. A rich surface leaking into a piped or `--plain` run is precisely the
/// regression the epic's reorganization could introduce, and it is invisible to
/// a unit test.
#[tokio::test]
async fn every_lean_configuration_emits_plain_scroller_bytes() {
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_newt"));
    for case in CASES {
        let out = run_lean(&bin, case).await;
        for (capability, needles) in FORBIDDEN {
            for needle in *needles {
                assert!(
                    !contains(&out, needle),
                    "[{}] leaked a {capability} sequence ({:?}) on the lean path.\n\
                     The plain scroller must not address the cursor.\n\nstdout:\n{}",
                    case.name,
                    String::from_utf8_lossy(needle),
                    String::from_utf8_lossy(&out),
                );
            }
        }
    }
}

/// EOF on a pipe must end the session cleanly rather than hang or abort.
///
/// This is the `read_piped` contract (`newt-tui/src/lean_input.rs`): on a
/// zero-length read it writes a newline and reports EOF. A surface change that
/// left the reader blocking would make every scripted/headless invocation hang,
/// and no unit test observes it because the path takes `io::stdin()` directly.
#[tokio::test]
async fn eof_on_a_pipe_terminates_instead_of_hanging() {
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_newt"));
    for case in CASES {
        // `run_lean` closes stdin immediately and times out rather than
        // waiting forever; reaching here at all is the assertion.
        let out = run_lean(&bin, case).await;
        assert!(
            !out.is_empty(),
            "[{}] produced no output at all before EOF — the lean surface should \
             still print its header",
            case.name,
        );
    }
}

/// A custom `NEWT_PROMPT` template must reach the wire verbatim.
///
/// `prompt_str` prefers the env template over both defaults, and `read_piped`
/// echoes the prompt so a captured log reads `<prompt><input>`. Both halves have
/// to hold for a scripted session's transcript to be greppable.
#[tokio::test]
async fn a_custom_prompt_template_is_echoed_on_the_piped_path() {
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_newt"));
    let case = CASES
        .iter()
        .find(|c| c.name == "custom prompt template")
        .expect("the custom-prompt case exists");
    let out = run_lean(&bin, case).await;
    assert!(
        contains(&out, b"test> "),
        "the configured prompt never reached stdout.\n\nstdout:\n{}",
        String::from_utf8_lossy(&out),
    );
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// A purity assertion that cannot fail proves nothing, so prove the detector.
///
/// `every_lean_configuration_emits_plain_scroller_bytes` passes by finding
/// nothing. That is the correct result *and* the result a broken matcher would
/// give, so this pins the matcher itself against the real escape bytes from
/// `FORBIDDEN` — including the boundary cases where the needle sits at the very
/// start or end of the capture, which a naive windowing bug would miss.
#[test]
fn the_forbidden_sequence_detector_actually_detects() {
    for (capability, needles) in FORBIDDEN {
        for needle in *needles {
            let mut embedded = b"before ".to_vec();
            embedded.extend_from_slice(needle);
            embedded.extend_from_slice(b" after");
            assert!(
                contains(&embedded, needle),
                "matcher missed a {capability} needle mid-buffer: {:?}",
                String::from_utf8_lossy(needle),
            );
            assert!(
                contains(needle, needle),
                "matcher missed a {capability} needle occupying the whole buffer",
            );
        }
    }
    // And it must not fire on text that merely looks similar.
    assert!(!contains(b"ESC[?1049 as literal text", b"\x1b[?1049"));
    assert!(!contains(b"", b"\x1b[A"));
}

/// Run one case to EOF in full isolation and return its stdout.
///
/// Isolation that matters:
/// * a fresh `HOME` with a seeded empty config, so no first-run wizard;
/// * a fresh **cwd**, so the `.newt/config.toml` walk-up cannot reach the repo's
///   own config (which carries a real backend URL and model);
/// * an unreachable backend on `127.0.0.1:1`, so startup probing fails fast and
///   deterministically instead of contacting anything;
/// * `NEWT_CONFIG`/`NEWT_CONFIG_DIR` cleared, so an operator's environment does
///   not steer the run.
async fn run_lean(bin: &Path, case: &Case) -> Vec<u8> {
    let home = tempfile::tempdir().expect("tempdir for HOME");
    std::fs::create_dir_all(home.path().join(".newt")).expect("mk .newt");
    std::fs::write(home.path().join(".newt/config.toml"), "").expect("seed config");
    let cwd = tempfile::tempdir().expect("tempdir for cwd");

    let mut cmd = Command::new(bin);
    cmd.args(case.args)
        .current_dir(cwd.path())
        .env("HOME", home.path())
        .env("OLLAMA_HOST", "http://127.0.0.1:1")
        .env_remove("NEWT_CONFIG")
        .env_remove("NEWT_CONFIG_DIR")
        .env_remove("NEWT_PROMPT")
        .env_remove("NEWT_FOOTER")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    for (k, v) in case.env {
        cmd.env(k, v);
    }

    let mut child = cmd.spawn().expect("spawn newt");
    // Immediate EOF: the header and prompt still render, and the session ends
    // without needing a reachable model.
    drop(child.stdin.take());

    let output = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
        .await
        .unwrap_or_else(|_| panic!("[{}] did not terminate on EOF", case.name))
        .expect("collect newt output");
    output.stdout
}
