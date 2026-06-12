//! Shared test helpers for the newt-agent workspace.
//!
//! Every crate in the workspace can add `tests-common` as a dev-dependency
//! to get tracing init, temp dirs, mock backends, and mock plugin binaries.

use std::fs;
#[cfg(unix)]
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

/// Write a script into `dir` that echoes canned JSON-RPC responses.
///
/// Each entry in `replies` is emitted on stdout after the script reads one
/// request line from stdin. This is useful for testing the provider-plugin host
/// without a real provider implementation.
///
/// Returns the path to the created executable.
///
/// # Write-then-exec safety (issue #288)
///
/// The script is about to be `exec`'d by the calling test. The write
/// handle is explicitly `sync_all`'d and dropped before this function
/// returns, so *our* fd can never be the one holding the executable
/// open across a concurrent `fork`/`exec` (the classic `ETXTBSY`
/// race). This closes only half the window — another test thread's
/// `fork` can still inherit a transiently-open fd — so the spawn side
/// (`plugins_protocol::PluginClient::spawn_command`) pairs this with a
/// bounded `ETXTBSY`-only retry.
pub fn mock_plugin_binary(dir: &Path, replies: &[&str]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::io::Write;

        let script_path = dir.join("mock-plugin");
        let body: String = replies
            .iter()
            .map(|r| {
                format!(
                    "IFS= read -r _request || exit 0\nprintf '%s\\n' {}",
                    sh_single_quote_arg(r)
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        let script = format!("#!/usr/bin/env bash\n{body}\n");
        // Explicit create/write/sync/drop instead of `fs::write` so the
        // write fd is provably closed (and the bytes durable) before any
        // caller spawns the script — see the doc comment above.
        let mut file = fs::File::create(&script_path).expect("failed to create mock plugin script");
        file.write_all(script.as_bytes())
            .expect("failed to write mock plugin script");
        file.sync_all().expect("failed to sync mock plugin script");
        drop(file);
        // Already inside the `#[cfg(unix)]` block — no inner attribute needed.
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755))
            .expect("failed to chmod mock plugin");
        script_path
    }

    #[cfg(windows)]
    {
        let script_path = dir.join("mock-plugin.cmd");
        let body: String = replies
            .iter()
            .map(|r| format!("set /p _request=\r\necho {}", escape_cmd_echo_arg(r)))
            .collect::<Vec<_>>()
            .join("\r\n");

        let script = format!("@echo off\r\n{body}\r\n");
        fs::write(&script_path, script).expect("failed to write mock plugin script");
        script_path
    }
}

#[cfg(unix)]
fn sh_single_quote_arg(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(windows)]
fn escape_cmd_echo_arg(value: &str) -> String {
    value
        .replace('^', "^^")
        .replace('&', "^&")
        .replace('|', "^|")
        .replace('<', "^<")
        .replace('>', "^>")
        .replace('%', "%%")
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
        #[cfg(unix)]
        assert_ne!(meta.permissions().mode() & 0o111, 0);
        #[cfg(not(unix))]
        assert!(!meta.permissions().readonly());
    }

    #[test]
    fn mock_plugin_binary_produces_expected_output() {
        use std::io::Write;

        let dir = tempdir();
        let r1 = r#"{"jsonrpc":"2.0","id":1,"result":"init"}"#;
        let r2 = r#"{"jsonrpc":"2.0","id":2,"result":"done"}"#;
        let path = mock_plugin_binary(dir.path(), &[r1, r2]);

        let mut child = std::process::Command::new(&path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("failed to run mock plugin");
        let mut stdin = child.stdin.take().expect("stdin piped");
        stdin.write_all(b"{}\n{}\n").expect("write mock input");
        drop(stdin);
        let output = child.wait_with_output().expect("mock plugin output");
        let stdout = String::from_utf8(output.stdout).unwrap();
        let lines: Vec<&str> = stdout.trim().lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], r1);
        assert_eq!(lines[1], r2);
    }
}
