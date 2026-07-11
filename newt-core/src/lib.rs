//! Newt-Agent core: shared types, errors, and the tier router.
//!
//! The router is the NeMoCode inheritance — it classifies an incoming turn
//! into a `Tier` (FAST / STANDARD / COMPLEX / REVIEW), and asks the
//! configured backends which can serve that tier.

pub mod agent_identity;
pub mod agentic;
pub mod agents;
pub mod api_surface;
pub mod caveats;
pub mod classifiers;
pub mod config;
pub mod conversation;
pub mod dgx;
pub mod error;
pub mod ffi_manifest;
pub mod ffi_surface;
pub mod git_caveats;
pub mod kit;
pub mod lazy_emission;
pub mod mcp;
pub mod memory;
pub mod metrics;
pub mod model_card;
pub mod model_id;
pub mod notes;
pub mod notes_scan;
pub mod ocap;
pub mod plan;
pub mod pricing;
pub mod prune;
pub mod reasoning;
pub mod retry;
/// #1030 node evaluators: Task/Plan done-ness from objective git state.
pub mod roadmap_eval;
/// #1082 roadmap-as-code: the on-repo TOML codec (`.newt/roadmap.toml`).
pub mod roadmap_file;
pub mod role_profile;
pub mod router;
pub mod scope_grounding;
pub mod scratch;
pub mod session;
pub mod settings;
pub mod ssh_caveats;
pub mod store;
pub mod symbols;
pub mod templates;
pub mod tokens;
pub mod tooling;
pub mod tuning;
pub mod verify_gate;
pub mod workflows;
pub mod workspace_key;

#[cfg(feature = "pyo3")]
pub mod pyo3_module;

