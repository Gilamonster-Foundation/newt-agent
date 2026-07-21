//! End-to-end mock test: real `newt worker` binary, fake Ollama via
//! `wiremock`.
//!
//! For each bundled case under `newt-eval/cases/`:
//!
//! 1. Stand up a wiremock server that, on `POST /api/chat`, returns the
//!    case's `mock_response.content` wrapped in Ollama's
//!    `{ "message": { "content": "..." } }` envelope.
//! 2. Spawn the real `newt` binary with `OLLAMA_HOST` pointed at the
//!    wiremock URL.
//! 3. Drive ACP (initialize → new_session → prompt) and collect the
//!    TaskReply.
//! 4. Run every evaluator the case declares.
//! 5. Assert the case passed every evaluator.
//!
//! This is the **single test** that proves the whole pipeline works:
//! ACP wire format, diff capture, evaluator wiring, scorecard render.
//! It runs in CI — no external Ollama required.

use std::path::PathBuf;

use newt_eval::{
    cases, evaluator_by_name, run_case, CaseScorecard, EvalContext, RunnerConfig, Scorecard,
};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test(flavor = "multi_thread")]
async fn all_bundled_cases_pass_in_mock_mode() {
    // Build the worker once. `cargo build` is a no-op if it's already
    // built; in CI the lint+test job has built it implicitly.
    ensure_worker_built();
    let worker = locate_worker_bin();
    assert!(
        worker.exists(),
        "expected newt binary somewhere in target/ — checked CARGO_TARGET_DIR={:?}, fell back to {}",
        std::env::var_os("CARGO_TARGET_DIR"),
        worker.display()
    );

    let cases_dir = cases::default_cases_dir();
    let all_cases = cases::load_all(&cases_dir).expect("bundled cases load");
    assert!(!all_cases.is_empty(), "expected at least one bundled case");

    let mut scorecard = Scorecard::new();

    for case in &all_cases {
        // One wiremock per case — keeps the URL stable and the mock
        // tied to the case's mock_response.content.
        let mock = MockServer::start().await;

        // As of the OLLAMA_HOST-verbatim fix, the worker's discover()
        // no longer probes GET /api/tags when OLLAMA_HOST is set — it
        // trusts the env var. So we only need the /api/chat mock here.
        // (The runner config below sets OLLAMA_HOST to the wiremock URL.)
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "model": "mock-llama",
                "message": {
                    "role": "assistant",
                    "content": case.mock_response.content,
                },
                "done": true,
            })))
            .mount(&mock)
            .await;

        let config = RunnerConfig::new(&worker).with_mock_endpoint(mock.uri());

        let outcome = run_case(case, &config)
            .await
            .unwrap_or_else(|e| panic!("[{}] runner failed: {e:#}", case.name));

        let ctx = EvalContext {
            case: outcome.case.clone(),
            workspace: outcome.workspace.clone(),
            baseline: outcome.baseline.clone(),
            reply: outcome.reply.clone(),
        };

        let mut results = Vec::with_capacity(case.evaluators.len());
        for name in &case.evaluators {
            let ev = evaluator_by_name(name)
                .unwrap_or_else(|| panic!("[{}] unknown evaluator '{}'", case.name, name));
            let r = ev.evaluate(&ctx);
            results.push(r);
        }

        let cs = CaseScorecard {
            case_name: case.name.clone(),
            results,
        };
        if !cs.all_passed() {
            eprintln!(
                "[{}] evaluator results:\n{}",
                case.name,
                Scorecard {
                    cases: vec![cs.clone()],
                }
                .render_table()
            );
            eprintln!(
                "[{}] worker captured diff was:\n{}",
                case.name, outcome.reply.diff
            );
        }
        scorecard.push(cs);
    }

    let table = scorecard.render_table();
    println!("\n{table}");
    assert!(
        scorecard.all_passed(),
        "at least one bundled case failed:\n{table}"
    );
}

