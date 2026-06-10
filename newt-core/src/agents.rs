//! Project-instruction provider — loads `AGENTS.md` / `CLAUDE.md` from the
//! workspace and injects them as a frozen system-prompt block.
//!
//! Modelled on [`crate::memory::SoulProvider`] (the soul provider), this is a
//! [`MemoryProvider`] that contributes only a `system_prompt_block()` — it
//! manages no conversation history.
//!
//! ## Resolution
//!
//! - If a `path_override` is configured (via `[agents] path` or
//!   `--agents-file`), that target is used. A relative override is joined onto
//!   the workspace dir; an absolute override is used as-is.
//! - Otherwise the workspace root (`.`) is searched.
//! - A **file** target is loaded as a single instruction block labelled by its
//!   file name.
//! - A **directory** target is scanned for `AGENTS.md` then `CLAUDE.md`; each
//!   that exists is loaded (both if both exist).
//!
//! Files are read once at `initialize()` and **frozen** — mid-session changes
//! don't rebuild the system prompt (preserves the KV/prefix cache). Each file
//! is capped at [`AgentsProvider::MAX_FILE_BYTES`]; larger files are truncated
//! with a `[... truncated]` marker. Empty or unreadable files are skipped
//! gracefully and never error the session.

use async_trait::async_trait;

use crate::memory::{MemMessage, MemoryProvider, SessionContext};
use crate::metrics::TurnMetrics;

/// Loads project instructions (`AGENTS.md` / `CLAUDE.md`) and injects them as a
/// frozen system-prompt block.
pub struct AgentsProvider {
    /// Whether the provider is active. When `false`, contributes nothing.
    enabled: bool,
    /// Optional explicit target: a directory to search or a specific file.
    /// `None` → search the workspace root.
    path_override: Option<String>,
    /// Loaded instruction blocks as `(file_name, contents)`, populated at
    /// `initialize()` and frozen.
    loaded: Vec<(String, String)>,
}

impl AgentsProvider {
    /// Cap each instruction file at this many bytes; larger files are truncated.
    pub const MAX_FILE_BYTES: usize = 65_536;

    /// The instruction file names searched inside a directory target, in order.
    const DIR_CANDIDATES: [&'static str; 2] = ["AGENTS.md", "CLAUDE.md"];

    /// Create a provider.
    ///
    /// `enabled` toggles the whole provider off; `path_override` is an optional
    /// directory-or-file target resolved against the workspace at `initialize`.
    pub fn new(enabled: bool, path_override: Option<String>) -> Self {
        Self {
            enabled,
            path_override,
            loaded: Vec::new(),
        }
    }

    /// Read a file, trim it, skip if empty/unreadable, and truncate if oversize.
    /// Returns the cleaned contents or `None`.
    fn read_capped(path: &std::path::Path) -> Option<String> {
        let raw = std::fs::read_to_string(path).ok()?;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        if trimmed.len() > Self::MAX_FILE_BYTES {
            // Truncate on a char boundary at or below the cap.
            let mut end = Self::MAX_FILE_BYTES;
            while end > 0 && !trimmed.is_char_boundary(end) {
                end -= 1;
            }
            let mut out = trimmed[..end].to_string();
            out.push_str("\n\n[... truncated]");
            Some(out)
        } else {
            Some(trimmed.to_string())
        }
    }

    /// Resolve the search target against the workspace and load instruction
    /// files. Called from `initialize`.
    pub fn load(&mut self, workspace: &str) {
        self.loaded.clear();
        if !self.enabled {
            return;
        }

        let target = match &self.path_override {
            Some(p) => {
                let p = std::path::Path::new(p);
                if p.is_absolute() {
                    p.to_path_buf()
                } else {
                    std::path::Path::new(workspace).join(p)
                }
            }
            None => std::path::PathBuf::from(workspace),
        };

        if target.is_file() {
            let name = target
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "instructions".to_string());
            if let Some(text) = Self::read_capped(&target) {
                self.loaded.push((name, text));
            }
        } else if target.is_dir() {
            for candidate in Self::DIR_CANDIDATES {
                let path = target.join(candidate);
                if let Some(text) = Self::read_capped(&path) {
                    self.loaded.push((candidate.to_string(), text));
                }
            }
        }
    }
}

#[async_trait]
impl MemoryProvider for AgentsProvider {
    fn name(&self) -> &str {
        "agents"
    }

    async fn initialize(&mut self, ctx: &SessionContext) -> anyhow::Result<()> {
        self.load(&ctx.workspace);
        tracing::info!(files = self.loaded.len(), "project instructions loaded");
        Ok(())
    }

    fn system_prompt_block(&self) -> Option<String> {
        if self.loaded.is_empty() {
            return None;
        }
        let mut block = String::from(
            "# Project instructions\n\n\
             Project-specific instructions found in the workspace. Follow them \
             unless they conflict with a direct user request.",
        );
        for (name, contents) in &self.loaded {
            block.push_str("\n\n## ");
            block.push_str(name);
            block.push('\n');
            block.push_str(contents);
        }
        Some(block)
    }

    fn build_messages(&self, _system_prompt: &str, _new_task: &str) -> Vec<MemMessage> {
        // System-prompt-only provider; history is managed elsewhere.
        Vec::new()
    }

    async fn sync_turn(&mut self, _user: &str, _assistant: &str, _metrics: &TurnMetrics) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(workspace: &str) -> SessionContext {
        SessionContext {
            workspace: workspace.to_string(),
            session_id: "s".into(),
        }
    }

