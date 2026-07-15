//! Integration tests for `newt_tui::probe` — the HTTP-facing fns
//! (`fetch_ollama_models`, `fetch_context_window`, `ensure_context_window`,
//! `probe_tool_conformance`) against a wiremock Ollama, plus the capability
//! cache persistence (`load_cache` / `save_cache`) redirected to a tempdir
//! via `$HOME` so the real `~/.newt` is never touched.
//!
//! The sync fetch fns use `tokio::task::block_in_place`, which requires a
//! multi-threaded runtime — hence `flavor = "multi_thread"` throughout.

use std::sync::Mutex;

use newt_core::TokenEstimation;
use newt_tui::probe::{
    ensure_context_window, fetch_context_window, fetch_ollama_models, load_cache,
    probe_input_boundary, probe_thinking, probe_tool_conformance,
    probe_tool_conformance_calibrated, refresh_context_window, save_cache, CapabilityCache,
    CapabilityEntry, ToolConformance, TuneConfidence,
};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Default estimation (chars_per_token = 4) for the probe integration tests.
const EST: TokenEstimation = TokenEstimation { chars_per_token: 4 };

/// An endpoint nothing listens on — connection refused, fast failure.
const DEAD_ENDPOINT: &str = "http://127.0.0.1:1";

// ---------------------------------------------------------------------------
// fetch_ollama_models  (/api/tags)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_models_parses_names_and_param_sizes() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [
                {"name": "llama3:8b", "details": {"parameter_size": "8.0B"}},
                // No details object — param_size must default to "".
                {"name": "tiny:latest"},
                // No name — entry must be skipped entirely.
                {"details": {"parameter_size": "1B"}},
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let models = fetch_ollama_models(&server.uri()).expect("fetch should succeed");
    assert_eq!(models.len(), 2, "nameless entry must be filtered out");
    assert_eq!(models[0].name, "llama3:8b");
    assert_eq!(models[0].param_size, "8.0B");
    assert_eq!(models[1].name, "tiny:latest");
    assert_eq!(models[1].param_size, "");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_models_trims_trailing_slash_in_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags")) // would not match "//api/tags"
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [{"name": "m1", "details": {"parameter_size": "7B"}}]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let endpoint = format!("{}/", server.uri());
    let models = fetch_ollama_models(&endpoint).expect("trailing slash must be trimmed");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].name, "m1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_models_returns_empty_when_models_key_missing() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
        .mount(&server)
        .await;

    let models = fetch_ollama_models(&server.uri()).expect("missing key is not an error");
    assert!(models.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_models_errors_on_http_failure() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let err = fetch_ollama_models(&server.uri()).expect_err("HTTP 500 must be an error");
    assert!(
        err.to_string().contains("HTTP 500"),
        "error should carry the status, got: {err}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_models_errors_when_unreachable() {
    assert!(fetch_ollama_models(DEAD_ENDPOINT).is_err());
}

// ---------------------------------------------------------------------------
// fetch_context_window  (/api/show)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_context_window_reads_arch_limit() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/show"))
        .and(body_partial_json(serde_json::json!({"name": "llama3:8b"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "model_info": {"llama.context_length": 32768}
        })))
        .expect(1)
        .mount(&server)
        .await;

    assert_eq!(
        fetch_context_window(&server.uri(), "llama3:8b", newt_core::BackendKind::Ollama),
        Some(32768)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_context_window_takes_smaller_modelfile_num_ctx() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/show"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "model_info": {"llama.context_length": 131072},
            "parameters": "num_ctx 8192\ntemperature 0.7"
        })))
        .mount(&server)
        .await;

    assert_eq!(
        fetch_context_window(&server.uri(), "m", newt_core::BackendKind::Ollama),
        Some(8192)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_context_window_none_when_response_lacks_fields() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/show"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "model_info": {"general.architecture": "llama"}
        })))
        .mount(&server)
        .await;

    assert_eq!(
        fetch_context_window(&server.uri(), "m", newt_core::BackendKind::Ollama),
        None
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_context_window_none_when_unreachable() {
    assert_eq!(
        fetch_context_window(DEAD_ENDPOINT, "m", newt_core::BackendKind::Ollama),
        None
    );
}

// ---------------------------------------------------------------------------
// ensure_context_window
// ---------------------------------------------------------------------------

fn entry_with(conformance: ToolConformance) -> CapabilityEntry {
    CapabilityEntry {
        conformance,
        tested_date: "2026-06-07".to_string(),
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ensure_context_window_skips_when_already_known() {
    let mut e = entry_with(ToolConformance::Native);
    e.context_window = Some(4096);
    // DEAD_ENDPOINT proves no HTTP call is attempted: if it were, the fetch
    // would fail and... actually it would return false either way, so the
    // real assertion is that the entry is untouched and the call is cheap.
    assert!(!ensure_context_window(
        &mut e,
        DEAD_ENDPOINT,
        "m",
        false,
        newt_core::BackendKind::Ollama
    ));
    assert_eq!(e.context_window, Some(4096));
    assert_eq!(e.safe_context, None, "safe_context must not be invented");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ensure_context_window_bootstraps_safe_context_at_80_percent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/show"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "model_info": {"llama.context_length": 32768}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let mut e = entry_with(ToolConformance::Native);
    assert!(ensure_context_window(
        &mut e,
        &server.uri(),
        "m",
        false,
        newt_core::BackendKind::Ollama
    ));
    assert_eq!(e.context_window, Some(32768));
    assert_eq!(e.safe_context, Some(32768 * 80 / 100)); // 26214
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ensure_context_window_preserves_existing_safe_context() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/show"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "model_info": {"llama.context_length": 32768}
        })))
        .mount(&server)
        .await;

    let mut e = entry_with(ToolConformance::Native);
    e.safe_context = Some(1234); // e.g. tuned down after an overflow
    assert!(ensure_context_window(
        &mut e,
        &server.uri(),
        "m",
        false,
        newt_core::BackendKind::Ollama
    ));
    assert_eq!(e.context_window, Some(32768));
    assert_eq!(e.safe_context, Some(1234), "tuned value must survive");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ensure_context_window_false_when_fetch_fails() {
    let mut e = entry_with(ToolConformance::Native);
    assert!(!ensure_context_window(
        &mut e,
        DEAD_ENDPOINT,
        "m",
        false,
        newt_core::BackendKind::Ollama
    ));
    assert_eq!(e.context_window, None);
    assert_eq!(e.safe_context, None);
}