// ── golden masters over the product boundaries (the refactor net) ──────────
//
// Characterization tests for the kernel-first decomposition: byte-level golden
// masters over the THREE product boundaries that must survive the refactor —
//
//   1. the ACP TaskReply (`newt worker`, driven via `run_case` exactly like the
//      bundled-cases test above — widened, not forked);
//   2. the MCP stdio handshake + tool catalog (`newt mcp`);
//   3. the plain help surface via startup-free `newt help` (#1318) — the byte
//      source of the interactive `/help`. (The full chat-turn surface remains
//      blocked by a real coupling — see the recorded finding below.)
//
// NOT internal seams — a refactor may rearrange everything behind these bytes.
//
// Discipline (this repo's pathology is gates that pass while nothing changed):
//   * a MISSING golden file FAILS the test (it never silently passes);
//   * every master has a NEGATIVE CONTROL — `golden_negative_controls` proves
//     the comparator rejects a perturbed expectation for every stored golden;
//   * masters are captured TWICE per run and must agree post-normalization, so
//     a nondeterministic master cannot be baselined in the first place.
//
// Baseline capture / intentional update:  NEWT_GOLDEN_UPDATE=1 cargo test
//   -p newt-eval --test mock_e2e   (then commit newt-eval/tests/golden/*.golden)
//
// Unix-gated like `stdout_purity.rs`: byte-exact CLI output on Windows adds
// CRLF/path hazards the refactor net doesn't need — gnuc/beaver (the boxes that
// run it) are both unix.
#[cfg(unix)]
mod golden {
    use super::{ensure_worker_built, locate_worker_bin};
    use newt_eval::{cases, run_case, RunnerConfig};
    use serde_json::json;
    use std::path::PathBuf;
    use std::process::Stdio;
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn golden_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("golden")
    }

    /// Compare `actual` against the stored golden `name`. `NEWT_GOLDEN_UPDATE=1`
    /// rewrites the file and passes (the baseline-capture mode). A missing
    /// golden is a FAILURE with capture instructions — never a silent pass.
    fn golden_compare(name: &str, actual: &str) -> Result<(), String> {
        let path = golden_dir().join(format!("{name}.golden"));
        if std::env::var("NEWT_GOLDEN_UPDATE").as_deref() == Ok("1") {
            std::fs::create_dir_all(golden_dir()).map_err(|e| e.to_string())?;
            std::fs::write(&path, actual).map_err(|e| e.to_string())?;
            eprintln!("[golden] UPDATED {}", path.display());
            return Ok(());
        }
        let expected = std::fs::read_to_string(&path).map_err(|_| {
            format!(
                "golden `{name}` missing at {} — capture the baseline with \
                 NEWT_GOLDEN_UPDATE=1 and commit it (a missing master must \
                 never pass)",
                path.display()
            )
        })?;
        if expected != actual {
            return Err(format!(
                "golden `{name}` MISMATCH — the product boundary changed.\n\
                 If intentional, re-baseline with NEWT_GOLDEN_UPDATE=1 and \
                 review the diff in the PR.\n--- expected ---\n{expected}\n\
                 --- actual ---\n{actual}"
            ));
        }
        Ok(())
    }

    /// Replace volatile tokens so the masters are machine- and run-independent:
    /// the workspace version string and any explicitly-passed volatile substrings
    /// (temp paths, model ids). Purely mechanical — no regex, no cleverness.
    fn normalize(text: &str, volatile: &[(&str, &str)]) -> String {
        let mut out = text.replace(env!("CARGO_PKG_VERSION"), "<VER>");
        for (needle, marker) in volatile {
            if !needle.is_empty() {
                out = out.replace(needle, marker);
            }
        }
        out
    }

    /// Boundary 1 — ACP: the T0 TaskReply, normalized. Reuses the SAME
    /// wiremock + `run_case` path as `all_bundled_cases_pass_in_mock_mode`.
    async fn capture_acp_reply() -> String {
        ensure_worker_built();
        let worker = locate_worker_bin();
        let cases_dir = cases::default_cases_dir();
        let all = cases::load_all(&cases_dir).expect("bundled cases load");
        let t0 = all
            .iter()
            .find(|c| c.name == "T0-fix-add")
            .expect("T0-fix-add is bundled");

        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "model": "mock-llama",
                "message": { "role": "assistant", "content": t0.mock_response.content },
                "done": true,
            })))
            .mount(&mock)
            .await;

        let config = RunnerConfig::new(&worker).with_mock_endpoint(mock.uri());
        let outcome = run_case(t0, &config).await.expect("T0 runs");
        let ws = outcome.workspace.display().to_string();
        let bl = outcome.baseline.display().to_string();
        // The boundary artifact: the fields a downstream consumer contracts on.
        let r = &outcome.reply;
        let raw = format!(
            "model_id: {}\nempty_diff: {}\ndiff_applied: {}\n--- diff ---\n{}",
            r.model_id, r.empty_diff, r.diff_applied, r.diff
        );
        normalize(&raw, &[(&ws, "<WS>"), (&bl, "<BASELINE>")])
    }

    /// Boundary 2 — MCP stdio: initialize + tools/list frames, normalized.
    /// HOME-isolated so a developer's `~/.newt` MCP config can't leak into the
    /// golden (the master must be machine-independent).
    async fn capture_mcp_handshake() -> String {
        ensure_worker_built();
        let worker = locate_worker_bin();
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".newt")).expect("mk .newt");
        std::fs::write(home.path().join(".newt/config.toml"), "").expect("seed config");

        let mut cmd = Command::new(&worker);
        cmd.arg("mcp")
            .env("OLLAMA_HOST", "http://127.0.0.1:1")
            .env("HOME", home.path())
            .env("TERM", "dumb")
            .env_remove("NEWT_CONFIG")
            .env_remove("NEWT_CONFIG_DIR")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().expect("spawn newt mcp");
        {
            let mut stdin = child.stdin.take().expect("stdin");
            let frames = [
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
                json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
            ];
            for f in &frames {
                let line = format!("{}\n", serde_json::to_string(f).unwrap());
                stdin.write_all(line.as_bytes()).await.expect("write frame");
            }
            // drop closes stdin → server loop exits.
        }
        let output = tokio::time::timeout(Duration::from_secs(20), child.wait_with_output())
            .await
            .expect("newt mcp timed out")
            .expect("collect output");
        let stdout = String::from_utf8_lossy(&output.stdout);
        // Re-serialize each frame with sorted keys (serde_json maps preserve
        // order; Value round-trip is stable) so the golden is layout-stable.
        let mut lines = Vec::new();
        for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
            let v: serde_json::Value = serde_json::from_str(line).expect("stdout frame is JSON");
            lines.push(serde_json::to_string_pretty(&v).unwrap());
        }
        normalize(&lines.join("\n"), &[])
    }

    // Boundary 3 — the piped plain chat surface: BLOCKED BY COUPLING (recorded
    // finding for the refactor review, 2026-07-20 baseline attempt on gnuc).
    //
    // A hermetic capture needs the chat backend pinned to a mock. It cannot be:
    //   * `OLLAMA_HOST` — honored by the worker (verbatim contract), NOT by the
    //     interactive chat path;
    //   * `$NEWT_CONFIG` — in `candidate_paths()` but observed INERT end-to-end
    //     (`newt config` under it shows pure defaults) — bug, filed on the board;
    //   * `--config` — routes through `Config::load` for subcommands (`newt
    //     config` shows the pinned backends) but is not plumbed into the chat
    //     REPL's backend choice;
    //   * cwd `./newt.toml` / project `.newt/config.toml` — loaded, but AMBIENT
    //     (#1301 trust boundary): an untrusted repo config must not redirect
    //     inference to an attacker endpoint, so its backends don't drive the
    //     chat. Correct security posture; fatal for a mock pin.
    //   Net effect on a box with a live ollama on :11434 the chat silently runs
    //   a REAL model (observed: llama3.1:8b answering with live tool calls) —
    //   nondeterministic and non-hermetic by construction.
    //
    // This is exactly the coupling the kernel-first refactor should dissolve
    // (backend choice as an injectable seam). Until then the plain surface gets
    // its master from the startup-free `newt help` (#1318, now merged) — the
    // byte source of the interactive `/help`, no backend, fully deterministic:
    // see `golden_help_surface_boundary` below.

    /// Boundary 3 — the plain help surface via startup-free `newt help` /
    /// `newt help dgx` (#1318): the exact bytes the interactive `/help` and
    /// `/dgx help` print (both route through `newt_tui::render_help`; piped
    /// stdout ⇒ `color_supported()` is false ⇒ plain bytes). HOME- and
    /// cwd-isolated so no operator config/skills or `.newt` walk-up can leak
    /// into the master.
    async fn capture_help_surface() -> String {
        ensure_worker_built();
        let worker = locate_worker_bin();
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".newt")).expect("mk .newt");
        std::fs::write(home.path().join(".newt/config.toml"), "").expect("seed config");
        let workdir = tempfile::tempdir().expect("workdir");

        let mut out = String::new();
        for topic in [None, Some("dgx")] {
            let mut cmd = Command::new(&worker);
            cmd.arg("help");
            if let Some(t) = topic {
                cmd.arg(t);
            }
            cmd.current_dir(workdir.path())
                .env("HOME", home.path())
                .env("TERM", "dumb")
                .env("NO_COLOR", "1")
                .env_remove("NEWT_CONFIG")
                .env_remove("NEWT_CONFIG_DIR")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            let output = tokio::time::timeout(Duration::from_secs(20), async {
                cmd.spawn()
                    .expect("spawn newt help")
                    .wait_with_output()
                    .await
            })
            .await
            .expect("newt help timed out")
            .expect("collect output");
            out.push_str(&format!("=== newt help {} ===\n", topic.unwrap_or("(top)")));
            out.push_str(&String::from_utf8_lossy(&output.stdout));
        }
        normalize(&out, &[])
    }

    // ── the masters ─────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn golden_acp_reply_boundary() {
        // Double-capture: a master that can't agree with itself is not a
        // baseline, it's a flake generator — refuse to compare it at all.
        let a = capture_acp_reply().await;
        let b = capture_acp_reply().await;
        assert_eq!(a, b, "ACP capture is nondeterministic post-normalization");
        golden_compare("acp-t0-reply", &a).unwrap();
    }

    #[tokio::test]
    async fn golden_mcp_handshake_boundary() {
        let a = capture_mcp_handshake().await;
        let b = capture_mcp_handshake().await;
        assert_eq!(a, b, "MCP capture is nondeterministic post-normalization");
        golden_compare("mcp-initialize-tools", &a).unwrap();
    }

    #[tokio::test]
    async fn golden_help_surface_boundary() {
        let a = capture_help_surface().await;
        let b = capture_help_surface().await;
        assert_eq!(a, b, "help capture is nondeterministic post-normalization");
        golden_compare("help-surface", &a).unwrap();
    }

    /// The negative control the card demands: EVERY stored golden must be shown
    /// to fail against a perturbed expectation. A comparator that can't reject
    /// a mutation is the "gate that passes while nothing changed" pathology.
    #[test]
    fn golden_negative_controls() {
        // In baseline-capture mode the comparator WRITES what it's given — a
        // perturbed probe would corrupt the goldens. The control only means
        // anything in compare mode anyway.
        if std::env::var("NEWT_GOLDEN_UPDATE").as_deref() == Ok("1") {
            eprintln!("[golden] negative controls skipped in update mode");
            return;
        }
        let dir = golden_dir();
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.path().extension().is_some_and(|x| x == "golden"))
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            !entries.is_empty(),
            "no goldens found in {} — capture the baseline first \
             (NEWT_GOLDEN_UPDATE=1); an empty net must not pass",
            dir.display()
        );
        for e in entries {
            let name = e.path().file_stem().unwrap().to_string_lossy().into_owned();
            let stored = std::fs::read_to_string(e.path()).expect("read golden");
            let perturbed = format!("{stored}\nPERTURBED-LINE-MUST-FAIL");
            assert!(
                golden_compare(&name, &perturbed).is_err(),
                "golden `{name}` ACCEPTED a perturbed expectation — the master \
                 detects nothing"
            );
            // And the unperturbed content still round-trips (the comparator is
            // strict equality, not vibes).
            assert!(
                golden_compare(&name, &stored).is_ok(),
                "golden `{name}` rejected its own stored bytes"
            );
        }
    }
}

