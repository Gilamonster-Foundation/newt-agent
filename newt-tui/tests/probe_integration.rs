//! Integration tests for `newt_tui::probe` — the HTTP-facing fns
//! (`fetch_ollama_models`, `fetch_context_window`, `ensure_context_window`,
//! `probe_tool_conformance`) against a wiremock Ollama, plus the capability
//! cache persistence (`load_cache` / `save_cache`) redirected to a tempdir
//! via `$HOME` so the real `~/.newt` is never touched.
//!
//! The sync fetch fns use `tokio::task::block_in_place`, which requires a
//! multi-threaded runtime — hence `flavor = "multi_thread"` throughout.

use std::sync::Mutex;

use newt_tui::probe::{
    ensure_context_window, fetch_context_window, fetch_ollama_models, load_cache,
    probe_tool_conformance, save_cache, CapabilityCache, CapabilityEntry, ToolConformance,
    TuneConfidence,
};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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
        fetch_context_window(&server.uri(), "llama3:8b"),
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

    assert_eq!(fetch_context_window(&server.uri(), "m"), Some(8192));
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

    assert_eq!(fetch_context_window(&server.uri(), "m"), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_context_window_none_when_unreachable() {
    assert_eq!(fetch_context_window(DEAD_ENDPOINT, "m"), None);
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
    assert!(!ensure_context_window(&mut e, DEAD_ENDPOINT, "m"));
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
    assert!(ensure_context_window(&mut e, &server.uri(), "m"));
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
    assert!(ensure_context_window(&mut e, &server.uri(), "m"));
    assert_eq!(e.context_window, Some(32768));
    assert_eq!(e.safe_context, Some(1234), "tuned value must survive");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ensure_context_window_false_when_fetch_fails() {
    let mut e = entry_with(ToolConformance::Native);
    assert!(!ensure_context_window(&mut e, DEAD_ENDPOINT, "m"));
    assert_eq!(e.context_window, None);
    assert_eq!(e.safe_context, None);
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

    let c = probe_tool_conformance(&server.uri(), "good-model")
        .await
        .unwrap();
    assert_eq!(c, ToolConformance::Native);
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

    let c = probe_tool_conformance(&server.uri(), "m").await.unwrap();
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

    let c = probe_tool_conformance(&server.uri(), "m").await.unwrap();
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

    let c = probe_tool_conformance(&server.uri(), "m").await.unwrap();
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

    let err = probe_tool_conformance(&server.uri(), "m")
        .await
        .expect_err("HTTP 503 must be an error");
    assert!(err.to_string().contains("Ollama returned"), "got: {err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_errors_when_unreachable() {
    let err = probe_tool_conformance(DEAD_ENDPOINT, "m")
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