    #[tokio::test]
    async fn loads_agents_md_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "Use 4 spaces.").unwrap();

        let mut p = AgentsProvider::new(true, None);
        p.initialize(&ctx(dir.path().to_str().unwrap()))
            .await
            .unwrap();
        let block = p.system_prompt_block().unwrap();
        assert!(block.contains("# Project instructions"));
        assert!(block.contains("## AGENTS.md"));
        assert!(block.contains("Use 4 spaces."));
        assert!(!block.contains("## CLAUDE.md"));
    }

    #[tokio::test]
    async fn loads_claude_md_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "Never push to main.").unwrap();

        let mut p = AgentsProvider::new(true, None);
        p.initialize(&ctx(dir.path().to_str().unwrap()))
            .await
            .unwrap();
        let block = p.system_prompt_block().unwrap();
        assert!(block.contains("## CLAUDE.md"));
        assert!(block.contains("Never push to main."));
        assert!(!block.contains("## AGENTS.md"));
    }

    #[tokio::test]
    async fn loads_both_in_order() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "Agents content.").unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "Claude content.").unwrap();

        let mut p = AgentsProvider::new(true, None);
        p.initialize(&ctx(dir.path().to_str().unwrap()))
            .await
            .unwrap();
        let block = p.system_prompt_block().unwrap();
        let agents_at = block.find("## AGENTS.md").unwrap();
        let claude_at = block.find("## CLAUDE.md").unwrap();
        assert!(agents_at < claude_at, "AGENTS.md should come first");
        assert!(block.contains("Agents content."));
        assert!(block.contains("Claude content."));
    }

    #[tokio::test]
    async fn explicit_file_override_uses_real_filename() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("INSTRUCTIONS.md");
        std::fs::write(&file, "Explicit file body.").unwrap();

        let mut p = AgentsProvider::new(true, Some(file.to_string_lossy().into_owned()));
        // Workspace is unrelated; the absolute override wins.
        p.initialize(&ctx("/nonexistent")).await.unwrap();
        let block = p.system_prompt_block().unwrap();
        assert!(block.contains("## INSTRUCTIONS.md"));
        assert!(block.contains("Explicit file body."));
    }

    #[tokio::test]
    async fn explicit_dir_override_searches_inside() {
        let ws = tempfile::tempdir().unwrap();
        let sub = ws.path().join("subdir");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("AGENTS.md"), "Subdir agents.").unwrap();

        let mut p = AgentsProvider::new(true, Some(sub.to_string_lossy().into_owned()));
        p.initialize(&ctx(ws.path().to_str().unwrap()))
            .await
            .unwrap();
        let block = p.system_prompt_block().unwrap();
        assert!(block.contains("## AGENTS.md"));
        assert!(block.contains("Subdir agents."));
    }

    #[tokio::test]
    async fn disabled_produces_no_block() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "ignored").unwrap();

        let mut p = AgentsProvider::new(false, None);
        p.initialize(&ctx(dir.path().to_str().unwrap()))
            .await
            .unwrap();
        assert!(p.system_prompt_block().is_none());
    }

    #[tokio::test]
    async fn missing_and_empty_produce_no_block() {
        // Missing: empty workspace dir.
        let dir = tempfile::tempdir().unwrap();
        let mut p = AgentsProvider::new(true, None);
        p.initialize(&ctx(dir.path().to_str().unwrap()))
            .await
            .unwrap();
        assert!(p.system_prompt_block().is_none());

        // Empty (whitespace-only) AGENTS.md is skipped.
        let dir2 = tempfile::tempdir().unwrap();
        std::fs::write(dir2.path().join("AGENTS.md"), "   \n\t  ").unwrap();
        let mut p2 = AgentsProvider::new(true, None);
        p2.initialize(&ctx(dir2.path().to_str().unwrap()))
            .await
            .unwrap();
        assert!(p2.system_prompt_block().is_none());
    }

    #[tokio::test]
    async fn oversize_file_is_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let big = "a".repeat(AgentsProvider::MAX_FILE_BYTES + 5_000);
        std::fs::write(dir.path().join("AGENTS.md"), &big).unwrap();

        let mut p = AgentsProvider::new(true, None);
        p.initialize(&ctx(dir.path().to_str().unwrap()))
            .await
            .unwrap();
        let block = p.system_prompt_block().unwrap();
        assert!(block.contains("[... truncated]"));
        // The contents portion should not carry the full oversize payload.
        let (_, contents) = &p.loaded[0];
        assert!(contents.len() < big.len());
        assert!(contents.ends_with("[... truncated]"));
    }

    #[tokio::test]
    async fn relative_override_joined_to_workspace() {
        let ws = tempfile::tempdir().unwrap();
        let docs = ws.path().join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("CLAUDE.md"), "Docs claude.").unwrap();

        // Relative override "docs" is joined onto the workspace.
        let mut p = AgentsProvider::new(true, Some("docs".to_string()));
        p.initialize(&ctx(ws.path().to_str().unwrap()))
            .await
            .unwrap();
        let block = p.system_prompt_block().unwrap();
        assert!(block.contains("## CLAUDE.md"));
        assert!(block.contains("Docs claude."));
    }

    #[tokio::test]
    async fn absolute_override_honored() {
        let other = tempfile::tempdir().unwrap();
        std::fs::write(other.path().join("AGENTS.md"), "Absolute target.").unwrap();

        // Absolute override ignores the (unrelated) workspace.
        let mut p = AgentsProvider::new(true, Some(other.path().to_string_lossy().into_owned()));
        p.initialize(&ctx("/some/other/workspace")).await.unwrap();
        let block = p.system_prompt_block().unwrap();
        assert!(block.contains("Absolute target."));
    }

    #[tokio::test]
    async fn build_messages_is_empty() {
        let p = AgentsProvider::new(true, None);
        assert!(p.build_messages("sys", "task").is_empty());
    }
}
