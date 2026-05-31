//! End-to-end smoke for [`newt_coder::Coder`].
//!
//! Wires a real `MockBackend` (canned reply) through the full
//! orchestrator: `build_prompt` -> `complete` -> `normalize_emission`
//! -> apply. The mock replies with the S5 whole-file shape that the
//! 2026-05-29 bake-off (wf_ecc784ea-aa2) showed qwen3-coder:30b
//! produces; this verifies newt-coder lands the edit on the workspace
//! given that shape — closing failure mode T0b end-to-end.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use newt_coder::{Coder, CoderError};
use newt_core::router::Tier;
use newt_core::{Caveats, CountBound, Scope};
use newt_inference::backend::{ChatReply, ChatRequest, InferenceBackend};
use tempfile::TempDir;
use tests_common::MockBackend;

fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

#[tokio::test]
async fn coder_lands_whole_file_rename_end_to_end() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp.path().join("src/lib.rs"), "pub fn greet() {}\n");

    // Canned reply in the S5 shape: rename greet -> hello.
    let canned = "FILE: src/lib.rs\npub fn hello() {}\nEND-FILE\n";
    let backend = Arc::new(MockBackend::all_tiers("mock", canned)) as Arc<dyn InferenceBackend>;

    let coder = Coder::new(backend);
    let run = coder
        .run(
            tmp.path(),
            "Rename greet to hello in src/lib.rs",
            &Caveats::top(),
        )
        .await
        .unwrap();

    assert_eq!(run.emission_shape, "whole_files");
    assert_eq!(run.files_written, vec!["src/lib.rs".to_string()]);
    let content = std::fs::read_to_string(tmp.path().join("src/lib.rs")).unwrap();
    assert_eq!(content, "pub fn hello() {}");
}

#[tokio::test]
async fn coder_reports_prose_shape_when_model_emits_no_structure() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp.path().join("src/lib.rs"), "pub fn greet() {}\n");

    // T0a-style reply: pure prose, no structure.
    let canned = "I've updated src/lib.rs to rename greet to hello.";
    let backend = Arc::new(MockBackend::all_tiers("mock", canned)) as Arc<dyn InferenceBackend>;

    let coder = Coder::new(backend);
    let run = coder
        .run(
            tmp.path(),
            "Rename greet to hello in src/lib.rs",
            &Caveats::top(),
        )
        .await
        .unwrap();

    assert_eq!(run.emission_shape, "prose");
    assert!(run.files_written.is_empty());
    // The workspace must be unchanged — prose is a no-op.
    let content = std::fs::read_to_string(tmp.path().join("src/lib.rs")).unwrap();
    assert_eq!(content, "pub fn greet() {}\n");
}

#[tokio::test]
async fn coder_applies_unified_diff_when_model_emits_one() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp.path().join("src/lib.rs"), "pub fn greet() {}\n");

    // Legacy path: a real unified diff against the actual file.
    let canned = "\
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1 +1 @@
-pub fn greet() {}
+pub fn hello() {}
";
    let backend = Arc::new(MockBackend::all_tiers("mock", canned)) as Arc<dyn InferenceBackend>;

    let coder = Coder::new(backend);
    let run = coder
        .run(
            tmp.path(),
            "Rename greet to hello in src/lib.rs",
            &Caveats::top(),
        )
        .await
        .unwrap();

    assert_eq!(run.emission_shape, "unified_diff");
    // Diff path returns empty files_written (see Coder::apply docs).
    assert!(run.files_written.is_empty());
    let content = std::fs::read_to_string(tmp.path().join("src/lib.rs")).unwrap();
    assert_eq!(content, "pub fn hello() {}\n");
}

#[tokio::test]
async fn coder_writes_multi_file_emission() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp.path().join("src/lib.rs"), "pub fn a() {}\n");
    write_file(&tmp.path().join("src/util.rs"), "pub fn b() {}\n");

    let canned = "\
FILE: src/lib.rs
pub fn a_renamed() {}
END-FILE

FILE: src/util.rs
pub fn b_renamed() {}
END-FILE
";
    let backend = Arc::new(MockBackend::all_tiers("mock", canned)) as Arc<dyn InferenceBackend>;

    let coder = Coder::new(backend);
    let run = coder
        .run(
            tmp.path(),
            "Rename functions in src/lib.rs and src/util.rs",
            &Caveats::top(),
        )
        .await
        .unwrap();

    assert_eq!(run.emission_shape, "whole_files");
    assert_eq!(run.files_written.len(), 2);
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("src/lib.rs")).unwrap(),
        "pub fn a_renamed() {}"
    );
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("src/util.rs")).unwrap(),
        "pub fn b_renamed() {}"
    );
}

