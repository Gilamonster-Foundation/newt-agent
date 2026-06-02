//! Pluggable memory architecture for newt's chat REPL.
//!
//! Modelled on hermes-agent's `MemoryProvider` ABC + `MemoryManager`
//! orchestrator, adapted for Rust and newt's local-first constraints.
//!
//! ## Design principles (from hermes-agent)
//!
//! - **Fault isolation** — one provider's panic/error never blocks others.
//! - **Frozen system prompt** — `system_prompt_block()` is captured once at
//!   session start and never rebuilt mid-session (preserves the model's prefix
//!   cache / KV cache across turns).
//! - **Non-blocking sync** — `sync_turn` should queue writes; the chat loop
//!   never waits on memory backends.
//! - **Single integration point** — `MemoryManager` is the only thing the
//!   chat loop interacts with; it fans out to all registered providers.
//!
//! ## Built-in providers (shipped in order)
//!
//! | Provider | Issue |
//! |---|---|
//! | `RollingWindow` | #105 — keep last N turns (ships here) |
//! | `TokenBudget`   | #106 — prune by context-window % |
//! | `Summarizing`   | #107 — LLM summarization of old turns |
//! | `NoteStore`     | #108 — persistent NOTES.md the agent writes to |

use async_trait::async_trait;

use crate::metrics::TurnMetrics;

// ---------------------------------------------------------------------------
// Message type (mirrors the inference layer without creating a dep cycle)
// ---------------------------------------------------------------------------

/// A single message in a conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemMessage {
    pub role: Role,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

impl MemMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Session context passed to providers at initialization
// ---------------------------------------------------------------------------

/// Context provided to each `MemoryProvider::initialize` call.
#[derive(Debug, Clone)]
pub struct SessionContext {
    /// Absolute path to the current workspace.
    pub workspace: String,
    /// Identifier for this session (timestamp-based or UUID).
    pub session_id: String,
}

// ---------------------------------------------------------------------------
// MemoryProvider trait
// ---------------------------------------------------------------------------

/// Contract for all memory backends.
///
/// Implement this trait to create a custom memory backend — rolling window,
/// vector store, summarisation engine, or anything else.
#[async_trait]
pub trait MemoryProvider: Send + Sync {
    /// Short stable identifier, e.g. `"rolling_window"`.
    fn name(&self) -> &str;

    /// One-time setup at session start (load files, warm caches, etc.).
    /// Default: no-op.
    async fn initialize(&mut self, _ctx: &SessionContext) -> anyhow::Result<()> {
        Ok(())
    }

    /// Return a static block for the system prompt.
    ///
    /// Called once at session start and **frozen** — mid-session writes to
    /// memory must NOT change this return value (keeps the KV/prefix cache
    /// valid across all turns).  Return `None` to contribute nothing.
    fn system_prompt_block(&self) -> Option<String> {
        None
    }

    /// Recall relevant context to prepend to the user turn **before** the API
    /// call.  Should be fast — use cached results, don't block.
    /// Return empty string to contribute nothing.
    async fn prefetch(&self, _query: &str) -> anyhow::Result<String> {
        Ok(String::new())
    }

    /// Build the full message list (including history) to send to the model.
    ///
    /// **This is where history management lives.**  Implementations decide:
    /// - How many past turns to include
    /// - Whether to summarise old turns
    /// - Where to inject `prefetch` context
    ///
    /// `system_prompt` is the fully-assembled system message content.
    /// `new_task` is the current user turn.
    fn build_messages(&self, system_prompt: &str, new_task: &str) -> Vec<MemMessage>;

    /// Persist a completed turn.
    ///
    /// Implementations **should** queue writes and return immediately so the
    /// chat loop never blocks on memory I/O.
    async fn sync_turn(&mut self, user: &str, assistant: &str, metrics: &TurnMetrics);

    /// Called **before** old messages are discarded (e.g. during compression).
    /// Extract anything worth keeping from `messages`; return it as a string
    /// to include in the compression summary. Return empty string for nothing.
    async fn on_pre_compress(&self, _messages: &[MemMessage]) -> String {
        String::new()
    }

    /// Called once when the session ends. Use for final extraction / cleanup.
    async fn on_session_end(&mut self, _messages: &[MemMessage]) {}

    /// Report current usage for display (e.g. `/memory` command).
    /// Returns `(label, current, max)` — e.g. `("turns", 12, 20)`.
    fn usage(&self) -> Option<(String, usize, usize)> {
        None
    }
}

