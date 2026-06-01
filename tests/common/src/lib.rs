//! Shared test helpers for the newt-agent workspace.
//!
//! Every crate in the workspace can add `tests-common` as a dev-dependency
//! to get tracing init, temp dirs, mock backends, and mock plugin binaries.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use newt_core::router::Tier;
use newt_inference::backend::{ChatReply, ChatRequest, InferenceBackend};

// ── Tracing ──────────────────────────────────────────────────────────

/// Install a test-friendly tracing subscriber.
///
/// Safe to call multiple times (the second call is a no-op).
/// Output goes to stderr so `cargo test` captures it unless `--nocapture`.
pub fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("debug")
        .try_init();
}

// ── Temp directory ───────────────────────────────────────────────────

/// Create a temporary directory that is cleaned up when the guard drops.
pub fn tempdir() -> tempfile::TempDir {
    tempfile::tempdir().expect("failed to create temp dir")
}

// ── Mock backend ─────────────────────────────────────────────────────

/// A configurable mock that implements [`InferenceBackend`].
///
/// Every call to `complete()` returns the pre-configured `reply` string
/// along with the configured `model_id`.
#[derive(Debug, Clone)]
pub struct MockBackend {
    name: String,
    model_id: String,
    tiers: Vec<Tier>,
    reply: String,
}

impl MockBackend {
    /// Full constructor.
    pub fn new(name: &str, model_id: &str, tiers: Vec<Tier>, reply: &str) -> Self {
        Self {
            name: name.to_owned(),
            model_id: model_id.to_owned(),
            tiers,
            reply: reply.to_owned(),
        }
    }

    /// Factory that creates a backend supporting all four tiers.
    pub fn all_tiers(name: &str, reply: &str) -> Self {
        Self::new(
            name,
            &format!("{name}-model"),
            vec![Tier::Fast, Tier::Standard, Tier::Complex, Tier::Review],
            reply,
        )
    }
}

#[async_trait]
impl InferenceBackend for MockBackend {
    fn name(&self) -> &str {
        &self.name
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn supports_tier(&self, tier: Tier) -> bool {
        self.tiers.contains(&tier)
    }

    async fn complete(&self, _req: ChatRequest) -> anyhow::Result<ChatReply> {
        Ok(ChatReply {
            content: self.reply.clone(),
            model_id: self.model_id.clone(),
            usage: None,
        })
    }
}

// ── Mock plugin binary ───────────────────────────────────────────────

/// Write a bash script into `dir` that echoes canned JSON-RPC responses.
///
/// Each entry in `replies` is emitted line-by-line on stdout whenever the
/// script is invoked. This is useful for testing the provider-plugin host
/// without a real subprocess.
///
/// Returns the path to the created executable.
pub fn mock_plugin_binary(dir: &Path, replies: &[&str]) -> PathBuf {
    let script_path = dir.join("mock-plugin");
    let body: String = replies
        .iter()
        .map(|r| format!("echo '{r}'"))
        .collect::<Vec<_>>()
        .join("\n");

    let script = format!("#!/usr/bin/env bash\n{body}\n");
    fs::write(&script_path, script).expect("failed to write mock plugin script");
    fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755))
        .expect("failed to chmod mock plugin");
    script_path
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use newt_core::router::Tier;

    #[test]
    fn mock_backend_supports_configured_tiers() {
        let backend = MockBackend::new("test", "test-model", vec![Tier::Fast, Tier::Review], "ok");
        assert!(backend.supports_tier(Tier::Fast));
        assert!(backend.supports_tier(Tier::Review));
        assert!(!backend.supports_tier(Tier::Standard));
        assert!(!backend.supports_tier(Tier::Complex));
    }

    #[test]
    fn mock_backend_all_tiers_supports_everything() {
        let backend = MockBackend::all_tiers("omni", "hello");
        assert!(backend.supports_tier(Tier::Fast));
        assert!(backend.supports_tier(Tier::Standard));
        assert!(backend.supports_tier(Tier::Complex));
        assert!(backend.supports_tier(Tier::Review));
    }

    #[tokio::test]
    async fn mock_backend_complete_returns_configured_reply() {
        let backend = MockBackend::all_tiers("echo", "pong");
        let req = ChatRequest {
            messages: vec![],
            max_tokens: None,
        };
        let reply = backend.complete(req).await.unwrap();
        assert_eq!(reply.content, "pong");
        assert_eq!(reply.model_id, "echo-model");
    }

    #[test]
    fn tempdir_creates_real_directory() {
        let dir = tempdir();
        assert!(dir.path().exists());
        assert!(dir.path().is_dir());
    }

    #[test]
    fn mock_plugin_binary_creates_executable() {
        let dir = tempdir();
        let response = r#"{"jsonrpc":"2.0","id":1,"result":"ok"}"#;
        let path = mock_plugin_binary(dir.path(), &[response]);
        assert!(path.exists());
        assert!(path.is_file());

        // Verify the file is executable
        let meta = std::fs::metadata(&path).unwrap();
        assert_ne!(meta.permissions().mode() & 0o111, 0);
    }

    #[test]
    fn mock_plugin_binary_produces_expected_output() {
        let dir = tempdir();
        let r1 = r#"{"jsonrpc":"2.0","id":1,"result":"init"}"#;
        let r2 = r#"{"jsonrpc":"2.0","id":2,"result":"done"}"#;
        let path = mock_plugin_binary(dir.path(), &[r1, r2]);

        let output = std::process::Command::new(&path)
            .output()
            .expect("failed to run mock plugin");
        let stdout = String::from_utf8(output.stdout).unwrap();
        let lines: Vec<&str> = stdout.trim().lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], r1);
        assert_eq!(lines[1], r2);
    }
}