// ---------------------------------------------------------------------------
// refresh_context_window  (Step 20.2 §4.2 — always re-queries /api/show)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_context_window_updates_changed_window_and_bootstraps_when_unset() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/show"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "model_info": {"llama.context_length": 16384}
        })))
        // Unlike ensure_*, refresh always calls /api/show even when the window
        // is already known — so it sees the re-pulled Modelfile change.
        .expect(1)
        .mount(&server)
        .await;

    let mut e = entry_with(ToolConformance::Native);
    e.context_window = Some(32768); // stale, larger
    e.safe_context = None; // unset → eligible for re-bootstrap
    assert!(
        refresh_context_window(
            &mut e,
            &server.uri(),
            "m",
            false,
            newt_core::BackendKind::Ollama
        ),
        "a changed window must report dirty"
    );
    assert_eq!(e.context_window, Some(16384), "window updated to fetched");
    assert_eq!(
        e.safe_context,
        Some(16384 * 80 / 100),
        "safe_context bootstrapped at 80% because it was unset"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_context_window_never_raises_existing_safe_context() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/show"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "model_info": {"llama.context_length": 32768}
        })))
        .mount(&server)
        .await;

    let mut e = entry_with(ToolConformance::Native);
    e.context_window = Some(32768); // unchanged
    e.safe_context = Some(4096); // tuned down after an overflow — must survive
                                 // Window did not change → not dirty; safe_context never auto-raised.
    assert!(
        !refresh_context_window(
            &mut e,
            &server.uri(),
            "m",
            false,
            newt_core::BackendKind::Ollama
        ),
        "unchanged window is not dirty"
    );
    assert_eq!(e.context_window, Some(32768));
    assert_eq!(
        e.safe_context,
        Some(4096),
        "VRAM rule: a known safe_context is never auto-raised"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ensure_context_window_trust_declared_raises_reined_safe_context() {
    // The Bug3 regression (#382/#383): a model capped to ~6k by a past overflow
    // is un-stuck to ~80 % of its declared window in the default (trust-declared)
    // mode — using the cached window, no /api/show re-query (DEAD_ENDPOINT proves
    // no fetch is attempted).
    let mut e = entry_with(ToolConformance::Native);
    e.context_window = Some(1_048_576); // declared 1M (e.g. nemotron-3-nano:30b)
    e.safe_context = Some(6_000); // reined down by a past overflow and stuck
    assert!(ensure_context_window(
        &mut e,
        DEAD_ENDPOINT,
        "m",
        true,
        newt_core::BackendKind::Ollama
    ));
    assert_eq!(e.context_window, Some(1_048_576));
    assert_eq!(
        e.safe_context,
        Some(1_048_576 * 80 / 100),
        "trust-declared raises safe_context to 80 % of the declared window"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_context_window_trust_declared_raises_safe_context() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/show"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "model_info": {"llama.context_length": 32768}
        })))
        .mount(&server)
        .await;

    let mut e = entry_with(ToolConformance::Native);
    e.context_window = Some(32768); // unchanged
    e.safe_context = Some(4096); // reined down — trust-declared must raise it
    assert!(
        refresh_context_window(
            &mut e,
            &server.uri(),
            "m",
            true,
            newt_core::BackendKind::Ollama
        ),
        "trust-declared refresh raises safe_context → reports dirty"
    );
    assert_eq!(
        e.safe_context,
        Some(32768 * 80 / 100),
        "trust-declared raises safe_context to 80 % of declared"
    );
}

