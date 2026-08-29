//! Newt-Agent core: shared types, errors, and the tier router.
//!
//! The router is the NeMoCode inheritance — it classifies an incoming turn
//! into a `Tier` (FAST / STANDARD / COMPLEX / REVIEW), and asks the
//! configured backends which can serve that tier.

pub mod agent_identity;
pub mod agentic;
pub mod agents;
pub mod api_surface;
pub mod artifact;
pub mod atomic_fs;
pub mod attribution;
pub mod backend_probe;
pub mod build_info;
pub mod card_catalog;
pub mod caveats;
pub mod classifiers;
/// The cognition session dial — the `/cognition` override resolved over a
/// persona's `cognition:` (psyche sibling of [`tenacity`]).
pub mod cognition;
pub mod config;
pub mod confined_exec;
pub mod conversation;
pub mod credential_registry;
pub mod denial_journal;
pub mod dgx;
pub mod dock_registry;
pub mod drift_cache;
pub mod enrollment;
pub mod error;
pub mod ffi_manifest;
pub mod ffi_surface;
pub mod flight_recorder;
pub mod netguard;
pub mod owned_hosts;
// Object-bound workspace filesystem capability (step-52.1). `openat2` is
// Linux-only, so the capability exists only there; consumers apply the
// cross-platform fallback + fail-closed-for-untrusted policy (step-52.2/52.3).
#[cfg(target_os = "linux")]
pub mod fs_cap;
pub mod git_caveats;
pub mod git_hardening;
pub mod grounding;
pub mod interaction_adapter;
pub mod interaction_form;
pub mod interaction_gate;
pub mod interaction_offer;
// C1 (#1862): the SEMANTIC half of the surface seam. Deliberately separate
// from newt-tui's thread-shaped `SurfaceRequest`, which carries channels and
// `Arc`s and can never cross a process (epic non-goal).
pub mod interaction_resolution;
pub mod interaction_surface;
// C2 (#1876): the interaction VIEW MODEL. Here rather than in `newt-tui`
// because `newt-core` has no ratatui dependency, so "no renderer type in the
// model" is a compile error instead of a source scan.
// C2a (#1876). Built on `markup::spans`, which is `markdown`-gated because it
// rides the one parser — so this module must carry the same gate. It did not,
// and `--no-default-features` therefore did not COMPILE (#1890): an unresolved
// `crate::markup::spans`, not merely a lint. The wyvern tier falls back to
// `markup::plain`, which stays unconditional for exactly this reason.
#[cfg(feature = "markdown")]
pub mod interaction_view;
pub mod kit;
pub mod launch_authority;
pub mod lazy_emission;
/// Generic agent lifecycle events — the seam integrations subscribe to.
pub mod lifecycle;
pub mod markup;
pub mod mcp;
pub mod mcp_catalog;
pub mod mcp_probe;
pub mod memory;
pub mod metrics;
pub mod model_card;
pub mod model_id;
pub mod navigator;
pub mod notes;
pub mod notes_scan;
pub mod nudger;
pub mod ocap;
pub mod ocap_propose;
pub mod ocap_store;
/// Danger-tier terminal-echo policy. Was `PermissionChallenge` too, until
/// #1839 deleted that hand-rolled encoding in favour of the definition's
/// `ContentId`.
pub mod permission_challenge;
pub mod plan;
pub mod pricing;
/// The one lock over the process environment (#1850) — every newt writer of
/// `NEWT_*` goes through it, production and tests alike.
pub mod process_env;
/// D2a (#1864): the typed progress/lifecycle contract and its
/// renderer-neutral sink. Deliberately separate from the interaction
/// request/reply path — a spinner tick is not an offer.
pub mod progress;
pub mod project_map;
pub mod project_model;
pub mod prompt;
/// Hosted-provider presets (the wizard's roster; Hermes-compatible drop-ins) —
/// distinct from `[[providers]]` subprocess plugins.
pub mod provider_preset;
pub mod prune;
/// Psyche posture macros (e.g. `obsessive`) — named acts that move several
/// psyche dials ([`cognition`] + [`tenacity`], + crew at the caller) at once.
pub mod psyche;
pub mod reasoning;
pub mod responses_wire;
pub mod retry;
/// #1030 node evaluators: Task/Plan done-ness from objective git state.
pub mod roadmap_eval;
/// #1082 roadmap-as-code: the on-repo TOML codec (`.newt/roadmap.toml`).
pub mod roadmap_file;
pub mod role_profile;
pub mod router;
pub mod runtime;
pub mod sas_confirm;
pub mod sas_transcript;
pub mod scope_grounding;
pub mod scratch;
pub mod secrets;
pub mod session;
pub mod settings;
pub mod shell_env;
pub mod ssh_caveats;
pub mod stack;
pub mod store;
pub mod symbols;
pub mod templates;
pub mod tenacity;
/// A shared RAII guard for tests that touch the process-global operator settings
/// (cognition / tenacity / `NEWT_*`) — one lock + Drop-restored snapshot.
pub mod test_guard;
/// Self-scheduled wake-up timers — see `timer` module docs.
pub mod timer;
pub mod tokens;
pub mod tooling;
/// Terminal-line ownership: the process-wide arbiter every ephemeral writer
/// (spinner, viewport, progress readout) leases the bottom line from, plus the
/// shared frame set / width-fitting primitives they all draw through.
pub mod tty;
pub mod tuning;
pub mod verify_gate;
pub mod where_is;
mod wire_framing;
pub mod workflows;
pub mod workspace_key;

