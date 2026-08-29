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
    // The worker is BUILT (fatally) and IDENTIFIED by cargo — see
    // `worker_under_test` for why this is no longer a hunt through target/.
    let worker = &worker_under_test().path;

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

        let config = RunnerConfig::new(worker).with_mock_endpoint(mock.uri());

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
            );
            eprintln!(
                "[{}] worker captured diff was:\n{}",
                case.name, outcome.reply.diff
            );
        }
        scorecard.push(cs);
    }

    let table = scorecard.to_string();
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
// CRLF/path hazards the refactor net doesn't need — gpu-runner (the box that
// run it) are both unix.
#[cfg(unix)]
mod golden {
    use super::worker_under_test;
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
        let worker = &worker_under_test().path;
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

        let config = RunnerConfig::new(worker).with_mock_endpoint(mock.uri());
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
        let worker = &worker_under_test().path;
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".newt")).expect("mk .newt");
        std::fs::write(home.path().join(".newt/config.toml"), "").expect("seed config");

        let mut cmd = Command::new(worker);
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
    // finding for the refactor review, 2026-07-20 baseline attempt on gpu-runner).
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
        let worker = &worker_under_test().path;
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".newt")).expect("mk .newt");
        std::fs::write(home.path().join(".newt/config.toml"), "").expect("seed config");
        let workdir = tempfile::tempdir().expect("workdir");

        let mut out = String::new();
        for topic in [None, Some("dgx")] {
            let mut cmd = Command::new(worker);
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

// ── the binary under test ───────────────────────────────────────────
//
// #1677 — the stale-binary hazard, and why this is no longer a search.
//
// Until #1677 this file did two separable things badly:
//
//   1. `ensure_worker_built()` ran `cargo build --bin newt` and threw the
//      result away ("errors are non-fatal"). Run from the newt-eval package
//      directory — which is where cargo puts a test process's cwd — that
//      invocation does not build anything at all:
//
//          error: no bin target named `newt` in default-run packages
//          help: available bin in `newt-agent` package: newt
//
//      i.e. the build was a silent no-op on EVERY run, for every developer
//      and every CI job.
//
//   2. `locate_worker_bin()` then swept four candidate target directories ×
//      {debug, release} for the FIRST file named `newt` and graded that.
//
// Composed, the helper could not fail loudly and could not miss: a months-old
// `target/release/newt` from an unrelated branch satisfies step 2 perfectly. A
// green E2E over a binary nobody can identify is worse than a red one — it is
// the "gate that passes while nothing changed" pathology this file's golden
// section already names, arriving through the back door.
//
// The replacement removes the search entirely. Cargo is asked to build the
// binary and to SAY WHERE IT PUT IT (`--message-format=json` → the
// `compiler-artifact` message's `executable`), so the path is an output of the
// build rather than a guess about it; a failed build is fatal; and the SHA-256
// of the exact bytes is printed once as run evidence and quoted in failures.
// This also makes `$CARGO_TARGET_DIR` / `[build] target-dir` / `cargo llvm-cov`
// handling automatic — cargo reports its own layout (the newt-agent#64 concern
// the old sweep was hand-coding).

/// The built worker: absolute path + SHA-256 of the bytes under test.
#[derive(Debug, Clone)]
struct WorkerBin {
    path: PathBuf,
    sha256: String,
}

/// Build `newt` with `args` and return the artifact cargo reports.
///
/// `Err` on: cargo failing to start, a non-zero build, or a build that
/// produced no `newt` executable. Fallible (rather than panicking inline) so
/// the regression test can drive the OLD argument list through this exact code
/// path and see it rejected.
fn build_worker(args: &[&str]) -> Result<WorkerBin, String> {
    let out = std::process::Command::new(env!("CARGO"))
        .arg("build")
        .args(args)
        .arg("--message-format=json-render-diagnostics")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .map_err(|e| format!("could not run cargo: {e}"))?;
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if !out.status.success() {
        return Err(format!(
            "`cargo build {}` FAILED ({}). The worker E2E must never fall back \
             to an older binary — fix the build.\n{stderr}",
            args.join(" "),
            out.status
        ));
    }
    // Last artifact wins: a fresh (cached) unit still reports its executable,
    // so this works whether or not the build actually recompiled anything.
    let mut exe: Option<PathBuf> = None;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("reason").and_then(|r| r.as_str()) != Some("compiler-artifact") {
            continue;
        }
        if v.pointer("/target/name").and_then(|n| n.as_str()) != Some("newt") {
            continue;
        }
        if let Some(p) = v.get("executable").and_then(|e| e.as_str()) {
            exe = Some(PathBuf::from(p));
        }
    }
    let path = exe.ok_or_else(|| {
        format!(
            "`cargo build {}` reported no `newt` executable — it built something \
             else (or nothing). Refusing to guess a binary from target/.\n{stderr}",
            args.join(" ")
        )
    })?;
    let bytes = std::fs::read(&path)
        .map_err(|e| format!("cargo named {} but it is unreadable: {e}", path.display()))?;
    let sha256 = format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(&bytes));
    Ok(WorkerBin { path, sha256 })
}

/// The one binary this whole suite grades — built once per test process, with
/// its identity printed so any run's evidence names the exact bytes.
fn worker_under_test() -> &'static WorkerBin {
    static WORKER: std::sync::OnceLock<WorkerBin> = std::sync::OnceLock::new();
    WORKER.get_or_init(|| {
        // `-p newt-agent` is load-bearing: `--bin newt` alone resolves against
        // the *cwd's* package (newt-eval), which has no such target.
        let w = build_worker(&["-p", "newt-agent", "--bin", "newt"])
            .unwrap_or_else(|e| panic!("worker build: {e}"));
        eprintln!(
            "[worker] under test: {} sha256={}",
            w.path.display(),
            w.sha256
        );
        w
    })
}

/// Regression (#1677): the worker build is FATAL and the binary under test is
/// cargo's own artifact, not a filesystem sweep.
///
/// Would have failed before the fix, in both halves:
///   * `ensure_worker_built()` swallowed the error from an invocation that
///     never built anything — here the same code path must return `Err`;
///   * `locate_worker_bin()` returned the first `newt`-shaped file it found —
///     here the path must be the executable cargo reported, and must hash.
#[test]
fn worker_build_failure_is_fatal_and_the_binary_is_identified() {
    let good = worker_under_test();
    assert!(
        good.path.is_file(),
        "cargo named a worker that is not a file: {}",
        good.path.display()
    );
    assert_eq!(good.sha256.len(), 64, "sha256 hex digest");

    // The exact invocation the old helper used, driven through the new
    // fallible path: it must be an ERROR, never a silent no-op that leaves a
    // stale binary standing in for a fresh one. (If a future cargo resolves
    // `--bin newt` workspace-wide from a member dir, this fails loudly and the
    // note above should be revised — that is the intent, not a flake.)
    let stale_lane = build_worker(&["--bin", "newt"]);
    assert!(
        stale_lane.is_err(),
        "the pre-#1677 build invocation succeeded from {}; the swallow-and-\
         fall-back-to-target/ hazard needs re-analysis",
        env!("CARGO_MANIFEST_DIR")
    );
}