// ---------------------------------------------------------------------------
// probe_thinking  (Step 20.2 §4.3/§4.4 — quirk + calibration sample)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_thinking_detects_thinking_only_and_returns_calibration() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .and(body_partial_json(serde_json::json!({"stream": false})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message": {"role": "assistant", "content": "", "thinking": "I should say ok"},
            "prompt_eval_count": 19
        })))
        .expect(1)
        .mount(&server)
        .await;

    let pt = probe_thinking(&server.uri(), "thinker", EST).expect("probe should succeed");
    assert!(
        pt.emits_thinking,
        "empty content + thinking → quirk detected"
    );
    let (observed, estimated) = pt.calibration.expect("prompt_eval_count present");
    assert_eq!(observed, 19, "observed real prompt tokens echoed");
    assert!(estimated > 0, "a chars/4 estimate of the request body");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_thinking_clean_content_leaves_quirk_false() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message": {"role": "assistant", "content": "ok"}
            // No prompt_eval_count → calibration absent.
        })))
        .mount(&server)
        .await;

    let pt = probe_thinking(&server.uri(), "clean", EST).unwrap();
    assert!(!pt.emits_thinking);
    assert!(pt.calibration.is_none(), "no prompt_eval_count → no sample");
}

// ---------------------------------------------------------------------------
// probe_input_boundary  (Step 20.2 §4.5 — empirical binary search)
// ---------------------------------------------------------------------------

/// A mock `/api/chat` that accepts a prompt (echoing the sent num_ctx as
/// `prompt_eval_count`) while `num_ctx <= threshold`, and returns an Ollama
/// context-window 400 above it. This is the boundary the binary search must
/// converge on.
struct BoundaryResponder {
    threshold: u32,
    limit: u32,
}

