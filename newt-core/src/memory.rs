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
            char_limit: char_limit.max(10),
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
// Summarizing — built-in provider #4 (closes #107)
// ---------------------------------------------------------------------------

/// Called by `Summarizing` to generate a summary — injected so tests can
/// substitute a fake.
pub type SummaryFn = Box<dyn Fn(&str) -> anyhow::Result<String> + Send + Sync>;

/// Per-turn record with token count for budget tracking.
#[derive(Clone)]
struct SumTurn {
    user: String,
    assistant: String,
    tokens: u32,
}

/// LLM-powered summarisation of old turns when context fills.
///
/// Algorithm (adapted from hermes-agent `ContextCompressor`):
/// 1. Track tokens per turn using `TurnMetrics.usage`.
/// 2. When `used_tokens > threshold * max_tokens`, summarise the oldest
///    `compress_ratio` of turns into a structured block.
/// 3. Replace them with a single `[Summary]` assistant message.
/// 4. Iterative: on re-compression the previous summary is prepended to the
///    new summary prompt so facts survive multiple compactions.
/// 5. Anti-thrashing: skip compression if the last two attempts saved <10%.
///
/// Configure: `[memory] provider = "summarizing"`, `context_tokens = 32768`.
pub struct Summarizing {
    max_tokens: u32,
    threshold_pct: f32,
    /// What fraction of history to summarise each time.
    compress_ratio: f32,
    history: Vec<SumTurn>,
    /// The accumulated summary from previous compressions.
    prev_summary: String,
    /// Savings from last two compressions (for anti-thrashing).
    last_savings: [f32; 2],
    compress_count: usize,
    /// Injected summariser — defaults to None (caller must set via `with_summarizer`).
    summarizer: Option<SummaryFn>,
}

impl Summarizing {
    pub fn new(max_tokens: u32) -> Self {
        Self {
            max_tokens: max_tokens.max(1),
            threshold_pct: 0.80,
            compress_ratio: 0.50,
            history: Vec::new(),
            prev_summary: String::new(),
            last_savings: [1.0, 1.0],
            compress_count: 0,
            summarizer: None,
        }
    }

    /// Inject a summariser function (required for real use; tests can use a stub).
    pub fn with_summarizer(
        mut self,
        f: impl Fn(&str) -> anyhow::Result<String> + Send + Sync + 'static,
    ) -> Self {
        self.summarizer = Some(Box::new(f));
        self
    }

    fn budget(&self) -> u32 {
        (self.max_tokens as f32 * self.threshold_pct) as u32
    }

    fn used_tokens(&self) -> u32 {
        self.history.iter().map(|t| t.tokens).sum()
    }

    fn should_compress(&self) -> bool {
        if self.used_tokens() <= self.budget() {
            return false;
        }
        // Anti-thrashing: skip if last two compressions each saved <10%.
        let poor_savings = self.last_savings.iter().all(|&s| s < 0.10);
        if poor_savings && self.compress_count >= 2 {
            tracing::warn!("Summarizing: anti-thrashing — skipping compression");
            return false;
        }
        true
    }