// ── helpers ─────────────────────────────────────────────────────────

/// Best-effort build of `newt` so the test doesn't have to assume the
/// caller already built it. Errors are non-fatal — we'll surface a
/// clearer "binary not found" assertion below.
fn ensure_worker_built() {
    let _ = std::process::Command::new(env!("CARGO"))
        .args(["build", "--bin", "newt"])
        .output();
}

fn worker_exe_name() -> String {
    format!("newt{}", std::env::consts::EXE_SUFFIX)
}

/// Locate the `newt` binary in the workspace's `target/` dir.
///
/// Searches cargo target directories in priority order:
/// 1. `$CARGO_TARGET_DIR` (set by `cargo llvm-cov` to `target/llvm-cov-target`)
/// 2. the cargo-resolved target dir (honors `~/.cargo/config.toml`, newt-agent#64)
/// 3. `<manifest>/../target/{debug,release}/`
fn locate_worker_bin() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest.parent().expect("manifest dir has parent");

    // cargo llvm-cov sets CARGO_TARGET_DIR — honor it first.
    let mut target_dirs: Vec<PathBuf> = Vec::new();
    if let Some(tdir) = std::env::var_os("CARGO_TARGET_DIR") {
        target_dirs.push(PathBuf::from(tdir));
    }
    // Honor `[build] target-dir` from cargo config (newt-agent#64) — a plain
    // `workspace_root/target` guess misses it (and can resolve to a stale path).
    if let Ok(meta) = cargo_metadata::MetadataCommand::new().exec() {
        target_dirs.push(meta.target_directory.into_std_path_buf());
    }
    target_dirs.push(workspace_root.join("target"));
    target_dirs.push(workspace_root.join("target").join("llvm-cov-target"));

    for tdir in &target_dirs {
        for profile in ["debug", "release"] {
            let candidate = tdir.join(profile).join(worker_exe_name());
            if candidate.exists() {
                return candidate;
            }
        }
    }
    // Fallback to the cargo-resolved debug path (newt-agent#64), else the
    // conventional path; the caller's assertion surfaces a missing path cleanly.
    cargo_metadata::MetadataCommand::new()
        .exec()
        .ok()
        .map(|m| m.target_directory.into_std_path_buf())
        .unwrap_or_else(|| workspace_root.join("target"))
        .join("debug")
        .join(worker_exe_name())
}
