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
        "expected newt binary at {} — `cargo build --bin newt` must have run",
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

        // The worker's discover() probes GET /api/tags before adopting
        // an endpoint; without this stub the probe would fail, discover
        // would fall through to its default endpoint list, and the
        // worker would (silently!) hit a real Ollama. Ask me how I know.
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "models": [{"name": "mock-llama"}],
            })))
            .mount(&mock)
            .await;

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

// ── helpers ─────────────────────────────────────────────────────────

/// Best-effort build of `newt` so the test doesn't have to assume the
/// caller already built it. Errors are non-fatal — we'll surface a
/// clearer "binary not found" assertion below.
fn ensure_worker_built() {
    let _ = std::process::Command::new(env!("CARGO"))
        .args(["build", "--bin", "newt"])
        .output();
}

/// Locate the `newt` binary in the workspace's `target/` dir.
///
/// During `cargo test -p newt-eval`, integration tests run with
/// `target/debug/deps/` on PATH and the binaries are in
/// `target/debug/`. The shared workspace target is at
/// `<manifest>/../target/<profile>/newt`.
fn locate_worker_bin() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_target = manifest
        .parent()
        .expect("manifest dir has parent")
        .join("target");
    for profile in ["debug", "release"] {
        let candidate = workspace_target.join(profile).join("newt");
        if candidate.exists() {
            return candidate;
        }
    }
    // Fallback to the debug build path even if it doesn't exist yet —
    // the caller's assertion will report the missing path cleanly.
    workspace_target.join("debug").join("newt")
}