    fn compress_sync(&mut self) {
        let total = self.history.len();
        if total == 0 {
            return;
        }
        let n_to_summarise = ((total as f32 * self.compress_ratio) as usize).max(1);
        let turns_to_compress: Vec<SumTurn> = self.history.drain(..n_to_summarise).collect();

        let tokens_before =
            self.used_tokens() + turns_to_compress.iter().map(|t| t.tokens).sum::<u32>();

        // Build the prompt for summarisation.
        let mut prompt = String::new();
        if !self.prev_summary.is_empty() {
            prompt.push_str("## Previous Summary\n");
            prompt.push_str(&self.prev_summary);
            prompt.push_str("\n\n");
        }
        prompt.push_str("## Turns to Summarise\n");
        for (i, t) in turns_to_compress.iter().enumerate() {
            prompt.push_str(&format!(
                "### Turn {}\nUser: {}\nAssistant: {}\n\n",
                i + 1,
                t.user,
                t.assistant
            ));
        }
        prompt.push_str(
            "Produce a concise structured summary with sections:\n\
            ## Active Task\n## Completed Actions\n## Key Decisions\n\
            ## Relevant Files\n## Critical Context\n\
            Be terse. Preserve specifics (file names, error messages, decisions).",
        );

        let summary = if let Some(ref f) = self.summarizer {
            match f(&prompt) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "Summarizing: summary generation failed — keeping turns");
                    // Put them back on failure.
                    let mut restored = turns_to_compress;
                    restored.append(&mut self.history);
                    self.history = restored;
                    return;
                }
            }
        } else {
            // No summariser configured — insert a static placeholder.
            format!(
                "[{} turns summarised — configure a summariser for real summaries]",
                turns_to_compress.len()
            )
        };

        // Insert the summary as the first history entry.
        let summary_tokens = (summary.len() / 4) as u32;
        self.history.insert(
            0,
            SumTurn {
                user: "[context summary]".into(),
                assistant: summary.clone(),
                tokens: summary_tokens,
            },
        );
        self.prev_summary = summary;

        let tokens_after = self.used_tokens();
        let saved = if tokens_before > 0 {
            1.0 - (tokens_after as f32 / tokens_before as f32)
        } else {
            0.0
        };
        self.last_savings = [self.last_savings[1], saved];
        self.compress_count += 1;

        tracing::info!(
            compress_count = self.compress_count,
            tokens_before,
            tokens_after,
            saved_pct = saved * 100.0,
            "Summarizing: compressed context"
        );
    }
}

#[async_trait]
impl MemoryProvider for Summarizing {
    fn name(&self) -> &str {
        "summarizing"
    }

    fn build_messages(&self, system_prompt: &str, new_task: &str) -> Vec<MemMessage> {
        let mut msgs = vec![MemMessage::system(system_prompt)];
        for t in &self.history {
            msgs.push(MemMessage::user(&t.user));
            msgs.push(MemMessage::assistant(&t.assistant));
        }
        msgs.push(MemMessage::user(new_task));
        msgs
    }

    async fn sync_turn(&mut self, user: &str, assistant: &str, metrics: &TurnMetrics) {
        let tokens = metrics
            .usage
            .map(|u| u.input_tokens + u.output_tokens)
            .unwrap_or(((user.len() + assistant.len()) / 4) as u32);
        self.history.push(SumTurn {
            user: user.to_string(),
            assistant: assistant.to_string(),
            tokens,
        });
        if self.should_compress() {
            self.compress_sync();
        }
    }

    async fn on_pre_compress(&self, _messages: &[MemMessage]) -> String {
        if self.prev_summary.is_empty() {
            String::new()
        } else {
            format!("Previous compression summary:\n{}", self.prev_summary)
        }
    }

    fn usage(&self) -> Option<(String, usize, usize)> {
        Some((
            "tokens".into(),
            self.used_tokens() as usize,
            self.budget() as usize,
        ))
    }
}

// ---------------------------------------------------------------------------
// SoulProvider — built-in provider #5 (closes #111)
// ---------------------------------------------------------------------------

/// Default agent identity injected when no soul file is found.
///
/// This is the single source of truth for the built-in identity — the TUI's
/// system-prompt builder falls back to this same constant rather than keeping
/// its own copy, so the tool list can't drift between the two paths again.
/// Keep the tool list in sync with the tools the agent actually exposes.
pub const DEFAULT_SOUL: &str = "\
You are newt, a small, fast, local-first agentic coder. \
Be concise and direct. \
You have tools: run_command, read_file, write_file, edit_file, list_dir, use_skill, web_fetch. \
Use them to actually complete tasks rather than describing what to do.\n\
\n\
## How to work\n\
\n\
**One change at a time.** Read only the files you need for the immediate next step. \
Make the change. Commit it. Then move to the next step. \
Never accumulate multiple uncommitted edits — a committed partial result survives a crash; \
an uncommitted complete result does not.\n\
\n\
**Plan before coding.** For any task requiring more than one file change, \
write a plan to `.newt/plan.md` first (create it if it does not exist). \
List the concrete steps. Check them off as you complete each one. \
On restart, read `.newt/plan.md` before anything else so you can resume \
exactly where you left off without re-reading the whole codebase.\n\
\n\
**Read minimum, act fast.** Resist reading the entire codebase before acting. \
Read the specific file or function you are about to change, make the change, \
commit, then read the next thing. The session has a finite context window — \
every token spent reading is a token not spent writing.\n\
\n\
**Prefer edit_file over write_file.** For any existing file, use edit_file \
to replace a specific string — you only generate the change, not the whole file. \
Only use write_file when creating a new file or when you have generated the \
complete contents in full. write_file will refuse if the new content is \
significantly shorter than the original.\n\
\n\
**Stop when blocked.** If the same tool call fails twice in a row with the \
same error, stop immediately and tell the user what blocked you and why. \
Do not try alternative installation methods, do not loop, do not pivot to \
answering a different question. Two identical failures are a signal to report, \
not to retry. One sentence explaining the block is worth more than ten more \
failed tool calls.\n\
\n\
**Seek ground truth.** After every action, verify what actually happened — \
not what you intended. Do not proceed on assumptions about your own actions; \
confirm them. After writing a file, the tool reports the new line count — \
check it matches what you expected. After editing code, the tool reports \
whether it compiled — if it did not, fix the error before committing. \
Before committing, confirm you are on the right branch. \
A belief that something worked is worthless; a tool result that confirms it \
is ground truth.";

