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

use newt_coder::Coder;
use newt_inference::InferenceBackend;
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
        .run(tmp.path(), "Rename greet to hello in src/lib.rs")
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
        .run(tmp.path(), "Rename greet to hello in src/lib.rs")
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
        .run(tmp.path(), "Rename greet to hello in src/lib.rs")
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
        .run(tmp.path(), "Rename functions in src/lib.rs and src/util.rs")
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
    let run = coder.run(tmp.path(), "rename in a.rs").await.unwrap();

    // MockBackend::all_tiers names the model "<name>-model".
    assert_eq!(run.model_id, "mock-model");
}
