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

use crate::agentic::compress::is_compaction_text;
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

    /// Clear conversation-local history while preserving provider configuration
    /// and system-prompt state. Used when the TUI starts a fresh conversation
    /// inside the same running process.
    fn reset(&mut self) {}

    /// Replace conversation-local history with durable restored turns.
    ///
    /// The default is a no-op for providers without conversation-local state.
    /// Providers that include prior user/assistant turns in `build_messages`
    /// must override this, otherwise `/conversation restore` will silently
    /// leave their in-memory history unchanged.
    fn restore_turns(&mut self, _turns: &[crate::ConversationTurn]) {}

    /// Take (and clear) the compaction record minted since the last call —
    /// the full marked summary message a compressing provider inserted into
    /// its working set (Step 18.5, #247). The caller persists it as a turn
    /// record (`user` = the marked message, `assistant` = empty, token
    /// columns NULL — it is not a backend-measured turn) so a later restore
    /// can rehydrate the same working-set shape via [`Self::restore_turns`].
    /// Default: `None` — providers that never compress mint nothing.
    fn take_compaction_record(&mut self) -> Option<String> {
        None
    }

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

    /// Add a persistent note.
    ///
    /// Providers that persist notes (e.g. `NoteStore`) override this.
    /// Default: return [`NotesUnsupported`], which `MemoryManager::add_note`
    /// recognises and skips while looking for a note-capable provider.
    fn add_note(&mut self, _fact: &str) -> anyhow::Result<()> {
        Err(NotesUnsupported.into())
    }

    /// Replace the single persisted note containing `old_substring` with
    /// `new_text` (Step 19.3 — the `save_note` tool's replace action).
    /// Default: [`NotesUnsupported`], same routing contract as `add_note`.
    fn replace_note(&mut self, _old_substring: &str, _new_text: &str) -> anyhow::Result<()> {
        Err(NotesUnsupported.into())
    }

    /// Remove the single persisted note containing `substring` (Step 19.3 —
    /// the `save_note` tool's remove action).
    /// Default: [`NotesUnsupported`], same routing contract as `add_note`.
    fn remove_note(&mut self, _substring: &str) -> anyhow::Result<()> {
        Err(NotesUnsupported.into())
    }
}