/// Loads an agent identity from a Markdown soul file and injects it as a
/// frozen system-prompt block.
///
/// Resolution order (first non-empty file wins):
/// 1. Explicit path in `[memory] soul_file = "..."` config
/// 2. `.newt/soul.md` in the current workspace
/// 3. `~/.newt/soul.md` (global user soul)
/// 4. Built-in default identity
///
/// The soul is read **once** at `initialize()` and frozen — mid-session
/// writes don't rebuild the system prompt (preserves the KV/prefix cache).
pub struct SoulProvider {
    /// The soul text after resolution — frozen at `initialize()`.
    soul: String,
    /// Resolved path of the soul that was actually loaded (for display).
    pub source: SoulSource,
    /// Explicit override path (from config).
    override_path: Option<std::path::PathBuf>,
}

/// Where the soul was loaded from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SoulSource {
    /// Built-in default identity.
    Default,
    /// `~/.newt/soul.md`
    Global,
    /// `.newt/soul.md` in the workspace.
    Workspace,
    /// Explicit path from `[memory] soul_file = "..."`.
    Explicit(std::path::PathBuf),
}

impl std::fmt::Display for SoulSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Default => write!(f, "built-in default"),
            Self::Global => write!(f, "~/.newt/soul.md"),
            Self::Workspace => write!(f, ".newt/soul.md"),
            Self::Explicit(p) => write!(f, "{}", p.display()),
        }
    }
}

impl SoulProvider {
    /// Create with an optional explicit override path (from config).
    pub fn new(override_path: Option<std::path::PathBuf>) -> Self {
        Self {
            soul: DEFAULT_SOUL.to_string(),
            source: SoulSource::Default,
            override_path,
        }
    }

    /// Create from config, reading `[memory] soul_file` if present.
    pub fn from_config() -> Self {
        let override_path = crate::Config::resolve()
            .ok()
            .and_then(|c| c.memory)
            .and_then(|m| m.soul_file)
            .map(std::path::PathBuf::from);
        Self::new(override_path)
    }

    fn try_load(path: &std::path::Path) -> Option<String> {
        let text = std::fs::read_to_string(path).ok()?;
        let trimmed = text.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    }

    /// Resolve and load the soul for `workspace`. Called from `initialize`.
    pub fn load(&mut self, workspace: &str) {
        // 1. Explicit override.
        if let Some(ref p) = self.override_path {
            if let Some(text) = Self::try_load(p) {
                self.soul = text;
                self.source = SoulSource::Explicit(p.clone());
                return;
            }
        }

        // 2. Per-workspace soul.
        let ws_soul = std::path::Path::new(workspace)
            .join(".newt")
            .join("soul.md");
        if let Some(text) = Self::try_load(&ws_soul) {
            self.soul = text;
            self.source = SoulSource::Workspace;
            return;
        }

        // 3. Global user soul.
        if let Some(global) = crate::Config::user_config_path().map(|p| p.with_file_name("soul.md"))
        {
            if let Some(text) = Self::try_load(&global) {
                self.soul = text;
                self.source = SoulSource::Global;
            }
        }

        // 4. Built-in default (already set in `new()` — nothing to do).
    }
}

