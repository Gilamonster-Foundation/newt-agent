use super::*;
use crate::agentic::{ArtifactReadContext, ArtifactReadRecord, PromptArtifactSink};
use crate::artifact::{ArtifactId, ArtifactKind, NewPromptArtifact};
use crate::PromptId;
use std::sync::Mutex;

#[derive(Default)]
struct RecordingArtifactSink {
    artifacts: Mutex<Vec<NewPromptArtifact>>,
}

impl RecordingArtifactSink {
    fn only_artifact(&self) -> NewPromptArtifact {
        let artifacts = self.artifacts.lock().unwrap();
        assert_eq!(artifacts.len(), 1, "expected exactly one artifact");
        artifacts[0].clone()
    }

    // Matches its only caller (`physical_symlink_escape_write_is_denied_
    // object_bound`, Linux-only) — `cfg(unix)` left it dead-code on macOS.
    #[cfg(target_os = "linux")]
    fn is_empty(&self) -> bool {
        self.artifacts.lock().unwrap().is_empty()
    }
}

impl PromptArtifactSink for RecordingArtifactSink {
    fn append_artifact(
        &self,
        originating_prompt_id: PromptId,
        objective_root_id: PromptId,
        artifact: NewPromptArtifact,
    ) -> anyhow::Result<ArtifactReadRecord> {
        let mut artifacts = self.artifacts.lock().unwrap();
        artifacts.push(artifact.clone());
        Ok(ArtifactReadRecord {
            id: ArtifactId::new(),
            prompt_id: originating_prompt_id,
            root_prompt_id: objective_root_id,
            writer_fingerprint: "tool-test".to_string(),
            seq: artifacts.len() as u64,
            prev_hash: "previous".to_string(),
            kind: format!("{:?}", artifact.kind()),
            relation: format!("{:?}", artifact.relation()),
            locator: artifact.locator().map(str::to_string),
            body: artifact.body().map(str::to_string),
            metadata: artifact.metadata().clone(),
            ts_claim: 1,
            artifact_hash: "hash".to_string(),
        })
    }
}

fn artifact_context() -> ArtifactReadContext<'static> {
    let prompt = PromptId::new();
    ArtifactReadContext::new(Some(prompt), Some(prompt), Some(prompt), None)
}

#[allow(clippy::too_many_arguments)]
async fn run_artifact_tool(
    name: &str,
    args: serde_json::Value,
    ws: &std::path::Path,
    caveats: &Caveats,
    build_check: Option<&str>,
    sink: &RecordingArtifactSink,
) -> String {
    execute_tool_with_offload_and_prompt_and_artifacts(
        name,
        &args,
        &ws.to_string_lossy(),
        false,
        20,
        caveats,
        &mut NoMcp,
        build_check,
        None, // note_sink
        None, // recall_source
        None, // memory_source
        None, // prompt_context
        Some(artifact_context()),
        Some(sink),
        None,  // permission_gate
        None,  // exec_floor
        None,  // git_tool
        None,  // crew_runner
        None,  // scratchpad_store
        None,  // code_search
        None,  // where_is
        None,  // experience_store
        None,  // step_ledger
        false, // tool_offload
        None,  // spill_store
        None,  // persona_tools
        PromptDisposition::Act,
    )
    .await
}

#[tokio::test]
async fn delete_file_records_digest_to_absent_transition() {
    let ws = tempfile::TempDir::new().unwrap();
    let original = b"retired implementation\n";
    std::fs::write(ws.path().join("old.rs"), original).unwrap();
    let sink = RecordingArtifactSink::default();

    let out = run_artifact_tool(
        "delete_file",
        serde_json::json!({"path": "old.rs"}),
        ws.path(),
        &caveats_rw(ws.path()),
        None,
        &sink,
    )
    .await;

    assert!(out.starts_with("deleted old.rs"), "got: {out}");
    let artifact = sink.only_artifact();
    assert_eq!(artifact.kind(), ArtifactKind::FileChange);
    assert_eq!(artifact.locator(), Some("old.rs"));
    assert_eq!(artifact.metadata()["operation"], "delete_file");
    assert_eq!(artifact.metadata()["before"]["available"], true);
    assert_eq!(artifact.metadata()["before"]["exists"], true);
    assert_eq!(
        artifact.metadata()["before"]["digest"],
        blake3::hash(original).to_hex().to_string()
    );
    assert_eq!(artifact.metadata()["after"]["available"], true);
    assert_eq!(artifact.metadata()["after"]["exists"], false);
    assert!(artifact.metadata()["after"]["digest"].is_null());
}