impl wiremock::Respond for BoundaryResponder {
    fn respond(&self, req: &wiremock::Request) -> ResponseTemplate {
        let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap_or_default();
        let num_ctx = body["options"]["num_ctx"].as_u64().unwrap_or(0) as u32;
        if num_ctx <= self.threshold {
            // Accept: report prompt_eval_count = num_ctx - reply_margin so it
            // lands ≥ 90% of the candidate N the search sized the prompt for.
            let prompt_eval = num_ctx.saturating_sub(256);
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"role": "assistant", "content": "ok"},
                "prompt_eval_count": prompt_eval,
                "eval_count": 3
            }))
        } else {
            ResponseTemplate::new(400).set_body_string(format!(
                "litellm.ContextWindowExceededError: prompt is too long: \
                 {num_ctx} tokens > {} maximum",
                self.limit
            ))
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_input_boundary_converges_near_threshold_and_marks_high() {
    let server = MockServer::start().await;
    // Accept up to num_ctx 24_000; 400 above with a 24_000 hard limit.
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(BoundaryResponder {
            threshold: 24_000,
            limit: 24_000,
        })
        .mount(&server)
        .await;

    let mut e = entry_with(ToolConformance::Native);
    e.context_window = Some(32_768); // declared — caps the search (VRAM)
    e.safe_context = Some(8_192); // low bound start

    let outcome = probe_input_boundary(&server.uri(), "m", &mut e, |_s| {}, "2026-06-13", EST)
        .expect("boundary search runs");

    // Highest accepted should be just under the threshold (within the
    // reply_margin + tolerance), and recorded as max_ok_input. The lower
    // bound is tightened to 22_000: the very first binary-search accept is
    // 20_480, so a stop-after-first-accept or records-the-high-bound bug
    // would still satisfy a 20_000 floor. Requiring ≥ 22_000 forces the
    // search to actually keep climbing toward the boundary.
    let max_ok = e.max_ok_input.expect("an acceptance was recorded");
    assert!(
        (22_000..=24_000).contains(&max_ok),
        "max_ok_input {max_ok} should converge near the 24k boundary, \
         not stall at the first accept"
    );
    assert_eq!(outcome.highest_accepted, Some(max_ok));
    assert_eq!(e.tune_confidence, TuneConfidence::High, "High on success");
    assert_eq!(e.tune_date.as_deref(), Some("2026-06-13"), "date stamped");
    assert!(outcome.steps >= 1 && outcome.steps <= 12, "bounded steps");
    // Convergence proof: the loop terminated because the bracket closed to
    // within tolerance (max(1024, 5% of high)), not because it gave up early.
    let (low, high) = outcome.final_bounds;
    let tolerance = 1_024u32.max(high / 20);
    assert!(
        high.saturating_sub(low) <= tolerance,
        "final bracket [{low}, {high}] must close to within tolerance \
         {tolerance} — proves convergence, not an early stop"
    );
    assert!(
        low >= 22_000 && high <= 24_000,
        "both bounds must hug the 24k boundary (low {low}, high {high})"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_input_boundary_doubles_to_bracket_when_window_unknown() {
    // §4.5 "If the window is unknown, probe upward by doubling from the low
    // bound until the first rejection, then binary-search the bracket." With
    // context_window None there is no declared cap, so `high` must be found
    // by the doubling phase — a path neither other test exercises (both pin a
    // Some(32768) window that short-circuits straight to binary search).
    let server = MockServer::start().await;
    // Accept up to num_ctx 40_000 (several doublings above the 4_096 floor),
    // then a hard 400 with a 40_000 limit. The doubling phase must climb
    // 8_192 → 16_384 → 32_768 (all accepted) before the 65_536 candidate's
    // num_ctx overshoots and brackets `high` at the reported limit.
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(BoundaryResponder {
            threshold: 40_000,
            limit: 40_000,
        })
        .mount(&server)
        .await;

    let mut e = entry_with(ToolConformance::Native);
    e.context_window = None; // unknown → forces the doubling-bracket path
    e.safe_context = Some(4_096); // low-bound start

    let outcome = probe_input_boundary(&server.uri(), "m", &mut e, |_s| {}, "2026-06-13", EST)
        .expect("boundary search runs without a declared window");

    // It must have climbed well past the first doubling candidate (8_192) by
    // doubling, then converged near the 40k boundary via binary search — not
    // stalled at the bracket-opening accept.
    let max_ok = e.max_ok_input.expect("an acceptance was recorded");
    assert!(
        (36_000..=40_000).contains(&max_ok),
        "max_ok_input {max_ok} should converge near the 40k boundary after \
         doubling past the floor"
    );
    assert_eq!(outcome.highest_accepted, Some(max_ok));
    assert_eq!(e.tune_confidence, TuneConfidence::High, "High on success");
    assert_eq!(e.tune_date.as_deref(), Some("2026-06-13"), "date stamped");
    // The cw-400 hit during doubling reins, but a later acceptance must lift
    // max_ok_input back above the 80%-of-limit (32_000) cw-400 cap.
    assert!(
        max_ok > 40_000 * 80 / 100,
        "a post-400 acceptance must override the cw-400 cap of 32_000"
    );
    // Convergence: bracket closed to within tolerance, bounds hug 40k.
    let (low, high) = outcome.final_bounds;
    let tolerance = 1_024u32.max(high / 20);
    assert!(
        high.saturating_sub(low) <= tolerance,
        "final bracket [{low}, {high}] must close within tolerance {tolerance}"
    );
    assert!(low >= 36_000 && high <= 40_000, "bounds hug 40k boundary");
    assert!(
        outcome.steps >= 4,
        "must take ≥ doublings + a binary step or two"
    );
    assert!(outcome.steps <= 12, "step-capped");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_input_boundary_truncation_lowers_high_bound() {
    // Always 200 but report a prompt_eval_count far below the sent estimate
    // (silent head-drop). Every probe classifies Truncated → high collapses
    // toward low and NO acceptance is recorded.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message": {"role": "assistant", "content": "ok"},
            "prompt_eval_count": 100, // ≪ any candidate N → Truncated
            "eval_count": 1
        })))
        .mount(&server)
        .await;

    let mut e = entry_with(ToolConformance::Native);
    e.context_window = Some(32_768);
    e.safe_context = Some(8_192);

    let outcome =
        probe_input_boundary(&server.uri(), "m", &mut e, |_s| {}, "2026-06-13", EST).unwrap();

    assert_eq!(outcome.highest_accepted, None, "nothing was accepted");
    assert_eq!(e.max_ok_input, None, "no false boundary recorded");
    // The high bound was driven down from the declared 32_768.
    let (low, high) = outcome.final_bounds;
    assert!(high < 32_768, "high {high} lowered by truncation");
    assert!(high >= low, "bounds stay ordered");
    // Confidence is untouched (still the entry default) since nothing passed.
    assert_ne!(e.tune_confidence, TuneConfidence::High);
}