#[tokio::test]
async fn coder_surfaces_model_id_in_run_outcome() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp.path().join("a.rs"), "fn x() {}\n");

    let canned = "FILE: a.rs\nfn y() {}\nEND-FILE\n";
    let backend = Arc::new(MockBackend::all_tiers("mock", canned)) as Arc<dyn InferenceBackend>;

    let coder = Coder::new(backend);
    let run = coder
        .run(tmp.path(), "rename in a.rs", &Caveats::top())
        .await
        .unwrap();

    // MockBackend::all_tiers names the model "<name>-model".
    assert_eq!(run.model_id, "mock-model");
}

// ── 35b: Caveats enforcement at Coder::run ─────────────────────────────────

/// A mock backend whose endpoint is configurable — lets the
/// `caveats.net` axis tests pretend the inference call is going to a
/// specific host.
#[derive(Debug, Clone)]
struct EndpointMock {
    endpoint: String,
    reply: String,
}

#[async_trait]
impl InferenceBackend for EndpointMock {
    fn name(&self) -> &str {
        "endpoint-mock"
    }
    fn model_id(&self) -> &str {
        "endpoint-mock-model"
    }
    fn supports_tier(&self, _t: Tier) -> bool {
        true
    }
    fn endpoint(&self) -> Option<&str> {
        Some(&self.endpoint)
    }
    async fn complete(&self, _req: ChatRequest) -> anyhow::Result<ChatReply> {
        Ok(ChatReply {
            content: self.reply.clone(),
            model_id: self.model_id().to_string(),
        })
    }
}

#[tokio::test]
async fn coder_run_top_caveats_preserves_existing_behavior() {
    // Back-compat: explicit Caveats::top() runs the same path as the
    // pre-35b API — everything succeeds. This is the safety net
    // protecting the existing call sites in newt-acp-worker (which
    // pass top while 35c is in flight).
    let tmp = TempDir::new().unwrap();
    write_file(&tmp.path().join("src/lib.rs"), "fn x() {}\n");
    let canned = "FILE: src/lib.rs\nfn y() {}\nEND-FILE\n";
    let backend = Arc::new(MockBackend::all_tiers("mock", canned)) as Arc<dyn InferenceBackend>;
    let coder = Coder::new(backend);

    let run = coder
        .run(tmp.path(), "rename x to y", &Caveats::top())
        .await
        .expect("top caveats must permit every dispatch");
    assert_eq!(run.emission_shape, "whole_files");
    assert_eq!(run.files_written, vec!["src/lib.rs".to_string()]);
}

#[tokio::test]
async fn coder_run_attenuated_fs_write_permits_allowed_path() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp.path().join("src/lib.rs"), "fn x() {}\n");
    let canned = "FILE: src/lib.rs\nfn y() {}\nEND-FILE\n";
    let backend = Arc::new(MockBackend::all_tiers("mock", canned)) as Arc<dyn InferenceBackend>;
    let coder = Coder::new(backend);

    let caveats = Caveats {
        fs_write: Scope::only(["src/lib.rs".to_string()]),
        ..Caveats::top()
    };
    let run = coder
        .run(tmp.path(), "rename x to y", &caveats)
        .await
        .expect("permitted fs_write must succeed");
    assert_eq!(run.files_written, vec!["src/lib.rs".to_string()]);
}

#[tokio::test]
async fn coder_run_attenuated_fs_write_denies_forbidden_path() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp.path().join("src/lib.rs"), "fn x() {}\n");
    // Model targets src/lib.rs, but caveats only allow "other.rs".
    let canned = "FILE: src/lib.rs\nfn y() {}\nEND-FILE\n";
    let backend = Arc::new(MockBackend::all_tiers("mock", canned)) as Arc<dyn InferenceBackend>;
    let coder = Coder::new(backend);

    let caveats = Caveats {
        fs_write: Scope::only(["other.rs".to_string()]),
        ..Caveats::top()
    };
    let err = coder
        .run(tmp.path(), "rename x to y", &caveats)
        .await
        .unwrap_err();
    match err {
        CoderError::CapabilityDenied { kind, target } => {
            assert_eq!(kind, "fs_write");
            assert_eq!(target, "src/lib.rs");
        }
        other => panic!("expected CapabilityDenied{{fs_write}}, got {other:?}"),
    }
    // The original file must be untouched.
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("src/lib.rs")).unwrap(),
        "fn x() {}\n"
    );
}

#[tokio::test]
async fn coder_run_attenuated_net_permits_allowed_host() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp.path().join("a.rs"), "fn x() {}\n");
    let canned = "FILE: a.rs\nfn y() {}\nEND-FILE\n".to_string();
    let backend = Arc::new(EndpointMock {
        endpoint: "https://allowed.example.com/v1/chat".to_string(),
        reply: canned,
    }) as Arc<dyn InferenceBackend>;
    let coder = Coder::new(backend);

    let caveats = Caveats {
        net: Scope::only(["allowed.example.com".to_string()]),
        ..Caveats::top()
    };
    let run = coder
        .run(tmp.path(), "rename x", &caveats)
        .await
        .expect("permitted net must succeed");
    assert_eq!(run.files_written, vec!["a.rs".to_string()]);
}