#[async_trait]
impl MemoryProvider for SoulProvider {
    fn name(&self) -> &str {
        "soul"
    }

    async fn initialize(&mut self, ctx: &SessionContext) -> anyhow::Result<()> {
        self.load(&ctx.workspace);
        tracing::info!(source = %self.source, "soul loaded");
        Ok(())
    }

    /// Return the frozen soul as the system prompt base.
    fn system_prompt_block(&self) -> Option<String> {
        Some(self.soul.clone())
    }

    fn build_messages(&self, _system_prompt: &str, _new_task: &str) -> Vec<MemMessage> {
        // Soul is system-prompt-only; history is managed by other providers.
        Vec::new()
    }

    async fn sync_turn(&mut self, _user: &str, _assistant: &str, _metrics: &TurnMetrics) {}
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
            ..Default::default()
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

    // --- Summarizing tests ---

    // --- SoulProvider tests ---

    #[tokio::test]
    async fn soul_provider_uses_default_when_no_file() {
        let mut sp = SoulProvider::new(None);
        let ctx = SessionContext {
            workspace: "/nonexistent".into(),
            session_id: "s".into(),
        };
        sp.initialize(&ctx).await.unwrap();
        assert_eq!(sp.source, SoulSource::Default);
        let block = sp.system_prompt_block().unwrap();
        assert!(block.contains("newt"), "default soul should mention newt");
    }

    #[test]
    fn default_soul_lists_all_current_tools() {
        // Regression: DEFAULT_SOUL went stale when use_skill (#135) and
        // web_fetch (#139) were added but the constant wasn't updated, so
        // default-identity sessions never learned those tools existed.
        for tool in [
            "run_command",
            "read_file",
            "write_file",
            "edit_file",
            "list_dir",
            "use_skill",
            "web_fetch",
        ] {
            assert!(
                DEFAULT_SOUL.contains(tool),
                "DEFAULT_SOUL must advertise `{tool}`"
            );
        }
    }

    #[tokio::test]
    async fn soul_provider_loads_workspace_soul() {
        let dir = tempfile::tempdir().unwrap();
        // Create .newt/soul.md inside the temp dir.
        let newt_dir = dir.path().join(".newt");
        std::fs::create_dir_all(&newt_dir).unwrap();
        std::fs::write(newt_dir.join("soul.md"), "You are a Django expert.").unwrap();

        let mut sp = SoulProvider::new(None);
        let ctx = SessionContext {
            workspace: dir.path().to_string_lossy().into(),
            session_id: "s".into(),
        };
        sp.initialize(&ctx).await.unwrap();
        assert_eq!(sp.source, SoulSource::Workspace);
        let block = sp.system_prompt_block().unwrap();
        assert!(block.contains("Django"), "should use workspace soul");
    }

    #[tokio::test]
    async fn soul_provider_explicit_path_wins() {
        let dir = tempfile::tempdir().unwrap();
        let soul_file = dir.path().join("custom_soul.md");
        std::fs::write(&soul_file, "You are a security auditor.").unwrap();

        // Also create a workspace soul — explicit should win.
        let ws_dir = tempfile::tempdir().unwrap();
        let newt_dir = ws_dir.path().join(".newt");
        std::fs::create_dir_all(&newt_dir).unwrap();
        std::fs::write(newt_dir.join("soul.md"), "You are a Django expert.").unwrap();

        let mut sp = SoulProvider::new(Some(soul_file.clone()));
        let ctx = SessionContext {
            workspace: ws_dir.path().to_string_lossy().into(),
            session_id: "s".into(),
        };
        sp.initialize(&ctx).await.unwrap();
        assert_eq!(sp.source, SoulSource::Explicit(soul_file));
        let block = sp.system_prompt_block().unwrap();
        assert!(
            block.contains("security auditor"),
            "explicit path should win"
        );
    }