// ---------------------------------------------------------------------------
// probe_tool_conformance  (/api/chat)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_classifies_native_tool_calls() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        // The probe must send the model name, the list_dir tool, and
        // stream:false — otherwise the classification is meaningless.
        .and(body_partial_json(serde_json::json!({
            "model": "good-model",
            "stream": false,
            "tools": [{"type": "function", "function": {"name": "list_dir"}}]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {"function": {"name": "list_dir", "arguments": {"path": "."}}}
                ]
            }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let c = probe_tool_conformance(&server.uri(), "good-model", EST)
        .await
        .unwrap();
    assert_eq!(c, ToolConformance::Native);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_conformance_calibrated_harvests_prompt_eval_count() {
    // §4.4: the tool-schema-bearing conformance request is one of the cheap
    // calibration sources. The calibrated variant must echo its
    // prompt_eval_count alongside the classification (the plain wrapper
    // discards it).
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {"function": {"name": "list_dir", "arguments": {"path": "."}}}
                ]
            },
            "prompt_eval_count": 142
        })))
        .expect(1)
        .mount(&server)
        .await;

    let pc = probe_tool_conformance_calibrated(&server.uri(), "good-model", EST)
        .await
        .expect("probe should succeed");
    assert_eq!(pc.conformance, ToolConformance::Native);
    let (observed, estimated) = pc
        .calibration
        .expect("prompt_eval_count present → calibration sample harvested");
    assert_eq!(observed, 142, "observed real prompt tokens echoed");
    assert!(
        estimated > 0,
        "a chars/4 estimate of the tool-schema request body"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_conformance_calibrated_absent_when_no_prompt_eval_count() {
    // No prompt_eval_count in the response → no calibration sample, but the
    // conformance classification still lands.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message": {"role": "assistant", "content": "I cannot run tools."}
        })))
        .mount(&server)
        .await;

    let pc = probe_tool_conformance_calibrated(&server.uri(), "m", EST)
        .await
        .unwrap();
    assert_eq!(pc.conformance, ToolConformance::NoTools);
    assert!(pc.calibration.is_none(), "no prompt_eval_count → no sample");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_classifies_text_mode_json_in_content() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message": {
                "role": "assistant",
                "content": r#"{"name": "list_dir", "arguments": {"path": "."}}"#
            }
        })))
        .mount(&server)
        .await;

    let c = probe_tool_conformance(&server.uri(), "m", EST)
        .await
        .unwrap();
    assert_eq!(c, ToolConformance::TextMode);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_classifies_no_tools_plain_text() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message": {"role": "assistant", "content": "I cannot run tools, sorry."}
        })))
        .mount(&server)
        .await;

    let c = probe_tool_conformance(&server.uri(), "m", EST)
        .await
        .unwrap();
    assert_eq!(c, ToolConformance::NoTools);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_empty_tool_calls_array_falls_through_to_content() {
    // An empty tool_calls array is NOT native — the content decides.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message": {
                "role": "assistant",
                "tool_calls": [],
                "content": r#"[{"name": "list_dir", "arguments": {"path": "."}}]"#
            }
        })))
        .mount(&server)
        .await;

    let c = probe_tool_conformance(&server.uri(), "m", EST)
        .await
        .unwrap();
    assert_eq!(c, ToolConformance::TextMode);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_errors_on_http_failure() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    let err = probe_tool_conformance(&server.uri(), "m", EST)
        .await
        .expect_err("HTTP 503 must be an error");
    assert!(err.to_string().contains("Ollama returned"), "got: {err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_errors_when_unreachable() {
    let err = probe_tool_conformance(DEAD_ENDPOINT, "m", EST)
        .await
        .expect_err("connection refused must be an error");
    assert!(err.to_string().contains("request failed"), "got: {err}");
}