// ---------------------------------------------------------------------------
// MemoryManager — single integration point
// ---------------------------------------------------------------------------

/// Orchestrates all registered `MemoryProvider`s.
///
/// The chat loop interacts exclusively with `MemoryManager`; individual
/// providers are invisible to the rest of the codebase.  Provider errors are
/// caught and logged so one failure never blocks others.
pub struct MemoryManager {
    providers: Vec<Box<dyn MemoryProvider>>,
}

impl MemoryManager {
    pub fn new() -> Self {
        Self {
            providers: Vec::new(),
        }
    }

    /// Register a provider. Providers are consulted in registration order.
    pub fn add_provider(&mut self, p: impl MemoryProvider + 'static) {
        self.providers.push(Box::new(p));
    }

    /// Initialize all providers. Called once at session start.
    pub async fn initialize_all(&mut self, ctx: &SessionContext) {
        for p in &mut self.providers {
            if let Err(e) = p.initialize(ctx).await {
                tracing::warn!(provider = p.name(), error = %e, "memory provider init failed");
            }
        }
    }

    /// Assemble the frozen system-prompt contribution from all providers.
    /// Call once at session start and cache the result.
    pub fn build_system_prompt_additions(&self) -> String {
        self.providers
            .iter()
            .filter_map(|p| p.system_prompt_block())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// Collect recalled context from all providers before a turn.
    pub async fn prefetch_all(&self, query: &str) -> String {
        let mut parts = Vec::new();
        for p in &self.providers {
            match p.prefetch(query).await {
                Ok(s) if !s.is_empty() => parts.push(s),
                Err(e) => {
                    tracing::warn!(provider = p.name(), error = %e, "prefetch failed");
                }
                _ => {}
            }
        }
        parts.join("\n\n")
    }

    /// Build the message list for the next API call.
    ///
    /// Uses the **first** provider that implements `build_messages` to a
    /// non-empty result.  Falls back to a minimal [system, user] pair.
    pub fn build_messages(&self, system_prompt: &str, new_task: &str) -> Vec<MemMessage> {
        for p in &self.providers {
            let msgs = p.build_messages(system_prompt, new_task);
            if !msgs.is_empty() {
                return msgs;
            }
        }
        // Minimal fallback (should not happen with at least one provider).
        vec![
            MemMessage::system(system_prompt),
            MemMessage::user(new_task),
        ]
    }

    /// Persist a completed turn to all providers.
    pub async fn sync_all(&mut self, user: &str, assistant: &str, metrics: &TurnMetrics) {
        for p in &mut self.providers {
            p.sync_turn(user, assistant, metrics).await;
        }
    }

    /// Notify all providers before old messages are dropped.
    pub async fn on_pre_compress(&self, messages: &[MemMessage]) -> String {
        let mut parts = Vec::new();
        for p in &self.providers {
            let s = p.on_pre_compress(messages).await;
            if !s.is_empty() {
                parts.push(s);
            }
        }
        parts.join("\n\n")
    }

    /// Notify all providers at session end.
    pub async fn on_session_end(&mut self, messages: &[MemMessage]) {
        for p in &mut self.providers {
            p.on_session_end(messages).await;
        }
    }

    /// Report usage from the first provider that has something to say.
    pub fn usage(&self) -> Vec<(String, usize, usize)> {
        self.providers.iter().filter_map(|p| p.usage()).collect()
    }
}

impl Default for MemoryManager {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// RollingWindow — built-in provider #1 (closes #105)
// ---------------------------------------------------------------------------

/// Keep the last `max_turns` conversation turns; discard older ones.
///
/// Simple, predictable, zero overhead.  The default provider.
/// Configure via `[memory] window = 20` in `newt.toml`.
pub struct RollingWindow {
    max_turns: usize,
    /// Each entry is `(user_message, assistant_message)`.
    history: Vec<(String, String)>,
}

impl RollingWindow {
    /// Create a rolling window that retains the last `max_turns` turns.
    pub fn new(max_turns: usize) -> Self {
        Self {
            max_turns: max_turns.max(1),
            history: Vec::new(),
        }
    }

    /// Create from config, falling back to the default window size.
    pub fn from_config() -> Self {
        let window = newt_core_memory_window();
        Self::new(window)
    }

    pub fn turn_count(&self) -> usize {
        self.history.len()
    }
}

fn newt_core_memory_window() -> usize {
    crate::Config::resolve()
        .ok()
        .and_then(|c| c.memory)
        .map(|m| m.window)
        .unwrap_or(20)
}

#[async_trait]
impl MemoryProvider for RollingWindow {
    fn name(&self) -> &str {
        "rolling_window"
    }