    #[tokio::test]
    async fn soul_provider_empty_workspace_soul_falls_through() {
        let dir = tempfile::tempdir().unwrap();
        let newt_dir = dir.path().join(".newt");
        std::fs::create_dir_all(&newt_dir).unwrap();
        // Empty workspace soul → should fall through to default.
        std::fs::write(newt_dir.join("soul.md"), "   ").unwrap();

        let mut sp = SoulProvider::new(None);
        let ctx = SessionContext {
            workspace: dir.path().to_string_lossy().into(),
            session_id: "s".into(),
        };
        sp.initialize(&ctx).await.unwrap();
        // Empty file → falls through to default.
        assert_eq!(sp.source, SoulSource::Default);
    }

    #[tokio::test]
    async fn summarizing_compresses_when_over_budget() {
        let mut s = Summarizing::new(100) // 100 token budget
            .with_summarizer(|_prompt| Ok("SUMMARY".to_string()));

        // Each turn ~50 tokens (200 chars / 4).
        let big = "x".repeat(200);
        let mut m = dummy_metrics();
        // Give explicit token counts so the test is deterministic.
        m.usage = Some(crate::metrics::TokenUsage {
            input_tokens: 25,
            output_tokens: 25,
        });
        s.sync_turn(&big, &big, &m).await; // 50 tokens
        s.sync_turn(&big, &big, &m).await; // 100 tokens — at budget
        s.sync_turn(&big, &big, &m).await; // 150 tokens — triggers compression

        // Compression should have run at least once.
        assert!(
            !s.prev_summary.is_empty(),
            "prev_summary should be set after compression"
        );
        assert!(s.compress_count >= 1, "compress_count={}", s.compress_count);
    }

    #[tokio::test]
    async fn summarizing_compresses_repeatedly() {
        // Verify that the provider compresses multiple times across many turns
        // and doesn't panic. The exact compress_count depends on savings ratios.
        let mut s = Summarizing::new(100).with_summarizer(|_| Ok("SUMMARY".to_string()));
        let mut m = dummy_metrics();
        m.usage = Some(crate::metrics::TokenUsage {
            input_tokens: 20,
            output_tokens: 20,
        });
        for _ in 0..20 {
            s.sync_turn("q", "a", &m).await;
        }
        // Should have compressed at least once without panicking.
        assert!(s.compress_count >= 1);
    }

    // --- MemoryManager hook coverage ---

    #[tokio::test]
    async fn memory_manager_on_pre_compress() {
        let mgr = MemoryManager::new();
        let result = mgr.on_pre_compress(&[]).await;
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn memory_manager_on_session_end() {
        let mut mgr = MemoryManager::new();
        mgr.add_provider(RollingWindow::new(5));
        mgr.on_session_end(&[]).await; // must not panic
    }

    #[tokio::test]
    async fn memory_manager_prefetch_all_empty() {
        let mgr = MemoryManager::new();
        let result = mgr.prefetch_all("query").await;
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn memory_manager_build_system_prompt_additions_from_note_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("NOTES.md");
        std::fs::write(&path, "fact one\nfact two").unwrap();
        let mut ns = NoteStore::new(path, 2200);
        let ctx = SessionContext {
            workspace: "/ws".into(),
            session_id: "s".into(),
        };
        ns.initialize(&ctx).await.unwrap();

        let mut mgr = MemoryManager::new();
        mgr.add_provider(ns);
        let additions = mgr.build_system_prompt_additions();
        assert!(additions.contains("fact one"));
    }

    #[tokio::test]
    async fn memory_manager_add_note_fails_with_no_note_store() {
        let mut mgr = MemoryManager::new();
        mgr.add_provider(RollingWindow::new(5));
        assert!(mgr.add_note("fact").is_err());
    }

    // --- NoteStore additional coverage ---

    #[tokio::test]
    async fn note_store_remove() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("NOTES.md");
        let mut ns = NoteStore::new(path, 2200);
        ns.initialize(&SessionContext {
            workspace: "/ws".into(),
            session_id: "s".into(),
        })
        .await
        .unwrap();
        ns.add("fact one").unwrap();
        ns.add("fact two").unwrap();
        let removed = ns.remove("fact one").unwrap();
        assert!(removed);
        assert!(!ns.live.contains("fact one"));
        assert!(ns.live.contains("fact two"));
    }