// ---------------------------------------------------------------------------
// Cache persistence (load_cache / save_cache via $HOME redirect)
// ---------------------------------------------------------------------------

// Setting env vars races across parallel tests; serialize these.
// (Same pattern as newt-acp-worker/src/diff.rs.)
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Point `$HOME` at a tempdir for the duration of the guard, restoring the
/// previous value (or removing the var) on drop — panic-safe.
struct HomeGuard {
    saved_home: Option<String>,
    saved_profile: Option<String>,
    _dir: Option<tempfile::TempDir>,
}

impl HomeGuard {
    /// Redirect `$HOME` to a fresh tempdir; returns the guard.
    fn tempdir() -> (Self, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().to_path_buf();
        let guard = Self {
            saved_home: std::env::var("HOME").ok(),
            saved_profile: std::env::var("USERPROFILE").ok(),
            _dir: Some(dir),
        };
        std::env::set_var("HOME", &path);
        std::env::remove_var("USERPROFILE");
        (guard, path)
    }

    /// Remove both home vars entirely (cache_path → None).
    fn unset() -> Self {
        let guard = Self {
            saved_home: std::env::var("HOME").ok(),
            saved_profile: std::env::var("USERPROFILE").ok(),
            _dir: None,
        };
        std::env::remove_var("HOME");
        std::env::remove_var("USERPROFILE");
        guard
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        match self.saved_home.take() {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match self.saved_profile.take() {
            Some(v) => std::env::set_var("USERPROFILE", v),
            None => std::env::remove_var("USERPROFILE"),
        }
    }
}

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[test]
fn save_then_load_cache_roundtrips_through_home_newt() {
    let _l = env_lock();
    let (_guard, home) = HomeGuard::tempdir();
    // save_cache writes to $HOME/.newt/model-capabilities.json and does not
    // create the parent dir itself — mirror the real ~/.newt layout.
    std::fs::create_dir_all(home.join(".newt")).unwrap();

    let mut cache = CapabilityCache::default();
    cache.insert(
        "llama3:8b".to_string(),
        CapabilityEntry {
            conformance: ToolConformance::Native,
            tested_date: "2026-06-07".to_string(),
            context_window: Some(32768),
            safe_context: Some(26214),
            tune_confidence: TuneConfidence::Medium,
            ..Default::default()
        },
    );
    save_cache(&cache);

    // The file must land at the documented path, as pretty JSON.
    let on_disk = home.join(".newt").join("model-capabilities.json");
    let raw = std::fs::read_to_string(&on_disk).expect("cache file written");
    assert!(raw.contains("llama3:8b"));
    assert!(raw.contains("native"));

    let back = load_cache();
    let e = back.get("llama3:8b").expect("entry round-trips");
    assert_eq!(e.conformance, ToolConformance::Native);
    assert_eq!(e.context_window, Some(32768));
    assert_eq!(e.safe_context, Some(26214));
    assert_eq!(e.tune_confidence, TuneConfidence::Medium);
}

/// Step 18.1 ratchet de-poison, end-to-end through the on-disk file: a
/// pre-18.1 cache (no `accounting_version` field) carrying the live B3
/// poisoned ratchet (`max_ok_input: 25602` at High confidence when no prompt
/// the backend evaluated exceeded 4,748 tokens) must be invalidated by
/// `load_cache` AND persisted back, so the migration runs exactly once.
#[test]
fn load_cache_migrates_legacy_poisoned_entry_once_and_persists() {
    let _l = env_lock();
    let (_guard, home) = HomeGuard::tempdir();
    let newt = home.join(".newt");
    std::fs::create_dir_all(&newt).unwrap();
    let on_disk = newt.join("model-capabilities.json");
    std::fs::write(
        &on_disk,
        serde_json::json!({
            "llama3.1:8b": {
                "conformance": "native",
                "tested_date": "2026-06-08",
                "context_window": 8192,
                "safe_context": 6553,
                "max_ok_input": 25602,
                "consecutive_ok": 3,
                "tune_confidence": "high",
                "tune_date": "2026-06-08"
            }
        })
        .to_string(),
    )
    .unwrap();

    let cache = load_cache();
    let e = cache.get("llama3.1:8b").expect("entry survives migration");
    assert_eq!(e.max_ok_input, None, "poisoned ratchet value dropped");
    assert_eq!(e.consecutive_ok, 0);
    assert_eq!(e.tune_confidence, TuneConfidence::None);
    assert_eq!(e.accounting_version, newt_tui::probe::ACCOUNTING_VERSION);
    // Non-regime state survives: the declared window, the VRAM-derived
    // safe_context, and the conformance probe result.
    assert_eq!(e.context_window, Some(8192));
    assert_eq!(e.safe_context, Some(6553));
    assert_eq!(e.conformance, ToolConformance::Native);

    // The invalidation was persisted: the stamp is on disk, the poisoned
    // value is gone...
    let raw = std::fs::read_to_string(&on_disk).unwrap();
    assert!(raw.contains("accounting_version"), "stamp must persist");
    assert!(!raw.contains("25602"), "poisoned value must not survive");
    // ...and a second load is a pure read — same bytes, nothing re-migrated.
    let again = load_cache();
    assert_eq!(again.get("llama3.1:8b").unwrap().max_ok_input, None);
    assert_eq!(
        std::fs::read_to_string(&on_disk).unwrap(),
        raw,
        "second load must not rewrite the file"
    );
}

#[test]
fn load_cache_empty_when_file_missing() {
    let _l = env_lock();
    let (_guard, home) = HomeGuard::tempdir();
    std::fs::create_dir_all(home.join(".newt")).unwrap();
    assert!(load_cache().is_empty());
}

#[test]
fn load_cache_empty_on_corrupt_json() {
    let _l = env_lock();
    let (_guard, home) = HomeGuard::tempdir();
    let newt = home.join(".newt");
    std::fs::create_dir_all(&newt).unwrap();
    std::fs::write(newt.join("model-capabilities.json"), "{not json!!").unwrap();
    assert!(load_cache().is_empty(), "corrupt cache must not propagate");
}

#[test]
fn save_cache_is_best_effort_when_dir_missing() {
    let _l = env_lock();
    let (_guard, home) = HomeGuard::tempdir();
    // No $HOME/.newt dir — the write fails silently (best-effort contract).
    let mut cache = CapabilityCache::default();
    cache.insert("m".to_string(), CapabilityEntry::default());
    save_cache(&cache); // must not panic
    assert!(!home.join(".newt").exists(), "save must not create dirs");
    assert!(load_cache().is_empty());
}

#[test]
fn cache_ops_are_noops_without_home() {
    let _l = env_lock();
    let _guard = HomeGuard::unset();
    // cache_path() resolves to None → load returns empty, save is a no-op.
    assert!(load_cache().is_empty());
    let mut cache = CapabilityCache::default();
    cache.insert("m".to_string(), CapabilityEntry::default());
    save_cache(&cache); // must not panic
}
