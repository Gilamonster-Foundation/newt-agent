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

    /// Add a persistent note (only meaningful for `NoteStore`).
    /// Default: return an error explaining the provider doesn't support notes.
    fn add_note(&mut self, _fact: &str) -> anyhow::Result<()> {
        anyhow::bail!("this memory provider does not support persistent notes")
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

    /// Report usage from all providers.
    pub fn usage(&self) -> Vec<(String, usize, usize)> {
        self.providers.iter().filter_map(|p| p.usage()).collect()
    }

    /// Add a fact to the first `NoteStore` provider found.
    /// Returns `Err` if no `NoteStore` is registered or the note is rejected.
    pub fn add_note(&mut self, fact: &str) -> anyhow::Result<()> {
        for p in &mut self.providers {
            // Downcast attempt: NoteStore exposes its name.
            if p.name() == "note_store" {
                // Safe: we know it's a NoteStore because of the name match.
                // Use Any downcasting for the actual mutation.
                // Since we can't easily downcast Box<dyn MemoryProvider>,
                // we use a dedicated add_note method on MemoryProvider.
                return p.add_note(fact);
            }
        }
        anyhow::bail!(
            "no NoteStore registered — add [memory] provider = \"note_store\" to newt.toml"
        )
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
// TokenBudget — built-in provider #2 (closes #106)
// ---------------------------------------------------------------------------

/// Per-turn token record stored by `TokenBudget`.
#[derive(Debug, Clone)]
struct TurnRecord {
    user: String,
    assistant: String,
    /// Total tokens (input + output) for this turn.
    tokens: u32,
}

/// Keep turns up to `threshold_pct` of the model's context window.
///
/// Uses `TurnMetrics.usage` (already collected) to track token consumption.
/// Prunes oldest turns first when approaching the budget.
///
/// Configure via `[memory] provider = "token_budget"` plus an optional
/// `context_tokens` override (default: 8192).
pub struct TokenBudget {
    /// Maximum context tokens (model's `num_ctx`; can be overridden).
    max_tokens: u32,
    /// Prune when used tokens exceed this fraction of `max_tokens`.
    threshold_pct: f32,
    history: Vec<TurnRecord>,
    pruned_count: usize,
}

impl TokenBudget {
    pub fn new(max_tokens: u32, threshold_pct: f32) -> Self {
        Self {
            max_tokens: max_tokens.max(512),
            threshold_pct: threshold_pct.clamp(0.1, 0.99),
            history: Vec::new(),
            pruned_count: 0,
        }
    }

    /// Create from config, defaulting to 8 192 tokens and 80% threshold.
    pub fn from_config() -> Self {
        let max = crate::Config::resolve()
            .ok()
            .and_then(|c| c.memory)
            .and_then(|m| m.context_tokens)
            .unwrap_or(8_192);
        Self::new(max, 0.80)
    }

    fn budget_tokens(&self) -> u32 {
        (self.max_tokens as f32 * self.threshold_pct) as u32
    }

    fn used_tokens(&self) -> u32 {
        self.history.iter().map(|r| r.tokens).sum()
    }

    /// Prune oldest turns until we're within budget. Returns how many were dropped.
    fn prune_to_budget(&mut self) -> usize {
        let budget = self.budget_tokens();
        let mut dropped = 0;
        while self.used_tokens() > budget && !self.history.is_empty() {
            self.history.remove(0);
            dropped += 1;
        }
        dropped
    }
}

#[async_trait]
impl MemoryProvider for TokenBudget {
    fn name(&self) -> &str {
        "token_budget"
    }

    fn build_messages(&self, system_prompt: &str, new_task: &str) -> Vec<MemMessage> {
        let mut msgs = vec![MemMessage::system(system_prompt)];
        for r in &self.history {
            msgs.push(MemMessage::user(&r.user));
            msgs.push(MemMessage::assistant(&r.assistant));
        }
        msgs.push(MemMessage::user(new_task));
        msgs
    }

    async fn sync_turn(&mut self, user: &str, assistant: &str, metrics: &TurnMetrics) {
        let tokens = metrics
            .usage
            .map(|u| u.input_tokens + u.output_tokens)
            .unwrap_or(
                // Rough estimate when Ollama doesn't report counts.
                ((user.len() + assistant.len()) / 4) as u32,
            );
        self.history.push(TurnRecord {
            user: user.to_string(),
            assistant: assistant.to_string(),
            tokens,
        });
        let dropped = self.prune_to_budget();
        self.pruned_count += dropped;
        if dropped > 0 {
            tracing::info!(
                dropped,
                budget = self.budget_tokens(),
                used = self.used_tokens(),
                "TokenBudget pruned old turns"
            );
        }
    }

    fn usage(&self) -> Option<(String, usize, usize)> {
        Some((
            "tokens".into(),
            self.used_tokens() as usize,
            self.budget_tokens() as usize,
        ))
    }
}

// ---------------------------------------------------------------------------
// NoteStore — built-in provider #3 (closes #108)
// ---------------------------------------------------------------------------

/// Persistent agent notes at `~/.newt/NOTES.md`.
///
/// Notes are read once at session start and **frozen** into the system prompt
/// so the model's prefix cache stays valid.  Mid-session writes (via
/// `/remember <fact>`) update the file but NOT the system prompt block —
/// changes take effect next session.
///
/// Modelled on hermes-agent's `MemoryStore` (MEMORY.md pattern).
pub struct NoteStore {
    path: std::path::PathBuf,
    /// Content read at initialize — frozen for the system prompt.
    snapshot: String,
    /// Live content (may differ from snapshot mid-session).
    live: String,
    char_limit: usize,
}

impl NoteStore {
    pub const DEFAULT_CHAR_LIMIT: usize = 2_200;

    pub fn new(path: impl Into<std::path::PathBuf>, char_limit: usize) -> Self {
        Self {
            path: path.into(),
            snapshot: String::new(),
            live: String::new(),
            char_limit: char_limit.max(200),
        }
    }

    /// Create at the default location `~/.newt/NOTES.md`.
    pub fn default_path() -> Self {
        let path = crate::Config::user_config_path()
            .map(|p| p.with_file_name("NOTES.md"))
            .unwrap_or_else(|| std::path::PathBuf::from("NOTES.md"));
        Self::new(path, Self::DEFAULT_CHAR_LIMIT)
    }

    /// Add a fact. Returns `Err` if it would exceed the char limit.
    pub fn add(&mut self, fact: &str) -> anyhow::Result<()> {
        let fact = fact.trim().to_string();
        if fact.is_empty() {
            return Ok(());
        }
        // Reject if already present (exact match).
        if self.live.contains(&fact) {
            return Ok(());
        }
        let separator = if self.live.is_empty() { "" } else { "\n" };
        let candidate = format!("{}{}{}", self.live, separator, fact);
        if candidate.len() > self.char_limit {
            anyhow::bail!(
                "NOTES.md would exceed {} char limit ({}/{} used)",
                self.char_limit,
                self.live.len(),
                self.char_limit
            );
        }
        self.live = candidate;
        self.save()?;
        Ok(())
    }

    /// Remove a fact by exact match.
    pub fn remove(&mut self, fact: &str) -> anyhow::Result<bool> {
        let before = self.live.len();
        self.live = self
            .live
            .lines()
            .filter(|l| l.trim() != fact.trim())
            .collect::<Vec<_>>()
            .join("\n");
        let removed = self.live.len() != before;
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    fn save(&self) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.path, &self.live)?;
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.live.trim().is_empty()
    }

    pub fn char_usage(&self) -> (usize, usize) {
        (self.live.len(), self.char_limit)
    }
}

#[async_trait]
impl MemoryProvider for NoteStore {
    fn name(&self) -> &str {
        "note_store"
    }

    async fn initialize(&mut self, _ctx: &SessionContext) -> anyhow::Result<()> {
        if self.path.exists() {
            self.live = std::fs::read_to_string(&self.path).unwrap_or_default();
        }
        // Freeze the snapshot — this is what goes into the system prompt.
        self.snapshot = self.live.clone();
        Ok(())
    }

    fn system_prompt_block(&self) -> Option<String> {
        if self.snapshot.trim().is_empty() {
            return None;
        }
        Some(format!(
            "## Agent Notes ({}/{})\n{}",
            self.snapshot.len(),
            self.char_limit,
            self.snapshot.trim()
        ))
    }

    fn build_messages(&self, _system_prompt: &str, _new_task: &str) -> Vec<MemMessage> {
        // NoteStore is a system-prompt-only provider — it doesn't manage history.
        Vec::new()
    }

    async fn sync_turn(&mut self, _user: &str, _assistant: &str, _metrics: &TurnMetrics) {}

    fn usage(&self) -> Option<(String, usize, usize)> {
        Some(("notes".into(), self.live.len(), self.char_limit))
    }

    fn add_note(&mut self, fact: &str) -> anyhow::Result<()> {
        self.add(fact)
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

    // --- TokenBudget tests ---

    #[tokio::test]
    async fn token_budget_prunes_oldest_when_over_budget() {
        let mut tb = TokenBudget::new(100, 1.0); // budget = 100 tokens
                                                 // Each turn costs ~50 tokens (200 chars / 4)
        let big = "x".repeat(200);
        tb.sync_turn(&big, &big, &dummy_metrics()).await;
        tb.sync_turn(&big, &big, &dummy_metrics()).await;
        tb.sync_turn(&big, &big, &dummy_metrics()).await;
        // Should have pruned to fit within 100 tokens
        assert!(tb.used_tokens() <= 100);
    }

    #[tokio::test]
    async fn token_budget_uses_metrics_when_available() {
        let mut tb = TokenBudget::new(1000, 1.0);
        let mut m = dummy_metrics();
        m.usage = Some(crate::metrics::TokenUsage {
            input_tokens: 30,
            output_tokens: 20,
        });
        tb.sync_turn("q", "a", &m).await;
        assert_eq!(tb.used_tokens(), 50); // 30 + 20
    }

    // --- NoteStore tests ---

    #[tokio::test]
    async fn note_store_add_and_system_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("NOTES.md");
        let mut ns = NoteStore::new(path, 2200);
        let ctx = SessionContext {
            workspace: "/ws".into(),
            session_id: "s1".into(),
        };
        ns.initialize(&ctx).await.unwrap();
        assert!(ns.system_prompt_block().is_none()); // empty at start

        ns.add("gemma4:e2b is the preferred model").unwrap();
        assert!(ns.live.contains("gemma4:e2b"));
    }

    #[tokio::test]
    async fn note_store_rejects_over_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("NOTES.md");
        let mut ns = NoteStore::new(path, 50);
        ns.initialize(&SessionContext {
            workspace: "/ws".into(),
            session_id: "s".into(),
        })
        .await
        .unwrap();
        let long = "x".repeat(60);
        assert!(ns.add(&long).is_err());
    }

    #[tokio::test]
    async fn note_store_frozen_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("NOTES.md");
        let mut ns = NoteStore::new(path, 2200);
        ns.initialize(&SessionContext {
            workspace: "/ws".into(),
            session_id: "s".into(),
        })
        .await
        .unwrap();
        // Snapshot is empty at init.
        assert!(ns.system_prompt_block().is_none());
        // Add a note mid-session.
        ns.add("new fact").unwrap();
        // Snapshot still empty — frozen.
        assert!(ns.system_prompt_block().is_none());
        assert!(ns.live.contains("new fact"));
    }

    #[tokio::test]
    async fn memory_manager_add_note_routes_to_note_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("NOTES.md");
        let mut mgr = MemoryManager::new();
        mgr.add_provider(RollingWindow::new(5));
        mgr.add_provider(NoteStore::new(path, 2200));
        let ctx = SessionContext {
            workspace: "/ws".into(),
            session_id: "s".into(),
        };
        mgr.initialize_all(&ctx).await;
        mgr.add_note("the answer is 42").unwrap();
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