#[tokio::test]
async fn coder_run_attenuated_net_denies_forbidden_host() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp.path().join("a.rs"), "fn x() {}\n");
    let backend = Arc::new(EndpointMock {
        endpoint: "http://evil.example.com:11434/api/chat".to_string(),
        reply: "FILE: a.rs\nfn y() {}\nEND-FILE\n".to_string(),
    }) as Arc<dyn InferenceBackend>;
    let coder = Coder::new(backend);

    let caveats = Caveats {
        net: Scope::only(["allowed.example.com".to_string()]),
        ..Caveats::top()
    };
    let err = coder
        .run(tmp.path(), "rename x", &caveats)
        .await
        .unwrap_err();
    match err {
        CoderError::CapabilityDenied { kind, target } => {
            assert_eq!(kind, "net");
            assert_eq!(target, "evil.example.com");
        }
        other => panic!("expected CapabilityDenied{{net}}, got {other:?}"),
    }
}

#[tokio::test]
async fn coder_run_max_calls_zero_denies_first_call() {
    // AtMost(0) must refuse the very first inference call. This is
    // the strict-cap regression: the dispatch must not slip through
    // even once.
    let tmp = TempDir::new().unwrap();
    write_file(&tmp.path().join("a.rs"), "fn x() {}\n");
    let canned = "FILE: a.rs\nfn y() {}\nEND-FILE\n";
    let backend = Arc::new(MockBackend::all_tiers("mock", canned)) as Arc<dyn InferenceBackend>;
    let coder = Coder::new(backend);

    let caveats = Caveats {
        max_calls: CountBound::AtMost(0),
        ..Caveats::top()
    };
    let err = coder
        .run(tmp.path(), "rename x", &caveats)
        .await
        .unwrap_err();
    match err {
        CoderError::CapabilityDenied { kind, target } => {
            assert_eq!(kind, "max_calls");
            assert!(target.contains("#1"), "want turn label, got {target:?}");
        }
        other => panic!("expected CapabilityDenied{{max_calls}}, got {other:?}"),
    }
}

#[tokio::test]
async fn coder_run_max_calls_one_permits_single_turn() {
    // AtMost(1) must permit exactly one happy-path inference call.
    let tmp = TempDir::new().unwrap();
    write_file(&tmp.path().join("a.rs"), "fn x() {}\n");
    let canned = "FILE: a.rs\nfn y() {}\nEND-FILE\n";
    let backend = Arc::new(MockBackend::all_tiers("mock", canned)) as Arc<dyn InferenceBackend>;
    let coder = Coder::new(backend);

    let caveats = Caveats {
        max_calls: CountBound::AtMost(1),
        ..Caveats::top()
    };
    let run = coder
        .run(tmp.path(), "rename x", &caveats)
        .await
        .expect("AtMost(1) must permit the first turn");
    assert_eq!(run.files_written, vec!["a.rs".to_string()]);
}

#[tokio::test]
async fn coder_run_attenuated_fs_read_denies_workspace_file() {
    // The included file (a.rs) is outside the fs_read scope —
    // build_prompt would have *read* it, so the dispatch is denied
    // before the model ever sees it.
    let tmp = TempDir::new().unwrap();
    write_file(&tmp.path().join("a.rs"), "fn x() {}\n");
    let canned = "FILE: a.rs\nfn y() {}\nEND-FILE\n";
    let backend = Arc::new(MockBackend::all_tiers("mock", canned)) as Arc<dyn InferenceBackend>;
    let coder = Coder::new(backend);

    let caveats = Caveats {
        fs_read: Scope::only(["only-this.rs".to_string()]),
        ..Caveats::top()
    };
    let err = coder
        .run(tmp.path(), "rename x", &caveats)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        CoderError::CapabilityDenied {
            kind: "fs_read",
            ..
        }
    ));
}

#[tokio::test]
async fn coder_run_denial_carries_axis_and_target_context() {
    // The error must carry enough context for the arbiter scorecard:
    // the failing axis name AND the concrete target. Lock that contract
    // explicitly so future error-format changes can't silently drop it.
    let tmp = TempDir::new().unwrap();
    write_file(&tmp.path().join("a.rs"), "fn x() {}\n");
    let canned = "FILE: a.rs\nfn y() {}\nEND-FILE\n";
    let backend = Arc::new(MockBackend::all_tiers("mock", canned)) as Arc<dyn InferenceBackend>;
    let coder = Coder::new(backend);

    let caveats = Caveats {
        fs_write: Scope::only(["else.rs".to_string()]),
        ..Caveats::top()
    };
    let err = coder
        .run(tmp.path(), "rename x", &caveats)
        .await
        .unwrap_err();
    let rendered = err.to_string();
    assert!(rendered.contains("fs_write"), "missing axis: {rendered}");
    assert!(rendered.contains("a.rs"), "missing target: {rendered}");
}
