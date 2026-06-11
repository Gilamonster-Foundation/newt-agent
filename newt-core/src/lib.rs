//! Newt-Agent core: shared types, errors, and the tier router.
//!
//! The router is the NeMoCode inheritance — it classifies an incoming turn
//! into a `Tier` (FAST / STANDARD / COMPLEX / REVIEW), and asks the
//! configured backends which can serve that tier.

pub mod agentic;
pub mod agents;
pub mod caveats;
pub mod config;
pub mod conversation;
pub mod dgx;
pub mod error;
pub mod mcp;
pub mod memory;
pub mod metrics;
pub mod model_id;
pub mod notes;
pub mod notes_scan;
pub mod pricing;
pub mod prune;
pub mod retry;
pub mod role_profile;
pub mod router;
pub mod session;
pub mod store;
pub mod tuning;
pub mod workspace_key;

#[cfg(feature = "pyo3")]
pub mod pyo3_module;

pub use agent_mesh_protocol::{Caveats, CountBound, Scope};
// Step 9.7: clean top-level import paths for the relocated agentic loop.
// Step 18.4 (#247): CompressState (session anti-thrash) + Summarizer (the
// loop's injected compression summarizer) join the surface.
// Step 19.4 (#248): trim_for_summary joins it — the TUI's close-time note
// extraction bounds its transcript with the cap-exit summary's own helper.
// Step 18.6 (#247): compress_user_initiated (the `/compress` entry into the
// same pipeline) + CompressCounters (read-only `/memory` snapshot).
pub use agentic::{
    chat_complete, compress_user_initiated, execute_tool, openai_chat_complete, trim_for_summary,
    ChatCtx, CompressCounters, CompressState, ManualCompressOutcome, McpTools, NoMcp, NoteNudge,
    NoteSink, RecallSource, StoreRecallSource, SummarizeFn, SummarizeFuture, Summarizer,
};
pub use agents::AgentsProvider;
pub use caveats::{CaveatsExt, CountBoundExt, ScopeExt};
pub use config::{
    AgentsConfig, BackendConfig, BackendKind, ChatStyle, Config, ConversationsConfig, EditMode,
    LogConfig, MemoryConfig, MemoryProviderKind, PermissionPreset, ProviderConfig, SkillsConfig,
    ToolPermissions, TuiConfig,
};
pub use conversation::{
    new_conversation_id, session_plan_dir, session_plan_path, ConversationRecord,
    ConversationSummary, ConversationTurn, ToolEvent,
};
// Steps 17.1a/17.1b (issue #246): `ConversationStore` is the SQLite backend
// (`store` module, §6 causal ordering). The legacy JSON write path is gone;
// any legacy tree is imported once on open. The `conversation` module keeps
// the storage-agnostic shared types and free functions re-exported above.
pub use dgx::{DgxConfig, DgxFormation, DgxNode, DgxNotConfigured, EndpointKind};
pub use error::NewtError;
pub use memory::{
    MemMessage, MemoryManager, MemoryProvider, NoteStore, NotesUnsupported, Role, RollingWindow,
    SessionContext, SoulProvider, SoulSource, Summarizing, TokenBudget, DEFAULT_CONTEXT_TOKENS,
    DEFAULT_SOUL,
};
pub use metrics::{TokenUsage, TurnMetrics};
pub use model_id::ModelId;
pub use pricing::{ModelRate, PricingConfig};
pub use role_profile::{CaveatProfile, RoleProfile, ScopeKeyword, ScopeSpec};
pub use router::{Router, Tier};
pub use session::SessionId;
pub use store::{sanitize_fts5_query, ConversationStore, SearchHit};
pub use workspace_key::workspace_key_v2;