#[tokio::test]
async fn write_only_authority_does_not_record_a_preimage_digest() {
    let ws = tempfile::TempDir::new().unwrap();
    let original = b"secret preimage\n";
    let replacement = b"public result\n";
    std::fs::write(ws.path().join("state.txt"), original).unwrap();
    let caveats = Caveats {
        fs_read: Scope::none(),
        ..caveats_rw(ws.path())
    };
    let sink = RecordingArtifactSink::default();

    let out = run_artifact_tool(
        "write_file",
        serde_json::json!({
            "path": "state.txt",
            "content": std::str::from_utf8(replacement).unwrap(),
        }),
        ws.path(),
        &caveats,
        None,
        &sink,
    )
    .await;

    assert!(out.starts_with("wrote state.txt"), "got: {out}");
    let artifact = sink.only_artifact();
    assert_eq!(artifact.metadata()["before"]["available"], false);
    assert_eq!(
        artifact.metadata()["before"]["reason"],
        "fs_read_not_granted"
    );
    assert!(artifact.metadata()["before"].get("digest").is_none());
    assert_eq!(
        artifact.metadata()["after"]["digest"],
        blake3::hash(replacement).to_hex().to_string()
    );
    assert!(
        !artifact
            .metadata()
            .to_string()
            .contains(&blake3::hash(original).to_hex().to_string()),
        "the preimage digest must not become a persistent read oracle"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn build_check_mutation_is_not_recorded_as_the_governed_write_postimage() {
    let ws = tempfile::TempDir::new().unwrap();
    let governed = b"governed bytes\n";
    let build_hook = b"build-hook bytes\n";
    let sink = RecordingArtifactSink::default();

    let out = run_artifact_tool(
        "write_file",
        serde_json::json!({
            "path": "target.txt",
            "content": std::str::from_utf8(governed).unwrap(),
        }),
        ws.path(),
        &caveats_rw(ws.path()),
        Some("printf 'build-hook bytes\\n' > target.txt"),
        &sink,
    )
    .await;

    assert!(out.contains("build check passed"), "got: {out}");
    assert_eq!(
        std::fs::read(ws.path().join("target.txt")).unwrap(),
        build_hook
    );
    let artifact = sink.only_artifact();
    assert_eq!(artifact.metadata()["operation"], "write_file");
    assert_eq!(
        artifact.metadata()["after"]["digest"],
        blake3::hash(governed).to_hex().to_string(),
        "the artifact must describe the tool's immediate verified write"
    );
    assert_ne!(
        artifact.metadata()["after"]["digest"],
        blake3::hash(build_hook).to_hex().to_string(),
        "a later build hook mutation must not be attributed to write_file"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn physical_symlink_escape_write_is_denied_object_bound() {
    // step-52.4 (#522 closure for write_file): a symlink UNDER the workspace
    // pointing outside no longer lets a CONFINED write escape. Object-bound
    // via openat2(RESOLVE_BENEATH), so the create is refused, the outside file
    // is untouched, and no artifact is minted. BEFORE object-binding this
    // mutated the outside file under the lexical policy — the named residual;
    // this test is that residual, flipped from "mutates" to "denied".
    let ws = tempfile::TempDir::new().unwrap();
    let outside = tempfile::TempDir::new().unwrap();
    std::fs::write(outside.path().join("target.txt"), "outside before\n").unwrap();
    std::os::unix::fs::symlink(outside.path(), ws.path().join("link")).unwrap();
    let sink = RecordingArtifactSink::default();

    let out = run_artifact_tool(
        "write_file",
        serde_json::json!({
            "path": "link/target.txt",
            "content": "outside after\n",
        }),
        ws.path(),
        &caveats_rw(ws.path()),
        None,
        &sink,
    )
    .await;

    assert_eq!(
        out,
        denied_fs_result("fs_write", "link/target.txt"),
        "the symlink-escape write must be denied by the object fence: {out}"
    );
    assert_eq!(
        std::fs::read_to_string(outside.path().join("target.txt")).unwrap(),
        "outside before\n",
        "the outside file must be UNCHANGED — the write never escaped"
    );
    assert!(sink.is_empty(), "a denied write records no artifact");
}