#[cfg(feature = "pyo3")]
pub mod pyo3_module;

/// Carried-coreutils dispatch (agent-bridle #206): a newt binary calls this at
/// the top of `main` to become dispatch-capable, so the brush engine's carried
/// `ls`/`cat` shims re-exec against the newt binary itself.
pub use agent_bridle::maybe_dispatch;
pub use agent_identity::{
    default_operator, AgentIdentity, GithubApp, IdentitySource, Secret, SecretRef,
    AGENT_IDENTITY_FILENAME, DEFAULT_AGENT_EMAIL, DEFAULT_AGENT_NAME, GITHUB_APP_BOT_EMAIL,
    GITHUB_APP_BOT_NAME,
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
pub use agentic::preserve_mcp_resource_url_affinity;
pub use agentic::wrap_untrusted;
pub use agentic::{
    append_denial, chat_complete, chat_complete_with_prompt, compress_user_initiated,
    compress_user_initiated_for_task, execute_tool, experience_block, format_index_status,
    format_search_hits, format_search_model, format_search_preview, format_search_rejects,
    gather_code_files, gather_with_manifest, index_files, load_denials,
    memory_fetch_tool_definition, openai_chat_complete, openai_chat_complete_with_prompt,
    openai_responses_complete, openai_responses_complete_with_prompt, parse_context_window_error,
    plan_block, recover_context_window_400, render_code_evidence, retrieve_evidence,
    retrieve_evidence_steered, retrieve_ranked, set_spill_lines, set_spill_summary,
    transcript_lines, transcript_lines_styled, trim_for_summary, widen_caveats, BehaviorSignal,
    ChatCtx, CodeSearch, CompressCounters, CompressState, DenialKind, Embedder, EmbeddingsClient,
    ErrorClass, EvidenceKind, ExperienceStore, ExposureSettings, GatherCaps, GatherManifest,
    HeadlessCodeSearch, HumanQuestionOutcome, IndexStatus, LiveToolOutput, ManualCompressOutcome,
    ManualCompressPolicy, McpTools, MemAddr, MemPayload, MemorySource, NoMcp, NoteNudge, NoteSink,
    ParseSignal, PermissionAction, PermissionDecision, PermissionGate, PermissionRecord,
    PermissionRequest, PlanModeControl, PlanSnapshot, RankedHit, RecallSource, RejectReason,
    RetrievalResult, RetrievalSteer, RoundObservation, ScratchpadStore, SemanticIndex,
    SessionExperienceStore, SessionScratchpadStore, SessionSemanticIndex, SessionSpillStore,
    SessionStepLedger, ShellObservation, SolveObservation, SpillStore, Step, StepLedger,
    StepStatus, StoreMemorySource, StoreRecallSource, SummarizeFn, SummarizeFuture, Summarizer,
    ToolCallDialect, ToolOutputStream, TranscriptLine, TranscriptRole, TranscriptStyle, TurnDriver,
    TurnDriverConfig, TurnDriverError, TurnOutcome, TurnStatus, EXPERIENCE_TOP_K,
    MCP_RESOURCE_URL_PREFIXES_META_KEY,
};
pub use agents::AgentsProvider;
pub use api_surface::{resolve_surface_budget, ApiSurfaceProvider};
pub use artifact::{
    ArtifactId, ArtifactKind, ArtifactRelation, NewPromptArtifact, PromptArtifact,
    MAX_ARTIFACT_BODY_BYTES, MAX_ARTIFACT_LOCATOR_BYTES, MAX_ARTIFACT_METADATA_BYTES,
};
pub use caveats::{permits_path, CaveatsExt, CountBoundExt, ScopeExt};
pub use classifiers::{
    classifier_config_dir, NudgeClass, NudgeClassification, NudgeClassifier, NudgeClassifierConfig,
};
#[allow(deprecated)]
pub use config::writeback_probed_backend;
pub use config::{
    claim_backend_dropin_as_operator, classify_backend_dropin, confined_default_engine,
    derive_serving, full_access_default_engine, mcp_stdio_env_passthrough, ocap_l3_backend,
    persist_probe_observation, render_operator_backend_dropin, resolve_shell_engine,
    resolve_shell_engine_choice, resolved_confined_default, shell_env_passthrough_default,
    write_backend_dropin, AgentsConfig, BackendConfig, BackendDestination, BackendKind,
    BackendRequest, BackendResolutionReceipt, BundleConfig, ChatStyle, ColorMode,
    CompactionTriggerPolicy, Config, ContextConfig, ContextFeature, ContextFeatureSet,
    ContextFeatures, ContextManager, ConversationsConfig, CrewPolicyConfig, DeclaredBackend,
    DropinOwnership, EditMode, ExposureProfile, FooterMode, IntakeConfig, Loadout, LoadoutSettings,
    LogConfig, ManagedMode, MarkdownMode, MemoryConfig, MemoryDisclosure, MemoryProviderKind,
    OnEmbedFailure, OpenAiApi, PermissionPreset, PickVia, PlanConfig, PlanPruneConfig,
    ProbeObservation, ProbeWriteback, ProbedServing, ProfilePick, ProviderConfig, RequestMode,
    ResolvedBackend, ResolvedConfig, ScratchConfig, SemanticConfig, Serving, ShellConfig,
    ShellEngine, SkillsConfig, SummarizerConfig, ThinkingMode, ToolExposureConfig, ToolPermissions,
    TuiConfig,
};
pub use conversation::{
    new_conversation_id, session_plan_dir, session_plan_path, ConversationRecord,
    ConversationSummary, ConversationTurn, PhantomReach, PhantomResolution, ToolEvent,
};
pub use ffi_surface::FfiSurfaceProvider;
pub use navigator::{
    compare_ledgers, compare_semantic_lexical, execute_nav_tool, export_ledger_json,
    export_ledger_markdown, find_callees, find_callers, find_hierarchy, find_implementations,
    find_references, find_tests, format_ledger_diff, format_ledger_human, format_ledger_model,
    goto_definition, hash_context, impact_analysis, inspect_type, project_map_nav, text_search,
    GotoDefinitionArgs, GraphIndex, ImpactReport, NavHit, NavResult, NavToolCtx, NavigatorSession,
    RetrievalLedger, TurnRetrieval, UsageIndex, UsageSite, NAV_TOOL_NAMES,
};
pub use project_map::ProjectMapProvider;
pub use runtime::{
    BackendAxisAction, BackendState, OperatorPreferencePin, PreferenceActions, PreferenceApplyPlan,
    PreferenceAxes, RouteModel, RuntimeSettingsSnapshot,
};
pub use tenacity::Tenacity;
pub use where_is::{
    build_where_is_index, build_where_is_index_from_workspace, execute_where_is,
    where_is_tool_definition, LookupVerdict, WhereIsIndex, Witness,
};
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
pub use prompt::{
    ActivePrompt, NewPrompt, PromptId, PromptOrigin, PromptReceipt, TurnPromptContext,
};
pub use reasoning::{split_reasoning, ThinkFilter};
pub use role_profile::{
    Altitude, CaveatProfile, NamedPermissionPreset, RoleProfile, ScopeKeyword, ScopeSpec,
};
pub use router::{Router, Tier};
pub use session::SessionId;
// B0b-2 (#1846): the audience a surface answers from. Re-exported so
// newt-web can name it without taking a direct newt-interaction
// dependency — it consumes the protocol through newt-core, as it does
// every other protocol type.
pub use interaction_offer::{OfferDanger, PendingOffer};
pub use newt_interaction::Audience;
/// Re-exported for the views that PROJECT a definition rather than adapt it.
///
/// `Audience` has been re-exported here since B0b for exactly this reason:
/// newt-web is a view, and a view reads the semantic model. C3c (#1867) adds
/// `ControlKind` because the web card now renders an offer's options straight
/// from its `InteractionDefinition` instead of round-tripping it back into a
/// legacy `Question`. Re-exporting beside `Audience` keeps one import path in
/// the consumer; reaching past `newt-core` for half the vocabulary and through
/// it for the other half is how two spellings of the same model start.
pub use newt_interaction::ControlKind;
pub use store::{
    sanitize_fts5_query, AnswerOutcome, ClaimOutcome, ConversationStore, InjectOutcome,
    InjectedPrompt, LivenessFn, Roadmap, RoadmapSummary, SearchHit, StoredOwner, Verdict,
};
pub use tokens::TokenEstimation;
pub use tty::{Action, Question};
pub use workflows::{
    builtin_workflows, load_workflows_from_dir, merge_workflows, workflow_config_dir,
    WorkflowClassifierConfig, WorkflowConfig, WorkflowSteerer, WorkflowStep,
};
pub use workspace_key::workspace_key_v2;
