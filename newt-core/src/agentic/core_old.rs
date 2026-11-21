//! Core agentic functionality for the newt agent system

// This module contains the core agentic loop implementation that coordinates
// model calls, tool execution, and feedback loops.

pub use crate::agentic::tools::{
    ambient_interpreter_description, builtin_tool_names, execute_tool, execute_tool_quiet,
    execute_tool_with_offload, execute_tool_with_offload_and_prompt_and_artifacts,
    exit_plan_mode_result, filter_advertised_tools, filter_tools_for_disposition,
    full_access_requested, ocap_disabled, persona_tool_allowed, plan_phase_clamp,
    preserve_mcp_resource_url_affinity, read_only_clamp, read_only_hidden_tool,
    run_command_dialect_sentence, set_max_output_tokens, set_output_cap_chars_per_token,
    set_output_head_tokens, shell_dialect_sentence, tool_allowed, tool_definitions,
    venv_cmd_prefix, ExposureSettings, CHILD_STRIPPED_AUTHORITY_ENV,
    MCP_RESOURCE_URL_PREFIXES_META_KEY,
};

pub use crate::agentic::tool_set::ToolSet;
pub use crate::agentic::prompt_intake::{
    discussion_request, AdjudicationCandidate, AdjudicationRefusal, AdjudicationVerdict, AtomicAsk,
    ClarificationRejection, DecisionLock, DecisionSource, DecisionStatus, DispositionLexicon,
    DispositionSource, PromptComprehensionManifest, PromptDisposition, PromptIntake,
    MAX_ADJUDICATION_BATCH,
};

pub use crate::agentic::driver::{
    HeadlessCodeSearch, InstantiatedFeatures, TurnDriver, TurnDriverConfig, TurnDriverError,
    TurnOutcome, TurnStatus,
};

pub use crate::agentic::mcp::{
    classify_mcp_effect, leash_mcp_call, LeasedMcpCall, LeashDenied, McpEffect, McpGrant, McpTools,
    NoMcp,
};

pub use crate::agentic::adjudicate::{adjudicate_decisions, AdjudicationFailure};

pub use crate::agentic::observation::{ShellObservation, SHELL_OBSERVATION_PREFIX};

pub use crate::agentic::artifact_hooks::{
    record_manual_compaction_checkpoint, record_memory_compaction_checkpoint,
    record_observed_head_transition, record_prompt_comprehension_manifest,
    record_retry_revert_file, record_turn_outcome,
};

pub use crate::agentic::artifact_read::{
    artifact_read_tool_definition, ArtifactPage, ArtifactReadContext, ArtifactReadRecord,
    ArtifactSource, PromptArtifactSink, SessionArtifactStore, StoreArtifactStore,
};

pub use crate::agentic::compress::{
    compress_user_initiated, compress_user_initiated_for_task, CompressCounters, CompressState,
    FloorTrend, ManualCompressOutcome, ManualCompressPolicy, SummarizeFn, SummarizeFuture,
    Summarizer, CONTINUATION_PREFIX, PLAN_APPROVAL_PREFIX, SUMMARY_END_MARKER, SUMMARY_PREFIX,
};

pub use crate::agentic::content_spill::{
    session_output_cid, SessionSpillStore, SpillCid, SpillCidError, SpillProvenance, SpillRecordV1,
    SpillScope, SpillStore, MIN_SPILL_TAIL_CHARS,
};

pub use crate::agentic::crew_attest::{crew_authz, crew_step_up_policy, CrewAuthz, Presence};

pub use crate::agentic::crew_tool::{compose_roster_tool_definition, crew_tool_definition, CrewRunner};

pub use crate::agentic::cw_overflow::{parse_context_window_error, recover_context_window_400};

