//! Retained context, soul-file selection, note policy, and memory disclosure.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Memory config
// ---------------------------------------------------------------------------

/// Memory management stored under `[memory]` in `newt.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    /// Which memory provider to activate.
    #[serde(default)]
    pub provider: MemoryProviderKind,
    /// Turns retained by `RollingWindow`. Default: 20.
    #[serde(default = "default_memory_window")]
    pub window: usize,
    /// Explicit context-token budget for `TokenBudget` / `Summarizing` — a
    /// deliberate user override that wins over everything else (Step 18.2,
    /// #247). When unset, the budget derives from the empirical capability
    /// cache (`max_ok_input` else `safe_context` in
    /// `model-capabilities.json`); the static default
    /// (`DEFAULT_CONTEXT_TOKENS`, 8,192) applies only when neither exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u32>,

    /// Explicit path to a soul file (overrides workspace + global resolution).
    /// Default: auto-resolve from `.newt/soul.md` → `~/.newt/soul.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub soul_file: Option<String>,

    /// User turns without an organic `save_note` call before the in-band
    /// memory nudge is appended to the next user message (Step 19.3, #248).
    /// `0` disables the nudge. Default: 10.
    #[serde(default = "default_note_nudge_interval")]
    pub note_nudge_interval: usize,

    /// End-of-conversation note extraction (Step 19.4, #248): when `true`,
    /// closing a conversation (`/new` or a clean exit) runs ONE synchronous
    /// tools-disabled completion that distills at most 3 durable facts into
    /// NOTES.md through the scanned `save_note` write path. Default: `false`
    /// — the pass is optional and costs one completion per close.
    #[serde(default)]
    pub extract_notes_on_close: bool,

    /// How memory is disclosed to the model (progressive-disclosure memory,
    /// Workstream A MVP, #319). `Frozen` (the default) is today's behavior
    /// exactly: NOTES are frozen verbatim into the system prompt and the
    /// `memory_fetch` tool is not wired. `Index` opts in to the budgeted
    /// memory INDEX (note titles/ids instead of full bodies) plus the
    /// `memory_fetch` tool that pulls a body on demand. This is a context-cost
    /// facet, never an authorization knob.
    #[serde(default)]
    pub disclosure: MemoryDisclosure,
}

/// Memory disclosure mode — the `[memory] disclosure` key (#319).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MemoryDisclosure {
    /// Today's behavior: NOTES frozen verbatim into the system prompt, no
    /// `memory_fetch` tool. The MVP default — inert unless opted in.
    #[default]
    Frozen,
    /// Progressive disclosure: a budgeted memory INDEX in the prompt plus the
    /// `memory_fetch` tool to pull bodies on demand.
    Index,
}

fn default_memory_window() -> usize {
    20
}

fn default_note_nudge_interval() -> usize {
    10
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            provider: MemoryProviderKind::RollingWindow,
            window: 20,
            context_tokens: None,
            soul_file: None,
            note_nudge_interval: 10,
            extract_notes_on_close: false,
            disclosure: MemoryDisclosure::Frozen,
        }
    }
}

/// Which built-in memory strategy to use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MemoryProviderKind {
    #[default]
    RollingWindow,
    TokenBudget,
    Summarizing,
}