/// Carried-coreutils dispatch (agent-bridle #206): a newt binary calls this at
/// the top of `main` to become dispatch-capable, so the brush engine's carried
/// `ls`/`cat` shims re-exec against the newt binary itself.
pub use agent_bridle::maybe_dispatch;
pub use agent_identity::{
    AgentIdentity, GithubApp, IdentitySource, Secret, SecretRef, DEFAULT_AGENT_EMAIL,
    DEFAULT_AGENT_NAME,
};
pub use agent_mesh_protocol::{Caveats, CountBound, Scope};
// Step 9.7: clean top-level import paths for the relocated agentic loop.
// Step 18.4 (#247): CompressState (session anti-thrash) + Summarizer (the
// loop's injected compression summarizer) join the surface.
// Step 19.4 (#248): trim_for_summary joins it — the TUI's close-time note
// extraction bounds its transcript with the cap-exit summary's own helper.
// Step 18.6 (#247): compress_user_initiated (the `/compress` entry into the
// same pipeline) + CompressCounters (read-only `/memory` snapshot).
// Issue #263: the prompted-ocap-grant seam (PermissionGate + friends) joins
// the surface — the TUI implements the gate; headless callers pass None.
pub use agentic::wrap_untrusted;
pub use agentic::{
    append_denial, chat_complete, compress_user_initiated, execute_tool, experience_block,
    gather_code_files, index_files, load_denials, memory_fetch_tool_definition,
    openai_chat_complete, plan_block, retrieve_evidence, transcript_lines, transcript_lines_styled,
    trim_for_summary, widen_caveats, ChatCtx, CodeSearch, CompressCounters, CompressState,
    DenialKind, Embedder, EmbeddingsClient, ExperienceStore, ManualCompressOutcome, McpTools,
    MemAddr, MemPayload, MemorySource, NoMcp, NoteNudge, NoteSink, PermissionDecision,
    PermissionGate, PermissionRecord, PermissionRequest, PlanSnapshot, RecallSource,
    RoundObservation, ScratchpadStore, SemanticIndex, SessionExperienceStore,
    SessionScratchpadStore, SessionSemanticIndex, SessionSpillStore, SessionStepLedger,
    ShellObservation, SpillStore, Step, StepLedger, StepStatus, StoreMemorySource,
    StoreRecallSource, SummarizeFn, SummarizeFuture, Summarizer, TranscriptLine, TranscriptRole,
    TranscriptStyle, TurnDriver, TurnDriverConfig, TurnDriverError, TurnOutcome, TurnStatus,
    EXPERIENCE_TOP_K,
};
pub use agents::AgentsProvider;
pub use api_surface::ApiSurfaceProvider;
pub use caveats::{CaveatsExt, CountBoundExt, ScopeExt};
pub use classifiers::{
    classifier_config_dir, NudgeClass, NudgeClassification, NudgeClassifier, NudgeClassifierConfig,
};
pub use config::{
    full_access_default_engine, ocap_l3_backend, resolve_shell_engine,
    shell_env_passthrough_default, AgentsConfig, BackendConfig, BackendKind, BundleConfig,
    ChatStyle, ColorMode, Config, ContextConfig, ContextFeature, ContextFeatureSet,
    ContextFeatures, ContextManager, ConversationsConfig, CrewPolicyConfig, EditMode, FooterMode,
    Loadout, LoadoutSettings, LogConfig, MarkdownMode, MemoryConfig, MemoryDisclosure,
    MemoryProviderKind, OnEmbedFailure, OpenAiApi, PermissionPreset, PickVia, PlanConfig,
    PlanPruneConfig, ProfilePick, ProviderConfig, ScratchConfig, SemanticConfig, ShellConfig,
    ShellEngine, SkillsConfig, SummarizerConfig, ThinkingMode, ToolPermissions, TuiConfig,
};
pub use conversation::{
    new_conversation_id, session_plan_dir, session_plan_path, ConversationRecord,
    ConversationSummary, ConversationTurn, PhantomReach, PhantomResolution, ToolEvent,
};
pub use ffi_surface::FfiSurfaceProvider;
// `kit::Tier` (Headless|TuiOnly) is *not* re-exported here — it would collide with
// the router's task-complexity `Tier`; reach it as `kit::Tier`.
pub use kit::{Axis, MountKind, RegistryEntry, COMPONENT_REGISTRY};
// Steps 17.1a/17.1b (issue #246): `ConversationStore` is the SQLite backend
// (`store` module, §6 causal ordering). The legacy JSON write path is gone;
// any legacy tree is imported once on open. The `conversation` module keeps
// the storage-agnostic shared types and free functions re-exported above.
pub use dgx::{DgxConfig, DgxFormation, DgxNode, DgxNotConfigured, EndpointKind};
pub use error::NewtError;
pub use memory::{
    MemMessage, MemoryIndex, MemoryManager, MemoryProvider, NoteStore, NotesUnsupported, Role,
    RollingWindow, SessionContext, SoulProvider, SoulSource, Summarizing, TokenBudget, COACH_SOUL,
    DEFAULT_CONTEXT_TOKENS, DEFAULT_SOUL, MEMORY_INDEX_BUDGET,
};
pub use metrics::{TokenUsage, TurnEndReason, TurnMetrics};
pub use model_id::ModelId;
pub use pricing::{ModelRate, PricingConfig};
pub use reasoning::{split_reasoning, ThinkFilter};
pub use role_profile::{
    Altitude, CaveatProfile, NamedPermissionPreset, RoleProfile, ScopeKeyword, ScopeSpec,
};
pub use router::{Router, Tier};
pub use session::SessionId;
pub use store::{
    sanitize_fts5_query, ClaimOutcome, ConversationStore, LivenessFn, Roadmap, RoadmapSummary,
    SearchHit, StoredOwner,
};
pub use tokens::TokenEstimation;
pub use workflows::{
    builtin_workflows, load_workflows_from_dir, merge_workflows, workflow_config_dir,
    WorkflowClassifierConfig, WorkflowConfig, WorkflowSteerer, WorkflowStep,
};
pub use workspace_key::workspace_key_v2;