pub use crate::agentic::display::{
    fmt_token_gauge, fmt_tokens_compact, gauge_level, interactive_recovery, newt_line,
    print_harness_notice, print_list_item, print_newt, reply_cols, set_mouse_recovery,
    set_spill_lines, set_spill_summary, set_time_marker_secs, spill_summary, Fold, GaugeLevel,
    Hidden, Recovery, ThinkingFold, NEWT_ORANGE_CT, REPLY_MARKER,
};

pub use crate::agentic::git_tool::{check_git_read_scope, git_tool_definition, GitTool};

pub use crate::agentic::markdown::{render_markdown, MarkdownStreamWriter, RenderOpts};

pub use crate::agentic::memory_fetch::{
    memory_fetch_tool_definition, MemAddr, MemPayload, MemorySource, StoreMemorySource,
};

pub use crate::agentic::note_sink::{save_note_tool_definition, NoteNudge, NoteSink};

pub use crate::agentic::operating_mode::{select_operating_mode_tool_definition, OperatingModeControl};

pub use crate::agentic::plan_mode::{
    plan_verdict, PlanDraft, PlanDraftSink, PlanEntry, PlanModeControl, PlanVerdict, PresentedPlan,
    PLAN_APPROVAL_CHOICES,
};

pub use crate::agentic::recall::{recall_tool_definition, RecallSource, StoreRecallSource};

pub use crate::agentic::resume::resume_context_tool_definition;

pub use crate::agentic::send_budget::{initial_context_input_budget, is_truncation_suspect};

pub use crate::agentic::steering::{ReplaceRejected, Rev, SessionSteeringInbox, SteeringInbox};

pub use crate::agentic::transcript::{
    transcript_lines, transcript_lines_styled, TranscriptLine, TranscriptRole, TranscriptStyle,
};

pub use crate::agentic::trim::{ollama_usage, openai_usage, trim_for_summary};

pub use crate::agentic::untrusted::{wrap_internal_summary, wrap_untrusted};

pub use crate::agentic::warmup::warmup_if_cold;

pub use crate::agentic::tool_recovery::ToolRecovery;

pub use crate::agentic::plan_exec::{run_plan, run_plan_with_reground, NoReground, PlanRun, Reground};

pub use crate::agentic::observability::{
    classify_reqwest, error_class, round_parse_signal, BehaviorSignal, DispatchError, Enforcement,
    ErrorClass, OutputAllowance, ParseSignal, SolveObservation, ToolCallDialect,
};

pub use crate::agentic::semantic::{
    chunk_source, code_search_tool_definition, cosine, format_index_status, format_search_hits,
    format_search_model, format_search_preview, format_search_rejects, gather_code_files,
    gather_with_manifest, index_files, plan_gather, render_code_evidence, retrieve_evidence,
    retrieve_evidence_steered, retrieve_ranked, retrieve_ranked_with_cap, CodeChunk, CodeSearch,
    Cut, CutClass, Embedder, EmbeddingsClient, EvidenceKind, GatherCaps, GatherManifest,
    IndexStatus, RankedHit, RejectReason, RetrievalResult, RetrievalSteer, SemanticIndex,
    SessionSemanticIndex,
};

pub use crate::agentic::experiential::{
    experience_block, ExperienceStore, SessionExperienceStore, EXPERIENCE_TOP_K,
};

pub use crate::agentic::scheduled::{
    fresh_multi_step_plan, plan_block, plan_reseat_pointer, step_change_line, PlanSnapshot,
    SessionStepLedger, Step, StepLedger, StepStatus,
};

pub use crate::agentic::scratchpad::{
    scratchpad_state_block, working_memory_head, ScratchpadStore, SessionScratchpadStore,
};

pub use crate::agentic::self_verify::{
    enabled as self_verify_enabled, outcomes_enabled as verify_outcomes_requested,
    verification_gate_present, verification_receipt,
};

pub use crate::agentic::budget::get_context_remaining_tool_definition;

pub use crate::agentic::prompt_read::{
    prompt_read_tool_definition, PromptReadContext, PromptSource, SessionPromptSource,
    SessionPromptStore, StorePromptSource,
};

