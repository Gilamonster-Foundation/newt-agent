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
    // INERT-CODE-RATCHET: X28 WIRE: memory prefetch default and manager fanout have no production caller.
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

    /// Persist a completed submitted turn while separately identifying the
    /// validated active operator task used by prompt-aware compression.
    /// Providers that do not distinguish retry presentation from operator
    /// authority retain the historical behavior through this default.
    async fn sync_turn_with_active_task(
        &mut self,
        user: &str,
        assistant: &str,
        metrics: &TurnMetrics,
        _active_task: &str,
    ) {
        self.sync_turn(user, assistant, metrics).await;
    }

    /// Rebind context-sensitive providers when the active model changes.
    /// Providers without a token budget ignore the update.
    fn set_context_tokens(&mut self, _tokens: u32) {}

    /// Whether this provider consumes a live [`crate::agentic::Summarizer`] and
    /// therefore wants it rebound when a session-inheriting summarizer must
    /// follow a `/model` or `/backend` switch. Only [`Summarizing`] returns
    /// `true`; the fan-out in [`MemoryManager::set_summarizer`] uses it to avoid
    /// building a (possibly GGUF-loading) summarizer for providers that ignore
    /// it.
    fn wants_summarizer(&self) -> bool {
        false
    }

    /// Swap this provider's embedded summarizer in place — the descriptor twin
    /// of [`set_context_tokens`](Self::set_context_tokens). Default no-op;
    /// [`Summarizing`] overrides it. The TUI supplies an already-built
    /// summarizer bound to the *current* route (newt-core stays free of
    /// backend-resolution knowledge, exactly as `with_summarizer` intends).
    fn set_summarizer(&mut self, _summarizer: crate::agentic::Summarizer) {}

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
    // INERT-CODE-RATCHET: X29 WIRE: pre-compression hook has a real override but its manager entry point is never called.
    async fn on_pre_compress(&self, _messages: &[MemMessage]) -> String {
        String::new()
    }

    /// Called once when the session ends. Use for final extraction / cleanup.
    // INERT-CODE-RATCHET: X30 WIRE: session-end memory hook and manager fanout have no production caller.
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

    /// Apply the currently selected model's context budget without rebuilding
    /// providers or losing their conversation-local history.
    pub fn set_context_tokens(&mut self, tokens: u32) {
        for provider in &mut self.providers {
            provider.set_context_tokens(tokens);
        }
    }

    /// Rebind the session-inheriting summarizer after a live `/model` or
    /// `/backend` switch, without rebuilding providers or losing history — the
    /// summarizer twin of [`set_context_tokens`](Self::set_context_tokens).
    ///
    /// `build` is called **at most once**, and only when a provider actually
    /// consumes a summarizer (i.e. [`Summarizing`] is configured), so a
    /// `token_budget` / `rolling` session never pays to construct one (the
    /// embedded engine may load a GGUF). The caller decides whether the
    /// summarizer *follows* the session at all; a pinned summarizer must simply
    /// not call this.
    pub fn set_summarizer(&mut self, build: impl FnOnce() -> crate::agentic::Summarizer) {
        if let Some(provider) = self.providers.iter_mut().find(|p| p.wants_summarizer()) {
            provider.set_summarizer(build());
        }
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

    /// Persist submitted presentation text to every provider while allowing
    /// prompt-aware providers to compress against validated operator authority.
    pub async fn sync_all_with_active_task(
        &mut self,
        user: &str,
        assistant: &str,
        metrics: &TurnMetrics,
        active_task: &str,
    ) {
        for provider in &mut self.providers {
            provider
                .sync_turn_with_active_task(user, assistant, metrics, active_task)
                .await;
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
    /// TUI's save site calls this after `sync_all_with_active_task` and
    /// persists the result as a turn record (Step 18.5, #247).
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

/// Ceiling estimate of one turn's content contribution under the configured
/// [`TokenEstimation`](crate::tokens::TokenEstimation) heuristic.
fn turn_content_estimate(user: &str, assistant: &str, est: crate::tokens::TokenEstimation) -> u32 {
    (est.tokens_for_chars(user.len()) + est.tokens_for_chars(assistant.len())) as u32
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
fn restored_token_anchor(
    turns: &[crate::ConversationTurn],
    est: crate::tokens::TokenEstimation,
) -> (Option<u32>, i64) {
    let Some(pos) = turns.iter().rposition(|t| t.tokens_in.is_some()) else {
        return (None, 0);
    };
    let mut delta = est.tokens_for_chars(turns[pos].assistant.len()) as i64;
    for t in &turns[pos + 1..] {
        delta += i64::from(turn_content_estimate(&t.user, &t.assistant, est));
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
/// Configure via `[memory] provider = "token_budget"`. The caller injects the
/// initial budget (Step 18.2, #247) with this precedence:
///
/// 1. explicit `[memory] context_tokens` (a deliberate user override),
/// 2. the active model's declared window,
/// 3. capability-derived (`max_ok_input` else `safe_context` from the
///    caller's probe cache — newt-core deliberately has no dependency on
///    the probe types, mirroring the `with_summarizer` injection),
/// 4. [`DEFAULT_CONTEXT_TOKENS`] when neither exists (fresh model).
///
/// The TUI rebinds the value in place before every turn, preserving history
/// while following mid-conversation model/backend changes.
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
    /// `[context.estimation]` token-estimation heuristic (config-set via
    /// [`TokenBudget::with_estimation`]); drives every chars→token estimate here.
    est: crate::tokens::TokenEstimation,
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
            est: crate::tokens::TokenEstimation::default(),
        }
    }

    /// Builder: set the token-estimation heuristic from `[context.estimation]`.
    #[must_use]
    pub fn with_estimation(mut self, est: crate::tokens::TokenEstimation) -> Self {
        self.est = est;
        self
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

    fn set_context_tokens(&mut self, tokens: u32) {
        self.max_tokens = tokens.max(512);
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
        let est = turn_content_estimate(user, assistant, self.est);
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
                self.delta_since_prompt = self.est.tokens_for_chars(assistant.len()) as i64;
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
                est_tokens: turn_content_estimate(&t.user, &t.assistant, self.est),
            })
            .collect();
        // Step 18.5 (#247): column-first restore — re-anchor on the last
        // backend-reported prompt size from the 17.6 columns instead of
        // re-estimating the whole history at chars/4. NULL columns fall
        // back to the estimate sum (anchor stays None — an estimate is
        // never presented as a measurement).
        let (anchor, delta) = restored_token_anchor(turns, self.est);
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
    fn new(
        user: impl Into<String>,
        assistant: impl Into<String>,
        est: crate::tokens::TokenEstimation,
    ) -> Self {
        let user = user.into();
        let assistant = assistant.into();
        let est_tokens = turn_content_estimate(&user, &assistant, est);
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
fn wire_to_history(
    messages: &[serde_json::Value],
    est: crate::tokens::TokenEstimation,
) -> Vec<SumTurn> {
    let mut out: Vec<SumTurn> = Vec::new();
    for m in messages {
        let content = m["content"].as_str().unwrap_or_default();
        match m["role"].as_str() {
            Some("user") => out.push(SumTurn::new(content, "", est)),
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
                        last.est_tokens = turn_content_estimate(&last.user, &last.assistant, est);
                    }
                    _ => out.push(SumTurn::new("", content, est)),
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
/// initial value injected by the caller with the same precedence
/// as [`TokenBudget`] (Step 18.2, #247): explicit config override →
/// capability-derived (`max_ok_input` else `safe_context`) →
/// [`DEFAULT_CONTEXT_TOKENS`]. The TUI rebinds it before every turn when the
/// active model's resolved budget changes.
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
    /// Whether the selected context manager may rewrite recorded turns. `false`
    /// (append-only) makes this provider decline to summarize rather than
    /// replacing `history` with a rewritten form — otherwise selecting the
    /// preset would leave the operator believing their transcript was untouched
    /// while this provider rewrote it underneath them.
    rewrites_history: bool,
    /// `[context.estimation]` token-estimation heuristic, and the summarizer
    /// input-cap floor (`[context] summary_input_cap_floor_chars`). Default to
    /// the universal values; the TUI sets them from config via
    /// [`Summarizing::with_estimation`].
    est: crate::tokens::TokenEstimation,
    summary_input_cap_floor_chars: usize,
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
            rewrites_history: true,
            est: crate::tokens::TokenEstimation::default(),
            summary_input_cap_floor_chars: 8_192,
        }
    }

    /// Builder: adopt the selected context manager's rewrite policy. `false`
    /// (append-only) stops this provider summarizing recorded turns — the
    /// `[context] manager` preset governs every path that rewrites history, not
    /// only the agentic loop's compaction trigger.
    #[must_use]
    pub fn with_rewrites_history(mut self, rewrites_history: bool) -> Self {
        self.rewrites_history = rewrites_history;
        self
    }

    /// Builder: set the token-estimation heuristic + summarizer cap floor from
    /// config (`[context.estimation]` / `[context] summary_input_cap_floor_chars`).
    #[must_use]
    pub fn with_estimation(
        mut self,
        est: crate::tokens::TokenEstimation,
        summary_input_cap_floor_chars: usize,
    ) -> Self {
        self.est = est;
        self.summary_input_cap_floor_chars = summary_input_cap_floor_chars;
        self
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
    async fn compress_via_pipeline(&mut self, active_task: &str) {
        use crate::agentic::compress::{
            compress, protect_active_prompt_for_compression, strip_active_prompt_pair,
            CompressAction, CompressRequest,
        };
        if self.history.is_empty() {
            return;
        }
        // Append-only: this provider is one of the paths that rewrites recorded
        // turns, so the preset has to reach it too. Declining here costs recall;
        // summarizing anyway would cost the operator the guarantee they selected.
        if !self.rewrites_history {
            return;
        }
        let messages: Vec<serde_json::Value> =
            self.history.iter().flat_map(SumTurn::to_wire).collect();
        // Carry the just-synced operator prompt as a transient protected pair.
        // It is removed structurally before the compressed wire form is
        // converted back to pair-shaped presentation history, so provider
        // compactions never accumulate harness cards or duplicate operator
        // turns.
        let protected = protect_active_prompt_for_compression(&messages, active_task);
        let outcome = compress(
            CompressRequest {
                rewrites_history: self.rewrites_history,
                messages: &protected,
                budget: self.budget() as usize,
                max_messages: None,
                replay_protected_tail_len: 0,
                task: active_task,
                hard_budget: true,
                // The memory budget is config-derived — authoritative, so
                // refuse semantics are preserved here (Step 20.3).
                authoritative: true,
                focus: None,
                est: self.est,
                summary_input_cap_floor_chars: self.summary_input_cap_floor_chars,
                compaction_store: None,
                compaction_stage: None,
            },
            self.summarizer.as_deref(),
            &mut self.state,
        )
        .await;
        if !outcome.fired {
            return;
        }
        let messages = strip_active_prompt_pair(outcome.messages, active_task);
        self.history = wire_to_history(&messages, self.est);
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

    /// Persist the submitted presentation while compressing against the
    /// independently validated operator task.  Retry/nudger text belongs in
    /// ordinary presentation history, but must never replace operator
    /// authority in the shared summarization prompt.
    async fn record_turn_with_active_task(
        &mut self,
        user: &str,
        assistant: &str,
        metrics: &TurnMetrics,
        active_task: &str,
    ) {
        let est = turn_content_estimate(user, assistant, self.est);
        self.history.push(SumTurn::new(user, assistant, self.est));
        match metrics.usage {
            Some(u) => {
                // Anchor on the largest single prompt the backend evaluated
                // this turn (Step 18.1); only the reply is not yet inside it.
                self.last_prompt_tokens = Some(u.input_tokens);
                self.delta_since_prompt = self.est.tokens_for_chars(assistant.len()) as i64;
            }
            None => self.delta_since_prompt += i64::from(est),
        }
        // Over budget -> the shared pipeline (which owns anti-thrash); the
        // cheap is_disabled gate just avoids rebuilding the wire view for
        // a call that would be refused anyway.
        if self.used_tokens() > self.budget() && !self.state.is_disabled() {
            self.compress_via_pipeline(active_task).await;
        }
    }
}

#[async_trait]
impl MemoryProvider for Summarizing {
    fn name(&self) -> &str {
        "summarizing"
    }

    fn set_context_tokens(&mut self, tokens: u32) {
        self.max_tokens = tokens.max(1);
    }

    fn wants_summarizer(&self) -> bool {
        true
    }

    fn set_summarizer(&mut self, summarizer: crate::agentic::Summarizer) {
        // Swap the embedded summarizer in place — history and budget are
        // untouched. The TUI only calls this for a SESSION-INHERITING summarizer
        // after a live route switch; a pinned summarizer is never rebound.
        self.summarizer = Some(summarizer);
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
        self.record_turn_with_active_task(user, assistant, metrics, user)
            .await;
    }

    async fn sync_turn_with_active_task(
        &mut self,
        user: &str,
        assistant: &str,
        metrics: &TurnMetrics,
        active_task: &str,
    ) {
        self.record_turn_with_active_task(user, assistant, metrics, active_task)
            .await;
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
            self.history
                .push(SumTurn::new(turns[k].user.clone(), "", self.est));
            self.prev_summary = turns[k].user.clone();
        }
        self.history.extend(
            live.iter()
                .map(|t| SumTurn::new(&*t.user, &*t.assistant, self.est)),
        );
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
        let (anchor, delta) = restored_token_anchor(live, self.est);
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
You are newt, a free, friendly, local agentic coder. \
Be concise and direct. \
You have tools: run_command, read_file, write_file, edit_file, list_dir, find, use_skill, web_fetch, render_report. \
Follow the per-turn disposition card supplied by the harness: only an `act` turn may mutate the workspace or receive execution pressure; `ask` asks its bounded clarification and stops, `explain` answers without mutation, `research` gathers bounded read-only evidence, and `plan` may update only the harness-owned plan ledger.\n\
\n\
## How to work\n\
\n\
**One change at a time.** Read only the files you need for the immediate next step. \
Make the change. Commit it. Then move to the next step. \
Never accumulate multiple uncommitted edits — a committed partial result survives a crash; \
an uncommitted complete result does not.\n\
\n\
**Read minimum, act deliberately.** Resist reading the entire codebase before acting. \
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
**Sign every commit.** Credit yourself on every commit you author with a \
`Co-authored-by:` trailer naming the model and harness, e.g. \
`Co-authored-by: <model> (v<version> <build>) <309460085+newt-agent@users.noreply.github.com>`. \
The account name is not repeated in the parenthetical — the address already \
carries it, so the qualifier holds only the harness version and the build it \
was compiled from. \
The harness identity is the newt-agent GitHub App — link it: \
<https://github.com/apps/newt-agent>. This is the house default, not an \
occasional courtesy — include it on every commit, including commits made \
through the shell `git` path (which bypasses the harness's automatic \
trailer). When you amend or reword, keep the trailer.\n\
\n\
**On an `act` turn, never describe a code change — make it.** Do not paste code into the chat. \
If the task requires a code change and the disposition is `act`, call edit_file or write_file immediately. \
A markdown code block in the conversation is invisible to the filesystem — \
it does not modify any file. Write the code once, into the file, via the tool. \
Showing code in text is NOT completing an `act` task; calling the tool IS. \
For `ask`, `explain`, `research`, or `plan`, respect the disposition instead of forcing a workspace write.\n\
\n\
**Present findings — don't just report a blocker.** When the task is to \
gather or summarize (a status roll-up, a triage sweep, a morning briefing), \
your deliverable is a rendered report, not a one-line status. The moment you \
have what was asked, call render_report to present it. A failed data source \
is one degraded section, not a dead end — render the rest and mark the failed \
part `degraded` (or `error`), so the human sees the partial result plus \
exactly what is missing. Ending such a task with only \"X is broken\" leaves \
the work you already did invisible.\n\
\n\
**Exploration budget for `act`.** Treat read-only rounds (list_dir, read_file) as expensive. \
Spend at most three consecutive rounds on exploration before making a write. \
Once you have read the file you need, stop reading and call edit_file or write_file. \
Continued reading without writing means you are lost — make your best attempt \
at the change based on what you have already read, then verify. In `research`, \
the bounded read-only evidence collection is the work; report it rather than \
forcing a mutation.\n\
\n\
**Working code first, then the three Cs.** Make it work, then make it right. \
Shipping a working result that hardcodes a list or a constant to get there is \
fine; functional results come first. Then RETURN to the three Cs: lift hardcoded \
knowledge (keyword lists, magic values, language or domain rules) into pure DATA \
that is Composed, Configured, and Convention-driven, so a new case is config, \
not code. Don't let this block shipping; do circle back and de-hardcode once it \
works.";

/// FR-5 (#999): the ADVISE-first identity installed when a persona's altitude is
/// [`crate::Altitude::Coach`] (front-matter `altitude = "coach"`). It REPLACES
/// [`DEFAULT_SOUL`] rather than layering over it — the doer soul ("never
/// describe a change, make it") directly contradicts a coach, so appending a
/// coach overlay onto the doer soul would ship two opposing identities in one
/// prompt. This IS the whole base identity for a coaching turn; the persona's
/// own markdown body still overlays on top, now without self-contradiction.
pub const COACH_SOUL: &str = "\
You are newt in COACH mode: an advisor, not a doer. Be concise and direct. \
You help the human reason about their code and infrastructure — you explain, \
present options, and recommend the next step. You do NOT make the change \
yourself.\n\
\n\
## How to coach\n\
\n\
**Advise; do not act.** Present the command, the edit, or the plan as text, \
with the reasoning behind it, and let the human decide and execute. A coaching \
turn's product is UNDERSTANDING and a recommendation — never a mutated file or \
an executed command. Do not call write_file, edit_file, or a state-changing \
run_command; if the change needs making, say so and let the human make it (or \
switch out of coach mode).\n\
\n\
**Ground your advice in the real code.** Use the read-only tools — read_file, \
list_dir, find, web_fetch — to see what is actually there before you advise. \
Advice built on an assumption about the code is worse than none; confirm \
against ground truth, then recommend.\n\
\n\
**Show the command, don't run it.** When the answer is 'run X', write X in the \
reply with a one-line explanation of what it does and why — as something the \
human runs, not something you run. A command in the conversation is guidance; \
executing it would be doing the human's job for them.\n\
\n\
**Teach the reasoning, not just the answer.** Explain WHY, name the trade-offs, \
and point at the one next step. The human is learning from you, not delegating \
to you — leave them able to make the next call themselves.\n\
\n\
**Stop when unsure.** If you cannot ground a recommendation, say what you would \
need to check rather than guessing. One honest 'here is what I'd verify first' \
beats a confident wrong answer.";

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
#[path = "memory_tests/mod.rs"]
mod tests;