/// Error returned by the default [`MemoryProvider::add_note`] for providers
/// that don't persist notes.
///
/// `MemoryManager::add_note` skips providers returning this and keeps
/// looking; any *other* error (e.g. the over-budget curator error from
/// `NoteStore`) is surfaced to the caller.
#[derive(Debug, thiserror::Error)]
#[error("this memory provider does not support persistent notes")]
pub struct NotesUnsupported;

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

    /// Clear conversation-local history from every provider.
    pub fn reset_all(&mut self) {
        for p in &mut self.providers {
            p.reset();
        }
    }

    /// Replace conversation-local history in every provider from durable turns.
    pub fn restore_turns(&mut self, turns: &[crate::ConversationTurn]) {
        for p in &mut self.providers {
            p.restore_turns(turns);
        }
    }

    /// Drain the first pending compaction record from any provider — the
    /// TUI's save site calls this after `sync_all` and persists the result
    /// as a turn record (Step 18.5, #247).
    pub fn take_compaction_record(&mut self) -> Option<String> {
        self.providers
            .iter_mut()
            .find_map(|p| p.take_compaction_record())
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

    /// Add a fact to the first provider that accepts it (first `Ok` wins).
    ///
    /// No name-based special-casing: every provider is offered the note via
    /// the trait's `add_note`. Providers whose default returns
    /// [`NotesUnsupported`] are skipped; the first *real* rejection (e.g.
    /// `NoteStore`'s over-budget curator error) is surfaced if no provider
    /// accepts the note.
    pub fn add_note(&mut self, fact: &str) -> anyhow::Result<()> {
        self.route_note_op(|p| p.add_note(fact))
    }

    /// Replace the single persisted note containing `old_substring` —
    /// same first-note-capable-provider routing as [`Self::add_note`].
    pub fn replace_note(&mut self, old_substring: &str, new_text: &str) -> anyhow::Result<()> {
        self.route_note_op(|p| p.replace_note(old_substring, new_text))
    }

    /// Remove the single persisted note containing `substring` —
    /// same first-note-capable-provider routing as [`Self::add_note`].
    pub fn remove_note(&mut self, substring: &str) -> anyhow::Result<()> {
        self.route_note_op(|p| p.remove_note(substring))
    }

    /// Shared routing for note mutations: offer the operation to every
    /// provider in registration order; first `Ok` wins, [`NotesUnsupported`]
    /// is skipped, and the first *real* rejection (over-budget curator error,
    /// scan rejection, ambiguous-substring error) is surfaced if no provider
    /// accepts.
    fn route_note_op(
        &mut self,
        mut op: impl FnMut(&mut dyn MemoryProvider) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let mut first_err: Option<anyhow::Error> = None;
        for p in &mut self.providers {
            match op(p.as_mut()) {
                Ok(()) => return Ok(()),
                Err(e) if e.is::<NotesUnsupported>() => continue,
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => anyhow::bail!(
                "no note-capable memory provider registered — add [memory] provider = \"note_store\" to newt.toml"
            ),
        }
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

        // Include the retained history window. A restored compaction record
        // (Step 18.5) is a user-side summary with no reply — never dispatch
        // an empty assistant message for it.
        let start = self.history.len().saturating_sub(self.max_turns);
        for (user, asst) in &self.history[start..] {
            msgs.push(MemMessage::user(user));
            if !asst.is_empty() {
                msgs.push(MemMessage::assistant(asst));
            }
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

    fn reset(&mut self) {
        self.history.clear();
    }

    fn restore_turns(&mut self, turns: &[crate::ConversationTurn]) {
        self.history = turns
            .iter()
            .map(|t| (t.user.clone(), t.assistant.clone()))
            .collect();
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

/// Static context-token budget used only when neither an explicit
/// `[memory] context_tokens` override nor empirical capability data exists
/// (a fresh model with no probe history — Step 18.2, #247).
///
/// This is the LAST tier of the budget precedence. Callers that have probe
/// capability data (the TUI's `model-capabilities.json`) must resolve a
/// budget from it and inject the value at provider construction
/// ([`TokenBudget::new`] / [`Summarizing::new`] / `with_budget`) — the old
/// `from_config()` path that silently fell back to this constant while
/// ignoring probe data was deleted in Step 18.2.
pub const DEFAULT_CONTEXT_TOKENS: u32 = 8_192;

/// Per-turn record stored by `TokenBudget`.
#[derive(Debug, Clone)]
struct TurnRecord {
    user: String,
    assistant: String,
    /// chars/4 (ceiling) estimate of THIS turn's content only — what dropping
    /// the turn removes from future prompts. NOT the backend usage reading:
    /// `input_tokens` already contains every prior turn, so storing and
    /// summing it per turn double-counts history (Step 18.1).
    est_tokens: u32,
}

/// `(len + 3) / 4` ceiling estimate of one turn's content contribution.
fn turn_content_estimate(user: &str, assistant: &str) -> u32 {
    (user.len().div_ceil(4) + assistant.len().div_ceil(4)) as u32
}

/// Column-first token anchor for restored history (Step 18.5, #247).
///
/// Finds the LAST turn carrying a backend-reported `tokens_in` (the 17.6
/// column) and reconstructs the anchored state the live session had right
/// after that turn's `sync_turn`: `last_prompt_tokens` = the measured prompt
/// (which already contained the system prompt and every prior turn), and the
/// delta = the chars/4 estimate of that turn's reply plus every LATER
/// (unmeasured) turn's content — the same arithmetic the live path applies
/// per turn. Rows with NULL columns (pre-17.6 rows, silent backends)
/// contribute estimates to the delta but never become the anchor: a fallback
/// estimate is never presented as a measurement (18.1 semantics). No
/// measured turn at all → `(None, 0)`, and the provider falls back to
/// summing per-turn content estimates exactly as before.
fn restored_token_anchor(turns: &[crate::ConversationTurn]) -> (Option<u32>, i64) {
    let Some(pos) = turns.iter().rposition(|t| t.tokens_in.is_some()) else {
        return (None, 0);
    };
    let mut delta = turns[pos].assistant.len().div_ceil(4) as i64;
    for t in &turns[pos + 1..] {
        delta += i64::from(turn_content_estimate(&t.user, &t.assistant));
    }
    (turns[pos].tokens_in, delta)
}

/// Keep turns up to `threshold_pct` of the model's context window.
///
/// Budget math (fixed in Step 18.1): the fullness figure anchors on the
/// backend's last-reported prompt size — which already includes the system
/// prompt and ALL prior turns — plus a chars/4 estimate of content added or
/// removed since that report. The previous implementation summed
/// `input + output` per turn across history, double-counting every prior turn
/// in every later reading (the B3 baseline measured 5.4× inflation by turn
/// 20), which forced spurious pruning. Without any backend report the figure
/// falls back to summing per-turn content estimates (each turn counted once).
///
/// Configure via `[memory] provider = "token_budget"`. The budget is a
/// construction-time value injected by the caller (Step 18.2, #247) with
/// this precedence:
///
/// 1. explicit `[memory] context_tokens` (a deliberate user override),
/// 2. capability-derived (`max_ok_input` else `safe_context` from the
///    caller's probe cache — newt-core deliberately has no dependency on
///    the probe types, mirroring the `with_summarizer` injection),
/// 3. [`DEFAULT_CONTEXT_TOKENS`] when neither exists (fresh model).
///
/// The value is fixed for the provider's lifetime: budgets refresh per
/// session; mid-session capability ratchets are tracked live by the
/// agentic loop's own pre-send guard, not by this provider.
pub struct TokenBudget {
    /// Maximum context tokens (model's `num_ctx`; can be overridden).
    max_tokens: u32,
    /// Prune when used tokens exceed this fraction of `max_tokens`.
    threshold_pct: f32,
    history: Vec<TurnRecord>,
    pruned_count: usize,
    /// Backend-reported prompt size of the most recent turn (truth anchor).
    last_prompt_tokens: Option<u32>,
    /// Estimated tokens added (+) / removed (−) relative to that anchor.
    delta_since_prompt: i64,
}

impl TokenBudget {
    pub fn new(max_tokens: u32, threshold_pct: f32) -> Self {
        Self {
            max_tokens: max_tokens.max(512),
            threshold_pct: threshold_pct.clamp(0.1, 0.99),
            history: Vec::new(),
            pruned_count: 0,
            last_prompt_tokens: None,
            delta_since_prompt: 0,
        }
    }

    /// Inject a resolved token budget (builder form of the `max_tokens`
    /// constructor argument, mirroring `Summarizing::with_summarizer`).
    /// Same ≥512 clamp as [`TokenBudget::new`].
    ///
    /// Step 18.2 (#247): this replaces the deleted `from_config()` path,
    /// whose silent 8,192 fallback ignored empirical capability data. The
    /// caller resolves the budget (explicit config override → capability
    /// cache → [`DEFAULT_CONTEXT_TOKENS`]) and injects the value here.
    pub fn with_budget(mut self, tokens: u32) -> Self {
        self.max_tokens = tokens.max(512);
        self
    }

    fn budget_tokens(&self) -> u32 {
        (self.max_tokens as f32 * self.threshold_pct) as u32
    }

    /// Current context fullness: last backend-reported prompt size plus the
    /// estimated delta since, or the per-turn content-estimate sum when no
    /// report exists (Step 18.1).
    fn used_tokens(&self) -> u32 {
        match self.last_prompt_tokens {
            Some(p) => (i64::from(p) + self.delta_since_prompt).max(0) as u32,
            None => self.history.iter().map(|r| r.est_tokens).sum(),
        }
    }

    /// Prune oldest turns until we're within budget. Returns how many were dropped.
    fn prune_to_budget(&mut self) -> usize {
        let budget = self.budget_tokens();
        let mut dropped = 0;
        while self.used_tokens() > budget && !self.history.is_empty() {
            let removed = self.history.remove(0);
            // Dropping a turn shrinks the NEXT prompt by its content estimate.
            self.delta_since_prompt -= i64::from(removed.est_tokens);
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
            // A restored compaction record (Step 18.5) has no reply — never
            // dispatch an empty assistant message for it.
            if !r.assistant.is_empty() {
                msgs.push(MemMessage::assistant(&r.assistant));
            }
        }
        msgs.push(MemMessage::user(new_task));
        msgs
    }

    async fn sync_turn(&mut self, user: &str, assistant: &str, metrics: &TurnMetrics) {
        let est = turn_content_estimate(user, assistant);
        self.history.push(TurnRecord {
            user: user.to_string(),
            assistant: assistant.to_string(),
            est_tokens: est,
        });
        match metrics.usage {
            Some(u) => {
                // `input_tokens` is the largest single prompt the backend
                // evaluated this turn (Step 18.1) — it already contains the
                // system prompt, all prior turns, and this user message. The
                // only content not yet inside any prompt is the new reply.
                self.last_prompt_tokens = Some(u.input_tokens);
                self.delta_since_prompt = (assistant.len().div_ceil(4)) as i64;
            }
            // No backend report this turn: the whole turn is unaccounted
            // relative to the (possibly absent) anchor.
            None => self.delta_since_prompt += i64::from(est),
        }
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

    fn reset(&mut self) {
        self.history.clear();
        self.pruned_count = 0;
        self.last_prompt_tokens = None;
        self.delta_since_prompt = 0;
    }

    fn restore_turns(&mut self, turns: &[crate::ConversationTurn]) {
        self.history = turns
            .iter()
            .map(|t| TurnRecord {
                user: t.user.clone(),
                assistant: t.assistant.clone(),
                est_tokens: turn_content_estimate(&t.user, &t.assistant),
            })
            .collect();
        // Step 18.5 (#247): column-first restore — re-anchor on the last
        // backend-reported prompt size from the 17.6 columns instead of
        // re-estimating the whole history at chars/4. NULL columns fall
        // back to the estimate sum (anchor stays None — an estimate is
        // never presented as a measurement).
        let (anchor, delta) = restored_token_anchor(turns);
        self.last_prompt_tokens = anchor;
        self.delta_since_prompt = delta;
        self.pruned_count = self.prune_to_budget();
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
// NoteStore — built-in provider #3 (closes #108; v2 in Step 19.1 / #248)
// ---------------------------------------------------------------------------

// NoteStore grew its own module in Step 19.1 (`§`-delimited entries,
// substring addressing, curated cap, atomic writes + advisory lock).
// Re-exported here so the public path `newt_core::memory::NoteStore`
// stays stable.
pub use crate::notes::NoteStore;

// ---------------------------------------------------------------------------
// MemoryIndex — budgeted progressive-disclosure index (Workstream A MVP, #319)
// ---------------------------------------------------------------------------

/// The memory index budget: the maximum number of items the frozen memory
/// INDEX may list, **pinned by CI** (the modulex `DEFAULT_TOOL_BUDGET = 12`
/// pattern — progressive-disclosure memory design §2.3/§3.3). The index is
/// the cheap layer that rides in every request; the verbatim bodies are pulled
/// on demand via `memory_fetch`. Growing this is a deliberate edit to this
/// constant with its own justification, asserted by a test — never a side
/// effect of a feature. Starts small (≈ the `DEFAULT_TOOL_BUDGET` order of
/// magnitude); tune empirically against probe data, never down silently.
pub const MEMORY_INDEX_BUDGET: usize = 12;

/// A budgeted, frozen INDEX of memory the model can navigate (#319).
///
/// Instead of freezing every NOTE body verbatim into the system prompt (what
/// [`NoteStore::system_prompt_block`] does today), this provider surfaces a
/// SMALL index — note ids + first-line titles — capped at
/// [`MEMORY_INDEX_BUDGET`] items. The bodies are pulled on demand via the
/// `memory_fetch` tool (`note:<id>`). This is `use_skill`'s index-then-fetch
/// shape applied to memory.
///
/// **Additive and opt-in.** It is registered only under
/// `[memory] disclosure = "index"` (default `frozen`), so with the default
/// config it is never constructed and behavior is bit-for-bit unchanged. Like
/// [`NoteStore`] and [`SoulProvider`] it is system-prompt-only —
/// `build_messages` returns `Vec::new()` so it never competes for the
/// "first non-empty `build_messages`" slot in [`MemoryManager::build_messages`].
///
/// The index is frozen at session start (KV-cache-safe). Notes created
/// mid-session don't appear until next session — the same accepted limitation
/// `NoteStore`'s frozen snapshot has today; `memory_fetch` by a known id still
/// works mid-session even when the index hasn't refreshed (the index is a
/// convenience surface, the fetch is the capability — design §8.7).
///
/// Past-turn keywords are deliberately NOT duplicated here: that is exactly
/// what `recall` provides (snippet search over the store). The index points at
/// recall for that axis rather than competing with it (design §3.1/§8.5).
pub struct MemoryIndex {
    /// `(id, title)` rows captured at `initialize`, capped at the budget.
    rows: Vec<(usize, String)>,
    /// True when more notes existed than the budget could list — the overflow
    /// line tells the model the tail is reachable via `recall`.
    truncated: bool,
    /// Source NOTES path (read once at `initialize`, like `NoteStore`).
    notes_path: std::path::PathBuf,
}

impl MemoryIndex {
    /// Build an index over the NOTES file at `notes_path` (the same file
    /// [`NoteStore`] reads). The bodies are fetched via `memory_fetch`; this
    /// provider only ever surfaces ids + titles.
    pub fn new(notes_path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            rows: Vec::new(),
            truncated: false,
            notes_path: notes_path.into(),
        }
    }

    /// Construct over the default NOTES location (`~/.newt/NOTES.md`), mirroring
    /// [`NoteStore::default_path`].
    pub fn default_path() -> Self {
        let path = crate::Config::user_config_path()
            .map(|p| p.with_file_name("NOTES.md"))
            .unwrap_or_else(|| std::path::PathBuf::from("NOTES.md"));
        Self::new(path)
    }

    /// Rows currently listed in the index (test introspection).
    pub fn rows(&self) -> &[(usize, String)] {
        &self.rows
    }

    /// Capture the index from a fresh [`NoteStore`] read, capping at
    /// [`MEMORY_INDEX_BUDGET`]. Shared by `initialize` and tests.
    fn capture_from(&mut self, notes: &NoteStore) {
        let all = notes.index_entries();
        self.truncated = all.len() > MEMORY_INDEX_BUDGET;
        self.rows = all
            .into_iter()
            .take(MEMORY_INDEX_BUDGET)
            .map(|(id, title)| (id, title.to_string()))
            .collect();
    }
}

#[async_trait]
impl MemoryProvider for MemoryIndex {
    fn name(&self) -> &str {
        "memory_index"
    }

    async fn initialize(&mut self, ctx: &SessionContext) -> anyhow::Result<()> {
        // Read the same NOTES file NoteStore reads, once, and freeze the index.
        let mut notes = NoteStore::new(self.notes_path.clone(), NoteStore::DEFAULT_CHAR_LIMIT);
        notes.initialize(ctx).await?;
        self.capture_from(&notes);
        Ok(())
    }

    fn system_prompt_block(&self) -> Option<String> {
        if self.rows.is_empty() {
            return None;
        }
        let mut block =
            String::from("## Memory index (call `memory_fetch` with an id to read a body)\n");
        for (id, title) in &self.rows {
            let title = if title.is_empty() {
                "(untitled)"
            } else {
                title
            };
            block.push_str(&format!("- note:{id}  {title}\n"));
        }
        if self.truncated {
            block
                .push_str("(more notes exist than are listed — use `recall` to search the rest)\n");
        }
        Some(block.trim_end().to_string())
    }

    fn build_messages(&self, _system_prompt: &str, _new_task: &str) -> Vec<MemMessage> {
        // System-prompt-only (like NoteStore / SoulProvider) — never competes
        // for the first-non-empty build_messages slot.
        Vec::new()
    }

    async fn sync_turn(&mut self, _user: &str, _assistant: &str, _metrics: &TurnMetrics) {}
}

// ---------------------------------------------------------------------------
// Summarizing — built-in provider #4 (closes #107)
// ---------------------------------------------------------------------------

/// Per-turn record with a content-size estimate for budget tracking.
#[derive(Clone)]
struct SumTurn {
    user: String,
    assistant: String,
    /// chars/4 (ceiling) estimate of THIS turn's content only — see
    /// [`TurnRecord::est_tokens`] for why backend usage is not stored per
    /// turn (it would double-count history; Step 18.1).
    est_tokens: u32,
}

impl SumTurn {
    fn new(user: impl Into<String>, assistant: impl Into<String>) -> Self {
        let user = user.into();
        let assistant = assistant.into();
        let est_tokens = turn_content_estimate(&user, &assistant);
        Self {
            user,
            assistant,
            est_tokens,
        }
    }

    /// Wire-shape view of this entry for the shared compression pipeline.
    /// A compaction entry (or any unpaired side) renders only its non-empty
    /// half — the pipeline's summary message is a lone user-role message.
    fn to_wire(&self) -> Vec<serde_json::Value> {
        let mut v = Vec::with_capacity(2);
        if !self.user.is_empty() {
            v.push(serde_json::json!({"role": "user", "content": self.user}));
        }
        if !self.assistant.is_empty() {
            v.push(serde_json::json!({"role": "assistant", "content": self.assistant}));
        }
        v
    }
}

/// Rebuild pair-shaped history from the pipeline's assembled wire messages.
/// A compaction message (and any other unpaired side) becomes a lone-sided
/// entry; `system`/`tool` roles never occur in the provider's wire view.
fn wire_to_history(messages: &[serde_json::Value]) -> Vec<SumTurn> {
    let mut out: Vec<SumTurn> = Vec::new();
    for m in messages {
        let content = m["content"].as_str().unwrap_or_default();
        match m["role"].as_str() {
            Some("user") => out.push(SumTurn::new(content, "")),
            Some("assistant") => {
                match out.last_mut() {
                    // Pair with the preceding reply-less user entry — unless
                    // that entry is a compaction message, which stands alone.
                    Some(last)
                        if last.assistant.is_empty()
                            && !last.user.is_empty()
                            && !is_compaction_text(&last.user) =>
                    {
                        last.assistant = content.to_string();
                        last.est_tokens = turn_content_estimate(&last.user, &last.assistant);
                    }
                    _ => out.push(SumTurn::new("", content)),
                }
            }
            _ => {}
        }
    }
    out
}

/// LLM-powered summarisation of old turns when context fills.
///
/// Since Step 18.5 (#247) this provider owns no summarisation logic of its
/// own: when its anchored fullness figure crosses the budget it delegates to
/// the SAME prune → boundary → redacted summary → marker assembly pipeline
/// the agentic loop uses ([`crate::agentic::compress`], Step 18.4). The
/// pre-18.4 implementation — its own prompt template, placeholder text, and
/// anti-thrash counters — is deleted; the summary message in history is now
/// the pipeline's marked compaction message ([`SUMMARY_PREFIX`]).
///
/// Fullness tracking is unchanged (Step 18.1): last backend-reported prompt
/// size plus the estimated content delta since — the old per-turn
/// `input + output` running sum double-counted history and triggered
/// spurious compressions.
///
/// Continuity (Step 18.5): the latest compaction message is offered to the
/// caller once via [`MemoryProvider::take_compaction_record`] for durable
/// persistence, and [`MemoryProvider::restore_turns`] rehydrates it — the
/// restored working set is `[compaction message] + [turns recorded after
/// it]`, so the next compression chains off the previous summary instead of
/// re-summarizing from scratch.
///
/// Configure: `[memory] provider = "summarizing"`, plus an optional explicit
/// `context_tokens` override. The compression budget (`max_tokens`) is a
/// construction-time value injected by the caller with the same precedence
/// as [`TokenBudget`] (Step 18.2, #247): explicit config override →
/// capability-derived (`max_ok_input` else `safe_context`) →
/// [`DEFAULT_CONTEXT_TOKENS`]. Fixed per session; the loop's pre-send guard
/// tracks mid-session capability ratchets live.
pub struct Summarizing {
    max_tokens: u32,
    threshold_pct: f32,
    history: Vec<SumTurn>,
    /// The latest compaction message (full marked text) — the prev-summary
    /// chain. Rehydrated by `restore_turns` (Step 18.5).
    prev_summary: String,
    /// Compactions minted this session (summaries + static fallbacks).
    compress_count: usize,
    /// Anti-thrash state, shared semantics with the loop (Step 18.4): two
    /// consecutive <10% reclaims disable compression until reset.
    state: crate::agentic::CompressState,
    /// Injected summariser — the loop's async [`crate::agentic::Summarizer`]
    /// shape; `None` degrades to the pipeline's static fallback marker.
    summarizer: Option<crate::agentic::Summarizer>,
    /// Backend-reported prompt size of the most recent turn (truth anchor).
    last_prompt_tokens: Option<u32>,
    /// Estimated tokens added (+) / removed (−) relative to that anchor.
    delta_since_prompt: i64,
    /// Compaction message minted by the last compression, awaiting durable
    /// persistence via `take_compaction_record` (Step 18.5).
    pending_record: Option<String>,
}

impl Summarizing {
    pub fn new(max_tokens: u32) -> Self {
        Self {
            max_tokens: max_tokens.max(1),
            threshold_pct: 0.80,
            history: Vec::new(),
            prev_summary: String::new(),
            compress_count: 0,
            state: crate::agentic::CompressState::new(),
            summarizer: None,
            last_prompt_tokens: None,
            delta_since_prompt: 0,
            pending_record: None,
        }
    }

    /// Inject a resolved token budget (builder form of the `max_tokens`
    /// constructor argument). Same ≥1 clamp as [`Summarizing::new`].
    /// See [`TokenBudget::with_budget`] for the resolution precedence the
    /// caller is expected to apply (Step 18.2, #247).
    pub fn with_budget(mut self, tokens: u32) -> Self {
        self.max_tokens = tokens.max(1);
        self
    }

    /// Inject a summariser (required for real summaries; tests can use a
    /// stub). Takes the loop's async [`crate::agentic::SummarizeFn`] shape
    /// since Step 18.5 — the TUI passes the very same summarizer it builds
    /// for the loop, so there is exactly one HTTP wiring. The old sync
    /// `Fn(&str) -> Result<String>` form (whose TUI impl blocked inside
    /// `sync_turn`) is gone with the logic that called it.
    pub fn with_summarizer(
        mut self,
        f: impl Fn(String) -> crate::agentic::SummarizeFuture + Send + Sync + 'static,
    ) -> Self {
        self.summarizer = Some(Box::new(f));
        self
    }

    fn budget(&self) -> u32 {
        (self.max_tokens as f32 * self.threshold_pct) as u32
    }

    /// Current context fullness: last backend-reported prompt size plus the
    /// estimated delta since, or the per-turn content-estimate sum when no
    /// report exists (Step 18.1).
    fn used_tokens(&self) -> u32 {
        match self.last_prompt_tokens {
            Some(p) => (i64::from(p) + self.delta_since_prompt).max(0) as u32,
            None => self.history.iter().map(|t| t.est_tokens).sum(),
        }
    }

    /// Delegate one compression to the shared 18.4 pipeline and apply the
    /// assembled result back to pair-shaped history (Step 18.5, #247).
    async fn compress_via_pipeline(&mut self) {
        use crate::agentic::compress::{compress, CompressAction, CompressRequest};
        if self.history.is_empty() {
            return;
        }
        let messages: Vec<serde_json::Value> =
            self.history.iter().flat_map(SumTurn::to_wire).collect();
        // The original-task anchor: the first REAL user message — a leading
        // compaction message (rehydrated history) is not the task.
        let task = self
            .history
            .iter()
            .find(|t| !t.user.is_empty() && !is_compaction_text(&t.user))
            .map(|t| t.user.clone())
            .unwrap_or_default();
        let outcome = compress(
            CompressRequest {
                messages: &messages,
                budget: self.budget() as usize,
                max_messages: None,
                task: &task,
                hard_budget: true,
                focus: None,
            },
            self.summarizer.as_deref(),
            &mut self.state,
        )
        .await;
        if !outcome.fired {
            return;
        }
        self.history = wire_to_history(&outcome.messages);
        // Reflect the content change against the backend anchor. The
        // pipeline's figures are chars/4 estimates over the wire shape —
        // the same currency the delta already tracks.
        self.delta_since_prompt += outcome.tokens_after as i64 - outcome.tokens_before as i64;
        if matches!(
            outcome.action,
            CompressAction::Summarized | CompressAction::StaticFallback
        ) {
            // The pipeline inserted its marked summary at the boundary head —
            // the first compaction entry is the freshly minted one.
            if let Some(c) = self.history.iter().find(|t| is_compaction_text(&t.user)) {
                self.prev_summary = c.user.clone();
                self.pending_record = Some(c.user.clone());
                self.compress_count += 1;
            }
        }
        tracing::info!(
            compress_count = self.compress_count,
            action = outcome.action.describe(),
            tokens_before = outcome.tokens_before,
            tokens_after = outcome.tokens_after,
            "Summarizing: compressed context via shared pipeline"
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
            if !t.user.is_empty() {
                msgs.push(MemMessage::user(&t.user));
            }
            if !t.assistant.is_empty() {
                msgs.push(MemMessage::assistant(&t.assistant));
            }
        }
        msgs.push(MemMessage::user(new_task));
        msgs
    }

    async fn sync_turn(&mut self, user: &str, assistant: &str, metrics: &TurnMetrics) {
        let est = turn_content_estimate(user, assistant);
        self.history.push(SumTurn::new(user, assistant));
        match metrics.usage {
            Some(u) => {
                // Anchor on the largest single prompt the backend evaluated
                // this turn (Step 18.1); only the reply is not yet inside it.
                self.last_prompt_tokens = Some(u.input_tokens);
                self.delta_since_prompt = (assistant.len().div_ceil(4)) as i64;
            }
            None => self.delta_since_prompt += i64::from(est),
        }
        // Over budget → the shared pipeline (which owns anti-thrash); the
        // cheap is_disabled gate just avoids rebuilding the wire view for
        // a call that would be refused anyway.
        if self.used_tokens() > self.budget() && !self.state.is_disabled() {
            self.compress_via_pipeline().await;
        }
    }

    fn reset(&mut self) {
        self.history.clear();
        self.prev_summary.clear();
        self.compress_count = 0;
        self.state.reset();
        self.last_prompt_tokens = None;
        self.delta_since_prompt = 0;
        self.pending_record = None;
    }

    fn restore_turns(&mut self, turns: &[crate::ConversationTurn]) {
        // Step 18.5 (#247): rehydrate the prev-summary chain instead of
        // dropping it. The latest persisted compaction record cuts the
        // working set: everything before it is covered by the summary (and
        // stays durable in the store); the record itself was appended just
        // before the turn that triggered the compression, so the turns after
        // it are exactly the ones the live boundary's last-user anchor
        // guaranteed survived.
        let cut = turns
            .iter()
            .rposition(|t| is_compaction_text(&t.user) && t.assistant.is_empty());
        let live = match cut {
            Some(k) => &turns[k + 1..],
            None => turns,
        };
        self.history.clear();
        self.prev_summary.clear();
        if let Some(k) = cut {
            self.history.push(SumTurn::new(turns[k].user.clone(), ""));
            self.prev_summary = turns[k].user.clone();
        }
        self.history
            .extend(live.iter().map(|t| SumTurn::new(&*t.user, &*t.assistant)));
        self.compress_count = 0;
        self.pending_record = None;
        // A restore is a conversation boundary — re-arm anti-thrash (F4).
        self.state.reset();
        // Column-first token accounting (Step 18.5): anchor on the last
        // backend-reported prompt size among the turns actually in the
        // working set — those prompts already contained the compaction
        // message. NULL columns fall back to estimates without ever
        // becoming the anchor. NO re-compression here: re-summarizing on
        // restore is exactly the from-scratch behavior this step removes;
        // the next live turn compresses if genuinely over budget.
        let (anchor, delta) = restored_token_anchor(live);
        self.last_prompt_tokens = anchor;
        self.delta_since_prompt = delta;
    }

    fn take_compaction_record(&mut self) -> Option<String> {
        self.pending_record.take()
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
is ground truth.\n\
\n\
**Never describe a code change — make it.** Do not paste code into the chat. \
If the task requires a code change, call edit_file or write_file immediately. \
A markdown code block in the conversation is invisible to the filesystem — \
it does not modify any file. Write the code once, into the file, via the tool. \
Showing code in text is NOT completing a task; calling the tool IS.\n\
\n\
**Exploration budget.** Treat read-only rounds (list_dir, read_file) as expensive. \
Spend at most three consecutive rounds on exploration before making a write. \
Once you have read the file you need, stop reading and call edit_file or write_file. \
Continued reading without writing means you are lost — make your best attempt \
at the change based on what you have already read, then verify.";

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

    /// Metrics with a given backend-reported prompt size (`input_tokens`).
    fn metrics_with_input(input_tokens: u32) -> TurnMetrics {
        let mut m = dummy_metrics();
        m.usage = Some(TokenUsage {
            input_tokens,
            output_tokens: 20,
        });
        m
    }

    /// SEMANTICS CHANGED in Step 18.1: pruning fires when the BACKEND-reported
    /// prompt size exceeds the budget — the prompt already contains all prior
    /// turns, so the provider no longer manufactures overflow by summing
    /// per-turn readings. Here the reported prompt genuinely grows past the
    /// 100-token budget, so the oldest turns must be dropped.
    #[tokio::test]
    async fn token_budget_prunes_oldest_when_over_budget() {
        // max_tokens floors at 512; threshold caps at 0.99 → budget = 506.
        let mut tb = TokenBudget::new(512, 0.99);
        let budget = 506;
        let big = "x".repeat(200); // each turn adds 100 est tokens (2×200/4)
        tb.sync_turn(&big, &big, &metrics_with_input(200)).await;
        // Turn 1: used = 200 (reported) + 50 (reply est) = 250 ≤ 506 → kept.
        assert_eq!(tb.history.len(), 1);
        tb.sync_turn(&big, &big, &metrics_with_input(520)).await;
        // Turn 2: used = 520 (reported, includes both turns) + 50 = 570 > 506
        // → prune the oldest turn, reclaiming its 100-token estimate.
        assert!(tb.used_tokens() <= budget, "used = {}", tb.used_tokens());
        assert_eq!(tb.history.len(), 1, "the oldest turn must have been pruned");
    }

    /// SEMANTICS CHANGED in Step 18.1: with a backend report, fullness =
    /// reported prompt size (30 — already includes system + history + the
    /// user message) + chars/4 ceiling of the reply ("a" → 1). The old
    /// assertion (50 = input 30 + output 20) double-counted: output tokens
    /// were added on top of every FUTURE turn's input that re-contains them.
    #[tokio::test]
    async fn token_budget_uses_metrics_when_available() {
        let mut tb = TokenBudget::new(1000, 1.0);
        let mut m = dummy_metrics();
        m.usage = Some(crate::metrics::TokenUsage {
            input_tokens: 30,
            output_tokens: 20,
        });
        tb.sync_turn("q", "a", &m).await;
        assert_eq!(tb.used_tokens(), 31); // 30 (reported prompt) + 1 (reply est)
    }

    /// Regression for the B3 baseline's runaway drift (the 5.4× scenario):
    /// a 20-turn session whose backend-evaluated prompts grow 2,582 → 4,748
    /// tokens. The old running sum tracked 25,602 "used" tokens by the end —
    /// 5.4× the largest prompt the backend ever evaluated — and would have
    /// pruned the entire history against an 8,192 budget. The anchored math
    /// must stay pinned to the last real prompt (+ reply estimate) and never
    /// prune a conversation that genuinely fits.
    #[tokio::test]
    async fn token_budget_no_runaway_drift_b3_regression() {
        let mut tb = TokenBudget::new(8_192, 0.80); // budget = 6,553
        let mut old_running_sum: u64 = 0;
        let turns = 20u32;
        for i in 0..turns {
            // Backend-reported prompt grows linearly 2,582 → 4,748 (B3 drift
            // table endpoints); output fixed at 20 tokens per turn.
            let input = 2_582 + (4_748 - 2_582) * i / (turns - 1);
            old_running_sum += u64::from(input) + 20;
            tb.sync_turn("reply ok", "ok", &metrics_with_input(input))
                .await;
        }
        // Truthful fullness: the last real prompt (4,748) + the tiny reply
        // estimate — comfortably inside the budget, so nothing was pruned.
        let used = u64::from(tb.used_tokens());
        assert!(
            (4_748..4_800).contains(&used),
            "used must track the last real prompt, got {used}"
        );
        assert_eq!(tb.history.len(), turns as usize, "no spurious pruning");
        assert_eq!(tb.pruned_count, 0);
        // And it must be nowhere near the old inflating sum (≥ 5× smaller —
        // the B3 baseline measured 5.4× on the real session).
        assert!(
            used * 5 < old_running_sum,
            "anchored used ({used}) must be at least 5× below the old \
             running sum ({old_running_sum})"
        );
    }

    /// Async stub summarizer in the loop's `SummarizeFn` shape (the only
    /// shape since Step 18.5 — the provider delegates to the shared
    /// pipeline, which is async).
    fn stub_summarizer(
        reply: &'static str,
    ) -> impl Fn(String) -> crate::agentic::SummarizeFuture + Send + Sync {
        move |_req: String| -> crate::agentic::SummarizeFuture {
            Box::pin(async move { Ok(reply.to_string()) })
        }
    }

    /// Stub summarizer that records every request it receives — proves the
    /// provider routes through the shared pipeline (Step 18.5).
    fn capturing_summarizer(
        reply: &'static str,
        calls: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) -> impl Fn(String) -> crate::agentic::SummarizeFuture + Send + Sync {
        move |req: String| -> crate::agentic::SummarizeFuture {
            calls.lock().unwrap().push(req);
            Box::pin(async move { Ok(reply.to_string()) })
        }
    }

    /// A turn carrying 17.6 token columns, for restore tests.
    fn turn_with_tokens(
        user: &str,
        assistant: &str,
        tokens_in: Option<u32>,
        tokens_out: Option<u32>,
    ) -> crate::ConversationTurn {
        let mut t = crate::ConversationTurn::new(user, assistant);
        t.tokens_in = tokens_in;
        t.tokens_out = tokens_out;
        t
    }

    /// Same B3 drift scenario through `Summarizing`: the old running sum
    /// blew past an 8,192-token budget by turn ~2 and burned summarizer
    /// calls on a conversation whose real prompts never exceeded 4,748
    /// tokens. The anchored math must never trigger compression here.
    #[tokio::test]
    async fn summarizing_no_runaway_compression_b3_regression() {
        let mut s = Summarizing::new(8_192) // budget = 6,553
            .with_summarizer(stub_summarizer("SUMMARY"));
        for i in 0..20u32 {
            let input = 2_582 + (4_748 - 2_582) * i / 19;
            s.sync_turn("reply ok", "ok", &metrics_with_input(input))
                .await;
        }
        assert_eq!(
            s.compress_count, 0,
            "prompts never exceeded the budget — compression must not fire"
        );
        assert!(s.prev_summary.is_empty());
        assert_eq!(s.history.len(), 20);
    }

    // --- Budget injection (Step 18.2, #247) ---

    /// The budget is a plain construction-time value: whatever number the
    /// caller resolved (e.g. the TUI's capability-derived figure) is exactly
    /// what governs pruning — `usage()` exposes it as the denominator.
    #[test]
    fn token_budget_construction_injects_budget() {
        let tb = TokenBudget::new(24_000, 0.80);
        let (label, _used, budget) = tb.usage().unwrap();
        assert_eq!(label, "tokens");
        assert_eq!(budget, 19_200); // 24_000 × 0.80
    }

    /// `with_budget` is the builder form of the same injection, mirroring
    /// `with_summarizer` — it replaces the constructor value.
    #[test]
    fn token_budget_with_budget_overrides_constructor_value() {
        let tb = TokenBudget::new(512, 0.80).with_budget(24_000);
        assert_eq!(tb.usage().unwrap().2, 19_200);
        // Same ≥512 clamp as the constructor.
        let clamped = TokenBudget::new(4_096, 1.0).with_budget(0);
        assert_eq!(clamped.max_tokens, 512);
    }

    #[test]
    fn summarizing_construction_injects_budget() {
        let s = Summarizing::new(32_768);
        let (label, _used, budget) = s.usage().unwrap();
        assert_eq!(label, "tokens");
        assert_eq!(budget, 26_214); // 32_768 × 0.80
    }

    #[test]
    fn summarizing_with_budget_overrides_constructor_value() {
        let s = Summarizing::new(100).with_budget(32_768);
        assert_eq!(s.usage().unwrap().2, 26_214);
        // Same ≥1 clamp as the constructor.
        let clamped = Summarizing::new(100).with_budget(0);
        assert_eq!(clamped.max_tokens, 1);
    }

    /// The deleted `from_config()` path silently fell back to 8,192 even
    /// when the caller had empirical capability data. Construction-time
    /// injection means a capability-derived number flows through verbatim —
    /// the provider must NOT sit at the static default.
    #[test]
    fn token_budget_does_not_sit_at_static_default_with_capability_data() {
        // A capability-derived budget (e.g. max_ok_input = 24,000) injected
        // at construction.
        let capability_derived = 24_000;
        let tb = TokenBudget::new(capability_derived, 0.80);
        let static_default_budget = (DEFAULT_CONTEXT_TOKENS as f32 * 0.80) as usize;
        assert_ne!(
            tb.usage().unwrap().2,
            static_default_budget,
            "provider budget must reflect injected capability data, \
             not the static default"
        );
        assert_eq!(tb.usage().unwrap().2, 19_200);
    }

    // --- NoteStore tests live in crate::notes (split out in Step 19.1) ---

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

    /// SEMANTICS CHANGED in Step 18.5: compression delegates to the shared
    /// 18.4 pipeline, whose boundary protects the head (the original task)
    /// and a ≥3-message tail — so a conversation needs enough turns to leave
    /// a summarizable middle (the old provider summarized the oldest 50%
    /// unconditionally). The summary in history is the pipeline's marked
    /// compaction message.
    #[tokio::test]
    async fn summarizing_compresses_when_over_budget() {
        let mut s = Summarizing::new(100) // budget = 80 tokens
            .with_summarizer(stub_summarizer("SUMMARY"));

        let big = "x".repeat(200);
        // Five turns whose anchored fullness stays at/under the budget.
        for i in 0..5u32 {
            s.sync_turn(&big, &big, &metrics_with_input(10 + i)).await;
        }
        assert_eq!(s.compress_count, 0);
        // The reported prompt crosses the budget → delegate to the pipeline.
        s.sync_turn(&big, &big, &metrics_with_input(120)).await;

        assert!(s.compress_count >= 1, "compress_count={}", s.compress_count);
        // The chain head is the pipeline's marked compaction message.
        assert!(
            s.prev_summary.starts_with(crate::agentic::SUMMARY_PREFIX),
            "prev_summary must be the marked compaction message"
        );
        assert!(s.prev_summary.contains("SUMMARY"));
        assert!(s.prev_summary.contains(crate::agentic::SUMMARY_END_MARKER));
        assert!(
            s.history
                .iter()
                .any(|t| t.user.starts_with(crate::agentic::SUMMARY_PREFIX)
                    && t.assistant.is_empty()),
            "the compaction message must live in history as a lone user entry"
        );
    }

    /// SEMANTICS CHANGED in Step 18.1: the reported prompt sizes must
    /// actually grow past the budget for compression to fire (the old test's
    /// flat 40-token turns only crossed it because the running sum inflated).
    #[tokio::test]
    async fn summarizing_compresses_repeatedly() {
        // Verify that the provider compresses across many turns and doesn't
        // panic. The exact compress_count depends on savings/anti-thrash.
        let mut s = Summarizing::new(100).with_summarizer(stub_summarizer("SUMMARY"));
        let text = "x".repeat(120);
        for i in 0..20u32 {
            // Backend-reported prompt grows 50, 100, 150, … — repeatedly
            // crossing the 80-token budget.
            s.sync_turn(&text, &text, &metrics_with_input(50 * (i + 1)))
                .await;
        }
        // Should have compressed at least once without panicking.
        assert!(s.compress_count >= 1);
    }

    // --- Continuity: restore + delegation (Step 18.5, #247) ---

    /// Restore must route through the shared pipeline exactly once when a
    /// later turn overflows, with the shared template and redaction applied
    /// — the proof the legacy duplicate path is gone.
    #[tokio::test]
    async fn summarizing_delegates_to_shared_pipeline() {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let mut s =
            Summarizing::new(100).with_summarizer(capturing_summarizer("PIPE-SUM", calls.clone()));
        let secret = "sk-aaaaaaaaaaaaaaaaaaaaaaaa";
        let big = "x".repeat(200);
        // An early reply carries a credential — it lands in the summarized
        // middle and must be redacted by the SHARED pipeline's pass.
        s.sync_turn(
            "the original task",
            &format!("noted {secret}"),
            &metrics_with_input(10),
        )
        .await;
        for i in 0..4u32 {
            s.sync_turn(&big, &big, &metrics_with_input(11 + i)).await;
        }
        assert!(calls.lock().unwrap().is_empty(), "under budget — no calls");
        s.sync_turn(&big, &big, &metrics_with_input(120)).await;

        let reqs = calls.lock().unwrap();
        assert_eq!(reqs.len(), 1, "exactly one summarizer call per compression");
        let req = &reqs[0];
        assert!(
            req.contains("## Conversation middle to summarise"),
            "must be the shared pipeline's request template"
        );
        assert!(req.contains("## Original Task"));
        assert!(req.contains("the original task"), "task anchored verbatim");
        assert!(
            !req.contains(secret),
            "secret must not reach the summarizer"
        );
        assert!(req.contains("[REDACTED]"), "shared redaction pass applied");
        drop(reqs);
        // Assembly used the shared markers around the returned body.
        assert!(s.prev_summary.starts_with(crate::agentic::SUMMARY_PREFIX));
        assert!(s.prev_summary.contains("PIPE-SUM"));
        assert!(s.prev_summary.contains(crate::agentic::SUMMARY_END_MARKER));
    }

    /// The compaction record is offered for persistence exactly once.
    #[tokio::test]
    async fn summarizing_compaction_record_is_minted_once_and_drained() {
        let mut s = Summarizing::new(100).with_summarizer(stub_summarizer("SUMMARY"));
        assert!(s.take_compaction_record().is_none(), "nothing minted yet");
        let big = "x".repeat(200);
        for i in 0..5u32 {
            s.sync_turn(&big, &big, &metrics_with_input(10 + i)).await;
        }
        s.sync_turn(&big, &big, &metrics_with_input(120)).await;
        let record = s
            .take_compaction_record()
            .expect("compression must mint a record");
        assert!(record.starts_with(crate::agentic::SUMMARY_PREFIX));
        assert_eq!(record, s.prev_summary, "the record IS the chain head");
        assert!(
            s.take_compaction_record().is_none(),
            "the record is drained on take — never persisted twice"
        );
    }

    /// The manager drains the record from whichever provider minted it.
    #[tokio::test]
    async fn memory_manager_routes_take_compaction_record() {
        let mut mgr = MemoryManager::new();
        mgr.add_provider(RollingWindow::new(50)); // mints nothing
        mgr.add_provider(Summarizing::new(100).with_summarizer(stub_summarizer("SUMMARY")));
        assert!(mgr.take_compaction_record().is_none());
        let big = "x".repeat(200);
        for i in 0..5u32 {
            mgr.sync_all(&big, &big, &metrics_with_input(10 + i)).await;
        }
        mgr.sync_all(&big, &big, &metrics_with_input(120)).await;
        let record = mgr
            .take_compaction_record()
            .expect("manager must surface the Summarizing provider's record");
        assert!(record.starts_with(crate::agentic::SUMMARY_PREFIX));
        assert!(mgr.take_compaction_record().is_none());
    }

    /// The memory.rs:919-class bug (#247 / 18.5): restoring a compressed
    /// conversation must rehydrate the summary message and the prev-summary
    /// chain instead of rebuilding raw history and silently dropping both.
    #[tokio::test]
    async fn summarizing_restore_rehydrates_compaction_summary() {
        let marked = format!(
            "{}\nsummary of earlier work\n{}",
            crate::agentic::SUMMARY_PREFIX,
            crate::agentic::SUMMARY_END_MARKER
        );
        let turns = vec![
            crate::ConversationTurn::new("old task", "old reply"), // covered by the summary
            crate::ConversationTurn::new(marked.clone(), ""),      // the persisted record
            crate::ConversationTurn::new("recent task", "recent reply"),
        ];
        let mut s = Summarizing::new(8_192);
        s.restore_turns(&turns);

        // The chain is back: the next compression sees the previous summary.
        let pre = s.on_pre_compress(&[]).await;
        assert!(pre.contains("summary of earlier work"), "got: {pre:?}");

        // Working set = [compaction message] + turns recorded after it; the
        // summarized turn is not duplicated alongside its own summary.
        let msgs = s.build_messages("sys", "next");
        assert!(msgs
            .iter()
            .any(|m| m.content.starts_with(crate::agentic::SUMMARY_PREFIX)));
        assert!(!msgs.iter().any(|m| m.content == "old task"));
        assert!(msgs.iter().any(|m| m.content == "recent task"));
        assert!(msgs.iter().any(|m| m.content == "recent reply"));
        // The lone-sided compaction entry never dispatches an empty message.
        assert!(!msgs.iter().any(|m| m.content.is_empty()));
    }

    /// Restore must NOT burn a summarizer call: re-summarizing from scratch
    /// on restore is exactly the behavior 18.5 removes. The next live turn
    /// compresses if genuinely over budget.
    #[tokio::test]
    async fn summarizing_restore_never_resummarizes() {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let mut s =
            Summarizing::new(10).with_summarizer(capturing_summarizer("SUMMARY", calls.clone()));
        let big = "x".repeat(400);
        let turns: Vec<crate::ConversationTurn> = (0..6)
            .map(|i| crate::ConversationTurn::new(format!("q{i} {big}"), format!("a{i} {big}")))
            .collect();
        s.restore_turns(&turns); // far over the 8-token budget
        assert!(
            calls.lock().unwrap().is_empty(),
            "restore must never call the summarizer"
        );
        assert_eq!(s.history.len(), 6, "restored history intact");
    }

    /// After restore, the rehydrated compaction message chains into the NEXT
    /// compression as part of the summarized middle — it is neither the task
    /// anchor nor a fresh user message (the F1 self-poisoning shape).
    #[tokio::test]
    async fn restored_compaction_chains_into_next_compression() {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let mut s =
            Summarizing::new(100).with_summarizer(capturing_summarizer("NEW-SUM", calls.clone()));
        let marked = format!(
            "{}\nCHAIN-ME: facts from the first compaction\n{}",
            crate::agentic::SUMMARY_PREFIX,
            crate::agentic::SUMMARY_END_MARKER
        );
        let big = "y".repeat(100);
        let mut turns = vec![crate::ConversationTurn::new(marked.clone(), "")];
        for i in 0..4 {
            turns.push(crate::ConversationTurn::new(
                format!("task {i} {big}"),
                format!("reply {i} {big}"),
            ));
        }
        s.restore_turns(&turns);

        // One live over-budget turn → one pipeline compression.
        s.sync_turn(&big, &big, &metrics_with_input(200)).await;
        let reqs = calls.lock().unwrap();
        assert_eq!(reqs.len(), 1, "one call through the shared path");
        assert!(
            reqs[0].contains("CHAIN-ME"),
            "the previous summary must be summarizer INPUT (the chain), got: {}",
            &reqs[0]
        );
        // The Original-Task anchor (the text right under the header) must be
        // the first REAL user message, not the compaction message.
        let anchored = reqs[0]
            .split("## Original Task")
            .nth(1)
            .and_then(|rest| rest.lines().nth(1).map(str::to_string))
            .unwrap_or_default();
        assert!(
            anchored.starts_with("task 0"),
            "task anchor must skip the compaction message, got: {anchored:?}"
        );
        drop(reqs);
        // The old compaction message was replaced by the new one.
        let compactions: Vec<&SumTurn> = s
            .history
            .iter()
            .filter(|t| t.user.starts_with(crate::agentic::SUMMARY_PREFIX))
            .collect();
        assert_eq!(compactions.len(), 1, "exactly one compaction in history");
        assert!(compactions[0].user.contains("NEW-SUM"));
        assert!(s.prev_summary.contains("NEW-SUM"));
    }

    /// 18.5 token restore: the anchor comes from the persisted 17.6 column
    /// of the LAST measured turn — not a chars/4 re-estimation of history.
    #[test]
    fn token_budget_restore_anchors_on_column_tokens() {
        let mut tb = TokenBudget::new(100_000, 0.80);
        let turns = vec![
            turn_with_tokens("u1", "a1", Some(900), Some(10)),
            turn_with_tokens("u2", "aa", Some(1_000), Some(12)),
        ];
        tb.restore_turns(&turns);
        // 1,000 (measured prompt — already contains everything before it)
        // + ceil(2/4) = 1 for the reply not yet inside any prompt.
        assert_eq!(tb.last_prompt_tokens, Some(1_000));
        assert_eq!(tb.used_tokens(), 1_001);
    }

    /// NULL columns (pre-17.6 rows, silent backends) fall back to the
    /// estimate WITHOUT ever becoming the anchor — an estimate is never
    /// presented as a measurement (18.1 honesty).
    #[test]
    fn token_budget_restore_null_columns_fall_back_to_estimate() {
        let mut tb = TokenBudget::new(100_000, 0.80);
        let text = "x".repeat(40); // 10 est tokens per side
        let turns = vec![
            crate::ConversationTurn::new(text.clone(), text.clone()),
            crate::ConversationTurn::new(text.clone(), text.clone()),
        ];
        tb.restore_turns(&turns);
        assert_eq!(
            tb.last_prompt_tokens, None,
            "no measurement in the store → no anchor, only estimates"
        );
        assert_eq!(tb.used_tokens(), 40, "2 turns × (10 + 10) est tokens");
    }

    /// Turns recorded after the last measured one (silent backend) extend
    /// the delta with estimates while the anchor stays the real measurement.
    #[test]
    fn token_budget_restore_unmeasured_tail_extends_delta() {
        let mut tb = TokenBudget::new(100_000, 0.80);
        let turns = vec![
            turn_with_tokens("u1", "aaaa", Some(1_000), Some(8)), // reply est 1
            crate::ConversationTurn::new("xxxx", "yyyy"),         // est 2
        ];
        tb.restore_turns(&turns);
        assert_eq!(tb.last_prompt_tokens, Some(1_000));
        assert_eq!(tb.used_tokens(), 1_003);
    }

    /// Same column-first restore through `Summarizing`.
    #[test]
    fn summarizing_restore_anchors_on_column_tokens() {
        let mut s = Summarizing::new(100_000);
        let turns = vec![
            turn_with_tokens("u1", "a1", Some(700), Some(9)),
            turn_with_tokens("u2", "aaaa", Some(2_000), Some(11)),
        ];
        s.restore_turns(&turns);
        assert_eq!(s.last_prompt_tokens, Some(2_000));
        assert_eq!(s.used_tokens(), 2_001); // 2,000 + ceil(4/4)
    }

    /// Measurements taken BEFORE the compaction cut describe prompts of a
    /// pre-compression shape — they must not anchor the restored (smaller)
    /// working set.
    #[test]
    fn summarizing_restore_ignores_measurements_before_the_cut() {
        let marked = format!(
            "{}\nolder work compressed\n{}",
            crate::agentic::SUMMARY_PREFIX,
            crate::agentic::SUMMARY_END_MARKER
        );
        let mut s = Summarizing::new(100_000);
        let turns = vec![
            turn_with_tokens("old", "old", Some(50_000), Some(10)),
            crate::ConversationTurn::new(marked, ""),
            crate::ConversationTurn::new("after", "compaction"), // unmeasured
        ];
        s.restore_turns(&turns);
        assert_eq!(
            s.last_prompt_tokens, None,
            "a pre-compression measurement must not anchor the cut working set"
        );
    }

    /// A compaction record restored into the default provider stays in the
    /// window as a user-side summary — never dispatched with an empty
    /// assistant half.
    #[test]
    fn rolling_window_restores_compaction_record_without_empty_assistant() {
        let marked = format!(
            "{}\nearlier work\n{}",
            crate::agentic::SUMMARY_PREFIX,
            crate::agentic::SUMMARY_END_MARKER
        );
        let mut rw = RollingWindow::new(10);
        rw.restore_turns(&[
            crate::ConversationTurn::new(marked.clone(), ""),
            crate::ConversationTurn::new("next task", "next reply"),
        ]);
        let msgs = rw.build_messages("sys", "go");
        assert!(msgs.iter().any(|m| m.content == marked));
        assert!(!msgs.iter().any(|m| m.content.is_empty()));
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
        let err = mgr.add_note("fact").unwrap_err().to_string();
        assert!(
            err.contains("no note-capable memory provider"),
            "guidance error expected: {err}"
        );
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
        tb.sync_turn("q", "a", &m).await; // 50 + 1 reply-est — within budget
        assert_eq!(tb.history.len(), 1);
    }

    /// Without any backend report the provider falls back to summing per-turn
    /// content estimates — each turn counted once (no double-count).
    #[tokio::test]
    async fn token_budget_estimate_fallback_counts_each_turn_once() {
        let mut tb = TokenBudget::new(1000, 1.0);
        let mut m = dummy_metrics();
        m.usage = None; // backend reported nothing
        let text = "x".repeat(40); // 40 chars → 10 est tokens per side
        tb.sync_turn(&text, &text, &m).await;
        tb.sync_turn(&text, &text, &m).await;
        assert_eq!(tb.used_tokens(), 40, "2 turns × (10 + 10) est tokens");
    }

    // --- Summarizing additional coverage ---

    /// SEMANTICS CHANGED in Step 18.5: with no summarizer the shared
    /// pipeline inserts its STATIC fallback marker (the only surviving form
    /// of the old placeholder-discard) — the provider's own "turns
    /// summarised" placeholder is deleted with the rest of the legacy path.
    #[tokio::test]
    async fn summarizing_fallback_placeholder_when_no_summarizer() {
        let mut s = Summarizing::new(10); // tiny budget (8) to trigger compression
        for i in 0..6u32 {
            s.sync_turn(
                &format!("question {i}"),
                &format!("answer {i}"),
                &metrics_with_input(6 + 4 * i),
            )
            .await;
        }
        let compaction = s
            .history
            .iter()
            .find(|t| t.user.starts_with(crate::agentic::SUMMARY_PREFIX))
            .expect("static fallback marker should be inserted");
        assert!(
            compaction
                .user
                .contains("Summary generation was unavailable"),
            "got: {}",
            compaction.user
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
        let mut s = Summarizing::new(10).with_summarizer(stub_summarizer("PRIOR SUMMARY"));
        // Build up history UNDER budget first: the pipeline's boundary needs
        // enough turns to leave a summarizable middle (Step 18.5 — head +
        // ≥3-message tail are protected), and a too-early trigger would burn
        // anti-thrash slots on nothing-to-summarize passes.
        for _ in 0..5u32 {
            s.sync_turn("question text", "answer text", &metrics_with_input(2))
                .await;
        }
        // Now cross the 8-token budget → compression sets prev_summary.
        s.sync_turn("question text", "answer text", &metrics_with_input(50))
            .await;
        let pre = s.on_pre_compress(&[]).await;
        assert!(
            pre.contains("PRIOR SUMMARY"),
            "compression must have run and set prev_summary, got: {pre:?}"
        );
    }

    // --- RollingWindow additional coverage ---

    #[tokio::test]
    async fn rolling_window_on_session_end_noop() {
        let mut rw = RollingWindow::new(5);
        rw.on_session_end(&[]).await; // must not panic
    }

    #[tokio::test]
    async fn rolling_window_add_note_returns_notes_unsupported() {
        let mut rw = RollingWindow::new(5);
        let err = rw.add_note("fact").unwrap_err();
        assert!(err.is::<NotesUnsupported>());
    }

    #[tokio::test]
    async fn rolling_window_replace_and_remove_note_return_notes_unsupported() {
        // The trait defaults (Step 19.3) mirror add_note so the manager's
        // routing can skip note-less providers for every mutation kind.
        let mut rw = RollingWindow::new(5);
        assert!(rw
            .replace_note("old", "new")
            .unwrap_err()
            .is::<NotesUnsupported>());
        assert!(rw.remove_note("old").unwrap_err().is::<NotesUnsupported>());
    }

    #[tokio::test]
    async fn memory_manager_replace_and_remove_route_to_note_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("NOTES.md");
        let mut mgr = MemoryManager::new();
        // A note-less provider first — routing must skip it (NotesUnsupported).
        mgr.add_provider(RollingWindow::new(5));
        mgr.add_provider(NoteStore::new(path.clone(), 2200));
        let ctx = SessionContext {
            workspace: "/ws".into(),
            session_id: "s".into(),
        };
        mgr.initialize_all(&ctx).await;

        mgr.add_note("model alpha is the fast tier").unwrap();
        mgr.add_note("workspace uses just check").unwrap();

        mgr.replace_note("alpha", "model beta is the fast tier")
            .unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("model beta"), "{raw}");
        assert!(!raw.contains("model alpha"), "{raw}");

        mgr.remove_note("just check").unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("just check"), "{raw}");
        assert!(raw.contains("model beta"), "other entry untouched: {raw}");

        // Real rejections surface: ambiguity / zero-match errors come back.
        let err = mgr.remove_note("nonexistent").unwrap_err().to_string();
        assert!(err.contains("no entry contains"), "{err}");
    }

    #[tokio::test]
    async fn memory_manager_replace_and_remove_fail_with_no_note_store() {
        let mut mgr = MemoryManager::new();
        mgr.add_provider(RollingWindow::new(5));
        let err = mgr.replace_note("a", "b").unwrap_err().to_string();
        assert!(err.contains("no note-capable memory provider"), "{err}");
        let err = mgr.remove_note("a").unwrap_err().to_string();
        assert!(err.contains("no note-capable memory provider"), "{err}");
    }

    #[tokio::test]
    async fn memory_manager_add_note_routes_to_note_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("NOTES.md");
        let mut mgr = MemoryManager::new();
        mgr.add_provider(RollingWindow::new(5));
        mgr.add_provider(NoteStore::new(path.clone(), 2200));
        let ctx = SessionContext {
            workspace: "/ws".into(),
            session_id: "s".into(),
        };
        mgr.initialize_all(&ctx).await;
        mgr.add_note("the answer is 42").unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("the answer is 42"));
    }

    /// A provider with a non-`note_store` name that accepts notes — proves
    /// the manager no longer special-cases `name() == "note_store"`.
    struct CustomNotes {
        notes: Vec<String>,
    }

    #[async_trait]
    impl MemoryProvider for CustomNotes {
        fn name(&self) -> &str {
            "custom_notes"
        }
        fn build_messages(&self, _system_prompt: &str, _new_task: &str) -> Vec<MemMessage> {
            Vec::new()
        }
        async fn sync_turn(&mut self, _user: &str, _assistant: &str, _metrics: &TurnMetrics) {}
        fn add_note(&mut self, fact: &str) -> anyhow::Result<()> {
            self.notes.push(fact.to_string());
            Ok(())
        }
    }

    #[tokio::test]
    async fn memory_manager_add_note_first_ok_wins_regardless_of_name() {
        let mut mgr = MemoryManager::new();
        mgr.add_provider(RollingWindow::new(5)); // unsupported — skipped
        mgr.add_provider(CustomNotes { notes: Vec::new() });
        mgr.add_note("routed by capability, not by name").unwrap();
    }

    #[tokio::test]
    async fn memory_manager_add_note_surfaces_curator_error() {
        // A real rejection from a note-capable provider (the over-budget
        // curator error) must reach the caller, not be swallowed by the
        // generic "no provider" message.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("NOTES.md");
        let mut mgr = MemoryManager::new();
        mgr.add_provider(RollingWindow::new(5));
        mgr.add_provider(NoteStore::new(path, 40));
        let ctx = SessionContext {
            workspace: "/ws".into(),
            session_id: "s".into(),
        };
        mgr.initialize_all(&ctx).await;
        mgr.add_note("an entry that fits").unwrap();
        let err = mgr.add_note(&"x".repeat(80)).unwrap_err().to_string();
        assert!(
            err.contains("Replace or remove existing entries first"),
            "curator error must propagate: {err}"
        );
        assert!(err.contains("an entry that fits"), "{err}");
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
    async fn memory_manager_reset_all_clears_conversation_history() {
        let mut mgr = MemoryManager::new();
        mgr.add_provider(RollingWindow::new(5));
        mgr.sync_all("old task", "old reply", &dummy_metrics())
            .await;

        let before = mgr.build_messages("system", "new task");
        assert!(before.iter().any(|m| m.content == "old task"));
        assert!(before.iter().any(|m| m.content == "old reply"));

        mgr.reset_all();

        let after = mgr.build_messages("system", "new task");
        assert!(!after.iter().any(|m| m.content == "old task"));
        assert!(!after.iter().any(|m| m.content == "old reply"));
        assert!(after.iter().any(|m| m.content == "new task"));
    }

    #[tokio::test]
    async fn memory_manager_restore_turns_replaces_conversation_history() {
        let mut mgr = MemoryManager::new();
        mgr.add_provider(RollingWindow::new(5));
        mgr.sync_all("old task", "old reply", &dummy_metrics())
            .await;

        mgr.restore_turns(&[
            crate::ConversationTurn::new("restored task", "restored reply"),
            crate::ConversationTurn::new("follow up", "followed up"),
        ]);

        let messages = mgr.build_messages("system", "new task");
        assert!(!messages.iter().any(|m| m.content == "old task"));
        assert!(!messages.iter().any(|m| m.content == "old reply"));
        assert!(messages.iter().any(|m| m.content == "restored task"));
        assert!(messages.iter().any(|m| m.content == "restored reply"));
        assert!(messages.iter().any(|m| m.content == "follow up"));
        assert!(messages.iter().any(|m| m.content == "followed up"));
        assert!(messages.iter().any(|m| m.content == "new task"));
    }

    #[tokio::test]
    async fn memory_manager_fallback_with_no_providers() {
        let mgr = MemoryManager::new();
        let msgs = mgr.build_messages("sys", "task");
        assert_eq!(msgs.len(), 2);
    }

    // -- MemoryIndex (progressive-disclosure memory, Workstream A MVP, #319) --

    fn index_ctx(dir: &std::path::Path) -> SessionContext {
        SessionContext {
            workspace: dir.to_string_lossy().into(),
            session_id: "s".into(),
        }
    }

    /// Seed a NOTES file with `n` entries (using NoteStore's own write path so
    /// the §-delimited on-disk format is exactly what `MemoryIndex` reads).
    async fn seed_notes(path: &std::path::Path, n: usize) {
        let mut ns = NoteStore::new(path, NoteStore::DEFAULT_CHAR_LIMIT);
        ns.initialize(&index_ctx(path.parent().unwrap()))
            .await
            .unwrap();
        for i in 0..n {
            ns.add(&format!("note number {i}\nbody line for {i}"))
                .unwrap();
        }
    }

    /// CI-PINNED BUDGET (the modulex `DEFAULT_TOOL_BUDGET` pattern, design
    /// §2.3/§3.3): the frozen memory index lists at most `MEMORY_INDEX_BUDGET`
    /// items, no matter how many notes exist. Growing the budget is a
    /// deliberate edit to the constant — this test fails if a feature grows the
    /// default surface as a side effect.
    #[tokio::test]
    async fn memory_index_stays_under_pinned_budget() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("NOTES.md");
        // Seed MANY more notes than the budget.
        seed_notes(&path, MEMORY_INDEX_BUDGET + 25).await;

        let mut idx = MemoryIndex::new(&path);
        idx.initialize(&index_ctx(dir.path())).await.unwrap();

        assert!(
            idx.rows().len() <= MEMORY_INDEX_BUDGET,
            "index surface ({}) exceeds the pinned budget ({MEMORY_INDEX_BUDGET})",
            idx.rows().len()
        );
        assert_eq!(idx.rows().len(), MEMORY_INDEX_BUDGET, "fills to the cap");
        // The block names the overflow recovery (recall), never silently drops.
        let block = idx.system_prompt_block().unwrap();
        assert!(block.contains("use `recall`"), "overflow hint: {block}");
    }

    #[tokio::test]
    async fn memory_index_lists_ids_and_titles_not_bodies() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("NOTES.md");
        seed_notes(&path, 2).await;

        let mut idx = MemoryIndex::new(&path);
        idx.initialize(&index_ctx(dir.path())).await.unwrap();
        let block = idx.system_prompt_block().unwrap();

        // Ids + first-line titles are listed …
        assert!(block.contains("note:1  note number 0"), "got: {block}");
        assert!(block.contains("note:2  note number 1"), "got: {block}");
        // … but NOT the bodies (those are fetched via memory_fetch).
        assert!(!block.contains("body line for 0"), "body leaked: {block}");
        assert!(
            block.contains("call `memory_fetch`"),
            "names the fetch tool: {block}"
        );
    }

    #[tokio::test]
    async fn memory_index_is_system_prompt_only() {
        // Like NoteStore / SoulProvider — never competes for the
        // first-non-empty build_messages slot.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("NOTES.md");
        seed_notes(&path, 1).await;
        let mut idx = MemoryIndex::new(&path);
        idx.initialize(&index_ctx(dir.path())).await.unwrap();
        assert!(idx.build_messages("sys", "task").is_empty());
    }

    #[tokio::test]
    async fn memory_index_empty_notes_contributes_no_block() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("NOTES.md");
        let mut idx = MemoryIndex::new(&path); // no notes file at all
        idx.initialize(&index_ctx(dir.path())).await.unwrap();
        assert!(idx.system_prompt_block().is_none());
    }

    /// INERT BY DEFAULT (#319 acceptance): with no MemoryIndex registered (the
    /// `disclosure = "frozen"` default), the manager's system-prompt additions
    /// and messages are byte-identical to a manager that also omits it — and
    /// crucially they carry NO "Memory index" block. Opting in (index mode)
    /// ADDS the block, proving the frozen path is genuinely the no-op branch.
    /// The MVP changes nothing unless opted in.
    #[tokio::test]
    async fn disclosure_frozen_default_is_bit_for_bit_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("NOTES.md");
        seed_notes(&path, 3).await;

        // Today's shape: RollingWindow + NoteStore, NO MemoryIndex (frozen).
        async fn build(
            path: &std::path::Path,
            ws: &std::path::Path,
            with_index: bool,
        ) -> (String, Vec<MemMessage>) {
            let mut mgr = MemoryManager::new();
            mgr.add_provider(RollingWindow::new(20));
            mgr.add_provider(NoteStore::new(path, NoteStore::DEFAULT_CHAR_LIMIT));
            if with_index {
                mgr.add_provider(MemoryIndex::new(path));
            }
            mgr.initialize_all(&SessionContext {
                workspace: ws.to_string_lossy().into(),
                session_id: "s".into(),
            })
            .await;
            (
                mgr.build_system_prompt_additions(),
                mgr.build_messages("sys", "task"),
            )
        }

        let (frozen_sys, frozen_msgs) = build(&path, dir.path(), false).await;
        // Registration is the only difference; omitting MemoryIndex is a no-op.
        let (frozen_sys2, frozen_msgs2) = build(&path, dir.path(), false).await;
        assert_eq!(frozen_sys, frozen_sys2);
        assert_eq!(frozen_msgs, frozen_msgs2);
        assert!(
            !frozen_sys.contains("Memory index"),
            "frozen mode must NOT add the block: {frozen_sys}"
        );

        // Opting in (index mode) ADDS the index block.
        let (index_sys, _index_msgs) = build(&path, dir.path(), true).await;
        assert!(
            index_sys.contains("Memory index"),
            "index mode adds the block: {index_sys}"
        );
    }
}