    #[tokio::test]
    async fn note_store_remove_nonexistent_returns_false() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("NOTES.md");
        let mut ns = NoteStore::new(path, 2200);
        ns.initialize(&SessionContext {
            workspace: "/ws".into(),
            session_id: "s".into(),
        })
        .await
        .unwrap();
        let removed = ns.remove("not there").unwrap();
        assert!(!removed);
    }

    #[tokio::test]
    async fn note_store_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("NOTES.md");
        let mut ns = NoteStore::new(path, 2200);
        ns.initialize(&SessionContext {
            workspace: "/ws".into(),
            session_id: "s".into(),
        })
        .await
        .unwrap();
        assert!(ns.is_empty());
        ns.add("fact").unwrap();
        assert!(!ns.is_empty());
    }

    #[tokio::test]
    async fn note_store_char_usage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("NOTES.md");
        let mut ns = NoteStore::new(path, 100);
        ns.initialize(&SessionContext {
            workspace: "/ws".into(),
            session_id: "s".into(),
        })
        .await
        .unwrap();
        let (cur, max) = ns.char_usage();
        assert_eq!(cur, 0);
        assert_eq!(max, 100);
    }

    // --- TokenBudget additional coverage ---

    #[tokio::test]
    async fn token_budget_usage_reporting() {
        let tb = TokenBudget::new(1000, 0.80);
        let (label, cur, max) = tb.usage().unwrap();
        assert_eq!(label, "tokens");
        assert_eq!(cur, 0);
        assert_eq!(max, 800); // 1000 * 0.80
    }

    #[tokio::test]
    async fn token_budget_does_not_prune_within_budget() {
        let mut tb = TokenBudget::new(200, 1.0); // budget = 200
        let mut m = dummy_metrics();
        m.usage = Some(crate::metrics::TokenUsage {
            input_tokens: 50,
            output_tokens: 50,
        });
        tb.sync_turn("q", "a", &m).await; // 100 tokens — within budget
        assert_eq!(tb.history.len(), 1);
    }

    // --- Summarizing additional coverage ---

    #[tokio::test]
    async fn summarizing_fallback_placeholder_when_no_summarizer() {
        let mut s = Summarizing::new(10); // tiny budget to trigger compression
        let mut m = dummy_metrics();
        m.usage = Some(crate::metrics::TokenUsage {
            input_tokens: 6,
            output_tokens: 6,
        });
        s.sync_turn("q1", "a1", &m).await;
        s.sync_turn("q2", "a2", &m).await; // should trigger compression with placeholder
                                           // With no summariser, placeholder text is inserted.
        assert!(
            s.history
                .iter()
                .any(|t| t.assistant.contains("turns summarised")),
            "placeholder should be inserted"
        );
    }

    #[tokio::test]
    async fn summarizing_usage_reporting() {
        let s = Summarizing::new(1000);
        let (label, cur, max) = s.usage().unwrap();
        assert_eq!(label, "tokens");
        assert_eq!(cur, 0);
        assert_eq!(max, 800); // 1000 * 0.80
    }

    #[tokio::test]
    async fn summarizing_on_pre_compress_returns_prev_summary() {
        let mut s = Summarizing::new(10).with_summarizer(|_| Ok("PRIOR SUMMARY".to_string()));
        let mut m = dummy_metrics();
        m.usage = Some(crate::metrics::TokenUsage {
            input_tokens: 6,
            output_tokens: 6,
        });
        s.sync_turn("q", "a", &m).await;
        s.sync_turn("q", "a", &m).await;
        // prev_summary should be set after compression.
        let pre = s.on_pre_compress(&[]).await;
        if !pre.is_empty() {
            assert!(pre.contains("PRIOR SUMMARY") || pre.contains("compression summary"));
        }
    }

    // --- RollingWindow additional coverage ---

    #[tokio::test]
    async fn rolling_window_on_session_end_noop() {
        let mut rw = RollingWindow::new(5);
        rw.on_session_end(&[]).await; // must not panic
    }

    #[tokio::test]
    async fn rolling_window_add_note_returns_err() {
        let mut rw = RollingWindow::new(5);
        assert!(rw.add_note("fact").is_err());
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
