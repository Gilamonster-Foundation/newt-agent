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
/// Searches common cargo target directories in priority order:
/// 1. `$CARGO_TARGET_DIR` — explicit env var always wins (set by
///    `cargo llvm-cov` to `target/llvm-cov-target`, and by tooling
///    that wants to redirect builds).
/// 2. `cargo metadata` — picks up `~/.cargo/config.toml`'s
///    `[build] target-dir`. Best-effort: if the call fails (e.g.
///    offline registry, partially-built sysroot) we just skip it
///    and fall back to the conventional paths below.
/// 3. `<manifest>/../target` — final fallback for environments where
///    `cargo metadata` itself fails.
/// 4. `<manifest>/../target/llvm-cov-target` — legacy llvm-cov path
///    kept for parity with older CI invocations that didn't set
///    `CARGO_TARGET_DIR`.
///
/// Each directory is probed for `release/newt` first, then `debug/newt`.
///
/// If nothing is found, panic with the full list of directories
/// searched — same UX as the runner-side #40/#43 fix and the
/// stdout_purity locator.
fn locate_worker_bin() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest.parent().expect("manifest dir has parent");

    let mut target_dirs: Vec<PathBuf> = Vec::new();

    // 1. Explicit env var — always wins.
    if let Some(tdir) = std::env::var_os("CARGO_TARGET_DIR") {
        target_dirs.push(PathBuf::from(tdir));
    }

    // 2. `cargo metadata` — picks up `~/.cargo/config.toml`'s
    //    `[build] target-dir`. Best-effort: if the call fails (e.g.
    //    offline registry, partially-built sysroot) we just skip it
    //    and fall back to the conventional paths below.
    if let Ok(meta) = cargo_metadata::MetadataCommand::new()
        .manifest_path(workspace_root.join("Cargo.toml"))
        .no_deps()
        .exec()
    {
        target_dirs.push(PathBuf::from(meta.target_directory.as_std_path()));
    }

    // 3 + 4. Conventional fallbacks.
    target_dirs.push(workspace_root.join("target"));
    target_dirs.push(workspace_root.join("target").join("llvm-cov-target"));

    for tdir in &target_dirs {
        for profile in ["release", "debug"] {
            let candidate = tdir.join(profile).join("newt");
            if candidate.exists() {
                return candidate;
            }
        }
    }

    // Nothing found. Surface every path we tried — the runner-side
    // #40/#43 fix taught us that "binary not found" is useless without
    // the search list.
    let searched: Vec<String> = target_dirs
        .iter()
        .flat_map(|d| {
            ["release", "debug"]
                .iter()
                .map(move |p| d.join(p).join("newt").display().to_string())
        })
        .collect();
    panic!(
        "newt binary not found. Searched:\n  - {}\n\
         Hint: run `cargo build -p newt-agent` (or `--release`) in the workspace root.",
        searched.join("\n  - "),
    );
}