pub use crate::agentic::claim_check::{
    files_changed_between, is_workspace_repo_root, locate_workspace_repo, nested_current_paths,
    nested_files_changed_between, snapshot_nested_repos, snapshot_workspace,
    snapshot_workspace_subtree, NestedRepoSnapshot, StatusSnapshot, WorkspaceRepoLocation,
};

pub use crate::agentic::observed_report::assistant_prose as model_reply_for_history;
pub use crate::agentic::observed_report::split_report as split_observed_report;

pub use crate::agentic::generation_policy::{
    preview_chat_generation, projected_cognition, validate_output_allowance, ChatGenerationPreview,
};

pub use crate::agentic::output_stream::{
    CompletedSpillRenderer, LiveToolOutput, RoundObservation, ToolOutputStream,
};

pub use crate::agentic::run_allowance::RunAllowance;
pub use crate::agentic::digest_fold::DigestFold;
pub use crate::agentic::smart_harness::SmartHarness;
pub use crate::agentic::request_preview::RequestPreview;
pub use crate::agentic::request_report::RequestReport;
pub use crate::agentic::anthropic_wire::AnthropicWire;
pub use crate::agentic::openai_sse::OpenaiSse;
pub use crate::agentic::attempt_capture::AttemptCapture;
pub use crate::agentic::selected_target::SelectedTarget;
pub use crate::agentic::semantic_pins::SemanticPins;
pub use crate::agentic::attribution::AttributionEpoch;
pub use crate::agentic::context_recovery::ContextRecovery;
pub use crate::agentic::display::Display;
pub use crate::agentic::generation_bounds::GenerationBounds;
pub use crate::agentic::generation_repetition::GenerationRepetition;
pub use crate::agentic::output_budget::OutputBudget;
pub use crate::agentic::output_limit_recovery::OutputLimitRecovery;
pub use crate::agentic::publish_early::PublishEarly;
pub use crate::agentic::compress::Compress;
pub use crate::agentic::cw_overflow::CwOverflow;
pub use crate::agentic::capability_check::CapabilityCheck;
pub use crate::agentic::final_reply::FinalReply;

// Re-exports for the core functionality
pub use crate::agentic::diagnostics::{
    announce_tool_activity, cancellable, dismiss_completed_spill, is_cancelled,
    is_ollama_tool_xml_error, is_tools_unsupported_error, legacy_caps, ollama_tool_xml_retry_nudge,
    print_synthetic_tool_result,
};

// Re-exports for the core functionality
pub use crate::agentic::tools::ToolDefinitions;

// Re-exports for the core functionality
pub use crate::agentic::tool_set::ToolSet;

// Re-exports for the core functionality
pub use crate::agentic::prompt_intake::PromptIntake;

// Re-exports for the core functionality
pub use crate::agentic::driver::TurnDriver;

// Re-exports for the core functionality
pub use crate::agentic::mcp::McpTools;

// Re-exports for the core functionality
pub use crate::agentic::adjudicate::AdjudicationFailure;

// Re-exports for the core functionality
pub use crate::agentic::observation::ShellObservation;

// Re-exports for the core functionality
pub use crate::agentic::artifact_hooks::ArtifactHooks;

// Re-exports for the core functionality
pub use crate::agentic::artifact_read::ArtifactRead;

// Re-exports for the core functionality
pub use crate::agentic::compress::Compress;

// Re-exports for the core functionality
pub use crate::agentic::content_spill::SpillStore;

// Re-exports for the core functionality
pub use crate::agentic::crew_attest::CrewAuthz;

// Re-exports for the core functionality
pub use crate::agentic::crew_tool::CrewRunner;

// Re-exports for the core functionality
pub use crate::agentic::cw_overflow::CwOverflow;

// Re-exports for the core functionality
pub use crate::agentic::display::Display;