    fn build_messages(&self, system_prompt: &str, new_task: &str) -> Vec<MemMessage> {
        let mut msgs = vec![MemMessage::system(system_prompt)];

        // Include the retained history window.
        let start = self.history.len().saturating_sub(self.max_turns);
        for (user, asst) in &self.history[start..] {
            msgs.push(MemMessage::user(user));
            msgs.push(MemMessage::assistant(asst));
        }

        // Current turn.
        msgs.push(MemMessage::user(new_task));
        msgs
    }

    async fn sync_turn(&mut self, user: &str, assistant: &str, _metrics: &TurnMetrics) {
        self.history.push((user.to_string(), assistant.to_string()));
        // Keep only the last max_turns entries in storage too, so memory
        // doesn't grow unboundedly over very long sessions.
        if self.history.len() > self.max_turns * 2 {
            let drain_to = self.history.len() - self.max_turns;
            self.history.drain(..drain_to);
        }
    }

    fn usage(&self) -> Option<(String, usize, usize)> {
        Some((
            "turns".into(),
            self.history.len().min(self.max_turns),
            self.max_turns,
        ))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::TokenUsage;

    fn dummy_metrics() -> TurnMetrics {
        TurnMetrics {
            elapsed_ms: 100,
            usage: Some(TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
            }),
            cost_usd: Some(0.0),
            model_id: "test".into(),
            endpoint: "http://localhost".into(),
        }
    }

    #[tokio::test]
    async fn rolling_window_empty_produces_two_messages() {
        let rw = RollingWindow::new(5);
        let msgs = rw.build_messages("sys", "hello");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, Role::System);
        assert_eq!(msgs[1].role, Role::User);
        assert_eq!(msgs[1].content, "hello");
    }

    #[tokio::test]
    async fn rolling_window_includes_history() {
        let mut rw = RollingWindow::new(5);
        rw.sync_turn("q1", "a1", &dummy_metrics()).await;
        rw.sync_turn("q2", "a2", &dummy_metrics()).await;
        let msgs = rw.build_messages("sys", "q3");
        // system + (q1,a1) + (q2,a2) + q3 = 6
        assert_eq!(msgs.len(), 6);
        assert_eq!(msgs[1].content, "q1");
        assert_eq!(msgs[2].content, "a1");
        assert_eq!(msgs[5].content, "q3");
    }

    #[tokio::test]
    async fn rolling_window_caps_at_max_turns() {
        let mut rw = RollingWindow::new(2);
        for i in 0..5u32 {
            rw.sync_turn(&format!("q{i}"), &format!("a{i}"), &dummy_metrics())
                .await;
        }
        let msgs = rw.build_messages("sys", "q5");
        // system + 2 turns * 2 messages + current = 6
        assert_eq!(msgs.len(), 6);
        // The last 2 turns should be q3/a3 and q4/a4
        assert_eq!(msgs[1].content, "q3");
        assert_eq!(msgs[3].content, "q4");
        assert_eq!(msgs[5].content, "q5");
    }

    #[tokio::test]
    async fn rolling_window_usage_reports_correctly() {
        let mut rw = RollingWindow::new(10);
        rw.sync_turn("q", "a", &dummy_metrics()).await;
        rw.sync_turn("q", "a", &dummy_metrics()).await;
        let (label, cur, max) = rw.usage().unwrap();
        assert_eq!(label, "turns");
        assert_eq!(cur, 2);
        assert_eq!(max, 10);
    }

    #[tokio::test]
    async fn memory_manager_routes_to_provider() {
        let mut mgr = MemoryManager::new();
        mgr.add_provider(RollingWindow::new(5));
        let msgs = mgr.build_messages("sys", "hello");
        assert_eq!(msgs[0].role, Role::System);
        assert_eq!(msgs.last().unwrap().content, "hello");
    }

    #[tokio::test]
    async fn memory_manager_sync_all() {
        let mut mgr = MemoryManager::new();
        mgr.add_provider(RollingWindow::new(5));
        mgr.sync_all("q", "a", &dummy_metrics()).await;
        let usage = mgr.usage();
        assert_eq!(usage[0].1, 1); // 1 turn stored
    }

    #[tokio::test]
    async fn memory_manager_fallback_with_no_providers() {
        let mgr = MemoryManager::new();
        let msgs = mgr.build_messages("sys", "task");
        assert_eq!(msgs.len(), 2);
    }
}