// Re-exports for the core functionality
pub use crate::agentic::git_tool::GitTool;

// Re-exports for the core functionality
pub use crate::agentic::markdown::MarkdownRenderer;

// Re-exports for the core functionality
pub use crate::agentic::memory_fetch::MemoryFetch;

// Re-exports for the core functionality
pub use crate::agentic::note_sink::NoteSink;

// Re-exports for the core functionality
pub use crate::agentic::operating_mode::OperatingModeControl;

// Re-exports for the core functionality
pub use crate::agentic::plan_mode::PlanModeControl;

// Re-exports for the core functionality
pub use crate::agentic::recall::Recall;

// Re-exports for the core functionality
pub use crate::agentic::resume::ResumeContext;

// Re-exports for the core functionality
pub use crate::agentic::send_budget::SendBudget;

// Re-exports for the core functionality
pub use crate::agentic::steering::SteeringInbox;

// Re-exports for the core functionality
pub use crate::agentic::transcript::Transcript;

// Re-exports for the core functionality
pub use crate::agentic::trim::Trim;

// Re-exports for the core functionality
pub use crate::agentic::untrusted::Untrusted;

// Re-exports for the core functionality
pub use crate::agentic::warmup::Warmup;

// Re-exports for the core functionality
pub use crate::agentic::tool_recovery::ToolRecovery;

// Re-exports for the core functionality
pub use crate::agentic::plan_exec::PlanRun;

// Re-exports for the core functionality
pub use crate::agentic::observability::Observability;

// Re-exports for the core functionality
pub use crate::agentic::semantic::SemanticIndex;

// Re-exports for the core functionality
pub use crate::agentic::experiential::ExperienceStore;

// Re-exports for the core functionality
pub use crate::agentic::scheduled::StepLedger;

// Re-exports for the core functionality
pub use crate::agentic::scratchpad::ScratchpadStore;

// Re-exports for the core functionality
pub use crate::agentic::self_verify::SelfVerify;

// Re-exports for the core functionality
pub use crate::agentic::budget::Budget;

// Re-exports for the core functionality
pub use crate::agentic::prompt_read::PromptRead;

// Re-exports for the core functionality
pub use crate::agentic::claim_check::ClaimCheck;

// Re-exports for the core functionality
pub use crate::agentic::observed_report::ObservedReport;

// Re-exports for the core functionality
pub use crate::agentic::generation_policy::GenerationPolicy;

// Re-exports for the core functionality
pub use crate::agentic::output_stream::OutputStream;

// Re-exports for the core functionality
pub use crate::agentic::run_allowance::RunAllowance;

// Re-exports for the core functionality
pub use crate::agentic::digest_fold::DigestFold;

// Re-exports for the core functionality
pub use crate::agentic::smart_harness::SmartHarness;

// Re-exports for the core functionality
pub use crate::agentic::request_preview::RequestPreview;

// Re-exports for the core functionality
pub use crate::agentic::request_report::RequestReport;

// Re-exports for the core functionality
pub use crate::agentic::anthropic_wire::AnthropicWire;

// Re-exports for the core functionality
pub use crate::agentic::openai_sse::OpenaiSse;

// Re-exports for the core functionality
pub use crate::agentic::attempt_capture::AttemptCapture;

// Re-exports for the core functionality
pub use crate::agentic::selected_target::SelectedTarget;

// Re-exports for the core functionality
pub use crate::agentic::semantic_pins::SemanticPins;

// Re-exports for the core functionality
pub use crate::agentic::attribution::AttributionEpoch;

// Re-exports for the core functionality
pub use crate::agentic::context_recovery::ContextRecovery;

// Re-exports for the core functionality
pub use crate::agentic::diagnostics::Diagnostics;

// Re-exports for the core functionality
pub use crate::agentic::tools::Tools;

// Re-exports for the core functionality
pub use crate::agentic::tool_set::ToolSet;