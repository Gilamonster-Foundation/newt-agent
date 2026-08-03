//! The agentic loop — call model → execute tool calls → feed results back →
//! repeat — extracted verbatim from `newt-tui` in Step 9.7 so the same
//! battle-tested loop can serve both the TUI and the ACP worker (Step 9.8)
//! without being written twice.
//!
//! `ChatCtx` stays a **concrete** type: there is exactly one loop
//! implementation and no `InferenceBackend` trait (YAGNI per the roadmap —
//! revisit only when a second concrete backend exists). The only seam is
//! [`McpTools`], which breaks the `newt-core` ← `newt-mcp-client` dependency
//! cycle; see `mcp.rs`.

// pub(crate) since Step 18.5 (#247): the `Summarizing` memory provider
// delegates to this same pipeline instead of keeping a duplicate one.
// #727: read-only context-budget introspection (the `get_context_remaining`
// tool) — a pure renderer the agentic loop feeds per-turn budget state into.
mod budget;
// #867: path-claim verification for the cap-exit summary (the file-name
// sibling of the #717 phantom-tool-reach telemetry).
mod claim_check;
pub(crate) mod compress;
mod crew_attest;
mod crew_tool;
pub(crate) mod cw_overflow;
mod display;
mod generation_policy;
mod git_tool;
pub(crate) mod self_verify;
// Step 26.4 (#583): scratchpad structured-state — the `scratchpad` context feature.
pub(crate) mod scratchpad;
// Step 26.5 (#582): semantic repo-evidence retrieval (embedding RAG-for-code).
pub(crate) mod semantic;
// Step 26.6a (#585): experiential memory — the `experiential` context feature.
pub(crate) mod experiential;
// Step 26.6b (#586): scheduled per-step compiled view — the `scheduled` feature.
pub(crate) mod scheduled;
// Step 26.3 (#584): tool-output offloading — the `tool_offload` context feature.
/// Drive an overseer-authored plan through a `CrewRunner` (#628 P2 execute side).
pub(crate) mod plan_exec;
pub(crate) mod spill;
// W0 (#1511, epic #1506): typed dispatch-error classification + per-round
// tool-call parse signals — the structural inputs of the observability
// contract `newt solve` emits for the external evaluator.
pub(crate) mod observability;
/// Recover tool calls a weak model emitted in CONTENT instead of the native
/// `tool_calls` field (the #1 weak-model failure — see the module docs).
pub(crate) mod tool_recovery;
// Issue #308 — the cowork foundation: a non-blocking turn driver around
// `chat_complete` (driver), a renderer-agnostic transcript render (transcript),
// and the redaction-gated ShellObservation seam (observation). All additive;
// they wrap/precede `chat_complete` and never touch its internals.
mod driver;
// Step 25.1 (#568): Markdown → ANSI rendering of assistant output. Behind the
// default `markdown` feature; a passthrough shim takes its place under
// --no-default-features so the headless wyvern strip carries no markdown deps.
#[cfg(feature = "markdown")]
mod markdown;
#[cfg(not(feature = "markdown"))]
mod markdown {
    //! Passthrough shim used when the `markdown` feature is disabled (the
    //! headless wyvern strip). Keeps `render_markdown` / `RenderOpts` /
    //! `MarkdownStreamWriter` in the public API with zero markdown
    //! dependencies; output is the source verbatim — identical to color-off
    //! behavior in the full renderer.
    use std::io::{self, Write};

    #[derive(Debug, Clone, Copy)]
    pub struct RenderOpts {
        pub color: bool,
        pub cols: usize,
    }
    pub fn render_markdown(src: &str, _opts: RenderOpts) -> String {
        src.to_string()
    }

    /// Raw passthrough writer — bytes through unchanged, with a trailing newline
    /// at `finish` if the stream didn't end with one (matching the raw token
    /// path's closing `println!`).
    pub struct MarkdownStreamWriter<W: Write> {
        out: W,
        wrote: bool,
        ended_nl: bool,
    }
    impl<W: Write> MarkdownStreamWriter<W> {
        pub fn new(out: W, _opts: RenderOpts) -> Self {
            Self {
                out,
                wrote: false,
                ended_nl: true,
            }
        }
        pub fn push(&mut self, delta: &str) -> io::Result<()> {
            if let Some(&last) = delta.as_bytes().last() {
                self.wrote = true;
                self.ended_nl = last == b'\n';
            }
            self.out.write_all(delta.as_bytes())
        }
        pub fn finish(&mut self) -> io::Result<()> {
            if self.wrote && !self.ended_nl {
                self.out.write_all(b"\n")?;
            }
            self.out.flush()
        }
    }
}
mod artifact_hooks;
mod artifact_read;
mod mcp;
mod memory_fetch;
mod note_sink;
mod observation;
mod operating_mode;
mod permissions;
mod plan_mode;
// PR5: deterministic prompt-comprehension intake owns the turn disposition,
// bounded clarification manifest, and content-free model projection.
mod prompt_intake;
mod prompt_read;
mod recall;
// #1004: the `render_report` tool — present collected findings as a rendered
// Markdown document in the plain scroller (the missing "present" affordance a
// doer-oriented gather-and-report task otherwise lacks).
mod report;
// #714: the `resume_context` tool — a self-scoped read of THIS conversation's
// own pre-interrupt work (the affordance `recall` structurally cannot be).
mod resume;
mod send_budget;
// FR-3 (#998): the grant-independent absolute deny-list — a fixed exec veto
// (ssh / rm / systemctl restart …) no capability, mode, or persona unlocks,
// classified STRUCTURALLY by exec target so a coach's runbook TEXT is untouched.
mod deny;
// facade P4 (#780): hidden tool-call routing — promote the model's read-only
// shell reaches (`cat`/`ls`/`find` + read-only `git`) to a silent rewrite onto
// the governed built-ins, gate the rest. The route/gate split is pure DATA.
mod routing;
// #725: the `tool_search` discovery tool — find a real tool by intent instead
// of fabricating a foreign name (the structural complement to the #716 alias
// seam + #717 phantom telemetry).
mod tool_search;
mod tools;
mod transcript;
mod trim;
// Scoped FR-14 (#1042): wrap a remote MCP tool's result as explicitly
// untrusted data before it re-enters context — an injection-guard framing,
// not a filter.
mod untrusted;
mod warmup;

pub use artifact_hooks::{
    record_manual_compaction_checkpoint, record_memory_compaction_checkpoint,
    record_observed_head_transition, record_prompt_comprehension_manifest,
    record_retry_revert_file, record_turn_outcome,
};
pub use artifact_read::{
    artifact_read_tool_definition, ArtifactPage, ArtifactReadContext, ArtifactReadRecord,
    ArtifactSource, PromptArtifactSink, SessionArtifactStore, StoreArtifactStore,
};
pub use compress::{
    compress_user_initiated, compress_user_initiated_for_task, CompressCounters, CompressState,
    ManualCompressOutcome, SummarizeFn, SummarizeFuture, Summarizer, CONTINUATION_PREFIX,
    SUMMARY_END_MARKER, SUMMARY_PREFIX,
};
pub use crew_attest::{crew_authz, crew_step_up_policy, CrewAuthz, Presence};
pub use crew_tool::{compose_roster_tool_definition, crew_tool_definition, CrewRunner};
pub use cw_overflow::{parse_context_window_error, recover_context_window_400};
pub use display::{
    fmt_token_gauge, fmt_tokens_compact, gauge_level, newt_line, print_harness_notice,
    print_list_item, print_newt, set_spill_lines, GaugeLevel, NEWT_ORANGE_CT,
};
pub use driver::{
    HeadlessCodeSearch, TurnDriver, TurnDriverConfig, TurnDriverError, TurnOutcome, TurnStatus,
    VISIBLE_TRANSCRIPT_ROLES,
};
pub use experiential::{
    experience_block, ExperienceStore, SessionExperienceStore, EXPERIENCE_TOP_K,
};
pub use git_tool::{git_tool_definition, GitTool};
pub use markdown::{render_markdown, MarkdownStreamWriter, RenderOpts};
pub use mcp::{McpTools, NoMcp};
pub use observability::{
    classify_reqwest, error_class, round_parse_signal, BehaviorSignal, DispatchError, ErrorClass,
    ParseSignal, SolveObservation, ToolCallDialect,
};
pub use plan_exec::{run_plan, run_plan_with_reground, NoReground, PlanRun, Reground};
pub use prompt_intake::{
    AtomicAsk, DecisionLock, DecisionSource, DecisionStatus, DispositionLexicon,
    PromptComprehensionManifest, PromptDisposition, PromptIntake,
};
#[cfg(test)]
pub(crate) use prompt_read::response_repository_policy_tokens;
pub use prompt_read::{
    prompt_read_tool_definition, PromptReadContext, PromptSource, SessionPromptSource,
    SessionPromptStore, StorePromptSource,
};
pub use scheduled::{
    plan_block, plan_reseat_pointer, PlanSnapshot, SessionStepLedger, Step, StepLedger, StepStatus,
};
pub use scratchpad::{scratchpad_state_block, ScratchpadStore, SessionScratchpadStore};
pub use semantic::{
    chunk_source, code_evidence_block, code_search_tool_definition, cosine, format_index_status,
    format_search_hits, format_search_model, format_search_preview, format_search_rejects,
    gather_code_files, gather_with_manifest, index_files, plan_gather, render_code_evidence,
    retrieve_evidence, retrieve_evidence_steered, retrieve_ranked, retrieve_ranked_with_cap,
    CodeChunk, CodeSearch, Cut, CutClass, Embedder, EmbeddingsClient, EvidenceKind, GatherCaps,
    GatherManifest, IndexStatus, RankedHit, RejectReason, RetrievalResult, RetrievalSteer,
    SemanticIndex, SessionSemanticIndex,
};
pub use spill::{SessionSpillStore, SpillStore};

/// Align GFM table pipes in Markdown **source** (Step 25.5, #568) — plain text,
/// no ANSI. The headless **wyvern** tier keeps Markdown as source (no rendering),
/// so this tidies ragged pipe tables for transcripts other agents read. It is
/// **independent of the `markdown` feature** (wyvern builds `--no-default-features`)
/// and gated on the optional `markdown-table-formatter` feature: identity unless
/// enabled, so it never pulls comrak/wasm-bindgen into a default build.
#[cfg(feature = "markdown-table-formatter")]
pub fn tidy_markdown_tables(src: &str) -> String {
    markdown_table_formatter::format_tables(src)
}

/// Identity passthrough when the `markdown-table-formatter` feature is off.
#[cfg(not(feature = "markdown-table-formatter"))]
pub fn tidy_markdown_tables(src: &str) -> String {
    src.to_string()
}

#[cfg(test)]
mod tidy_tables_tests {
    #[cfg(not(feature = "markdown-table-formatter"))]
    #[test]
    fn identity_without_the_feature() {
        // The default build (and the wyvern strip without the opt-in) leaves the
        // source untouched.
        let ragged = "| a | bb |\n|---|---|\n| ccc | d |";
        assert_eq!(super::tidy_markdown_tables(ragged), ragged);
    }

    #[cfg(feature = "markdown-table-formatter")]
    #[test]
    fn aligns_pipes_with_the_feature() {
        let ragged = "| a | bb |\n| --- | --- |\n| ccc | d |\n";
        let tidy = super::tidy_markdown_tables(ragged);
        assert_ne!(tidy, ragged, "the table should be reformatted");
        assert!(tidy.contains("ccc"), "content preserved");
        // Every pipe-bearing row lines its pipes up at the same columns.
        let pipe_cols = |s: &str| {
            s.char_indices()
                .filter(|(_, c)| *c == '|')
                .map(|(i, _)| i)
                .collect::<Vec<_>>()
        };
        let rows: Vec<&str> = tidy.lines().filter(|l| l.contains('|')).collect();
        let first = pipe_cols(rows[0]);
        for r in &rows {
            assert_eq!(pipe_cols(r), first, "pipes aligned across rows: {r:?}");
        }
    }
}
pub use budget::get_context_remaining_tool_definition;
pub use memory_fetch::{
    memory_fetch_tool_definition, MemAddr, MemPayload, MemorySource, StoreMemorySource,
};
pub use note_sink::{save_note_tool_definition, NoteNudge, NoteSink};
pub use observation::{ShellObservation, SHELL_OBSERVATION_PREFIX};
pub use operating_mode::{select_operating_mode_tool_definition, OperatingModeControl};
pub use permissions::{
    append_denial, load_denials, widen_caveats, DenialKind, PermissionDecision, PermissionGate,
    PermissionRecord, PermissionRequest, PersistentDenial,
};
pub use plan_mode::PlanModeControl;
pub use recall::{recall_tool_definition, RecallSource, StoreRecallSource};
pub use resume::resume_context_tool_definition;
pub use send_budget::initial_context_input_budget;
pub use tools::{
    execute_tool, execute_tool_with_offload, execute_tool_with_offload_and_prompt,
    execute_tool_with_offload_and_prompt_and_artifacts, filter_advertised_tools,
    filter_tools_for_disposition, full_access_requested, ocap_disabled, persona_tool_allowed,
    plan_phase_clamp, set_max_output_tokens, set_output_cap_chars_per_token,
    set_output_head_tokens, tool_allowed, tool_definitions, venv_cmd_prefix, ExposureSettings,
};
pub use transcript::{
    transcript_lines, transcript_lines_styled, TranscriptLine, TranscriptRole, TranscriptStyle,
};
pub use trim::trim_for_summary;
pub use untrusted::wrap_untrusted;
pub use warmup::warmup_if_cold;

use crate::retry::{with_backoff_notify, RetryPolicy};
use compress::{
    compress, compression_trigger, CompressAction, CompressRequest, CompressTrigger,
    CompressionTriggerLimits,
};
use crossterm::{
    execute,
    style::{Color as CtColor, Print, ResetColor, SetForegroundColor},
};
use display::{
    emit_compression_notice, emit_overflow_notice, print_debug, print_retry_indicator, print_trace,
};
use send_budget::{
    calibrate_down, calibrate_up, emit_accepted, emit_context_window_400, initial_send_budget,
    num_ctx_input_ceiling, recovered_input_budget, sanitize_estimate_ratio,
};
use std::io::{self, Write as _};
use tools::{is_hallucination, merged_tool_definitions};
use trim::{
    estimate_request_tokens, estimate_tokens, estimate_value_tokens, merge_round_usage,
    ollama_usage, openai_usage, protected_prompt_head_len, PromptTracker,
};

/// Retry policy for TUI inference calls: more patient than the hosted-API
/// default because local DGX nodes can drop for 30–60 s under load.
/// Total resilience window: ~90 s (2+4+8+16+30+30 s between attempts).
/// All thresholds are overridable via the standard `NEWT_HTTP_*` env vars.
fn tui_retry_policy() -> RetryPolicy {
    RetryPolicy::for_local_inference()
}

/// Tightest whole-request ceiling that carries authoritative semantics for
/// this turn. A proven-good high-water mark by itself is deliberately not a
/// ceiling; configured token thresholds and believed/declared windows are.
/// The LIVE usable input budget (in estimated tokens) the tool-exposure
/// controller sizes the schema set against — the initial send budget when known
/// (derived from probed `max_ok_input` / `safe_context` / `num_ctx`), else the
/// declared `safe_context`. `None` means no live signal: the controller then
/// does NOT clip (no starvation without a measurement). Deliberately not a
/// function of the model name (#TEC): a bigger probed window widens exposure
/// automatically.
fn exposure_budget_tokens(send_budget: Option<usize>, safe_context: Option<u32>) -> Option<usize> {
    send_budget.or_else(|| safe_context.map(|s| s as usize))
}

fn authoritative_request_budget(
    send_budget: Option<usize>,
    send_budget_authoritative: bool,
    token_threshold: Option<usize>,
) -> Option<usize> {
    let send = send_budget_authoritative.then_some(send_budget).flatten();
    match (send, token_threshold.filter(|budget| *budget > 0)) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

fn capped_accepted_prompt_tokens(
    accepted_prompt_tokens: u32,
    declared_ceiling: Option<usize>,
) -> usize {
    (accepted_prompt_tokens as usize).min(declared_ceiling.unwrap_or(usize::MAX))
}

/// Refuse before inference when the compression-immune system/card/exact-user
/// head, newest live user presentation, and advertised schemas cannot fit an
/// authoritative model budget. The live presentation intentionally remains at
/// the transcript tail so normal multi-turn ordering is preserved; counting
/// only the protected recovery copy would under-price every prompt by one full
/// copy and permit an over-window dispatch. Exact prompt text is never
/// truncated to manufacture a dispatchable request.
fn preflight_irreducible_request(
    messages: &[serde_json::Value],
    tools: Option<&serde_json::Value>,
    authoritative_budget: Option<usize>,
    calibration: f32,
    estimation: crate::tokens::TokenEstimation,
    model: &str,
) -> anyhow::Result<()> {
    let Some(budget) = authoritative_budget else {
        return Ok(());
    };
    let head = protected_prompt_head_len(messages, prompt_read::ACTIVE_PROMPT_PREFIX);
    let newest_live_user = messages[head..]
        .iter()
        .rev()
        .find(|message| message["role"].as_str() == Some("user"));
    let estimated = estimate_request_tokens(&messages[..head], tools, estimation)
        + newest_live_user
            .map(|message| estimate_value_tokens(message, estimation))
            .unwrap_or(0);
    let required = calibrate_up(estimated, calibration);
    if required > budget {
        anyhow::bail!(
            "the exact active prompt, live user presentation, and required request scaffolding \
             need ~{required} input \
             tokens (including advertised tool schemas), which cannot fit model `{model}`'s \
             authoritative {budget}-token input budget; refusing before inference dispatch — \
             the operator prompt was not truncated"
        );
    }
    Ok(())
}

/// Refuse any Chat-style dispatch when its complete dynamic message list plus
/// the schemas currently advertised on that request no longer fit an
/// authoritative budget. Count trimming alone is not a token bound: one fresh
/// tool or prompt-read result can be larger than the entire window.
fn preflight_full_message_request(
    messages: &[serde_json::Value],
    tools: Option<&serde_json::Value>,
    authoritative_budget: Option<usize>,
    calibration: f32,
    estimation: crate::tokens::TokenEstimation,
    model: &str,
) -> anyhow::Result<()> {
    let Some(budget) = authoritative_budget else {
        return Ok(());
    };
    let required = calibrate_up(
        estimate_request_tokens(messages, tools, estimation),
        calibration,
    );
    if required > budget {
        anyhow::bail!(
            "the complete inference request needs ~{required} input tokens, which cannot fit \
             model `{model}`'s authoritative {budget}-token input budget; refusing before \
             inference dispatch — the exact operator prompt and tool results were not truncated"
        );
    }
    Ok(())
}

fn preflight_responses_request(
    instructions: Option<&str>,
    input: &[serde_json::Value],
    tools: Option<&[serde_json::Value]>,
    authoritative_budget: Option<usize>,
    calibration: f32,
    estimation: crate::tokens::TokenEstimation,
    model: &str,
) -> anyhow::Result<()> {
    let Some(budget) = authoritative_budget else {
        return Ok(());
    };
    let instructions_tokens = instructions
        .map(|text| {
            estimate_value_tokens(
                &serde_json::json!({"role": "system", "content": text}),
                estimation,
            )
        })
        .unwrap_or(0);
    let input_tokens = estimate_tokens(input, estimation);
    let tool_tokens = tools
        .map(|tools| estimate_value_tokens(&serde_json::Value::Array(tools.to_vec()), estimation))
        .unwrap_or(0);
    let required = calibrate_up(
        instructions_tokens + input_tokens + tool_tokens,
        calibration,
    );
    if required > budget {
        anyhow::bail!(
            "the Responses request needs ~{required} input tokens, which cannot fit model \
             `{model}`'s authoritative {budget}-token input budget; refusing before inference \
             dispatch — the exact operator prompt and function outputs were not truncated"
        );
    }
    Ok(())
}

/// Hook recovering a hard context-window 400:
/// `(error, model, today) → parsed full context window`. The loop composes the
/// returned window with its percentage ceiling and generation output reserve;
/// callbacks must not pre-discount it into an input cap. See
/// [`ChatCtx::recover_cw_400`].
pub type RecoverCw400 = fn(&anyhow::Error, &str, &str) -> Option<u32>;

/// One per-round capability observation, reported through
/// [`ChatCtx::on_round_usage`] at the moment it is observed (Phase 20,
/// `docs/design/model-self-tuning.md` §2.2 — the `recover_cw_400` pattern,
/// generalized to the success direction). Evidence must not wait for a turn
/// epilogue an error can skip: the motivating failure discarded a backend-
/// accepted 8,734-token prompt because the turn later ended in `Err`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundObservation {
    /// Backend evaluated `prompt_tokens` and the round produced a usable
    /// response (tool calls or non-empty content). `estimated_tokens` is the
    /// loop's chars/4 estimate of the same request, for calibration.
    /// Quality-gated AND truncation-gated at the emission sites: never
    /// emitted when the prompt was within 5% of the request's `num_ctx`
    /// (Ollama may have silently dropped the head of the prompt).
    Accepted {
        prompt_tokens: u32,
        estimated_tokens: usize,
    },
    /// Persistent empty responses at `prompt_tokens` after retries (the
    /// 85%-of-safe-context silent-overflow exit).
    SuspectedOverflow { prompt_tokens: u32 },
    /// A hard HTTP error reported the endpoint's full context window. The TUI
    /// applies this through the same in-memory capability entry as subsequent
    /// accepted-round evidence, avoiding stale whole-cache overwrites.
    ContextWindow400 { context_window: u32 },
    /// Response carried only non-content fields (thinking/reasoning) with
    /// empty content.
    ThinkingOnly,
}

/// Source stream for one incremental shell-output chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolOutputStream {
    /// Bytes captured from the tool's standard output stream.
    Stdout,
    /// Bytes captured from the tool's standard error stream.
    Stderr,
}

/// Renderer-neutral, turn-scoped consumer for live tool output.
///
/// The interactive TUI supplies this only when both stdio handles are TTYs.
/// Headless callers pass `None` and retain completion-only output exactly.
/// Newt delivers accepted chunks serially on a bounded presentation worker;
/// slow rendering cannot backpressure a child process or its timeout. Live-only
/// chunks may be dropped when that queue is full. The independently captured
/// result envelope remains authoritative and complete.
pub trait LiveToolOutput: Send + Sync {
    /// Begin a new active-tool frame. No terminal output is required until the
    /// first chunk arrives. This runs on the bounded presentation worker, not
    /// on the tool-execution task. An implementation must ignore a generation
    /// that [`LiveToolOutput::abandon`] has already invalidated.
    fn start(&self, generation: u64);

    /// Consume one raw byte chunk. Display sanitization belongs to the sink;
    /// the authoritative result envelope is captured independently. A
    /// generation can be abandoned after a bounded completion wait, so a
    /// post-finish, abandoned, or stale-generation write must be a no-op.
    fn write(&self, generation: u64, stream: ToolOutputStream, chunk: &[u8]);

    /// Erase/close the active frame before the canonical result is printed.
    /// This is ordered after every accepted write on normal completion. Panics
    /// are contained by the dispatcher; implementations should still leave the
    /// terminal in a canonical state whenever possible.
    fn finish(&self, generation: u64);

    /// Invalidate a generation without producing terminal output.
    ///
    /// Cancellation and bounded teardown call this synchronously before the
    /// canonical result may be rendered. It must be fast, idempotent, and must
    /// make queued callbacks for `generation` no-ops. A later [`Self::start`]
    /// must discard abandoned frame bookkeeping without trying to erase it
    /// from the terminal's then-current cursor position.
    fn abandon(&self, generation: u64);
}

/// Everything one agentic turn needs, resolved once by the caller (the TUI
/// resolves config + capability cache + caveats per turn and threads them in
/// here, so the loop itself never re-reads config from disk).
pub struct ChatCtx<'a> {
    pub url: &'a str,
    pub model: &'a str,
    /// Wire protocol of the active backend (Ollama vs OpenAI-compatible).
    pub kind: crate::BackendKind,
    /// Bearer token for authenticated OpenAI-compatible endpoints.
    pub api_key: Option<&'a str>,
    /// Full message list already assembled by `MemoryManager::build_messages`.
    pub messages: &'a [crate::MemMessage],
    pub task: &'a str,
    pub workspace: &'a str,
    pub color: bool,
    /// Render assistant Markdown as ANSI in the live stream (Step 25.4, #568).
    /// Resolved by the caller as `[tui].markdown` (∧ `/markdown` override) ∧
    /// color. The loop only renders when this is true.
    pub markdown: bool,
    /// Offload oversized tool results to the session spill store (Step 26.3,
    /// #584). The resolved `tool_offload` composable feature (Step 26.1); false
    /// for headless/eval callers (bit-for-bit unchanged when off).
    pub tool_offload: bool,
    /// Session spill store for `tool_offload` (Step 26.3). `None` = no offload
    /// (and `spill:` re-reads resolve to a labelled absence). Shared `&dyn`
    /// (interior mutability) so it serves both the write path and `memory_fetch`.
    pub spill_store: Option<&'a dyn crate::agentic::spill::SpillStore>,
    /// Session compaction store (#661 group B): the compressor stores each
    /// evicted (redacted) middle span here and names a `compaction:<id>` handle
    /// in the marker, so the model can losslessly recover a detail the summary
    /// dropped. SEPARATE store from `spill_store` (own id space). `None` =
    /// lossy-only compaction (headless / progressive disclosure off).
    pub compaction_store: Option<&'a dyn crate::agentic::spill::SpillStore>,
    /// Inject the `<state>` scratchpad block + advertise the state tools (Step
    /// 26.4, #583). The resolved `scratchpad` feature; false for headless/eval.
    pub scratchpad: bool,
    /// Session scratchpad store (Step 26.4). `None` = state tools not advertised
    /// and no `<state>` injected. Shared `&dyn` (interior mutability).
    pub scratchpad_store: Option<&'a dyn crate::agentic::scratchpad::ScratchpadStore>,
    /// Semantic searcher for the `code_search` tool (Step 26.5.5). `None` = the
    /// tool is not advertised (semantic off / no index). Bundles embedder+index.
    pub code_search: Option<crate::agentic::semantic::CodeSearch<'a>>,
    /// #1285: the retained `where_is` symbol index for the exact typed-verdict
    /// lookup tool. `None` = no index built (the tool degrades honestly). Built
    /// from the honest gather + language packs — model-free, so it can ride
    /// every session.
    pub where_is: Option<&'a crate::where_is::WhereIsIndex>,
    /// #1387 Code Navigator tool context (usage/graph/project).
    pub nav: Option<crate::navigator::NavToolCtx<'a>>,
    /// Tool-exposure controller policy (Pass 1). `Default` is
    /// [`crate::agentic::tools::ExposureSettings::default`] =
    /// `ExposureProfile::Full`, i.e. the identity controller (advertise the full
    /// authorized catalog). Resolved by the TUI from `[tool_exposure]`; headless
    /// / eval callers take the default. Budget-driven selection uses the LIVE
    /// send budget (probed `safe_context`), never the model name.
    pub exposure: crate::agentic::tools::ExposureSettings,
    /// Experiential store for the record/recall tools (Step 26.6a). `None` = the
    /// tools are not advertised (experiential off). Shared `&dyn` (interior mut).
    pub experience_store: Option<&'a dyn crate::agentic::experiential::ExperienceStore>,
    /// Plan ledger for the update_plan tool (Step 26.6b). `None` = the tool is
    /// not advertised (scheduled off). Shared `&dyn` (interior mut).
    pub step_ledger: Option<&'a dyn crate::agentic::scheduled::StepLedger>,
    pub caveats: &'a crate::caveats::Caveats,
    /// FR-1 part 2 (#997): the active persona's tool allow-list (its `tools:`
    /// front-matter), or `None` when no persona is active / the persona sets no
    /// list. When `Some`, the loop advertises ONLY these tools (plus the
    /// always-on infra tools the loop can't run without), and the executor
    /// REFUSES any tool outside the list — a name-scoped complement to the
    /// axis-scoped `caveats` (which part 1, #1002, already meets in). Headless
    /// / driver / eval callers pass `None` (no persona surface).
    pub persona_tools: Option<&'a [String]>,
    /// The psyche **cognition** dial for this turn — how much reasoning effort to
    /// request. `Some(level)` emits OpenAI **Responses** `reasoning.effort` or,
    /// for an explicitly capable Chat Completions endpoint, resolves a local
    /// generation policy. `None` omits cognition-derived fields. The TUI
    /// resolves this from the active persona's `cognition:` front-matter
    /// alongside `persona_tools`; headless / eval callers pass `None`.
    pub cognition: Option<crate::role_profile::Cognition>,
    /// Chat Completions extensions explicitly accepted by this endpoint.
    /// Unknown endpoints use the all-unset default and retain the historical
    /// request body even when cognition is active.
    pub chat_completions_capability: crate::model_card::ChatCompletionsCapability,
    /// Whether assistant reasoning may be replayed to the active backend.
    /// Unknown endpoints default to `Never`; local reasoning backends opt in via
    /// their explicit capability profile.
    pub reasoning_replay_scope: crate::model_card::ReasoningReplayScope,
    /// Maximum tool-call rounds before forcing a final tools-disabled
    /// completion (from `[tui].max_tool_rounds`, default 40).
    pub max_tool_rounds: usize,
    /// Additional progress-aware rounds available after `max_tool_rounds` when
    /// the active workflow still has incomplete work and the recent rounds show
    /// repair evidence or concrete progress. `0` makes the normal cap hard.
    pub workflow_grace_rounds: usize,
    /// Max narrate-then-stop rescue nudges per turn (from
    /// `[tui].narration_nudge_cap`, default 1; `[[model_tuning]]` can override
    /// per model). Once spent, the next no-tool narration is accepted as the
    /// turn's final answer. The second and later nudges escalate: they name
    /// the active plan step and demand a bare tool call (lever L3,
    /// docs/design/next-loop-levers.md).
    pub narration_nudge_cap: usize,
    /// Master switch for ACTION-pressure nudges (#1162: the operator's live
    /// dial — `/nudge off` / `NEWT_NUDGE=off`). `false` disables the narration
    /// rescue, workflow repair steering, and pending-plan pushes for the
    /// session; factual-correction nudges are unaffected. Default `true`.
    pub action_nudges: bool,
    /// Validated prompt-comprehension disposition for this turn. The loop
    /// advertises only the corresponding catalog and lets only `Act` retain
    /// execution-pressure nudges; the dispatcher remains the final authority
    /// boundary for fabricated tool names.
    pub prompt_disposition: PromptDisposition,
    /// The resolved prompt-comprehension manifest for this turn, when the
    /// caller owns an intake boundary. It is borrowed only to place its
    /// content-free model card inside the protected active-prompt card;
    /// headless compatibility callers can leave it `None`.
    pub prompt_intake: Option<&'a PromptIntake>,
    /// Legacy line limit for pre-execution previews. Completed tool results use
    /// the `[tui].spill_lines` height published once per turn.
    pub tool_output_lines: usize,
    /// Enable per-round diagnostic output. Set via `NEWT_DEBUG=1` or the
    /// `[tui] debug = true` config key.
    pub debug: bool,
    /// Enable deeper backend compatibility traces. Set via `NEWT_TRACE=1` or
    /// the `[tui] trace = true` config key.
    pub trace: bool,
    /// Ollama `options.num_ctx` — caps KV-cache allocation to prevent VRAM
    /// exhaustion on large models. `None` → model default (often 131k).
    /// Also feeds the pre-send budget as a hard input ceiling for this turn's
    /// requests (issue #282): Ollama silently evaluates only the window's
    /// tail, so anything newt sends must already fit inside the `num_ctx` it
    /// sends it with. OpenAI requests do not serialize `num_ctx`, but local
    /// compatible endpoints still use it as an authoritative declared window
    /// for preflight, compaction, reporting, and 400 recovery.
    pub num_ctx: Option<u32>,
    /// TCP connect timeout. Short (5 s default) so a down endpoint fails fast
    /// rather than blocking the full `inference_timeout_secs`.
    pub connect_timeout_secs: u64,
    /// Total inference timeout. Must be long enough for the model to generate
    /// a complete response (120 s default).
    pub inference_timeout_secs: u64,
    /// Message list size at which the agent trims the middle of the in-flight
    /// conversation to prevent context overflow mid-turn.
    pub mid_loop_trim_threshold: usize,
    /// Policy deciding whether the message-count guard alone may trigger a
    /// compaction while an authoritative input ceiling is known. Resolved from
    /// `[context].compaction_trigger_policy` by the interactive caller;
    /// headless callers use the safe `headroom_aware` default.
    pub compaction_trigger_policy: crate::CompactionTriggerPolicy,
    /// Estimated-token threshold that also triggers a mid-loop trim, regardless
    /// of message count. Guards against a single huge tool result blowing past
    /// the context window in one round (from `[tui].mid_loop_trim_tokens`).
    /// `None` disables token-based trimming. See issue #223.
    pub mid_loop_trim_tokens: Option<usize>,
    /// Highest input-token count this model has accepted without a 400, from
    /// `model-capabilities.json`. Used as the pre-send budget gate: requests
    /// estimated to exceed it are trimmed *before* dispatch. `None` falls back
    /// to `safe_context`. See issue #223.
    pub max_ok_input: Option<u32>,
    /// Shell command run after every successful file write to give the model
    /// immediate ground-truth feedback (e.g. "cargo check -q --workspace").
    /// `None` disables auto-checking. Set per-workspace in `.newt/config.toml`.
    pub build_check_cmd: Option<String>,
    /// Empirically derived safe context size for this model (input tokens).
    /// Used to detect likely overflow when the model returns an empty response.
    /// Sourced from `model-capabilities.json` via `ensure_context_window`.
    /// `None` disables overflow detection.
    pub safe_context: Option<u32>,
    /// Hook invoked when a dispatch fails, to recover a hard context-window
    /// 400: `(error, model, today) → parsed full context window`. The loop
    /// derives the effective input cap after reserving its configured maximum
    /// output; callbacks must not return a pre-discounted input budget. The
    /// loop emits [`RoundObservation::ContextWindow400`] so the TUI's existing
    /// observation owner can persist the discovery. `None` disables numbered
    /// recovery.
    pub recover_cw_400: Option<RecoverCw400>,
    /// Model-writable note store behind the `save_note` tool (Step 19.3,
    /// #248). `None` ⇒ the tool is not advertised and the loop never writes
    /// memory (eval / headless callers unaffected). The TUI passes a sink
    /// over its session `MemoryManager`, so `save_note` and `/remember`
    /// share one store, one security scan, one char budget.
    pub note_sink: Option<&'a mut dyn NoteSink>,
    /// Turn-counted memory-nudge state ([`NoteNudge`]), owned by the caller
    /// across user turns and lent to the loop per call. Consulted only when
    /// `note_sink` is present; `None` disables the nudge.
    pub note_nudge: Option<&'a mut NoteNudge>,
    /// Read-only search over PAST conversations behind the `recall` tool
    /// (Step 17.5, #246). `None` ⇒ the tool is not advertised and the loop
    /// never searches (eval / headless callers unaffected). The TUI passes
    /// a [`StoreRecallSource`] over its session `ConversationStore` —
    /// workspace-fenced, current conversation excluded.
    pub recall_source: Option<&'a dyn RecallSource>,
    /// Read-only pull of an ADDRESSED memory item behind the `memory_fetch`
    /// tool (progressive-disclosure memory, Workstream A MVP, #319). `None` ⇒
    /// the tool is not advertised and the loop never fetches — eval / headless
    /// / ACP callers (which pass `memory_source: None`) are unaffected,
    /// bit-for-bit. The TUI passes a [`StoreMemorySource`] over its session
    /// `NoteStore` + `ConversationStore` (workspace-fenced), so `note:` and
    /// `turn:` addresses resolve against the same surfaces `/remember` and
    /// `recall` use. Gated exactly like `recall_source`.
    pub memory_source: Option<&'a dyn MemorySource>,
    /// Compression summarizer (Step 18.4, #247): given the redacted summary
    /// request, returns the summary text — typically one tools-disabled
    /// completion against the same backend (the TUI wires this, mirroring
    /// the `Summarizing` provider's `with_summarizer` injection). `None`
    /// (eval / headless) ⇒ the compression pipeline degrades to the static
    /// "Summary generation was unavailable" marker instead of an LLM summary.
    pub summarizer: Option<&'a SummarizeFn>,
    /// Session-scoped compression anti-thrash state ([`CompressState`]),
    /// owned by the caller across user turns and lent per call (the
    /// `note_nudge` pattern). `None` ⇒ a fresh per-turn state.
    pub compress_state: Option<&'a mut CompressState>,
    /// Per-turn tool-event recorder (Step 17.6, #246): when present, the
    /// loop pushes one [`crate::ToolEvent`] per executed tool call — name,
    /// privacy-preserving args digest (never raw args), outcome, duration
    /// claim — at the same site that renders tool activity live. The TUI
    /// lends a fresh `Vec` per turn (the `note_nudge` pattern) and persists
    /// it into the turn's `events` column. `None` (eval / headless) ⇒
    /// nothing is recorded.
    pub tool_events: Option<&'a mut Vec<crate::ToolEvent>>,
    /// Per-turn phantom-reach recorder (#717): when present, the loop pushes one
    /// [`crate::PhantomReach`] for each phantom tool/capability reach (alias
    /// resolve, hallucination, or a real-tool empty-by-design miss). Lent fresh
    /// per turn like `tool_events`; persisted into the turn's `phantom_reaches`.
    /// `None` (eval / headless) ⇒ nothing recorded.
    pub phantom_reaches: Option<&'a mut Vec<crate::PhantomReach>>,
    /// Out-param: why the loop ended the turn ([`crate::TurnEndReason`]) —
    /// narration-acceptance forensics, round cap, empty reply. Lent fresh per
    /// turn like `tool_events`; the TUI folds it into `TurnMetrics` (footer +
    /// usage.jsonl). `None` (eval / headless) ⇒ nothing reported. The
    /// Responses-API loop does not report it.
    pub end_reason: Option<&'a mut Option<crate::TurnEndReason>>,
    /// Out-param: per-turn observability for the solve contract (W0 #1511) —
    /// the backend-reported served `model` plus per-round tool-call parse
    /// signals ([`observability::ParseSignal`]). Lent fresh per turn like
    /// `tool_events`; the headless driver folds it into the `TurnOutcome` and
    /// `newt solve` serializes it. `None` (TUI / eval) ⇒ nothing recorded.
    /// The Responses-API loop does not report it (same as `end_reason`).
    pub solve_obs: Option<&'a mut observability::SolveObservation>,
    /// Prompted ocap grants (issue #263): when present, a capability denial
    /// inside `execute_tool` consults the human — allow once / session allow
    /// / deny — instead of failing outright; the loop blocks like a long
    /// tool call while the prompt is pending. `None` (the default — every
    /// headless caller: ACP worker, `newt-eval`) keeps each denial exactly
    /// as before, so nothing non-interactive can ever hang on a prompt.
    pub permission_gate: Option<&'a mut dyn PermissionGate>,
    /// Per-round capability observation hook (Phase 20,
    /// `docs/design/model-self-tuning.md` §2.2): the loop reports each
    /// round's evidence — accepted prompt sizes (with the matching chars/4
    /// estimate for calibration), persistent-empty suspected overflows, the
    /// thinking-only response quirk — at the moment of observation, so the
    /// caller can persist it even when the turn later bails or errors (the
    /// motivating failure discarded an accepted 8,734-token prompt because
    /// the only write-back lived in the TUI's `Ok`-arm epilogue). `None`
    /// (ACP worker, eval, cowork driver) preserves today's behavior exactly
    /// (spec §5).
    pub on_round_usage: Option<&'a mut dyn FnMut(RoundObservation)>,
    /// Learned observed/estimated prompt-token ratio for this model
    /// (Phase 20 §2.3), applied wherever chars/4 estimates meet
    /// backend-reported token budgets — compression triggers, targets, and
    /// the tool-schema overhead. `None` or out-of-clamp values degrade to
    /// 1.0 (no calibration; pre-Phase-20 behavior).
    pub estimate_ratio: Option<f32>,
    /// `[context.estimation]` token-estimation heuristic (chars-per-token),
    /// extracted from config so the loop never re-reads it — drives every
    /// chars→token estimate and the budget→chars summary-cap conversion.
    pub estimation: crate::tokens::TokenEstimation,
    /// `[context] summary_input_cap_floor_chars` — floor for the summarizer
    /// input cap so a tight budget never starves the summarizer of material.
    pub summary_input_cap_floor_chars: usize,
    /// `[context] input_ceiling_pct` — percentage-based input limit inside the
    /// declared context window. The effective ceiling is the tighter of this
    /// limit and the space left after the generation policy's maximum output.
    /// Historically hardcoded at 80 (20% headroom). Applied by
    /// `num_ctx_input_ceiling`.
    pub input_ceiling_pct: u32,
    /// `[context] low_budget_pct` — remaining-budget percent below which the
    /// loop treats the turn as "low budget" and nudges toward wrapping up.
    /// Historically hardcoded at 15.
    pub low_budget_pct: usize,
    /// #307 named-permission-preset exec FLOOR. When a `/posture` preset is active
    /// its exec clamp is threaded here so the `--disable-ocap` / `--yolo`
    /// bypass in `execute_tool` cannot raise exec authority above the preset:
    /// an out-of-floor command falls through to the confined shell and is
    /// denied. `None` (no active posture, and every headless caller) leaves the
    /// bypass bit-for-bit. The floor is also already `meet`-ed into `caveats`,
    /// so the confined-shell and gate paths enforce it too; this field is the
    /// one extra place the otherwise caveats-blind bypass must consult.
    pub exec_floor: Option<&'a crate::caveats::Scope<String>>,
    /// `retry` technique (R2 action arm): a turn-scoped copy-on-first-write ledger
    /// ([`crate::verify_gate::WriteLedger`]) the file-write tools record into before
    /// each `write_file` / `edit_file`, so the caller can revert exactly the files
    /// newt wrote this turn (and only those) after the gate runs. Shared via
    /// [`RefCell`](std::cell::RefCell) so the loop records while the caller reads.
    /// `None` (every headless caller, and any profile without `retry`) ⇒ nothing is
    /// recorded and no file is ever reverted — bit-for-bit today's behavior.
    pub write_ledger: Option<&'a std::cell::RefCell<crate::verify_gate::WriteLedger>>,
    /// User-interrupt flag (Esc / Ctrl-C during a turn). When set mid-turn the
    /// loop abandons at its next checkpoint — the round-loop top, and the two
    /// model awaits (the non-streaming probe and the token stream) — and
    /// returns early. `None` (every headless / eval caller) ⇒ no interrupt
    /// path, bit-for-bit today's behavior. The caller owns the `AtomicBool`,
    /// trips it from a keyboard watcher, and inspects it after the call to tell
    /// an interrupted turn from a genuinely empty reply.
    pub cancel: Option<&'a std::sync::atomic::AtomicBool>,
    /// Optional TTY-owned live tool-output sink (#1235). Owned in an `Arc`
    /// because shell stdout/stderr drains may publish from worker threads.
    /// `None` is the mandatory headless/non-TTY path.
    pub live_tool_output: Option<std::sync::Arc<dyn LiveToolOutput>>,
    /// The injected embedded-git capability (PR4, #461). `Some` ⇒ the `git`
    /// tool is advertised and dispatches through it (`LocalGitTool` in
    /// `newt-git`, injected by the binary). `None` (every headless / eval
    /// caller, and any session not in a git repo) ⇒ the tool is never
    /// advertised — bit-for-bit today's behavior. The trait-injection seam, not
    /// a direct dep, because `newt-git` depends on `newt-core` (circular).
    pub git_tool: Option<&'a dyn GitTool>,
    /// The injected crew/team orchestration capability (#479). `Some` ⇒ the
    /// `compose_roster` + `crew` tools are advertised and dispatch through it
    /// (`LocalCrewRunner` in `newt-cli`, injected by the `/team` toggle). `None`
    /// ⇒ never advertised. Trait-injection seam like `git_tool` (newt-scheduler
    /// depends on newt-core, so the dep can't be direct).
    pub crew_runner: Option<&'a dyn CrewRunner>,
    /// Session-local working-style selector behind `/mode auto`. `Some` only
    /// when the human configured Auto mode, so every other session omits the
    /// model-facing selector entirely. A selection affects a future turn and
    /// cannot change this turn's disposition or caveats.
    pub operating_mode_control: Option<&'a dyn OperatingModeControl>,
    /// Session-local state behind `enter_plan_mode` / `exit_plan_mode`.
    ///
    /// The dispatcher checks this collaborator before every tool call, so
    /// entering Plan immediately clamps subsequent calls in the same model
    /// round. `None` means the model-entered Plan phase is unavailable.
    pub plan_mode_control: Option<&'a dyn PlanModeControl>,
}

/// retry technique (R2 action arm): before a `write_file`/`edit_file` is dispatched,
/// capture the target's pre-write bytes into the turn ledger. Recorded at the loop's
/// dispatch site (not inside `execute_tool`) so the seam stays narrow and only the
/// two file-writing tools are ever tracked. [`WriteLedger::note_before_write`] is
/// idempotent on the turn's first write of a path, so the *pre-turn* state is what
/// is preserved. A no-op when no ledger is lent — every headless caller and any
/// profile without `retry` — so behavior is bit-for-bit unchanged there.
fn ledger_note_write(
    write_ledger: Option<&std::cell::RefCell<crate::verify_gate::WriteLedger>>,
    name: &str,
    args: &serde_json::Value,
    workspace: &str,
) {
    let Some(led) = write_ledger else {
        return;
    };
    if name != "write_file" && name != "edit_file" {
        return;
    }
    if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
        // Key on the lexically-normalized path so a raw model path like
        // `examples/../foo.py` matches the gate's filesystem-normalized `foo.py`
        // and the revert lookup actually hits — otherwise a non-normalized path
        // would silently evade revert (the fabrication persists, the gate is gamed).
        let abs = lexical_normalize(&std::path::Path::new(workspace).join(p));
        led.borrow_mut().note_before_write(abs);
    }
}

/// Lexically normalize a path — collapse `.`, resolve `..`, drop empty components —
/// **without** touching the filesystem, so a ledger key built from a raw
/// model-supplied path matches the gate's `read_dir`-normalized path. Purely
/// lexical: it deliberately does not resolve symlinks (the gate never follows them
/// and revert is workspace-boundary-guarded), so it cannot itself escape the tree.
fn lexical_normalize(path: &std::path::Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut out = std::path::PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                // Only pop a real path segment; never climb above a root/prefix.
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                } else {
                    out.push("..");
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod retry_ledger_tests {
    use super::*;

    #[test]
    fn lexical_normalize_collapses_dot_and_parent() {
        assert_eq!(
            lexical_normalize(std::path::Path::new("/ws/examples/../foo.py")),
            std::path::PathBuf::from("/ws/foo.py")
        );
        assert_eq!(
            lexical_normalize(std::path::Path::new("/ws/./a//b/foo.py")),
            std::path::PathBuf::from("/ws/a/b/foo.py")
        );
        // A leading `..` with no segment to pop is preserved, not climbed past root.
        assert_eq!(
            lexical_normalize(std::path::Path::new("../x.py")),
            std::path::PathBuf::from("../x.py")
        );
    }

    #[test]
    fn ledger_note_write_keys_on_the_normalized_path() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("foo.py"), "real\n").unwrap();
        let led = std::cell::RefCell::new(crate::verify_gate::WriteLedger::new());
        // a raw, non-normalized model path
        let args = serde_json::json!({ "path": "examples/../foo.py" });
        ledger_note_write(
            Some(&led),
            "write_file",
            &args,
            tmp.path().to_str().unwrap(),
        );
        // the key normalized to <ws>/foo.py — the same path the gate would produce —
        // so revert finds and restores it (returns true).
        assert!(
            led.borrow().revert(&tmp.path().join("foo.py")).unwrap(),
            "the normalized key matches the gate's path"
        );
        // a read-only tool is never recorded
        ledger_note_write(
            Some(&led),
            "read_file",
            &serde_json::json!({ "path": "foo.py" }),
            tmp.path().to_str().unwrap(),
        );
        assert_eq!(led.borrow().len(), 1, "only write tools are tracked");
    }
}

/// True when a backend 400 says the model can't accept a `tools` field.
/// Ollama phrases it `"<model> does not support tools"`; OpenAI-compatible
/// servers vary, so we also accept the looser `"not support tools"`. Used to
/// drop tools and retry once, then keep them off for the turn (deepseek-r1).
fn is_tools_unsupported_error(e: &anyhow::Error) -> bool {
    let s = e.to_string().to_lowercase();
    s.contains("does not support tools") || s.contains("not support tools")
}

/// True when Ollama accepted the `tools` field but its internal XML tool-call
/// parser rejected the model's generated `<function>/<parameter>` markup before
/// returning an assistant message. This is a malformed generation, not proof
/// that the model lacks tool support, so retry with tools still advertised and
/// a corrective nudge instead of disabling tools for the whole turn.
fn is_ollama_tool_xml_error(e: &anyhow::Error) -> bool {
    let s = e.to_string().to_lowercase();
    s.contains("ollama") && s.contains("xml syntax error")
}

fn ollama_tool_xml_retry_nudge() -> &'static str {
    "The previous assistant turn failed inside Ollama's XML tool-call parser. \
     Keep using tools, but emit exactly one valid native tool call with well-formed \
     arguments now. Do not answer in prose or wrap the call in explanatory XML."
}

/// Main agentic loop: call model → execute tool calls → feed results back → repeat.
/// Returns `(reply_text, was_streamed, token_usage, hallucination_count)`.
/// When `was_streamed` is true the text was already printed token-by-token.
///
/// Token-usage semantics (Step 18.1): `input_tokens` is the **largest single
/// prompt** the backend evaluated across the turn's rounds — the truthful
/// "how full did the context get" figure that feeds the capability ratchet —
/// NOT the per-round sum, which double-counts history (every round's prompt
/// re-includes all prior rounds; the B3 baseline measured 5.4× inflation).
/// `output_tokens` is the sum across rounds (each completion is new).
/// `true` once the caller's interrupt flag is set (Esc / Ctrl-C). Cheap relaxed
/// load — the flag is a one-way latch, so no ordering guarantees are needed.
fn is_cancelled(cancel: Option<&std::sync::atomic::AtomicBool>) -> bool {
    cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed))
}

/// Resolve as soon as the interrupt flag is set, polling at ~50 ms — fine for a
/// human keypress and cheap enough to lose the race to any real I/O future.
/// Never resolves when `cancel` is `None`, so a `select!` against it collapses
/// to just the other arm (no behavior change for headless callers).
async fn cancelled(cancel: Option<&std::sync::atomic::AtomicBool>) {
    match cancel {
        None => std::future::pending().await,
        Some(flag) => {
            // Poll briskly so an interrupt is felt promptly (well under the
            // ~100 ms human threshold) — the cost is negligible next to a model
            // round-trip.
            while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                tokio::time::sleep(std::time::Duration::from_millis(15)).await;
            }
        }
    }
}

/// Race a model future against the interrupt flag: `Some(v)` if it finished,
/// `None` if the user interrupted first (the future is dropped, cancelling any
/// in-flight request).
async fn cancellable<F: std::future::Future>(
    cancel: Option<&std::sync::atomic::AtomicBool>,
    fut: F,
) -> Option<F::Output> {
    tokio::select! {
        biased;
        _ = cancelled(cancel) => None,
        v = fut => Some(v),
    }
}

fn present_synthetic_tool_result<W: std::io::Write>(
    display: &mut display::ToolDisplay<W>,
    name: &str,
    args: &serde_json::Value,
    workspace: &std::path::Path,
    result: &str,
) {
    let (name, detail) = tools::tool_presentation(name, args, workspace);
    display.call(&name, &detail);
    display.result(result);
}

fn print_synthetic_tool_result(
    name: &str,
    args: &serde_json::Value,
    workspace: &str,
    result: &str,
    color: bool,
) {
    let mut tool_display = display::ToolDisplay::new(
        io::stdout(),
        color,
        display::term_cols(),
        display::spill_lines(),
    );
    present_synthetic_tool_result(
        &mut tool_display,
        name,
        args,
        std::path::Path::new(workspace),
        result,
    );
}

/// The legacy `animate` / `show_thinking` gate expressed as a [`LineCaps`]
/// override.
///
/// **Temporary, by design.** The real gate is `LineCaps::detect()`, which
/// requires two real terminals; `animate` is `color && thinking_stream_enabled()`
/// and never checked a TTY at all. Passing the legacy answer through the
/// arbiter's override seam keeps *when* the spinner appears bit-for-bit
/// identical while *how* it is drawn, ticked and erased moves onto the shared
/// implementation. Protocol mode still vetoes absolutely, so the override can
/// never re-open the JSON-RPC hazard.
///
/// Retiring this — so `thinking = "off"` informs the opt-out only and capability
/// is decided in one place — is the final step of the migration.
fn legacy_caps(animate: bool) -> crate::tty::LineCaps {
    if animate {
        crate::tty::LineCaps::Own
    } else {
        crate::tty::LineCaps::None
    }
}

pub async fn chat_complete(
    ctx: ChatCtx<'_>,
    mcp: &mut dyn McpTools,
) -> anyhow::Result<(String, bool, Option<crate::TokenUsage>, u32)> {
    chat_complete_with_prompt(ctx, None, None, mcp).await
}

/// Provenance-aware variant of [`chat_complete`]. Callers with a durable
/// receipt and conversation-fenced resolver pass them here; legacy/headless
/// callers retain source compatibility through [`chat_complete`] and receive
/// one stable in-memory receipt synthesized for the turn.
pub async fn chat_complete_with_prompt(
    ctx: ChatCtx<'_>,
    turn_prompt_context: Option<&crate::TurnPromptContext>,
    prompt_source: Option<&dyn PromptSource>,
    mcp: &mut dyn McpTools,
) -> anyhow::Result<(String, bool, Option<crate::TokenUsage>, u32)> {
    chat_complete_with_prompt_and_artifacts(
        ctx,
        turn_prompt_context,
        prompt_source,
        None,
        None,
        mcp,
    )
    .await
}

/// Prompt- and artifact-aware variant used by the interactive harness.
///
/// The separate source/sink arguments keep legacy embedders source-compatible
/// while letting persistent and ephemeral TUI sessions expose the same
/// append-only artifact surface. Both implementations are bound to the active
/// conversation before they reach this function.
pub async fn chat_complete_with_prompt_and_artifacts(
    ctx: ChatCtx<'_>,
    turn_prompt_context: Option<&crate::TurnPromptContext>,
    prompt_source: Option<&dyn PromptSource>,
    artifact_source: Option<&dyn artifact_read::ArtifactSource>,
    artifact_sink: Option<&dyn artifact_read::PromptArtifactSink>,
    mcp: &mut dyn McpTools,
) -> anyhow::Result<(String, bool, Option<crate::TokenUsage>, u32)> {
    // OpenAI-compatible endpoints speak a different wire format (request,
    // tool_calls, and usage shapes all differ), so they get their own loop.
    if ctx.kind == crate::BackendKind::Openai {
        // A backend with `api = "responses"` speaks the newer Responses API
        // (gpt-5-codex et al., served only there); the default stays on
        // /v1/chat/completions.
        if responses_api_selected() {
            return openai_responses_complete_with_prompt_and_artifacts(
                ctx,
                turn_prompt_context,
                prompt_source,
                artifact_source,
                artifact_sink,
                mcp,
            )
            .await;
        }
        return openai_chat_complete_with_prompt_and_artifacts(
            ctx,
            turn_prompt_context,
            prompt_source,
            artifact_source,
            artifact_sink,
            mcp,
        )
        .await;
    }
    // Step 25.4 (#568): capture the markdown decision before `ctx` is consumed
    // by the destructure (the destructures ignore it via `markdown: _`).
    let markdown = ctx.markdown;
    let ChatCtx {
        url,
        model,
        kind: _,
        api_key: _,
        messages: mem_messages,
        task,
        workspace,
        color,
        markdown: _,
        tool_offload,
        spill_store,
        compaction_store,
        scratchpad,
        scratchpad_store,
        code_search,
        where_is,
        nav,
        exposure,
        experience_store,
        step_ledger,
        caveats,
        persona_tools,
        // Ollama has no Chat Completions capability projection. Bound these
        // fields to `_` so its request body remains unchanged.
        cognition: _,
        chat_completions_capability: _,
        reasoning_replay_scope: _,
        max_tool_rounds,
        workflow_grace_rounds,
        narration_nudge_cap,
        action_nudges,
        prompt_disposition,
        prompt_intake,
        tool_output_lines,
        debug,
        trace,
        num_ctx,
        connect_timeout_secs,
        inference_timeout_secs,
        mid_loop_trim_threshold,
        compaction_trigger_policy,
        mid_loop_trim_tokens,
        max_ok_input,
        build_check_cmd,
        safe_context,
        recover_cw_400,
        mut note_sink,
        mut note_nudge,
        recall_source,
        memory_source,
        summarizer,
        compress_state,
        mut tool_events,
        mut phantom_reaches,
        mut end_reason,
        mut solve_obs,
        mut permission_gate,
        mut on_round_usage,
        estimate_ratio,
        estimation,
        summary_input_cap_floor_chars,
        input_ceiling_pct,
        low_budget_pct,
        exec_floor,
        write_ledger,
        cancel,
        live_tool_output,
        git_tool,
        crew_runner,
        operating_mode_control,
        plan_mode_control,
    } = ctx;
    // Explain / Research / Ask turns may still use bounded read-only tools, but
    // must never inherit the harness's execution-pressure repairs.
    let action_nudges = action_nudges && prompt_disposition == PromptDisposition::Act;
    let max_tool_rounds = prompt_disposition.tool_round_limit(max_tool_rounds);
    // Headless callers may pass no session state — compression still works,
    // with per-turn anti-thrash accounting.
    let mut local_compress_state = CompressState::new();
    let compress_state = match compress_state {
        Some(s) => s,
        None => &mut local_compress_state,
    };
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(connect_timeout_secs))
        .timeout(std::time::Duration::from_secs(inference_timeout_secs))
        .build()?;
    // #643: the streaming re-issue (the final-text round consumed token-by-token
    // by `stream_response`) must NOT use a whole-request `.timeout()`. That bounds
    // connect + headers + the ENTIRE body, so a slow-but-progressing token stream
    // is aborted mid-flight the instant total time crosses the deadline, and the
    // retry envelope then restarts the full prefill — the DGX retry-storm wedge.
    // An IDLE `read_timeout` is the right bound: it caps the gap between chunks and
    // resets on every token, so a progressing stream runs as long as it keeps
    // producing while a genuinely stalled connection still bails after
    // `inference_timeout_secs` of silence. The one-shot `stream:false` probe below
    // keeps `client` (a whole-request bound is correct for a single-shot response).
    let stream_client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(connect_timeout_secs))
        .read_timeout(std::time::Duration::from_secs(inference_timeout_secs))
        .build()?;
    let chat_url = format!("{}/api/chat", url.trim_end_matches('/'));
    let retry = tui_retry_policy();
    // The save_note tool is advertised only when a sink exists (Step 19.3);
    // recall only when a source exists (Step 17.5); memory_fetch only when a
    // memory source exists (#319) — same presence gating.
    let advertise_save_note = note_sink.is_some();
    let advertise_recall = recall_source.is_some();
    let advertise_memory_fetch = memory_source.is_some();
    // Step 26.4 (#583): state tools only when the feature is on AND a store exists.
    let advertise_scratchpad = scratchpad_store.is_some() && scratchpad;
    // Step 26.5.5 (#582): the code_search tool when a searcher is present.
    let advertise_code_search = code_search.is_some();
    // Step 26.6a (#585): the experiential tools when a store is present.
    let advertise_experiential = experience_store.is_some();
    // Step 26.6b (#586): the scheduled plan tools when a ledger is present.
    let advertise_scheduled = step_ledger.is_some();
    let advertise_git = git_tool.is_some();
    let advertise_team = crew_runner.is_some();
    let advertise_operating_mode = operating_mode_control.is_some();
    let advertise_plan_mode = plan_mode_control.is_some();
    let advertise_plan_mode_active =
        plan_mode_control.is_some_and(|control| control.is_plan_mode());

    // Convert MemMessage list to Ollama JSON format.
    // The memory manager already included the current task as the last user message.
    let mut messages: Vec<serde_json::Value> = mem_messages
        .iter()
        .map(|m| serde_json::json!({"role": m.role.as_str(), "content": m.content}))
        .collect();
    let ephemeral_prompt = turn_prompt_context.is_none().then(|| {
        crate::TurnPromptContext::ephemeral_operator(
            "ephemeral-headless",
            task.as_bytes().to_vec(),
            task.as_bytes().to_vec(),
        )
    });
    let turn_prompt_context = turn_prompt_context.or(ephemeral_prompt.as_ref());
    let prompt_context =
        prompt_read::PromptReadContext::new(turn_prompt_context, task, prompt_source);
    let artifact_context = turn_prompt_context
        .map(|turn| artifact_read::ArtifactReadContext::from_turn(turn, artifact_source));
    let active_task = prompt_context.active_text();
    if let Some(intake) = prompt_intake {
        prompt_read::ensure_active_prompt_card_with_intake(&mut messages, prompt_context, intake);
    } else {
        prompt_read::ensure_active_prompt_card(&mut messages, prompt_context);
    }

    // In-band memory nudge (Step 19.3): after `[memory] note_nudge_interval`
    // user turns with zero organic save_note use, append a one-line reminder
    // to this turn's user message. Only when a sink exists — without one the
    // save_note tool isn't even advertised.
    if note_sink.is_some() {
        if let Some(line) = note_nudge.as_deref_mut().and_then(NoteNudge::begin_turn) {
            append_nudge_line(&mut messages, &line);
        }
    }

    let mut accumulated_usage: Option<crate::TokenUsage> = None;
    let mut hallucination_count: u32 = 0;
    // Step 27.3/#771: guard against exact-repeat tool loops this run.
    let mut repeat_calls = RepeatCallGuard::default();
    let mut overflow_retries: u32 = 0;
    let mut suspicious_empty_retries: u32 = 0;
    // Hard context-window 400s recovered (parse limit → trim → retry). See #223.
    let mut cw_retries: u32 = 0;
    // Some models reject ANY request carrying a `tools` field (e.g.
    // deepseek-r1). Once one 400s with "does not support tools", drop tools for
    // the rest of the session so even a bare "hello" works; notice it once.
    let mut tools_supported = true;
    let mut tools_unsupported_notified = false;
    // Pre-send token budget gate: trim before dispatch when the current context
    // size exceeds the model's empirically-confirmed max input (or the safe
    // context) — capped by the `num_ctx` every request this turn will carry
    // (#282: on a fresh capability cache the cached numbers are unset or huge,
    // so without the ceiling the first turn dispatched 10× over the real
    // window with zero events — B6). Mutable because a recovered 400 tightens
    // it mid-turn. See #223.
    let mut effective_input_ceiling = num_ctx_input_ceiling(num_ctx, input_ceiling_pct, None);
    let mut send_budget: Option<usize> =
        initial_send_budget(max_ok_input, safe_context, effective_input_ceiling);
    // Step 20.3: is the send budget backed by an authoritative ceiling, or
    // does it rest on the proven-good high-water mark (`max_ok_input`) alone?
    // `safe_context` (a believed/declared window) and the per-request
    // `num_ctx` ceiling are authoritative; a cw-400 recovery flips this true
    // mid-turn. Cloud endpoints with no `/api/show` seed neither, so their
    // guard is non-authoritative and fails open instead of refusing.
    let mut send_budget_authoritative = safe_context.is_some() || effective_input_ceiling.is_some();
    // Tool schemas ride along in every request body; count them once (18.1).
    // Stable for the whole turn: the builtin + MCP tool set doesn't change
    // mid-turn, so hoisting out of the round loop is safe.
    let tools = merged_tool_definitions(
        mcp,
        advertise_save_note,
        advertise_recall,
        advertise_memory_fetch,
        advertise_git,
        advertise_team,
        advertise_scratchpad,
        advertise_code_search,
        advertise_experiential,
        advertise_scheduled,
        advertise_operating_mode,
        advertise_plan_mode,
        advertise_plan_mode_active,
    );
    // FR-1 part 2 (#997): scope the advertised catalog to the active persona's
    // `tools:` allow-list (no-op when `persona_tools` is `None`). The executor
    // enforces the same set, so what the model sees and what it may run agree.
    let tools = filter_advertised_tools(tools, persona_tools);
    let tools = filter_tools_for_disposition(tools, prompt_disposition);
    // #TEC Pass 1: the exposure stage. Clip the AUTHORIZED catalog to what the
    // model's LIVE usable budget can afford (probed `safe_context` → send
    // budget), never by model name. `ExposureProfile::Full` (the default) is
    // identity, so this is bit-for-bit unchanged unless `[tool_exposure]` opts
    // in. Applied before the token estimate so what we count equals what we
    // send. Dispatch still authorizes on the full set — exposure ≠ authority.
    let tools = crate::agentic::tools::select_exposed(
        tools,
        &exposure,
        exposure_budget_tokens(send_budget, safe_context),
        &std::collections::BTreeSet::new(),
        estimation,
    );
    let tool_tokens = estimate_value_tokens(&tools, estimation);
    // Phase 20 §2.3: one sanitized calibration ratio per turn. The
    // tool-schema overhead converts to real-token space once — the schema
    // set is stable for the whole turn, and the send budget it is subtracted
    // from is real-token currency.
    let cal = sanitize_estimate_ratio(estimate_ratio);
    let tool_tokens_real = calibrate_up(tool_tokens, cal);
    preflight_irreducible_request(
        &messages,
        Some(&tools),
        authoritative_request_budget(send_budget, send_budget_authoritative, mid_loop_trim_tokens),
        cal,
        estimation,
        model,
    )?;
    // Animate the in-place "thinking…/compressing…" status line only on a TTY
    // with streaming enabled (never in a pipe / `newt worker`). Constant for the
    // turn — both the compression and probe waits reuse it.
    let animate = color && thinking_stream_enabled();
    // Truthful context-size tracker: anchors on the backend's last-reported
    // prompt token count, chars/4 + schema estimate as fallback (Step 18.1).
    let mut prompt_tracker = PromptTracker::new();
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    // Consecutive rounds where the model only called read-only tools (no writes).
    // When this hits READ_ONLY_NUDGE_AFTER, a brief injected message tells the
    // model to stop exploring and start writing.
    let mut read_only_rounds: usize = 0;
    // "Narrate-then-stop" rescue: a weak model often ANNOUNCES its next action
    // in prose ("Let me edit …") and emits no tool call. The loop would treat
    // that zero-tool round as a final answer and end the turn, forcing a human
    // "continue". `narration_nudges` bounds the auto-continue that instead
    // nudges the model to actually call the tool (≤ `narration_nudge_cap` per
    // turn); the trigger is the configurable NudgeClassifier, so a genuine
    // conclusion (with or without prior tool calls this turn) is never nudged.
    let mut narration_nudges: usize = 0;
    // State-driven final-answer gate: a no-tool reply is suspicious when the
    // active plan still has open steps. Nudge once to update_plan / act / block.
    let mut pending_plan_nudges: usize = 0;
    // A stricter sibling: when a model concludes "the file changed under me"
    // without proving it, force one read-only ground-truth check round instead
    // of letting it hand the stale-context claim back to the human.
    let mut stale_file_nudges: usize = 0;
    let nudge_classifier = crate::NudgeClassifier::load_default();
    // #1152/#1162: action-pressure nudges (narration rescue, workflow repair
    // steering, pending-plan pushes) fire ONLY when the user's turn actually
    // invited action. A question or acknowledgment gets none — narration IS
    // the deliverable, and pressuring a model answering a question is how the
    // "I'm genuinely finished" defense loop (#1158) gets seeded.
    let action_turn = action_nudges && crate::classifiers::user_turn_invites_action(active_task);
    let workflow_steerer = crate::WorkflowSteerer::load_default();
    let mut workflow_runtime = WorkflowRuntimeState {
        tenacity: crate::tenacity::effective_tenacity(),
        ..Default::default()
    };
    // The matching workflow's round-cap grace horizon override, resolved once
    // from the turn's opening context (diagnostic workflows need more
    // read-only rounds between checkpoints than routine edits — see
    // `diagnose_failure.toml`'s `progress_horizon_rounds`).
    workflow_runtime.set_progress_horizon(
        workflow_steerer.progress_horizon(&workflow_classifier_text(&messages, "")),
    );
    let mut ollama_xml_retry_nudges: usize = 0;
    // Phase 20 §2.2: the thinking-only quirk is reported at most once per
    // turn — re-detection adds no information and would thrash the cache.
    let mut thinking_only_reported = false;
    // #867 Part A: ledger of REAL workspace paths surfaced by tool results,
    // collected as the rounds happen so it survives the cap-exit trim.
    let mut observed_paths = claim_check::ObservedPaths::default();
    let observed_resolver = claim_check::workspace_resolver(workspace);
    // #1214: HEAD at turn start — the ground truth "did THIS turn actually
    // commit anything" compares against at cap-exit.
    let turn_start_head = claim_check::git_head(workspace);

    // Agentic loop — up to `max_tool_rounds` tool-call rounds, with an optional
    // evidence-backed grace window when the normal cap lands during active
    // workflow progress. The hard ceiling remains finite and configurable.
    let hard_tool_rounds = max_tool_rounds.saturating_add(workflow_grace_rounds);
    let mut workflow_grace_active = false;
    let mut current_tool_round_limit = max_tool_rounds;
    'round_loop: for round in 0..hard_tool_rounds {
        if round >= current_tool_round_limit {
            if workflow_grace_active {
                break;
            }
            if !action_nudges {
                break;
            }
            let Some(nudge) = workflow_runtime.cap_grace_nudge(
                step_ledger,
                max_tool_rounds,
                workflow_grace_rounds,
            ) else {
                break;
            };
            workflow_grace_active = true;
            current_tool_round_limit = hard_tool_rounds;
            if debug {
                print_debug(
                    "workflow progress at soft round cap — granting configured grace window",
                    color,
                );
            }
            messages.push(serde_json::json!({ "role": "user", "content": nudge }));
        }
        // Interrupt checkpoint (Esc / Ctrl-C): bail before spending another
        // round on the model or a tool. The reply is empty — the caller sees
        // `cancel` set and treats the turn as abandoned regardless.
        if is_cancelled(cancel) {
            return Ok((String::new(), false, accumulated_usage, hallucination_count));
        }
        if round > 0 {
            // Brief separator between rounds so user can follow the flow.
            if color {
                execute!(
                    io::stdout(),
                    SetForegroundColor(CtColor::DarkGrey),
                    Print("…\n"),
                    ResetColor
                )
                .ok();
            }
        }

        // Conditional plan re-seat (#630 b): re-show the ACTIVE step each round
        // so a weak model doesn't lose track of a multi-step plan as the round-0
        // <plan> snapshot goes stale (validated on dgx1: re-seat 12/12 vs baseline
        // 8/12 under drift). Compact + gated to multi-step in-progress plans.
        // Supersedes the env-gated NEWT_RESEAT_PLAN experiment that #629 carried
        // to main by accident.
        if round > 0 && action_nudges {
            if let Some(ptr) = step_ledger.and_then(plan_reseat_pointer) {
                messages.push(serde_json::json!({ "role": "user", "content": ptr }));
            }
            if let Some(nudge) = workflow_runtime.round_start_nudge(step_ledger) {
                messages.push(serde_json::json!({ "role": "user", "content": nudge }));
            }
        }

        // Read-only round nudge: if the model has spent several consecutive
        // rounds only reading (list_dir / read_file / web_fetch / search /
        // use_skill) without writing anything, inject a brief reminder to
        // stop exploring and call edit_file or write_file.  This breaks the
        // "endless exploration → empty response" failure mode seen with some
        // local models (e.g. nemotron3:33b).
        // The action-forcing threshold is now a `Tenacity` level rather than a
        // magic constant (#tenacity). `Standard` preserves the historical value
        // of 3; config + per-family wiring that lets an operator raise it lands
        // in a follow-up, plugging into exactly this seam.
        let read_only_nudge_after = crate::tenacity::Tenacity::Standard.read_only_nudge_after();
        if action_nudges && read_only_rounds >= read_only_nudge_after {
            let remaining = current_tool_round_limit.saturating_sub(round + 1);
            // Sustained read-only exploration on a task that classifies as a
            // diagnose/fix workflow is exactly the shape `crew`/`team`
            // delegation exists for — offer it here (only when sub-agent
            // dispatch is actually available this session) instead of only
            // ever telling the model to act inline.
            let delegate_hint = workflow_steerer
                .delegate_hint(&workflow_classifier_text(&messages, ""), advertise_team);
            messages.push(serde_json::json!({
                "role": "user",
                "content": read_only_action_nudge(
                    read_only_rounds,
                    remaining,
                    step_ledger,
                    delegate_hint.as_deref(),
                )
            }));
            read_only_rounds = 0;
        }

        // Context compression (Step 18.4, #247): one shared pipeline —
        // structural prune → boundary → redacted LLM summary → marker
        // assembly — serves both the mid-loop trigger (message count OR
        // current tokens: the VRAM guard and issue #223's token guard) and
        // the pre-send budget guard (`max_ok_input`/`safe_context`). The
        // current-token figure is prompt-tokens-preferred (Step 18.1). The
        // old amputation trim survives only as the pipeline's no-summarizer
        // static-marker path.
        {
            // Phase 20 §2.3: `current` is calibrated into real-token space —
            // the same currency as the (backend-derived) send budget and the
            // configured token threshold it is compared against.
            let current = prompt_tracker.current(&messages, Some(&tools), cal, estimation);
            // The count-only budget is priced in message-token space — the
            // same chars/4 currency the pipeline compares its budget against
            // (F1); `current` (schema/template-inclusive) still drives the
            // token triggers.
            let message_tokens = estimate_tokens(&messages, estimation);
            // A learned `max_ok_input` alone is a proven-good floor, not a
            // context-window ceiling. Only a configured token cap or a
            // budget backed by a known/recovered window suppresses a
            // count-only compaction under the default policy.
            let has_authoritative_headroom = authoritative_request_budget(
                send_budget,
                send_budget_authoritative,
                mid_loop_trim_tokens,
            )
            .is_some();
            if let Some(trigger) = compression_trigger(
                messages.len(),
                current,
                message_tokens,
                CompressionTriggerLimits {
                    count_threshold: mid_loop_trim_threshold,
                    token_threshold: mid_loop_trim_tokens,
                    send_budget,
                    tool_tokens: tool_tokens_real,
                    policy: compaction_trigger_policy,
                    has_authoritative_headroom,
                },
            ) {
                // A hard trigger's budget is real-token currency; the
                // pipeline measures and reclaims in chars/4 — convert once
                // (Phase 20 §2.3). Count-only budgets are already priced in
                // message-token space (F1) and pass through unconverted.
                let pipeline_budget = if trigger.hard_budget {
                    calibrate_down(trigger.budget, cal)
                } else {
                    trigger.budget
                };
                // Step 20.3: does this budget rest on an authoritative ceiling
                // or the lone proven-good HWM? A fired token threshold is
                // user-authoritative; otherwise the guard fired, authoritative
                // only when the send budget is backed by a believed window.
                let token_fired = mid_loop_trim_tokens.is_some_and(|t| t > 0 && current > t);
                // Compression makes its own summarizer model call — animate the
                // line with a "compressing context…" stage so it doesn't sit
                // frozen, and race it against the interrupt flag so Esc bails
                // out of a slow summarize instead of waiting for it to finish.
                let outcome = match crate::tty::with_spinner(
                    legacy_caps(animate),
                    "compressing context…",
                    crate::tty::Sink::Stdout,
                    color,
                    cancellable(
                        cancel,
                        compress(
                            CompressRequest {
                                messages: &messages,
                                budget: pipeline_budget,
                                max_messages: trigger.max_messages,
                                replay_protected_tail_len: 0,
                                task: active_task,
                                hard_budget: trigger.hard_budget,
                                authoritative: token_fired || send_budget_authoritative,
                                focus: None,
                                est: estimation,
                                summary_input_cap_floor_chars,
                                compaction_store,
                            },
                            summarizer,
                            compress_state,
                        ),
                    ),
                )
                .await
                {
                    Some(o) => o,
                    None => {
                        return Ok((String::new(), false, accumulated_usage, hallucination_count))
                    }
                };
                if let Some(notice) = outcome.notice {
                    print_harness_notice(&notice, color);
                }
                if outcome.action == CompressAction::Refused {
                    // Anti-thrash disabled compression and the context still
                    // exceeds the budget: refuse the send rather than let the
                    // backend silently truncate the task away (baseline B6).
                    // Phase 20: name the model and the reset escape hatch —
                    // a poisoned learned budget is a known cause of this bail.
                    anyhow::bail!(
                        "context (~{current} tokens) exceeds the model's input budget and \
                         auto-compression is disabled after repeated ineffective passes — \
                         start a new conversation or ask a more focused question, or run \
                         `newt tunings reset {model}` if this model's learned budget looks wrong"
                    );
                }
                if outcome.fired {
                    // N2: a hard-budget compression whose assembled result is
                    // still over budget says so before the full-request
                    // preflight below refuses its dispatch. The comparison is
                    // in the SAME (chars/4) currency the pipeline measured
                    // `tokens_after` in (Phase 20 §2.3).
                    let suffix = if trigger.hard_budget && outcome.tokens_after > pipeline_budget {
                        ", still over budget"
                    } else {
                        ""
                    };
                    emit_compression_notice(
                        color,
                        outcome.tokens_before,
                        outcome.tokens_after,
                        outcome.action,
                        suffix,
                    );
                    if debug {
                        print_debug(
                            &format!(
                                "compression: {} → {} messages (budget ~{} tokens, \
                                 +~{tool_tokens} tool-schema tokens ride along)",
                                messages.len(),
                                outcome.messages.len(),
                                pipeline_budget,
                            ),
                            color,
                        );
                    }
                    messages = outcome.messages;
                    prompt_tracker.invalidate();
                    apply_post_compaction_continuation(
                        &mut messages,
                        &mut narration_nudges,
                        outcome.action,
                        step_ledger,
                        prompt_context,
                        round > 0,
                        action_nudges,
                    );
                    // Persist provenance only after the transformed working
                    // set and its continuation have been installed.
                    record_compaction_artifact(
                        artifact_sink,
                        artifact_context,
                        outcome.action,
                        outcome.tokens_before,
                        outcome.tokens_after,
                        pipeline_budget,
                        round,
                        trigger.primary_cause.artifact_reason(),
                        Some(&trigger),
                        send_budget_authoritative,
                        color,
                    );
                }
            }
        }

        // Compression is best-effort; an irreducible fresh result (including
        // spill-exempt prompt_read output) may remain larger than the window.
        // Re-price the exact request after every compression/nudge and refuse
        // before the wire rather than relying on a backend 400 or silent head
        // truncation. Only schemas actually advertised on this request count.
        preflight_full_message_request(
            &messages,
            tools_supported.then_some(&tools),
            authoritative_request_budget(
                send_budget,
                send_budget_authoritative,
                mid_loop_trim_tokens,
            ),
            cal,
            estimation,
            model,
        )?;

        // Phase 20 §2.2: chars/4 estimate of EXACTLY the request about to be
        // dispatched (the message list as sent, plus tool schemas) — paired
        // with the backend's reported prompt size in the `Accepted`
        // observation so the caller can learn the calibration ratio.
        let round_est_raw =
            estimate_request_tokens(&messages, tools_supported.then_some(&tools), estimation);

        // Tool-call rounds: stream:false (fast, just JSON).
        // Final text round: stream:true so the user sees tokens arrive.
        // We don't know which round is last, so we probe with stream:false first
        // and switch to streaming only when the model returns no tool calls.
        let mut body_no_stream = if let Some(ctx_size) = num_ctx {
            serde_json::json!({
                "model": model,
                "messages": messages,
                "stream": false,
                "tools": tools.clone(),
                "options": { "num_ctx": ctx_size },
            })
        } else {
            serde_json::json!({
                "model": model,
                "messages": messages,
                "stream": false,
                "tools": tools.clone(),
            })
        };
        // Drop tools entirely for a model that rejects them (set below on a
        // "does not support tools" 400) — an empty array still trips strict
        // models, so remove the key.
        if !tools_supported {
            if let Some(o) = body_no_stream.as_object_mut() {
                o.remove("tools");
            }
        }

        // Retry the send+status+parse as one unit — a connection drop at any
        // of these steps is transient and worth retrying with backoff. Raced
        // against the interrupt flag so Esc bails out of a slow / stuck probe
        // (the common "the model isn't answering" case) without waiting for it.
        // Wrapped in the thinking animation so this otherwise-silent wait shows
        // a live hourglass + clock instead of a frozen line.
        let dispatch = match crate::tty::with_spinner(
            legacy_caps(animate),
            "thinking…",
            crate::tty::Sink::Stdout,
            color,
            cancellable(
                cancel,
                with_backoff_notify(
                    &retry,
                    || async {
                        // W0 (#1511): classify while the error is TYPED — the
                        // DispatchError keeps the historical message text and
                        // carries the structural class to the driver boundary.
                        let resp = client
                            .post(&chat_url)
                            .json(&body_no_stream)
                            .send()
                            .await
                            .map_err(|e| {
                                anyhow::Error::new(observability::DispatchError::from_reqwest(
                                    "request failed",
                                    e,
                                ))
                            })?;
                        if !resp.status().is_success() {
                            let status = resp.status();
                            let text = resp.text().await.unwrap_or_default();
                            return Err(observability::DispatchError::http_status(format!(
                                "Ollama {status}: {text}"
                            ))
                            .into());
                        }
                        resp.json::<serde_json::Value>()
                            .await
                            .map_err(anyhow::Error::from)
                    },
                    |attempt, delay| print_retry_indicator(attempt, delay, color),
                ),
            ),
        )
        .await
        {
            Some(d) => d,
            // Interrupted mid-probe: abandon the turn.
            None => return Ok((String::new(), false, accumulated_usage, hallucination_count)),
        };
        let json: serde_json::Value = match dispatch {
            Ok(j) => j,
            Err(e) => {
                // No-tools recovery: a model that rejects the `tools` field
                // (deepseek-r1) 400s even on "hello". Drop tools, notice once,
                // and re-dispatch the same turn — self-limiting because the
                // rebuilt body omits tools. A malformed XML tool-call error is
                // different: Ollama accepted tools but choked on the model's
                // generated markup, so keep tools available and retry with a
                // bounded corrective nudge.
                let tools_unsupported = is_tools_unsupported_error(&e);
                let malformed_xml_tool_call = is_ollama_tool_xml_error(&e);
                if tools_supported && tools_unsupported {
                    tools_supported = false;
                    if !tools_unsupported_notified {
                        tools_unsupported_notified = true;
                        let notice = format!(
                            "{model} does not support tools — tools disabled for this turn"
                        );
                        print_newt(&notice, color, false);
                    }
                    continue 'round_loop;
                }
                if tools_supported && malformed_xml_tool_call && ollama_xml_retry_nudges < 2 {
                    ollama_xml_retry_nudges += 1;
                    print_newt(
                        &format!(
                            "{model} produced malformed Ollama XML tool-call syntax — \
                             retrying with a stricter tool-call nudge"
                        ),
                        color,
                        false,
                    );
                    messages.push(serde_json::json!({
                        "role": "user",
                        "content": ollama_tool_xml_retry_nudge()
                    }));
                    continue 'round_loop;
                }
                // Graceful context-window overflow recovery: parse the model's
                // real limit, tighten the budget, compress, and retry once (#223;
                // compress-not-trim since Step 18.4). A numberless overflow with
                // no parseable limit (llama.cpp served over the Ollama-compat
                // `/api/chat`) falls back to a cap derived from the current send
                // budget / the `num_ctx` ceiling, so the turn self-heals.
                if cw_retries < 2 {
                    let recovered_window = recover_cw_400.and_then(|f| f(&e, model, &today));
                    if let Some(recovered_budget) = recovered_window
                        .map(|context_window| {
                            recovered_input_budget(
                                context_window,
                                input_ceiling_pct,
                                None,
                                effective_input_ceiling,
                            )
                        })
                        .or_else(|| {
                            cw_overflow::core_recover_overflow(
                                &e.to_string(),
                                send_budget,
                                effective_input_ceiling,
                            )
                            .map(|cap| cap as usize)
                        })
                    {
                        if let Some(context_window) = recovered_window {
                            emit_context_window_400(&mut on_round_usage, context_window);
                        }
                        // A recovered full window is composed through the same
                        // effective-ceiling operation as the declared window;
                        // numberless recovery already derives an input cap.
                        let new_budget = effective_input_ceiling
                            .map_or(recovered_budget, |c| recovered_budget.min(c));
                        emit_overflow_notice(
                            color,
                            accumulated_usage.as_ref(),
                            Some(new_budget.min(u32::MAX as usize) as u32),
                            model,
                            cw_retries + 1,
                        );
                        // A recovered cap can only tighten — the request still
                        // carries the same `num_ctx`, so its ceiling holds (#282).
                        send_budget = Some(new_budget);
                        effective_input_ceiling = Some(new_budget);
                        // The endpoint's parsed hard limit is authoritative —
                        // a refuse on it is correct from here on (Step 20.3).
                        send_budget_authoritative = true;
                        let outcome = compress(
                            CompressRequest {
                                // Real-token budget minus real-token schema
                                // overhead, converted into the pipeline's
                                // chars/4 currency (Phase 20 §2.3).
                                messages: &messages,
                                budget: calibrate_down(
                                    new_budget.saturating_sub(tool_tokens_real),
                                    cal,
                                ),
                                max_messages: None,
                                replay_protected_tail_len: 0,
                                task: active_task,
                                hard_budget: true,
                                authoritative: true,
                                focus: None,
                                est: estimation,
                                summary_input_cap_floor_chars,
                                compaction_store,
                            },
                            summarizer,
                            compress_state,
                        )
                        .await;
                        if let Some(notice) = outcome.notice {
                            print_harness_notice(&notice, color);
                        }
                        if outcome.action == CompressAction::Refused {
                            // Refuse the resend; surface the endpoint's 400.
                            return Err(e);
                        }
                        if outcome.fired {
                            messages = outcome.messages;
                            prompt_tracker.invalidate();
                            apply_post_compaction_continuation(
                                &mut messages,
                                &mut narration_nudges,
                                outcome.action,
                                step_ledger,
                                prompt_context,
                                round > 0,
                                action_nudges,
                            );
                            record_compaction_artifact(
                                artifact_sink,
                                artifact_context,
                                outcome.action,
                                outcome.tokens_before,
                                outcome.tokens_after,
                                calibrate_down(new_budget.saturating_sub(tool_tokens_real), cal),
                                round,
                                "context_window_400",
                                None,
                                false,
                                color,
                            );
                        }
                        cw_retries += 1;
                        continue 'round_loop;
                    }
                }
                return Err(e);
            }
        };

        // Merge token usage from this non-streaming probe round (input = max
        // single prompt, output = sum — Step 18.1) and anchor the context-size
        // tracker on the backend-reported prompt size of this dispatch.
        let round_usage = ollama_usage(&json);
        if let Some(u) = round_usage {
            prompt_tracker.record(u.input_tokens, messages.len());
        }
        accumulated_usage = merge_round_usage(accumulated_usage, round_usage);

        // Phase 20 §2.2: a prompt within 5% of the request's `num_ctx` may
        // have been silently head-truncated by Ollama — such a round is
        // window evidence of NOTHING and must neither raise the budget nor
        // emit an `Accepted` observation.
        let truncation_suspect = round_usage
            .is_some_and(|u| num_ctx.is_some_and(|c| u.input_tokens >= c.saturating_mul(95) / 100));
        // Mid-turn budget raise on window evidence alone: the backend just
        // evaluated this many prompt tokens inside the `num_ctx` it was sent,
        // so one over-budget acceptance stops the compress-every-round thrash
        // within the same turn (Phase 20 §2.2). Never lowers; stays under the
        // per-request input ceiling.
        if let (Some(u), Some(budget), false) = (round_usage, send_budget, truncation_suspect) {
            let raised =
                (u.input_tokens as usize).min(effective_input_ceiling.unwrap_or(usize::MAX));
            if raised > budget {
                send_budget = Some(raised);
                if debug {
                    print_debug(
                        &format!(
                            "send budget raised to ~{raised} tokens (backend accepted \
                             {}-token prompt)",
                            u.input_tokens
                        ),
                        color,
                    );
                }
            }
        }

        let message = &json["message"];
        // Capture the probe content now — it may be our only copy of the
        // model's reply if the subsequent streaming re-issue returns empty.
        let probe_content = message["content"].as_str().unwrap_or("").to_string();

        let native_calls = message["tool_calls"].as_array();
        // Recover tool calls a weak model emitted in CONTENT instead of the
        // native `tool_calls` field — the #1 weak-model failure (see
        // `tool_recovery`). Only attempted when the native array is empty;
        // recovered calls are produced in native shape and flow unchanged into
        // the executor + `is_hallucination` + dup-guard + caveat path below.
        let recovered = if native_calls.map(|t| t.is_empty()).unwrap_or(true) {
            tool_recovery::recover_tool_calls(&probe_content)
        } else {
            tool_recovery::Recovery::default()
        };
        let tool_calls: Option<&Vec<serde_json::Value>> = match native_calls {
            Some(t) if !t.is_empty() => Some(t),
            _ if !recovered.calls.is_empty() => Some(&recovered.calls),
            _ => None,
        };
        let has_tools = tool_calls.map(|tc| !tc.is_empty()).unwrap_or(false);
        // W0 (#1511): record what the backend SAYS it served plus this
        // round's parse status for the solve contract. `json["model"]` is the
        // served reality (the contract's `effective_model` source, never an
        // echo of our request); the signal is the ADR §5
        // recovered_tool_call{dialect} / no_parseable_tool_call trace event.
        if let Some(obs) = solve_obs.as_deref_mut() {
            if let Some(m) = json["model"].as_str().filter(|m| !m.is_empty()) {
                obs.served_model = Some(m.to_string());
            }
            let native = native_calls.is_some_and(|t| !t.is_empty());
            if let Some(sig) = observability::round_parse_signal(
                round,
                !probe_content.is_empty(),
                native,
                recovered.dialect,
            ) {
                obs.parse_signals.push(sig);
            }
        }
        if debug && !recovered.calls.is_empty() {
            print_debug(
                &format!(
                    "recovered {} tool call(s) from content (non-native emission)",
                    recovered.calls.len()
                ),
                color,
            );
        }

        if debug {
            let content_excerpt = if probe_content.is_empty() {
                "(empty)".to_string()
            } else {
                let chars: String = probe_content.chars().take(80).collect();
                if probe_content.len() > 80 {
                    format!("{chars}…")
                } else {
                    chars
                }
            };
            let tc_count = tool_calls.map(|tc| tc.len()).unwrap_or(0);
            let usage_str = match round_usage {
                Some(u) => format!("{} in / {} out", u.input_tokens, u.output_tokens),
                None => "no usage".into(),
            };
            print_debug(
                &format!(
                    "round {round} probe: tool_calls={tc_count} usage=[{usage_str}] content={content_excerpt:?}"
                ),
                color,
            );
        }

        if !has_tools {
            // Format-hallucination tracker: the content looked like a tool-call
            // attempt but could not be recovered into one — count it so cap-exit
            // and metrics see a tooling failure, not a clean final answer.
            if recovered.tool_shaped {
                hallucination_count += 1;
                if debug {
                    print_debug(
                        "format-hallucination: tool call emitted as unrecoverable text",
                        color,
                    );
                }
            }
            // A final text candidate can come from either the streaming re-issue
            // or the non-streamed probe fallback. Run both through the same
            // no-tool final-answer gates so "Let me inspect..." does not force a
            // human "continue" just because the stream returned empty.
            macro_rules! maybe_nudge_no_tool_content {
                ($content:expr, $usage:expr) => {{
                    let content = $content;
                    if !content.is_empty() {
                        let nudge_classification = nudge_classifier.classify(content);
                        let workflow_classifier_text =
                            workflow_classifier_text(&messages, content);
                        let workflow_hint = nudge_classification
                            .is_plan_update()
                            .then(|| workflow_steerer.plan_update_hint(&workflow_classifier_text))
                            .flatten();
                        let classifier_plan_direction = nudge_classification
                            .is_plan_update()
                            .then(|| {
                                nudge_classifier.direction_for(crate::NudgeClass::PlanUpdate)
                            })
                            .flatten();
                        let plan_nudge_hint =
                            combine_nudge_hints(classifier_plan_direction, workflow_hint.as_deref());
                        if action_nudges && round + 1 < current_tool_round_limit {
                            if let Some(nudge) = workflow_runtime.rediscovery_nudge(
                                Some(&nudge_classification),
                                content,
                                step_ledger,
                            ) {
                                if debug {
                                    print_debug(
                                        "workflow evidence rediscovery — nudging toward active repair",
                                        color,
                                    );
                                }
                                messages.push(serde_json::json!({
                                    "role": "assistant",
                                    "content": content
                                }));
                                messages.push(serde_json::json!({
                                    "role": "user",
                                    "content": format!(
                                        "{} {}",
                                        compress::LOOP_GUIDANCE_PREFIX, nudge
                                    )
                                }));
                                accumulated_usage = merge_round_usage(accumulated_usage, $usage);
                                continue 'round_loop;
                            }
                        }
                        if action_nudges
                            && pending_plan_nudges < PENDING_PLAN_NUDGE_CAP
                            && round + 1 < current_tool_round_limit
                        {
                            if let Some(nudge) = pending_plan_completion_nudge(
                                step_ledger,
                                nudge_classification.is_plan_update(),
                                plan_nudge_hint.as_deref(),
                            ) {
                                if debug {
                                    print_debug(
                                        "active plan has unfinished steps — nudging before final answer",
                                        color,
                                    );
                                }
                                messages.push(serde_json::json!({
                                    "role": "assistant",
                                    "content": content
                                }));
                                messages.push(serde_json::json!({
                                    "role": "user",
                                    "content": format!(
                                        "{} {}",
                                        compress::LOOP_GUIDANCE_PREFIX, nudge
                                    )
                                }));
                                pending_plan_nudges += 1;
                                accumulated_usage = merge_round_usage(accumulated_usage, $usage);
                                continue 'round_loop;
                            }
                        }
                        if stale_file_nudges < STALE_FILE_NUDGE_CAP
                            && round + 1 < current_tool_round_limit
                            && action_nudges
                            && looks_like_unverified_stale_file_blocker(content)
                        {
                            if debug {
                                print_debug(
                                    "unverified stale-file blocker — nudging to check ground truth",
                                    color,
                                );
                            }
                            messages.push(serde_json::json!({
                                "role": "assistant",
                                "content": content
                            }));
                            messages.push(serde_json::json!({
                                "role": "user",
                                "content": format!(
                        "{} {}",
                        compress::LOOP_GUIDANCE_PREFIX,
                        stale_file_ground_truth_nudge()
                    ),
                            }));
                            stale_file_nudges += 1;
                            accumulated_usage = merge_round_usage(accumulated_usage, $usage);
                            continue 'round_loop;
                        }
                        if narration_nudges < narration_nudge_cap
                            && round + 1 < current_tool_round_limit
                            && nudge_classification.is_pending_action()
                            && action_turn
                        {
                            if debug {
                                print_debug(
                                    "narrated intent with no tool call — nudging to act and continuing",
                                    color,
                                );
                            }
                            // #1158: drop the PRIOR nudge exchange first so
                            // successive nudges replace rather than pile up.
                            strip_trailing_nudge_exchange(&mut messages);
                            // Record the model's own narration, then the
                            // corrective, so the next round sees both (mirrors
                            // the has-tools assistant turn).
                            messages.push(serde_json::json!({
                                "role": "assistant",
                                "content": content
                            }));
                            // First nudge: the (tunable) classifier direction.
                            // Later nudges (cap > 1): generic text already
                            // failed to convert intent into action, so
                            // escalate — name the active step, demand a bare
                            // tool call.
                            let direction = if narration_nudges == 0 {
                                nudge_classifier
                                    .direction_for(nudge_classification.class)
                                    .map(str::to_string)
                                    .unwrap_or_else(narration_action_nudge)
                            } else {
                                escalated_narration_action_nudge(
                                    narration_nudges + 1,
                                    narration_nudge_cap,
                                    step_ledger,
                                )
                            };
                            messages.push(serde_json::json!({
                                "role": "user",
                                "content": format!("{} {}", compress::LOOP_GUIDANCE_PREFIX, direction),
                            }));
                            narration_nudges += 1;
                            accumulated_usage = merge_round_usage(accumulated_usage, $usage);
                            continue 'round_loop;
                        }
                        // Every rescue gate passed on this content: it is
                        // about to be accepted as the final answer. Record
                        // WHY — before this, a narration acceptance was
                        // indistinguishable from a normal completion, even
                        // under NEWT_DEBUG (2026-07-08 ornith:35b forensics).
                        //
                        // #1261: the reason MIRRORS the rescue gate above —
                        // including its `action_turn` guard. On a turn where
                        // the rescue can never arm (non-Act disposition forces
                        // `action_nudges = false`, or the prompt does not
                        // invite action), prose is this turn's LEGITIMATE
                        // ending: reporting "rescue budget spent" there blamed
                        // the model for a harness decision — the budget was
                        // untouched and no cap value could have changed
                        // anything (the diagnosed ornith:35b Explain turn).
                        let accepted_reason = if nudge_classification.is_pending_action()
                            && action_turn
                        {
                            if round + 1 >= current_tool_round_limit {
                                crate::TurnEndReason::NarrationFinalRound
                            } else {
                                crate::TurnEndReason::NarrationCapExhausted
                            }
                        } else {
                            crate::TurnEndReason::Completed
                        };
                        if debug && accepted_reason != crate::TurnEndReason::Completed {
                            print_debug(
                                &format!(
                                    "no-tool narration accepted as final answer ({accepted_reason:?})"
                                ),
                                color,
                            );
                        }
                        if let Some(slot) = &mut end_reason {
                            **slot = Some(accepted_reason);
                        }
                    }
                }};
            }
            // No tool calls — re-issue with stream:true so the user sees tokens.
            // `messages` already contains the task; just replay with streaming.
            //
            // IMPORTANT: the probe round already generated the model's answer in
            // `probe_content`. The streaming re-issue is a *second* inference call
            // from the same history; if it returns empty (non-determinism, context
            // pressure, or model quirk) we fall back to the probe content so the
            // user never sees a silent blank response.
            let mut body_stream = if let Some(ctx_size) = num_ctx {
                serde_json::json!({
                    "model": model,
                    "messages": &messages,
                    "stream": true,
                    "tools": tools.clone(),
                    "options": { "num_ctx": ctx_size },
                })
            } else {
                serde_json::json!({
                    "model": model,
                    "messages": &messages,
                    "stream": true,
                    "tools": tools.clone(),
                })
            };
            // A no-tools model (set on a prior "does not support tools" 400)
            // must not see the key on the streaming round either.
            if !tools_supported {
                if let Some(o) = body_stream.as_object_mut() {
                    o.remove("tools");
                }
            }
            // Retry the connection; if we connect successfully but the stream
            // drops mid-token, that's a separate (harder) failure mode. Raced
            // against the interrupt flag like the probe above.
            let sresp = match cancellable(
                cancel,
                with_backoff_notify(
                    &retry,
                    || async {
                        stream_client
                            .post(&chat_url)
                            .json(&body_stream)
                            .send()
                            .await
                            .map_err(|e| {
                                // Typed classification at the source (W0 #1511).
                                anyhow::Error::new(observability::DispatchError::from_reqwest(
                                    "stream request failed",
                                    e,
                                ))
                            })
                    },
                    |attempt, delay| print_retry_indicator(attempt, delay, color),
                ),
            )
            .await
            {
                Some(r) => r?,
                None => return Ok((String::new(), false, accumulated_usage, hallucination_count)),
            };

            if !sresp.status().is_success() {
                if debug {
                    print_debug("stream request non-2xx — using probe content", color);
                }
                maybe_nudge_no_tool_content!(probe_content.as_str(), None);
                // Phase 20 §2.2: the probe round produced usable content —
                // quality gate met, report it before returning.
                if !probe_content.is_empty() {
                    emit_accepted(
                        &mut on_round_usage,
                        round_usage,
                        truncation_suspect,
                        round_est_raw,
                    );
                }
                if probe_content.is_empty() {
                    if let Some(slot) = &mut end_reason {
                        **slot = Some(crate::TurnEndReason::Empty);
                    }
                }
                return Ok((probe_content, false, accumulated_usage, hallucination_count));
            }
            // Cargo-style reasoning spinner: TTY-gated (`color`) and opt-out via
            // `[tui] thinking = "off"`. Never in a pipe / `newt worker`.
            let show_thinking = color && thinking_stream_enabled();
            // #528: models that stream a lone-leading `</think>` (Nemotron et al.)
            // need the filter to start inside the reasoning block so the closer
            // and the reasoning it follows don't leak into the reply.
            let leading_reasoning = crate::reasoning::emits_leading_reasoning(model);
            // Step 25.4 (#568): `markdown` is now resolved by the caller
            // (`[tui].markdown` ∧ `/markdown` override ∧ color) and read off the
            // ctx above — no longer hardcoded to `color`.
            let (streamed, stream_usage) = match stream_response(
                sresp,
                color,
                show_thinking,
                leading_reasoning,
                cancel,
                markdown,
            )
            .await
            {
                Ok(v) => v,
                Err(e) => {
                    // #640: the stream connected (2xx) but the BODY broke
                    // mid-response — the backend dropped/truncated the stream, or
                    // an idle gap exceeded the read timeout. `stream_response`'s
                    // only fallible step is the `resp.chunk()` body read, so any
                    // error here IS a mid-stream break; left to `?` it surfaces as
                    // an opaque "error decoding response body" and ends the whole
                    // turn. It is recoverable, not fatal: the `stream:false` probe
                    // above already produced the full answer in `probe_content`.
                    // Warn and fall back to it — the SAME recovery as the non-2xx
                    // path above. (Tiers 2+ of the ladder — retry / shrink /
                    // fallback-model / prompt-and-save-preference — are #640.)
                    print_harness_notice(
                        &format!(
                            "stream broke mid-response ({e}) — recovered the answer \
                             from the non-streamed probe"
                        ),
                        color,
                    );
                    if !probe_content.is_empty() {
                        maybe_nudge_no_tool_content!(probe_content.as_str(), None);
                        emit_accepted(
                            &mut on_round_usage,
                            round_usage,
                            truncation_suspect,
                            round_est_raw,
                        );
                    }
                    if probe_content.is_empty() {
                        if let Some(slot) = &mut end_reason {
                            **slot = Some(crate::TurnEndReason::Empty);
                        }
                    }
                    return Ok((probe_content, false, accumulated_usage, hallucination_count));
                }
            };

            if streamed.is_empty() {
                // The streaming re-issue produced no tokens. Fall back to the
                // probe content rather than returning silence.
                if debug {
                    print_debug(
                        &format!(
                            "stream returned empty — falling back to probe content ({} chars)",
                            probe_content.len()
                        ),
                        color,
                    );
                }
                if probe_content.is_empty() {
                    let merged = merge_round_usage(accumulated_usage, stream_usage);
                    let empty_round_usage = merge_round_usage(round_usage, stream_usage);
                    let generated_unusable_output = empty_round_usage
                        .as_ref()
                        .map(|u| u.output_tokens > 0)
                        .unwrap_or(false);

                    if generated_unusable_output
                        && suspicious_empty_retries < SUSPICIOUS_EMPTY_RETRY_CAP
                    {
                        if trace {
                            print_trace(&ollama_response_shape(&json), color);
                        }
                        if debug {
                            let fields = ollama_non_content_fields(&json);
                            let field_note = if fields.is_empty() {
                                "no known non-content fields".to_string()
                            } else {
                                format!("non-content fields: {}", fields.join(", "))
                            };
                            print_debug(
                                &format!(
                                    "empty assistant content with generated tokens — retrying ({}/{SUSPICIOUS_EMPTY_RETRY_CAP}; {field_note})",
                                    suspicious_empty_retries + 1
                                ),
                                color,
                            );
                        }
                        // Phase 20 §2.2: empty content carrying non-content
                        // fields is the thinking-only quirk — report it at
                        // detection (at most once per turn) so the prompt-
                        // inflating corrective retry isn't re-learned from
                        // scratch every session.
                        if !thinking_only_reported && !ollama_non_content_fields(&json).is_empty() {
                            thinking_only_reported = true;
                            if let Some(hook) = on_round_usage.as_deref_mut() {
                                hook(RoundObservation::ThinkingOnly);
                            }
                        }
                        messages.push(serde_json::json!({
                            "role": "user",
                            "content": suspicious_empty_retry_nudge(suspicious_empty_retries, &json)
                        }));
                        accumulated_usage = merged;
                        suspicious_empty_retries += 1;
                        continue 'round_loop;
                    }

                    // Both probe and stream are empty — likely context overflow.
                    // `input_tokens` is the largest single prompt evaluated this
                    // turn (Step 18.1), so the 85%-of-safe-context check now
                    // compares one real prompt against the window instead of a
                    // multi-round sum that inflated past it after ~2 rounds.
                    let overflow_likely = merged
                        .as_ref()
                        .zip(safe_context)
                        .map(|(u, safe)| u.input_tokens >= safe * 85 / 100)
                        .unwrap_or(false);
                    if overflow_likely && overflow_retries < 2 {
                        emit_overflow_notice(
                            color,
                            merged.as_ref(),
                            safe_context,
                            model,
                            overflow_retries + 1,
                        );
                        // Compress toward 3/4 of the safe window — comfortably
                        // under the 85% trigger (was a blunt count trim before
                        // Step 18.4). The retry happens regardless: it is
                        // already bounded by `overflow_retries`. The target
                        // arithmetic stays in real-token space (`safe_context`
                        // and the schema overhead are real-token figures),
                        // then converts once into the pipeline's chars/4
                        // currency (Phase 20 §2.3).
                        let target = calibrate_down(
                            safe_context
                                .map(|s| (s as usize).saturating_mul(3) / 4)
                                .unwrap_or(0)
                                .saturating_sub(tool_tokens_real),
                            cal,
                        );
                        let outcome = compress(
                            CompressRequest {
                                messages: &messages,
                                budget: target,
                                max_messages: None,
                                replay_protected_tail_len: 0,
                                task: active_task,
                                hard_budget: true,
                                // A suspected silent overflow is a real failure
                                // signal — refuse semantics apply (Step 20.3).
                                authoritative: true,
                                focus: None,
                                est: estimation,
                                summary_input_cap_floor_chars,
                                compaction_store,
                            },
                            summarizer,
                            compress_state,
                        )
                        .await;
                        if let Some(notice) = outcome.notice {
                            print_harness_notice(&notice, color);
                        }
                        if outcome.fired {
                            messages = outcome.messages;
                            prompt_tracker.invalidate();
                            apply_post_compaction_continuation(
                                &mut messages,
                                &mut narration_nudges,
                                outcome.action,
                                step_ledger,
                                prompt_context,
                                round > 0,
                                action_nudges,
                            );
                            record_compaction_artifact(
                                artifact_sink,
                                artifact_context,
                                outcome.action,
                                outcome.tokens_before,
                                outcome.tokens_after,
                                target,
                                round,
                                "silent_overflow_recovery",
                                None,
                                false,
                                color,
                            );
                        } else {
                            // N1: the retry must differ from the request that
                            // just returned empty — when compress was a no-op
                            // (Fit / nothing reclaimable), fall back to one
                            // structural prune with a tight protected tail.
                            let fallback = crate::prune::prune(
                                &messages,
                                &crate::prune::PruneConfig {
                                    keep_last: 2,
                                    ..Default::default()
                                },
                            );
                            if fallback.chars_reclaimed > 0 {
                                let tokens_before = estimate_tokens(&messages, estimation);
                                let tokens_after = estimate_tokens(&fallback.messages, estimation);
                                messages = fallback.messages;
                                prompt_tracker.invalidate();
                                record_compaction_artifact(
                                    artifact_sink,
                                    artifact_context,
                                    CompressAction::Pruned,
                                    tokens_before,
                                    tokens_after,
                                    target,
                                    round,
                                    "silent_overflow_structural_fallback",
                                    None,
                                    false,
                                    color,
                                );
                            }
                        }
                        accumulated_usage = merged;
                        overflow_retries += 1;
                        continue 'round_loop;
                    }
                    // Phase 20 §2.2: persistent empties past the retry budget
                    // at ≥85% of the safe window are silent-overflow evidence
                    // — reported at the exit, with the merged prompt figure,
                    // before either return below.
                    if overflow_likely {
                        if let (Some(hook), Some(u)) =
                            (on_round_usage.as_deref_mut(), merged.as_ref())
                        {
                            hook(RoundObservation::SuspectedOverflow {
                                prompt_tokens: u.input_tokens,
                            });
                        }
                    }
                    if generated_unusable_output {
                        if trace {
                            print_trace(&ollama_response_shape(&json), color);
                        }
                        // Phase 20 §2.2: the diagnostic exit is also a
                        // thinking-only detection site (at most once per
                        // turn; the function returns right after, so the
                        // turn-local flag needs no update here).
                        if !thinking_only_reported && !ollama_non_content_fields(&json).is_empty() {
                            if let Some(hook) = on_round_usage.as_deref_mut() {
                                hook(RoundObservation::ThinkingOnly);
                            }
                        }
                        if let Some(slot) = &mut end_reason {
                            **slot = Some(crate::TurnEndReason::Empty);
                        }
                        return Ok((
                            suspicious_empty_ollama_diagnostic(&json),
                            false,
                            merged,
                            hallucination_count,
                        ));
                    }
                    let msg = "(model returned an empty response — try rephrasing, or check the model with `newt doctor`)";
                    if let Some(slot) = &mut end_reason {
                        **slot = Some(crate::TurnEndReason::Empty);
                    }
                    return Ok((msg.to_string(), false, merged, hallucination_count));
                }
                // Use probe content; print it since it was never streamed.
                maybe_nudge_no_tool_content!(probe_content.as_str(), stream_usage);
                // Phase 20 §2.2: non-empty probe content is usable output.
                emit_accepted(
                    &mut on_round_usage,
                    round_usage,
                    truncation_suspect,
                    round_est_raw,
                );
                return Ok((
                    probe_content,
                    false,
                    merge_round_usage(accumulated_usage, stream_usage),
                    hallucination_count,
                ));
            }

            // Narrate-then-stop rescue: the model produced prose and no tool
            // call. If it has already acted this turn (mid-task) or the prose
            // reads as intent-to-act, nudge it to actually call the tool and run
            // another round instead of ending the turn — what a human "continue"
            // does. Bounded by the configured narration_nudge_cap and the round budget so a
            // chronic narrator can't loop; after the cap the prose is accepted
            // as the final answer (the return below). A genuine from-the-start
            // final answer (no prior call, no intent cue) is never nudged.
            maybe_nudge_no_tool_content!(streamed.as_str(), stream_usage);
            // Phase 20 §2.2: a non-empty streamed answer is usable output.
            emit_accepted(
                &mut on_round_usage,
                round_usage,
                truncation_suspect,
                round_est_raw,
            );
            return Ok((
                streamed,
                true,
                merge_round_usage(accumulated_usage, stream_usage),
                hallucination_count,
            ));
        }

        // Has tool calls — add assistant turn and execute them.
        // Phase 20 §2.2: tool calls are usable output — the dispatched prompt
        // is proven accepted regardless of how the turn later ends.
        emit_accepted(
            &mut on_round_usage,
            round_usage,
            truncation_suspect,
            round_est_raw,
        );
        messages.push(message.clone());
        let mut round_wrote = false;
        let mut round_modified_workspace = false;
        let mut round_progress = false;
        let tcs = tool_calls.unwrap();
        // Phase 1 (invariant #3, BATCH level): validate the ENTIRE batch before
        // any side effect. This Ollama/Anthropic-native wire carries no per-call
        // ids, so ids are not required — but a malformed sibling still rejects the
        // WHOLE batch: echo the reason for every call and execute nothing, so no
        // valid call mutates the workspace ahead of an unvalidated batch.
        let extracted: Vec<(Option<&str>, Option<&str>, &serde_json::Value)> = tcs
            .iter()
            .map(|tc| {
                if tc["function"].is_null() {
                    (None, tc["name"].as_str(), &tc["input"])
                } else {
                    (
                        None,
                        tc["function"]["name"].as_str(),
                        &tc["function"]["arguments"],
                    )
                }
            })
            .collect();
        let validated = match tools::validate_tool_call_batch(&extracted, false) {
            Ok(v) => Some(v),
            Err(reason) => {
                for _tc in tcs {
                    print_synthetic_tool_result(
                        "(rejected tool-call batch)",
                        &serde_json::Value::Null,
                        workspace,
                        &reason,
                        color,
                    );
                    if let Some(rec) = tool_events.as_deref_mut() {
                        rec.push(crate::ToolEvent::from_call(
                            "(rejected tool-call batch)",
                            &serde_json::Value::Null,
                            false,
                            Some(0),
                        ));
                    }
                    messages.push(serde_json::json!({
                        "role": "tool",
                        "content": format!("tool-call batch rejected before execution: {reason}"),
                    }));
                }
                None
            }
        };
        // Phase 2: every call in the batch is valid — execute in order. `flatten`
        // yields nothing (so this runs zero tools) when the batch was rejected.
        for (_tc, vc) in tcs.iter().zip(validated.iter().flatten()) {
            let name = vc.name.as_str();
            let args = vc.args.clone();
            if is_hallucination(name, &args) {
                hallucination_count += 1;
            }
            // Step 27.3/#771: short-circuit selected exact repeats — steer
            // instead of re-executing a dead or already-useful call. The bogus
            // emission is still counted above; we just don't run it again.
            if let Some(steer) = repeat_calls.repeat_steer(name, &args) {
                print_synthetic_tool_result(name, &args, workspace, &steer, color);
                if let Some(rec) = tool_events.as_deref_mut() {
                    rec.push(crate::ToolEvent::from_call(name, &args, false, Some(0)));
                }
                messages.push(serde_json::json!({ "role": "tool", "content": steer }));
                continue;
            }
            if !is_read_only_call(name, &args) {
                round_wrote = true;
            }
            // Organic save_note use resets the memory-nudge counter (the
            // read-only-rounds reset pattern) — active curators never see it.
            if name == "save_note" && note_sink.is_some() {
                if let Some(n) = note_nudge.as_deref_mut() {
                    n.note_saved();
                }
            }
            // retry technique: snapshot the file's pre-write bytes before the
            // write tool runs, so the post-turn gate can revert exactly newt's writes.
            ledger_note_write(write_ledger, name, &args, workspace);
            let tool_t0 = std::time::Instant::now();
            // #727: intercept the read-only budget self-read here. Its answer is
            // dynamic per-turn loop state — the num_ctx input ceiling and the
            // conversation's token estimate — which are in scope in the loop, not
            // inside execute_tool. `prompt_tracker.current` is real-token currency,
            // the same the ceiling is in. The rendered string then flows through
            // all the normal bookkeeping below (ok, tool_events, phantom_reaches,
            // spill), so aliases recorded as Rewrites stay correct.
            let result = if tools::is_context_remaining_call(name) {
                let report = budget::render_context_budget(
                    prompt_tracker.current(&messages, Some(&tools), cal, estimation),
                    effective_input_ceiling,
                    num_ctx,
                    input_ceiling_pct,
                    low_budget_pct,
                );
                print_synthetic_tool_result(name, &args, workspace, &report, color);
                report
            } else {
                // #297 follow-up: race the tool dispatch against the turn's
                // cancel flag. A mid-tool interrupt (Esc / Ctrl-C) now drops the
                // in-flight future here instead of waiting for the tool to
                // return — and for exec that dropped future triggers
                // `kill_on_drop` on the child *tree*, so a hung `run_command`
                // dies the instant the user asks for it rather than at the
                // host-exec timeout ceiling. `is_cancelled` between rounds still
                // catches the abandoned turn; this closes the *during-a-tool*
                // window that a foreground child on the tty used to wedge.
                let Some(result) = tools::execute_tool_with_collaborators(
                    name,
                    &args,
                    workspace,
                    color,
                    tool_output_lines,
                    caveats,
                    mcp,
                    tools::ToolCollaborators {
                        build_check_cmd: build_check_cmd.as_deref(),
                        // Reborrow + re-coerce: shortens the trait-object
                        // lifetime to this call (Option<&mut dyn _> is
                        // invariant, so the longer ChatCtx lifetime can't
                        // unify directly).
                        note_sink: note_sink
                            .as_deref_mut()
                            .map(|s| &mut *s as &mut dyn NoteSink),
                        recall_source,
                        memory_source,
                        prompt_context: Some(prompt_context),
                        artifact_context,
                        artifact_sink,
                        // #263 prompted grants — same reborrow pattern.
                        permission_gate: permission_gate
                            .as_deref_mut()
                            .map(|g| &mut *g as &mut dyn PermissionGate),
                        exec_floor,
                        git_tool,
                        crew_runner,
                        scratchpad_store,
                        code_search,
                        where_is,
                        nav,
                        experience_store,
                        step_ledger,
                        operating_mode_control,
                        plan_mode_control,
                        spill_store,
                        persona_tools,
                        live_tool_output: live_tool_output.clone(),
                    },
                    tool_offload,
                    prompt_disposition,
                    cancel,
                )
                .await
                else {
                    return Ok((String::new(), false, accumulated_usage, hallucination_count));
                };
                result
            };
            // 17.6: record the call for the turn's events column — args are
            // digested (never stored raw), duration is a display claim.
            // Step 27.3/#771: classify once; remember outcomes that should make
            // an exact repeat self-correct next round.
            let ok = tools::tool_result_ok(&result);
            if ok && is_workspace_write_call(name) {
                round_modified_workspace = true;
            }
            if ok && meaningful_workflow_progress(name, &result) {
                round_progress = true;
            }
            repeat_calls.record(name, &args, ok, &result);
            if workflow_runtime.record_tool_result(&result) {
                round_progress = true;
            }
            if let Some(rec) = tool_events.as_deref_mut() {
                rec.push(crate::ToolEvent::from_call(
                    name,
                    &args,
                    ok,
                    u64::try_from(tool_t0.elapsed().as_millis()).ok(),
                ));
            }
            // #717: record any phantom/capability reach (alias / hallucination
            // / real-tool empty miss) for the alias-seam telemetry. #479 (G4)
            // composes the gated-off seam here, where `advertise_team` is known:
            // a `crew`/`compose_roster` reach with the surface OFF is a real name
            // (so `classify_phantom_reach` never flags it) but exactly the
            // delegation signal we want to mine for the common OFF default.
            if let Some(pr) = phantom_reaches.as_deref_mut() {
                if let Some(resolution) = tools::classify_phantom_reach(name, &args, &result, ok)
                    .or_else(|| tools::classify_gated_off_reach(name, advertise_team))
                {
                    pr.push(crate::PhantomReach {
                        name_as_called: name.to_string(),
                        resolution,
                        active_context_features: Vec::new(),
                    });
                }
            }
            // #867 Part A: ledger the verified paths this result surfaced
            // BEFORE the offload may spill the text out of the transcript.
            observed_paths.record(&result, &observed_resolver);
            messages.push(serde_json::json!({
                "role": "tool",
                // Step 26.3 (#584): offload an oversized result (redact → spill →
                // teaser+handle) when tool_offload is on; unchanged otherwise.
                "content": maybe_offload_tool_result(name, result, tool_offload, spill_store)
            }));
        }
        if round_wrote {
            read_only_rounds = 0;
        } else {
            read_only_rounds = read_only_rounds.saturating_add(1);
        }
        workflow_runtime.record_round_outcome(round_modified_workspace, round_progress);
    }

    // Reached the round cap. Trim the bloated message list so the final
    // summary request doesn't overflow the model's context window, then
    // make ONE tools-disabled completion so the user gets a real partial answer.
    let protected_head = protected_prompt_head_len(&messages, prompt_read::ACTIVE_PROMPT_PREFIX);
    let trimmed = trim_for_summary(&messages, protected_head, 6);
    // Step 27.5: salvage the plan/state ledger + the failed-call count so the
    // summary reflects progress and the fallback advice is honest.
    let progress = cap_exit_progress(step_ledger, scratchpad_store);
    let (text, streamed, usage) = final_summary_ollama(
        &client,
        &chat_url,
        model,
        trimmed,
        CapExit {
            max_tool_rounds,
            accumulated: accumulated_usage,
            wasted_calls: repeat_calls.total_failures(),
            progress,
            observed: observed_paths.into_vec(),
            request_budget: authoritative_request_budget(
                send_budget,
                send_budget_authoritative,
                mid_loop_trim_tokens,
            ),
            calibration: cal,
            estimation,
            ollama_num_ctx: num_ctx,
        },
    )
    .await?;
    // #867: the evidence for this summary was just trimmed away, which is
    // exactly when a model fabricates plausible file paths — verify every
    // cited path against the workspace and append a visible refutation for
    // any that don't exist. Appends only; the model's prose is never edited.
    let text = claim_check::annotate_against_workspace(text, workspace);
    // #1214: the sibling check for claimed ACTIONS (commits, branches, pushes,
    // passing tests) — refuted against the workspace's real git state across
    // this turn. Fail-quiet off-repo (no evidence, no annotation).
    let text = claim_check::annotate_action_claims(
        text,
        claim_check::collect_git_evidence(workspace, turn_start_head.as_deref()).as_ref(),
    );
    if let Some(slot) = &mut end_reason {
        **slot = Some(crate::TurnEndReason::RoundCap);
    }
    Ok((text, streamed, usage, hallucination_count))
}

/// Returns `true` when `name` is a tool that doesn't modify the workspace.
/// Used to count consecutive read-only rounds and inject a write-nudge.
/// `save_note` writes *memory*, not the workspace — a round that only saved
/// a note must not suppress the stop-exploring-start-writing nudge; `recall`
/// (17.5) reads past conversations and is likewise pure exploration.
/// First line of a tool result, capped — used as the remembered error reason in
/// [`RepeatCallGuard`] so the steering message is short.
fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").chars().take(200).collect()
}

/// Per-run guard against a weak model looping on a tool call whose result should
/// already be actionable. Step 27.3 covered failures: the forensic session showed
/// the model re-issuing the *identical* failed `run_command` three times and
/// re-reading the same file ~8×, burning rounds. Later field reports added
/// success-shaped loops: no-result probes (`recall`, `state_get`), successful
/// `web_fetch` calls with real content, and successful read-only shell probes.
///
/// Keyed by `(name, canonical args)`, it short-circuits selected exact repeats
/// with steering instead of re-executing them. The classifier sees every call
/// outcome, but most successes deliberately stay repeatable; only failures,
/// success-shaped no-results, successful `web_fetch`, and successful read-only
/// shell probes are memoized. It also counts failures per tool name so the steer
/// can escalate ("stop using `run_command` — it keeps failing this session; use
/// the embedded tools"). This handles ANY persistently-failing tool — a dead
/// shell, a denied command, an unimplemented op — without needing to know *why*
/// it fails (shell availability is a build/config property with no clean runtime
/// signal; see `tools::ocap_disabled` docs).
#[derive(Debug, Clone, PartialEq, Eq)]
enum RepeatMemo {
    Failure {
        first_line: String,
    },
    NoResult {
        reason: String,
    },
    EvidenceObserved {
        subject: String,
        advice: &'static str,
    },
}

#[derive(Default)]
struct RepeatCallGuard {
    /// `(name + canonical args)` → the prior outcome that should steer an exact repeat.
    repeat_memos: std::collections::HashMap<String, RepeatMemo>,
    /// `name` → how many times it has failed this run (any args).
    fails_by_tool: std::collections::HashMap<String, usize>,
}

impl RepeatCallGuard {
    /// How many consecutive failures of one tool before the steer escalates to
    /// "stop using it".
    const ESCALATE_AFTER: usize = 2;

    fn key(name: &str, args: &serde_json::Value) -> String {
        // The model emits byte-identical args when it loops (confirmed by the
        // identical forensic args digests), so the compact JSON is a stable key.
        format!("{name}\u{1}{args}")
    }

    /// Steering message if this exact `(name, args)` already produced a memoized
    /// outcome that should not be repeated this run, else `None` (let it execute).
    fn repeat_steer(&self, name: &str, args: &serde_json::Value) -> Option<String> {
        let key = Self::key(name, args);
        match self.repeat_memos.get(&key)? {
            RepeatMemo::Failure { first_line: prev } => {
                let mut msg = format!(
                    "You already called `{name}` with these exact arguments and it failed: {prev}. \
                     Do NOT repeat the same call — use a different tool or different arguments."
                );
                if self.fails_by_tool.get(name).copied().unwrap_or(0) >= Self::ESCALATE_AFTER {
                    msg.push_str(&format!(
                        " `{name}` has failed repeatedly this session; stop using it and prefer \
                         the embedded tools (read_file, edit_file, write_file, find, git)."
                    ));
                }
                Some(msg)
            }
            RepeatMemo::NoResult { reason } => Some(format!(
                "You already ran `{name}` with these exact arguments this turn and {reason}. \
                 Don't repeat the identical call — create or update the missing state, change \
                 the arguments when the tool accepts them, or use a different tool."
            )),
            RepeatMemo::EvidenceObserved { subject, advice } => Some(format!(
                "You already observed {subject} with `{name}` and received output. Do NOT repeat \
                 the identical call — {advice}"
            )),
        }
    }

    fn successful_fetch_url(name: &str, args: &serde_json::Value, result: &str) -> Option<String> {
        if name != "web_fetch" {
            return None;
        }
        let url = args.get("url")?.as_str()?.trim();
        if url.is_empty() || result.trim().is_empty() {
            None
        } else {
            Some(url.chars().take(200).collect())
        }
    }

    fn successful_read_only_shell_command(
        name: &str,
        args: &serde_json::Value,
        result: &str,
    ) -> Option<String> {
        if name != "run_command" || result.trim().is_empty() {
            return None;
        }
        let command = args.get("command")?.as_str()?.trim();
        if is_read_only_shell_probe(command) {
            Some(command.chars().take(200).collect())
        } else {
            None
        }
    }

    /// #718: classify a SUCCESS-shaped result that is empty *by design* — it
    /// passes `tool_result_ok` (ok=true) so the failure path never sees it, yet
    /// the model loops the identical call. Pure; keyed on the exact result
    /// prefixes (`recall.rs` keeps "no matches in past conversations";
    /// `scratchpad.rs` returns "no such key: ..."). `None` when the result
    /// carries real content (nothing to steer).
    fn no_result_reason(name: &str, result: &str) -> Option<&'static str> {
        match name {
            "recall" if result.starts_with("no matches in past conversations") => Some(
                "it returned no matches (and recall cannot see the current conversation \
                 — use resume_context for THIS conversation)",
            ),
            "state_get" if result.starts_with("no such key") => {
                Some("the key is not set (state_get returned \"no such key\")")
            }
            "plan_get" if result.starts_with("no active plan") => Some(
                "it found no active plan; call update_plan now with a short ordered plan if the \
                 work has more than one step",
            ),
            _ => None,
        }
    }

    /// Classify a just-executed call into the subset of outcomes that should
    /// steer an exact repeat. This function sees all calls, but deliberately
    /// returns `None` for ordinary successes so valid repeated work (builds,
    /// tests, rereads after edits, write-capable commands) can keep running.
    fn classify_repeat_memo(
        name: &str,
        args: &serde_json::Value,
        ok: bool,
        result: &str,
    ) -> Option<RepeatMemo> {
        if !ok {
            return Some(RepeatMemo::Failure {
                first_line: first_line(result),
            });
        }
        if let Some(reason) = Self::no_result_reason(name, result) {
            return Some(RepeatMemo::NoResult {
                reason: reason.to_string(),
            });
        }
        if let Some(url) = Self::successful_fetch_url(name, args, result) {
            return Some(RepeatMemo::EvidenceObserved {
                subject: format!("`{url}`"),
                advice: "use the fetched content above, fetch a different URL, inspect local \
                         files, or answer the user.",
            });
        }
        if let Some(command) = Self::successful_read_only_shell_command(name, args, result) {
            return Some(RepeatMemo::EvidenceObserved {
                subject: format!("read-only shell probe `{command}`"),
                advice: "use the observed output above, change the query, inspect a different \
                         file, or make the next edit/test decision.",
            });
        }
        None
    }

    /// Record a just-executed call's outcome. Failures are also counted so the
    /// steer can escalate; success-shaped memos are not counted because they are
    /// not hard failures.
    fn record(&mut self, name: &str, args: &serde_json::Value, ok: bool, result: &str) {
        if !ok {
            *self.fails_by_tool.entry(name.to_string()).or_default() += 1;
        }
        if let Some(memo) = Self::classify_repeat_memo(name, args, ok, result) {
            self.repeat_memos.insert(Self::key(name, args), memo);
        }
    }

    /// Total failed tool executions this run (across all tools) — a signal that
    /// a cap exit was thrash, not lack of rounds (Step 27.5).
    fn total_failures(&self) -> usize {
        self.fails_by_tool.values().sum()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkflowErrorEvidence {
    fingerprint: String,
    observations: usize,
}

#[derive(Debug, Default)]
struct WorkflowRuntimeState {
    error_evidence: Option<WorkflowErrorEvidence>,
    read_only_rounds_after_evidence: usize,
    writes_after_evidence: usize,
    rounds_since_progress: Option<usize>,
    /// Override for [`WORKFLOW_RECENT_PROGRESS_ROUNDS`], set once per turn from
    /// the matching [`crate::WorkflowSteerer`] workflow's
    /// `progress_horizon_rounds` (diagnostic workflows legitimately need more
    /// read-only rounds between plan/edit checkpoints than routine edits do —
    /// see `diagnose_failure.toml`). `None` uses the shared default.
    progress_horizon_rounds: Option<usize>,
    step_lock_nudges: usize,
    rediscovery_nudges: usize,
    /// How hard to push the model from reading to acting (#tenacity). Set once
    /// per turn from [`crate::tenacity::effective_tenacity`]; `Default` is
    /// `Standard`, the behaviour-preserving level.
    tenacity: crate::tenacity::Tenacity,
    /// Consecutive tool rounds this turn that modified nothing in the workspace.
    /// Reset on any workspace-write round; drives the tenacity action-forcing
    /// nudge.
    consecutive_read_only_rounds: usize,
}

impl WorkflowRuntimeState {
    const STEP_LOCK_NUDGE_CAP: usize = 3;
    const REDISCOVERY_NUDGE_CAP: usize = 2;

    /// Set once per turn from the matching workflow's horizon override, if
    /// any. A no-op call (`None`) leaves the shared default in effect.
    fn set_progress_horizon(&mut self, rounds: Option<usize>) {
        self.progress_horizon_rounds = rounds;
    }

    fn progress_horizon(&self) -> usize {
        self.progress_horizon_rounds
            .unwrap_or(WORKFLOW_RECENT_PROGRESS_ROUNDS)
    }

    fn record_tool_result(&mut self, result: &str) -> bool {
        let Some(fingerprint) = workflow_error_fingerprint(result) else {
            return false;
        };
        match self.error_evidence.as_mut() {
            Some(evidence) if evidence.fingerprint == fingerprint => {
                evidence.observations = evidence.observations.saturating_add(1);
                false
            }
            _ => {
                self.error_evidence = Some(WorkflowErrorEvidence {
                    fingerprint,
                    observations: 1,
                });
                self.read_only_rounds_after_evidence = 0;
                self.writes_after_evidence = 0;
                self.step_lock_nudges = 0;
                self.rediscovery_nudges = 0;
                true
            }
        }
    }

    fn record_round_outcome(&mut self, round_wrote: bool, round_progress: bool) {
        // Tenacity counter (#tenacity): consecutive rounds that changed nothing
        // in the workspace. Unconditional — independent of the error-evidence
        // workflow below — so it drives action-forcing on ANY task, not only
        // diagnosed failures.
        if round_wrote {
            self.consecutive_read_only_rounds = 0;
        } else {
            self.consecutive_read_only_rounds = self.consecutive_read_only_rounds.saturating_add(1);
        }
        if round_progress {
            self.rounds_since_progress = Some(0);
        } else if let Some(rounds) = self.rounds_since_progress.as_mut() {
            *rounds = rounds.saturating_add(1);
        }
        if self.error_evidence.is_none() {
            return;
        }
        if round_wrote {
            self.writes_after_evidence = self.writes_after_evidence.saturating_add(1);
            self.read_only_rounds_after_evidence = 0;
        } else {
            self.read_only_rounds_after_evidence =
                self.read_only_rounds_after_evidence.saturating_add(1);
        }
    }

    fn round_start_nudge(
        &mut self,
        step_ledger: Option<&dyn scheduled::StepLedger>,
    ) -> Option<String> {
        let evidence = self.error_evidence.as_ref()?;
        if self.writes_after_evidence > 0 || self.read_only_rounds_after_evidence == 0 {
            return None;
        }
        if self.step_lock_nudges >= Self::STEP_LOCK_NUDGE_CAP {
            return None;
        }
        self.step_lock_nudges += 1;
        Some(workflow_step_lock_nudge(
            &evidence.fingerprint,
            evidence.observations,
            active_step_description(step_ledger).as_deref(),
        ))
    }

    /// Tenacity action-forcing nudge (#tenacity): once the model has spent the
    /// tenacity level's budget of consecutive read-only rounds without touching
    /// the workspace, inject the standard "stop exploring, make the change" nudge
    /// and reset the counter so it re-accumulates before firing again. This is
    /// the answer to the measured ceiling where a capable model reads/plans for
    /// its whole budget and never edits. `Standard` (budget 3) reproduces the
    /// historical Ollama-loop threshold; higher tenacity forces sooner.
    fn action_forcing_nudge(
        &mut self,
        remaining_rounds: usize,
        step_ledger: Option<&dyn scheduled::StepLedger>,
        delegate_hint: Option<&str>,
    ) -> Option<String> {
        if self.consecutive_read_only_rounds < self.tenacity.read_only_nudge_after() {
            return None;
        }
        let nudge = read_only_action_nudge(
            self.consecutive_read_only_rounds,
            remaining_rounds,
            step_ledger,
            delegate_hint,
        );
        self.consecutive_read_only_rounds = 0;
        Some(nudge)
    }

    fn rediscovery_nudge(
        &mut self,
        classification: Option<&crate::NudgeClassification>,
        content: &str,
        step_ledger: Option<&dyn scheduled::StepLedger>,
    ) -> Option<String> {
        let evidence = self.error_evidence.as_ref()?;
        if self.writes_after_evidence > 0 {
            return None;
        }
        if self.rediscovery_nudges >= Self::REDISCOVERY_NUDGE_CAP {
            return None;
        }
        let classified_stall = classification.is_some_and(|c| {
            matches!(
                c.class,
                crate::NudgeClass::PendingAction | crate::NudgeClass::PlanUpdate
            )
        });
        if !classified_stall && !looks_like_error_rediscovery(content) {
            return None;
        }
        self.rediscovery_nudges += 1;
        Some(workflow_rediscovery_nudge(
            &evidence.fingerprint,
            active_step_description(step_ledger).as_deref(),
        ))
    }

    fn cap_grace_nudge(
        &mut self,
        step_ledger: Option<&dyn scheduled::StepLedger>,
        max_tool_rounds: usize,
        workflow_grace_rounds: usize,
    ) -> Option<String> {
        if workflow_grace_rounds == 0 {
            return None;
        }
        let active_step = active_step_description(step_ledger);
        let recent_progress = self
            .rounds_since_progress
            .is_some_and(|rounds| rounds <= self.progress_horizon());
        if let Some(evidence) = self.error_evidence.as_ref() {
            if self.writes_after_evidence > 0 {
                return Some(workflow_post_write_grace_nudge(
                    &evidence.fingerprint,
                    active_step.as_deref(),
                    max_tool_rounds,
                    workflow_grace_rounds,
                ));
            }
            if self.read_only_rounds_after_evidence > 0 || recent_progress {
                return Some(workflow_cap_grace_nudge(
                    &evidence.fingerprint,
                    active_step.as_deref(),
                    max_tool_rounds,
                    workflow_grace_rounds,
                ));
            }
        }
        if active_step.is_some() && recent_progress {
            return Some(workflow_progress_grace_nudge(
                active_step.as_deref(),
                max_tool_rounds,
                workflow_grace_rounds,
            ));
        }
        None
    }
}

fn workflow_error_fingerprint(result: &str) -> Option<String> {
    build_error_fingerprint(result).or_else(|| edit_miss_fingerprint(result))
}

fn build_error_fingerprint(result: &str) -> Option<String> {
    let mut pending_error: Option<String> = None;
    let mut fingerprints = Vec::new();
    for line in result.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("error[") || trimmed.starts_with("error:") {
            pending_error = Some(normalize_error_line(trimmed));
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("-->") {
            if let Some(error) = pending_error.take() {
                let location = rest
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .trim()
                    .trim_start_matches("./");
                if location.is_empty() {
                    fingerprints.push(error);
                } else {
                    fingerprints.push(format!("{location} {error}"));
                }
                if fingerprints.len() >= 3 {
                    break;
                }
            }
        }
    }
    if fingerprints.is_empty() {
        pending_error.map(|e| e.chars().take(240).collect())
    } else {
        Some(fingerprints.join(" | ").chars().take(500).collect())
    }
}

fn edit_miss_fingerprint(result: &str) -> Option<String> {
    let lc = result.to_ascii_lowercase();
    if !(lc.contains("old_string")
        && (lc.contains("not found")
            || lc.contains("old string not found")
            || lc.contains("matches 0")
            || lc.contains("no match")))
    {
        return None;
    }
    let line = result
        .lines()
        .find(|line| {
            let l = line.to_ascii_lowercase();
            l.contains("old_string")
                && (l.contains("not found") || l.contains("matches 0") || l.contains("no match"))
        })
        .unwrap_or("edit_file old_string not found");
    Some(format!("edit_file {}", normalize_error_line(line)))
}

fn normalize_error_line(line: &str) -> String {
    let mut out = String::new();
    let mut in_ws = false;
    for c in line.chars() {
        if c.is_whitespace() {
            if !in_ws {
                out.push(' ');
                in_ws = true;
            }
        } else if c != '`' {
            out.push(c);
            in_ws = false;
        }
    }
    out.chars().take(240).collect()
}

fn active_step_description(step_ledger: Option<&dyn scheduled::StepLedger>) -> Option<String> {
    let snapshot = step_ledger?.snapshot();
    snapshot
        .steps
        .iter()
        .find(|step| step.status == StepStatus::Active)
        .or_else(|| {
            snapshot
                .steps
                .iter()
                .find(|step| step.status != StepStatus::Done)
        })
        .map(|step| step.description.clone())
}

fn workflow_step_lock_nudge(
    fingerprint: &str,
    observations: usize,
    active_step: Option<&str>,
) -> String {
    let active = active_step
        .map(|step| format!(" Active step: '{step}'."))
        .unwrap_or_default();
    format!(
        "<workflow_state>\nactive_step = \"repair the current tool/build error\"\nlast_error_fingerprint = \"{fingerprint}\"\nobservations = {observations}\nnext_allowed_actions = \"use the latest file evidence, then edit_file/write_file for the active repair, then run the focused verification\"\ndisallowed_actions = \"re-reading the same evidence, re-deriving the same plan, or restating findings without editing\"\n</workflow_state>\n{active} You already have the error evidence above. Do not re-read or summarize it again unless you need one exact replacement span. Make the smallest edit that addresses this exact fingerprint, then run the focused check."
    )
}

fn workflow_rediscovery_nudge(fingerprint: &str, active_step: Option<&str>) -> String {
    let active = active_step
        .map(|step| format!(" Active step: '{step}'."))
        .unwrap_or_default();
    format!(
        "You are rediscovering an error that is already recorded: {fingerprint}.{active} Do not restate findings, update the same plan, or claim handoff. Call the concrete edit tool for this repair now. After the edit, run one focused verification command and use its new output as ground truth."
    )
}

fn workflow_cap_grace_nudge(
    fingerprint: &str,
    active_step: Option<&str>,
    max_tool_rounds: usize,
    workflow_grace_rounds: usize,
) -> String {
    let active = active_step
        .map(|step| format!(" Active step: '{step}'."))
        .unwrap_or_default();
    format!(
        "<workflow_state>\nnormal_tool_round_cap = {max_tool_rounds}\nconfigured_workflow_grace_rounds = {workflow_grace_rounds}\nlast_error_fingerprint = \"{fingerprint}\"\nnext_allowed_actions = \"call edit_file or write_file now using the latest observed file contents; then run the focused verification\"\ndisallowed_actions = \"summary of findings, handoff, plan rediscovery, or another broad read-only pass\"\n</workflow_state>\nThe normal tool-call cap was reached immediately after repair evidence without a successful workspace edit.{active} This is a bounded grace window, not a final-answer round. Use the latest observed contents and call the concrete edit tool now. If one exact replacement span is still missing, read only that minimal span, then edit in the grace window."
    )
}

fn workflow_post_write_grace_nudge(
    fingerprint: &str,
    active_step: Option<&str>,
    max_tool_rounds: usize,
    workflow_grace_rounds: usize,
) -> String {
    let active = active_step
        .map(|step| format!(" Active step: '{step}'."))
        .unwrap_or_default();
    format!(
        "<workflow_state>\nnormal_tool_round_cap = {max_tool_rounds}\nconfigured_workflow_grace_rounds = {workflow_grace_rounds}\nlast_error_fingerprint = \"{fingerprint}\"\nnext_allowed_actions = \"run the focused verification for the edit you just made, or continue the active implementation step with one concrete tool call\"\ndisallowed_actions = \"summary of findings, handoff, or broad rediscovery\"\n</workflow_state>\nThe normal tool-call cap was reached immediately after a workspace edit related to recorded repair evidence.{active} This is a bounded verification window. Do not summarize or stop because of the normal cap; run the focused check or the next concrete implementation tool now."
    )
}

fn workflow_progress_grace_nudge(
    active_step: Option<&str>,
    max_tool_rounds: usize,
    workflow_grace_rounds: usize,
) -> String {
    let active = active_step
        .map(|step| format!(" Active step: '{step}'."))
        .unwrap_or_default();
    format!(
        "<workflow_state>\nnormal_tool_round_cap = {max_tool_rounds}\nconfigured_workflow_grace_rounds = {workflow_grace_rounds}\nnext_allowed_actions = \"continue the active workflow step with one concrete tool call, then update the plan or verify\"\ndisallowed_actions = \"summary of findings, handoff, or broad rediscovery\"\n</workflow_state>\nThe normal tool-call cap was reached while the active workflow was still making concrete progress.{active} This is a bounded grace window, not a final-answer round. Continue with the next concrete implementation or verification tool now."
    )
}

fn looks_like_error_rediscovery(content: &str) -> bool {
    let lc = content.to_ascii_lowercase();
    (lc.contains("summary of findings")
        || lc.contains("root cause")
        || lc.contains("current state")
        || lc.contains("remaining work")
        || lc.contains("build failure"))
        && (lc.contains("error") || lc.contains("build") || lc.contains("compile"))
}

/// Render the agent's working-memory progress (`<plan>` checklist + `<state>`)
/// at a cap exit, so partial work is salvaged into the final summary / fallback
/// instead of being lost (Step 27.5). `None` when both are empty.
fn cap_exit_progress(
    step_ledger: Option<&dyn scheduled::StepLedger>,
    scratchpad_store: Option<&dyn scratchpad::ScratchpadStore>,
) -> Option<String> {
    let plan = step_ledger.and_then(scheduled::plan_block);
    let state = scratchpad_store.and_then(scratchpad::scratchpad_state_block);
    let parts: Vec<String> = [plan, state].into_iter().flatten().collect();
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

fn is_read_only_tool(name: &str) -> bool {
    matches!(
        name,
        "list_dir"
            | "read_file"
            | "find"
            | "search"
            | "web_fetch"
            | "use_skill"
            | "save_note"
            | "recall"
            | "prompt_read"
            | "artifact_read"
    )
}

fn is_read_only_call(name: &str, args: &serde_json::Value) -> bool {
    is_read_only_tool(name)
        || (name == "run_command"
            && args
                .get("command")
                .and_then(|v| v.as_str())
                .is_some_and(is_read_only_shell_probe))
}

fn is_workspace_write_call(name: &str) -> bool {
    matches!(name, "write_file" | "edit_file")
}

fn maybe_offload_tool_result(
    name: &str,
    result: String,
    tool_offload: bool,
    spill_store: Option<&dyn spill::SpillStore>,
) -> String {
    if matches!(
        name,
        "run_command" | "lifecycle" | "prompt_read" | "artifact_read"
    ) {
        result
    } else {
        spill::maybe_offload(result, tool_offload, spill_store)
    }
}

fn meaningful_workflow_progress(name: &str, result: &str) -> bool {
    match name {
        "update_plan" => true,
        "write_file" => result.starts_with("wrote ") || result.starts_with("✓ wrote "),
        "edit_file" => edit_result_changed_file(result),
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)]
fn record_compaction_artifact(
    artifact_sink: Option<&dyn artifact_read::PromptArtifactSink>,
    artifact_context: Option<artifact_read::ArtifactReadContext<'_>>,
    action: CompressAction,
    tokens_before: usize,
    tokens_after: usize,
    budget: usize,
    round: usize,
    reason: &str,
    trigger: Option<&CompressTrigger>,
    send_budget_authoritative: bool,
    color: bool,
) {
    let (Some(sink), Some(context)) = (artifact_sink, artifact_context) else {
        return;
    };
    if let Err(error) = artifact_hooks::record_compaction_checkpoint(
        sink,
        context,
        action,
        tokens_before,
        tokens_after,
        budget,
        round,
        reason,
        trigger,
        send_budget_authoritative,
    ) {
        print_harness_notice(
            &format!("warning: failed to record compaction artifact: {error}"),
            color,
        );
    }
}

fn edit_result_changed_file(result: &str) -> bool {
    result.starts_with("edited ") || result.starts_with("✓ edited ")
}

fn is_read_only_shell_probe(command: &str) -> bool {
    let command = command.trim();
    if command.is_empty() {
        return false;
    }
    const SHELL_META: &[char] = &['&', '|', ';', '`', '$', '\n', '>', '<', '(', ')'];
    if command.contains(SHELL_META) {
        return false;
    }
    let mut tokens = command.split_ascii_whitespace();
    let Some(program) = tokens.next() else {
        return false;
    };
    match program {
        "grep" | "rg" | "head" | "tail" | "wc" | "pwd" => true,
        "sed" => !tokens.any(|t| t == "-i" || t.starts_with("-i")),
        _ => false,
    }
}

// The narrate-then-stop rescue budget is `ChatCtx.narration_nudge_cap`
// (`[tui] narration_nudge_cap`, default 1, per-model `[[model_tuning]]`
// override) — promoted from a hardcoded const here (lever L3). After the cap
// its narration is accepted as the final answer.
/// Max "you ended while a plan still has open steps" nudges per turn. This is
/// state-driven from the plan ledger, not prose-matched.
const PENDING_PLAN_NUDGE_CAP: usize = 1;
/// Max "generated hidden thinking but no visible content/tool call" retries per
/// turn. The first retry is generic; the second is explicit that hidden
/// thinking is not an action. Bounded so a broken backend still exits with the
/// diagnostic.
const SUSPICIOUS_EMPTY_RETRY_CAP: u32 = 2;
/// Max "you claimed file context changed under you without verifying" nudges
/// per turn. Kept separate from narration nudges: this is a blocker-specific
/// ground-truth check, not generic intent-to-act recovery.
const STALE_FILE_NUDGE_CAP: usize = 1;
/// Recent-progress horizon used to decide whether a configured workflow grace
/// window should activate at the normal round cap.
const WORKFLOW_RECENT_PROGRESS_ROUNDS: usize = 3;

fn tail_on_char_boundary(s: &str, max_bytes: usize) -> &str {
    let cut = s.len().saturating_sub(max_bytes);
    let start = (cut..=s.len())
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(0);
    &s[start..]
}

/// Compatibility wrapper for the narrate-then-stop classifier. Production turns
/// use [`crate::NudgeClassifier::load_default`] so `~/.newt/classifiers/nudge.toml`
/// can tune the examples; pure tests use the built-in prototypes here.
#[cfg(test)]
fn looks_like_intent_to_act(content: &str) -> bool {
    crate::NudgeClassifier::builtin().is_pending_action(content)
}

/// Heuristic: did the model stop because it *believes* a file changed under it,
/// without first proving that via git/filesystem ground truth? This catches the
/// "stale line numbers ⇒ operator should restore the file" stall: a context
/// summary or partial read gets mistaken for a concurrent edit, and the model
/// stops instead of running read-only checks.
fn looks_like_unverified_stale_file_blocker(content: &str) -> bool {
    const FILE_CUES: &[&str] = &[
        "file",
        "line reference",
        "line references",
        "old_string",
        "edit_file",
        ".rs",
        ".toml",
        ".md",
    ];
    const STALE_CUES: &[&str] = &[
        "modified out from under",
        "changed out from under",
        "edited out from under",
        "modified concurrently",
        "changed concurrently",
        "stale context",
        "contexts are stale",
        "context is stale",
        "old line references",
        "line references are invalid",
        "file grew from",
        "grew from",
    ];
    const BLOCKER_CUES: &[&str] = &[
        "blocked",
        "cannot safely",
        "can't safely",
        "could land in the wrong place",
        "corrupt the code",
        "restore",
        "git checkout",
        "revert",
        "operator should",
        "human should",
        "recommendation",
    ];

    let lc = content.to_lowercase();
    let tail = tail_on_char_boundary(&lc, 1_200);
    FILE_CUES.iter().any(|c| tail.contains(c))
        && STALE_CUES.iter().any(|c| tail.contains(c))
        && BLOCKER_CUES.iter().any(|c| tail.contains(c))
}

/// The corrective injected when the model narrated its next action but emitted
/// no tool call (the narrate-then-stop stall). Sibling of [`read_only_action_nudge`].
fn narration_action_nudge() -> String {
    "You described what you were about to do but did not call any tool, so \
     nothing actually happened. If you intended to act, emit the tool call now \
     (for example edit_file or write_file with the real arguments) — do not just \
     describe it. If you are genuinely finished, say so explicitly in one \
     sentence."
        .to_string()
}

/// The act-now directive appended after a mid-turn compaction replaced the
/// middle with a summary. The summary wrapper deliberately de-actions itself
/// ("REFERENCE ONLY" — weak models otherwise treat it as fresh instructions);
/// the inverse hazard is losing momentum entirely: post-compaction is exactly
/// where a weak model narrates instead of acting, and the corrective text of
/// an already-spent narration nudge may have just been summarized away. This
/// re-arms intent: mid-task, active step named, next output a tool call.
/// Carries [`compress::CONTINUATION_PREFIX`] so later compressions neither
/// anchor the tail on it nor keep more than one alive.
/// The operator's actual instruction for this turn comes from the caller's
/// authoritative task, not from rediscovering a user message in compacted
/// history. Compaction may retain an older conversation prompt while replacing
/// the current one with a lossy summary, so role-scanning cannot identify the
/// active task reliably (#1163, 2026-07-16 multi-turn repro).
fn post_compaction_continuation(
    step_ledger: Option<&dyn scheduled::StepLedger>,
    prompt_context: prompt_read::PromptReadContext<'_>,
) -> String {
    // #1163 (F): re-inject the FULL plan verbatim — every step with its
    // status — not just the active one. The corporate-box repro showed the
    // model, post-compaction, REWRITE its own plan (dropping the in-progress
    // implement steps for "stop implementation"). Showing the whole plan back
    // + ordering "advance, don't rewrite" makes the plan an anchor the model
    // continues from instead of re-deriving.
    let plan_clause = step_ledger
        .and_then(scheduled::plan_block)
        .map(|plan| {
            format!(
                " Your active plan is below — CONTINUE from the `→` step; call \
                 update_plan only to mark a step done (advance), NEVER to \
                 replace or shrink it:\n{plan}\n"
            )
        })
        .unwrap_or_default();
    // The exact prompt is already protected in the active-prompt metadata/user pair;
    // point at its immutable identity instead of injecting a lossy 400-char
    // quote as a fresh user message.
    let instruction_clause = prompt_context.active_receipt().map_or_else(
        || {
            " Re-read the protected [NEWT ACTIVE PROMPT v1] prompt pair (or call \
             prompt_read with address `current`) before continuing."
                .to_string()
        },
        |receipt| {
            format!(
                " The authoritative operator prompt is protected in the [NEWT ACTIVE \
                 PROMPT v1] metadata/user pair at address {} with model digest {}; call prompt_read \
                 with address `current` to recover it verbatim before continuing.",
                receipt.id(),
                receipt.model_digest()
            )
        },
    );
    format!(
        "{} You are mid-task: the context above was just compacted, not \
         completed.{instruction_clause}{plan_clause} For prompt-rooted work, \
         recover the objective's artifact chain with artifact_read {{\"address\":\"root\"}} \
         before deciding what remains. Continue working — your \
         next output should be the next concrete tool call (re-read any file \
         you are about to edit first, since full file contents were not \
         preserved). Before concluding ANYTHING about prior work, re-anchor on \
         ground truth: check the current git branch and the last few commits — \
         work from earlier in this task may already be COMMITTED (by you), so a \
         clean working tree does NOT mean no work happened (#1163). Do not \
         summarize what happened, do not re-plan, do not narrow the task, and \
         do not repeat work the log shows is done.",
        compress::CONTINUATION_PREFIX
    )
}

/// After a compaction pass replaced the middle with a summary (or the static
/// fallback), refund the narrate-then-stop rescue budget and (re-)append the
/// act-now continuation directive. The refund closes the counter/corrective
/// asymmetry: the spent-nudge counters are turn-locals that survive
/// compaction, while the corrective text they refer to lives in `messages`
/// and may have just been summarized away — leaving the harness refusing to
/// re-nudge a model that no longer remembers the correction. Prune-only and
/// fit passes keep the corrective text, so they neither refund nor anchor.
///
/// `mid_turn` (`round > 0` at the call sites) gates the directive: the
/// pre-dispatch compaction also fires on round 0 of a FRESH turn (a long
/// session's between-turn growth is first measured there), where "You are
/// mid-task … do not summarize" would be false and would countermand an
/// informational ask ("summarize what we changed today") sitting right above
/// it. At round 0 nothing has been spent or lost, so the whole repair is
/// skipped.
fn apply_post_compaction_continuation(
    messages: &mut Vec<serde_json::Value>,
    narration_nudges: &mut usize,
    action: CompressAction,
    step_ledger: Option<&dyn scheduled::StepLedger>,
    prompt_context: prompt_read::PromptReadContext<'_>,
    mid_turn: bool,
    action_nudges: bool,
) {
    if !action_nudges
        || !mid_turn
        || !matches!(
            action,
            CompressAction::Summarized | CompressAction::StaticFallback
        )
    {
        return;
    }
    *narration_nudges = 0;
    // At most one directive alive: drop any earlier copy before appending.
    messages.retain(|m| !compress::is_continuation_message(m));
    messages.push(serde_json::json!({
        "role": "user",
        "content": post_compaction_continuation(step_ledger, prompt_context),
    }));
}

/// The stronger corrective for the second and later narration nudges
/// (`[tui] narration_nudge_cap` > 1). The generic first nudge already failed
/// to convert intent into action, so this one is state-driven like
/// [`read_only_action_nudge`]: it names the active plan step and demands the
/// next output be a bare tool call.
fn escalated_narration_action_nudge(
    attempt: usize,
    cap: usize,
    step_ledger: Option<&dyn scheduled::StepLedger>,
) -> String {
    let step_clause = active_step_description(step_ledger)
        .map(|step| format!(" Active step: '{step}'."))
        .unwrap_or_default();
    format!(
        "Reminder {attempt}/{cap}: you again described an action without calling \
         a tool, so nothing has happened.{step_clause} Your NEXT output must be \
         exactly one tool call that starts that action (for example read_file, \
         edit_file, or run_command with real arguments) — no prose before it. \
         If you are blocked, state the one concrete blocker in a single \
         sentence instead of announcing more intentions."
    )
}

fn stale_file_ground_truth_nudge() -> String {
    "You claimed the file changed under you or that your edit context is stale, \
     but you did not prove that with ground truth. Before stopping or asking the \
     operator to restore/revert anything, run read-only verification: git status \
     --short, git diff -- <file>, wc -l <file>, and re-read the exact target \
     range. If those checks do not prove an actual concurrent change, continue \
     from the verified file contents. Never recommend git checkout/revert unless \
     git diff proves unwanted changes and the operator approves."
        .to_string()
}

fn workflow_classifier_text(messages: &[serde_json::Value], current_content: &str) -> String {
    let mut parts = Vec::new();
    let start = messages.len().saturating_sub(12);
    for message in &messages[start..] {
        if let Some(text) = message_text(message) {
            if !text.trim().is_empty() {
                parts.push(text);
            }
        }
    }
    if !current_content.trim().is_empty() {
        parts.push(current_content.to_string());
    }
    parts.join("\n")
}

fn combine_nudge_hints(first: Option<&str>, second: Option<&str>) -> Option<String> {
    let text = [first, second]
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|hint| !hint.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    (!text.is_empty()).then_some(text)
}

fn message_text(message: &serde_json::Value) -> Option<String> {
    match message.get("content")? {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(parts) => {
            let text = parts
                .iter()
                .filter_map(|part| part.get("text").and_then(|text| text.as_str()))
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }
}

fn pending_plan_completion_nudge(
    step_ledger: Option<&dyn scheduled::StepLedger>,
    needs_plan_update: bool,
    workflow_hint: Option<&str>,
) -> Option<String> {
    let snapshot = step_ledger?.snapshot();
    let total = snapshot.steps.len();
    if total == 0 {
        return None;
    }
    let unfinished = snapshot
        .steps
        .iter()
        .filter(|s| s.status != StepStatus::Done)
        .count();
    if unfinished == 0 {
        return None;
    }
    let active = snapshot
        .steps
        .iter()
        .find(|s| s.status == StepStatus::Active)
        .or_else(|| snapshot.steps.iter().find(|s| s.status != StepStatus::Done));
    let active_clause = active
        .map(|s| format!(" Active step: '{}'.", s.description))
        .unwrap_or_default();
    let step_word = if unfinished == 1 { "step" } else { "steps" };
    let workflow_clause = workflow_hint
        .map(str::trim)
        .filter(|hint| !hint.is_empty())
        .map(|hint| format!("\n\n{hint}"))
        .unwrap_or_default();
    if needs_plan_update {
        Some(format!(
            "You ended with a findings/next-steps summary while the active plan still has \
             {unfinished}/{total} unfinished {step_word}.{active_clause} Your summary says \
             immediate prerequisite repair work now blocks the active step. Call update_plan now \
             with the full ordered plan: mark completed steps completed, make the immediate \
             blocker repair the active step, and keep later feature work pending. Then call the \
             next concrete tool for that active repair. Do not repeat the findings summary or \
             claim a tool-call limit while this nudge is giving you another round.{workflow_clause}"
        ))
    } else {
        Some(format!(
            "You ended the turn while the active plan still has {unfinished}/{total} unfinished \
             {step_word}.{active_clause} Either call update_plan with completed steps marked \
             completed, call the next tool for the active step, or state the concrete blocker. \
             Do not hand off by only describing remaining work."
        ))
    }
}

fn read_only_action_nudge(
    read_only_rounds: usize,
    remaining_rounds: usize,
    step_ledger: Option<&dyn scheduled::StepLedger>,
    delegate_hint: Option<&str>,
) -> String {
    let plan_clause = if step_ledger.and_then(plan_reseat_pointer).is_some() {
        " You have an active multi-step plan; keep working the ACTIVE step instead of \
         restarting or re-planning."
    } else {
        ""
    };
    let delegate_clause = delegate_hint
        .map(|hint| format!(" {hint}"))
        .unwrap_or_default();
    format!(
        "[{read_only_rounds} read-only rounds so far. Stop AIMLESS exploring and start \
         making the change. This is a nudge, not a limit — you may still read, but if \
         you have enough context, call edit_file or write_file now. If a capability \
         denial blocks you, call request_permissions with the exact capability and \
         target, or take a different approach. If you truly cannot edit yet, state the \
         exact blocker. Before edit_file, read the ONE file you are about to change so \
         old_string matches exact text; never guess old_string or repeat a failed edit.\
         {plan_clause}{delegate_clause} ~{remaining_rounds} round(s) left.]"
    )
}

/// Append the memory-nudge line to the current user message — the last
/// message in the list per the memory-manager contract. Defensive fallback:
/// if the last message somehow isn't a user turn, push a standalone user
/// message instead (mirrors the read-only-rounds nudge injection).
/// Make narration nudges EPHEMERAL (#1158): remove the previously-injected
/// nudge exchange — a trailing `[assistant: narration][user: LOOP_GUIDANCE …]`
/// pair — before the loop injects the next one. Without this, a narrate →
/// nudge → narrate → nudge sequence PILES its own dithering into the
/// transcript, and the accumulated "I said I'd act, I was told to act, I said
/// I'd act…" residue is exactly what drove the model (Opus 4.8, 2026-07-14)
/// to defend its idleness ("I'm genuinely finished") and then refuse fresh
/// prompts. Keeping only the CURRENT correction means the model sees the
/// steer once, not a wall of its own hesitation. The escalation counter still
/// climbs, so the escalated wording still fires — only the stale pairs are
/// dropped. A no-op unless the tail actually is a nudge exchange.
fn strip_trailing_nudge_exchange(messages: &mut Vec<serde_json::Value>) {
    let tail_is_nudge = messages.last().is_some_and(|m| {
        m["role"] == "user"
            && m["content"]
                .as_str()
                .is_some_and(|c| c.starts_with(compress::LOOP_GUIDANCE_PREFIX))
    });
    if tail_is_nudge {
        messages.pop(); // the LOOP_GUIDANCE nudge
        if messages.last().is_some_and(|m| m["role"] == "assistant") {
            messages.pop(); // the narration it corrected
        }
    }
}

fn append_nudge_line(messages: &mut Vec<serde_json::Value>, line: &str) {
    match messages.last_mut() {
        Some(last) if last["role"] == "user" => {
            let cur = last["content"].as_str().unwrap_or_default();
            last["content"] = serde_json::Value::String(format!("{cur}\n\n{line}"));
        }
        _ => messages.push(serde_json::json!({"role": "user", "content": line})),
    }
}

fn ollama_non_content_fields(json: &serde_json::Value) -> Vec<&'static str> {
    let message = &json["message"];
    ["reasoning", "reasoning_content", "thinking"]
        .into_iter()
        .filter(|field| {
            message[*field]
                .as_str()
                .map(|value| !value.trim().is_empty())
                .unwrap_or(false)
        })
        .collect()
}

fn ollama_response_shape(json: &serde_json::Value) -> String {
    let message = &json["message"];
    let message_keys = message
        .as_object()
        .map(|obj| obj.keys().cloned().collect::<Vec<_>>().join(","))
        .unwrap_or_else(|| "<missing>".to_string());
    let content_chars = message["content"]
        .as_str()
        .map(|content| content.chars().count())
        .unwrap_or(0);
    let tool_calls = message["tool_calls"]
        .as_array()
        .map(|calls| calls.len())
        .unwrap_or(0);
    let non_content = ollama_non_content_fields(json);
    let non_content = if non_content.is_empty() {
        "none".to_string()
    } else {
        non_content.join(",")
    };
    format!(
        "ollama response shape: message_keys=[{message_keys}] content_chars={content_chars} tool_calls={tool_calls} non_content_fields=[{non_content}] prompt_eval_count={} eval_count={}",
        json["prompt_eval_count"]
            .as_u64()
            .map_or("missing".to_string(), |n| n.to_string()),
        json["eval_count"]
            .as_u64()
            .map_or("missing".to_string(), |n| n.to_string())
    )
}

fn suspicious_empty_retry_nudge(retry_index: u32, json: &serde_json::Value) -> String {
    if retry_index == 0 {
        return "Your previous response produced generated tokens but no assistant-visible content \
                and no tool call. Reply with either a tool call or final assistant content."
            .to_string();
    }
    let fields = ollama_non_content_fields(json);
    let field_note = if fields.is_empty() {
        "hidden/non-content fields".to_string()
    } else {
        format!("hidden/non-content field(s): {}", fields.join(", "))
    };
    format!(
        "Your previous response again produced generated tokens only in {field_note}, \
         with no assistant-visible content and no tool call. Hidden thinking is not an \
         action. If you intend to act, emit the exact tool call now; otherwise reply \
         with final assistant-visible content. Do not continue with hidden-only reasoning."
    )
}

fn suspicious_empty_ollama_diagnostic(json: &serde_json::Value) -> String {
    let fields = ollama_non_content_fields(json);
    let field_note = if fields.is_empty() {
        "no known non-content fields were present".to_string()
    } else {
        format!("non-content field(s) present: {}", fields.join(", "))
    };
    format!(
        "(model generated output tokens but returned no assistant-visible content or tool calls; {field_note}; rerun with `newt --trace` to capture the response shape)"
    )
}

/// Build the nudge appended to the message list when the tool-round cap is hit.
/// `progress` (the `<plan>`/`<state>` working memory, Step 27.5) is folded in so
/// the model summarizes against what it actually accomplished; `observed`
/// (#867 Part A) is the verified-paths manifest collected across the rounds.
fn cap_exit_nudge(max_tool_rounds: usize, progress: Option<&str>, observed: &[String]) -> String {
    // #867: the message list was just trimmed (`trim_for_summary`), so most
    // of the evidence this summary should cite is GONE — the forensic session
    // showed a model reconstructing plausible-but-nonexistent file paths from
    // its priors at exactly this point. Constrain the summary to what is
    // still verbatim in context; absence must be stated, not papered over.
    let mut nudge = format!(
        "You have reached the tool-call limit ({max_tool_rounds} rounds). \
         Do NOT call any more tools. Summarize what you found across the tool \
         calls above and give your best final answer now. Cite only file paths \
         that appear verbatim in the messages above — if the evidence you need \
         was in the omitted messages, say so plainly instead of reconstructing \
         file names or line numbers from memory. Do not answer with an intention \
         to keep working (for example, \"let me read/edit/verify\"); if work remains, \
         list it as remaining work and state that the round cap stopped further tool calls. \
         Report an ACTION (an edit made, a test run or passing, a branch created, a \
         commit, a push, a PR opened) ONLY if a successful tool result above confirms \
         it — if you did not see the tool result, the action did not happen; list it \
         as remaining work instead. The workspace's real git state is checked against \
         this summary."
    );
    // #867 Part A: the ledger survived the trim — hand the model the REAL
    // manifest so grounded citation is possible, not just demanded.
    if !observed.is_empty() {
        nudge.push_str(
            "\n\nFile paths actually observed in tool results this run \
             (these exist — cite from this list):",
        );
        for p in observed {
            nudge.push_str("\n- ");
            nudge.push_str(p);
        }
    }
    if let Some(p) = progress {
        nudge.push_str(&format!("\n\nYour progress so far:\n{p}"));
    }
    nudge
}

/// Fallback message returned when even the final tools-disabled completion
/// fails. Includes accumulated token counts, salvages the `<plan>`/`<state>`
/// progress so partial work survives (Step 27.5), and gives HONEST advice: a run
/// dominated by failed tool calls is a tooling/permissions problem, not too few
/// rounds, so we don't blindly tell the user to raise the cap.
fn cap_exit_tokens_hint(max_tool_rounds: usize, accumulated: Option<crate::TokenUsage>) -> String {
    match accumulated {
        Some(u) => format!(
            " ({} in / {} out tokens consumed across {max_tool_rounds} rounds)",
            u.input_tokens, u.output_tokens,
        ),
        None => String::new(),
    }
}

fn cap_exit_advice(max_tool_rounds: usize, wasted_calls: usize) -> &'static str {
    // If at least one failed tool call per round, the cap was thrash, not a
    // genuine need for more rounds.
    if wasted_calls >= max_tool_rounds.max(1) {
        "most of those rounds were spent on tool calls that failed — the model \
         could not find a working edit/shell path, which is usually a tooling or \
         permissions issue rather than too few rounds; check `newt doctor`"
    } else {
        "raise [tui].max_tool_rounds in your config, or ask a more focused question"
    }
}

fn cap_exit_progress_block(label: &str, progress: Option<&str>) -> String {
    match progress {
        Some(p) => format!("\n\n{label}:\n{p}"),
        None => String::new(),
    }
}

fn cap_exit_fallback(
    max_tool_rounds: usize,
    accumulated: Option<crate::TokenUsage>,
    wasted_calls: usize,
    progress: Option<&str>,
) -> String {
    let tokens_hint = cap_exit_tokens_hint(max_tool_rounds, accumulated);
    let advice = cap_exit_advice(max_tool_rounds, wasted_calls);
    let salvaged = cap_exit_progress_block("Progress captured before the summary failed", progress);
    format!(
        "(reached the tool-call limit of {max_tool_rounds} rounds{tokens_hint}, \
         and the final summarization request also failed — {advice}){salvaged}"
    )
}

fn cap_exit_action_handoff_fallback(
    max_tool_rounds: usize,
    accumulated: Option<crate::TokenUsage>,
    wasted_calls: usize,
    progress: Option<&str>,
) -> String {
    let tokens_hint = cap_exit_tokens_hint(max_tool_rounds, accumulated);
    let advice = cap_exit_advice(max_tool_rounds, wasted_calls);
    let salvaged = cap_exit_progress_block("Progress captured at the tool-call limit", progress);
    format!(
        "(reached the tool-call limit of {max_tool_rounds} rounds{tokens_hint}; \
         the final tools-disabled summary described future tool actions instead \
         of final state, so Newt preserved the verified progress instead of \
         accepting that handoff — {advice}){salvaged}"
    )
}

fn cap_exit_summary_is_action_handoff(content: &str) -> bool {
    crate::NudgeClassifier::load_default()
        .classify(content)
        .class
        == crate::NudgeClass::PendingAction
        || looks_like_unverified_stale_file_blocker(content)
}

/// The cap-exit context threaded into a final tools-disabled summary (Step
/// 27.5): the round limit, accumulated usage, the count of failed tool calls
/// (drives honest advice), the salvaged `<plan>`/`<state>` progress, and the
/// #867 observed-paths manifest (verified paths from tool results, collected
/// before the trim could delete them).
struct CapExit {
    max_tool_rounds: usize,
    accumulated: Option<crate::TokenUsage>,
    wasted_calls: usize,
    progress: Option<String>,
    observed: Vec<String>,
    /// Complete-request ceiling for the final tools-disabled dispatch.
    request_budget: Option<usize>,
    calibration: f32,
    estimation: crate::tokens::TokenEstimation,
    /// Ollama must repeat the configured context window on every request,
    /// including the tools-disabled cap exit. Ignored by OpenAI chat.
    ollama_num_ctx: Option<u32>,
}

/// Final tools-disabled completion for the Ollama (`/api/chat`) path.
///
/// `messages` is the already-trimmed list (caller uses `trim_for_summary`).
/// `cap.accumulated` carries usage from the preceding tool-call rounds so it
/// survives even when this summary request fails.
async fn final_summary_ollama(
    client: &reqwest::Client,
    chat_url: &str,
    model: &str,
    mut messages: Vec<serde_json::Value>,
    cap: CapExit,
) -> anyhow::Result<(String, bool, Option<crate::TokenUsage>)> {
    let CapExit {
        max_tool_rounds,
        accumulated,
        wasted_calls,
        progress,
        observed,
        request_budget,
        calibration,
        estimation,
        ollama_num_ctx,
    } = cap;
    messages.push(serde_json::json!({
        "role": "user",
        "content": cap_exit_nudge(max_tool_rounds, progress.as_deref(), &observed),
    }));
    if preflight_full_message_request(
        &messages,
        None,
        request_budget,
        calibration,
        estimation,
        model,
    )
    .is_err()
    {
        return Ok((
            cap_exit_fallback(
                max_tool_rounds,
                accumulated,
                wasted_calls,
                progress.as_deref(),
            ),
            false,
            accumulated,
        ));
    }
    // No `tools` key => the model cannot emit tool calls.
    let mut body = serde_json::json!({
        "model": model,
        "messages": &messages,
        "stream": false,
    });
    if let Some(num_ctx) = ollama_num_ctx {
        body["options"] = serde_json::json!({ "num_ctx": num_ctx });
    }
    let retry = tui_retry_policy();
    let result = with_backoff_notify(
        &retry,
        || async {
            let resp = client
                .post(chat_url)
                .json(&body)
                .send()
                .await
                .map_err(|e| {
                    // Typed classification at the source (W0 #1511).
                    anyhow::Error::new(observability::DispatchError::from_reqwest(
                        "request failed",
                        e,
                    ))
                })?;
            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                return Err(observability::DispatchError::http_status(format!(
                    "Ollama {status}: {text}"
                ))
                .into());
            }
            resp.json::<serde_json::Value>()
                .await
                .map_err(anyhow::Error::from)
        },
        |_, _| {}, // no color context here; tracing::warn covers it
    )
    .await;
    match result {
        Ok(json) => {
            // #385: strip inline <think>…</think> reasoning Nemotron-style models emit
            // in the content stream (the separate `thinking` field is handled elsewhere).
            // All-reasoning content collapses to empty → the thinking-only recovery below.
            let (content, _reasoning) = crate::reasoning::split_reasoning(
                json["message"]["content"].as_str().unwrap_or(""),
            );
            let total = merge_round_usage(accumulated, ollama_usage(&json));
            if content.is_empty() {
                Ok((
                    cap_exit_fallback(
                        max_tool_rounds,
                        accumulated,
                        wasted_calls,
                        progress.as_deref(),
                    ),
                    false,
                    accumulated,
                ))
            } else if cap_exit_summary_is_action_handoff(&content) {
                Ok((
                    cap_exit_action_handoff_fallback(
                        max_tool_rounds,
                        accumulated,
                        wasted_calls,
                        progress.as_deref(),
                    ),
                    false,
                    total,
                ))
            } else {
                Ok((content, false, total))
            }
        }
        // On any failure (including exhausted retries), still return the
        // accumulated usage so the caller can log the tokens consumed.
        Err(_) => Ok((
            cap_exit_fallback(
                max_tool_rounds,
                accumulated,
                wasted_calls,
                progress.as_deref(),
            ),
            false,
            accumulated,
        )),
    }
}

/// Project the internal protected-head representation onto chat templates that
/// accept exactly one system message at index zero.
///
/// Prompt provenance deliberately keeps the base prompt and active-prompt card
/// as separate leading system messages internally. Coalesce only that leading
/// run for the OpenAI Chat Completions wire; a genuinely late system message is
/// malformed and must not be silently promoted across conversation history.
fn openai_chat_wire_messages(
    messages: &[serde_json::Value],
) -> anyhow::Result<Vec<serde_json::Value>> {
    let leading_systems = messages
        .iter()
        .take_while(|message| message["role"].as_str() == Some("system"))
        .count();

    if messages[leading_systems..]
        .iter()
        .any(|message| message["role"].as_str() == Some("system"))
    {
        anyhow::bail!(
            "invalid OpenAI chat message order: system messages must precede conversation history"
        );
    }
    if leading_systems <= 1 {
        return Ok(messages.to_vec());
    }

    let content = messages[..leading_systems]
        .iter()
        .map(|message| {
            message["content"].as_str().ok_or_else(|| {
                anyhow::anyhow!(
                    "invalid OpenAI chat system message: content must be text before coalescing"
                )
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?
        .join("\n\n");
    let mut system = messages[0].clone();
    system["content"] = serde_json::Value::String(content);

    let mut wire = Vec::with_capacity(messages.len() - leading_systems + 1);
    wire.push(system);
    wire.extend(messages[leading_systems..].iter().cloned());
    Ok(wire)
}

fn prepare_openai_assistant_replay(
    message: &serde_json::Value,
    clean_content: &str,
    replay_scope: crate::model_card::ReasoningReplayScope,
    current_user_turn: bool,
) -> serde_json::Value {
    let mut assistant = message.clone();
    if assistant["role"].as_str().is_none() {
        assistant["role"] = serde_json::Value::String("assistant".into());
    }

    let keep_reasoning = replay_scope == crate::model_card::ReasoningReplayScope::FullHistory
        || (replay_scope == crate::model_card::ReasoningReplayScope::CurrentUserTurn
            && current_user_turn);
    if !keep_reasoning {
        assistant["content"] = serde_json::Value::String(clean_content.to_string());
        if let Some(object) = assistant.as_object_mut() {
            object.remove("reasoning_content");
        }
    }
    assistant
}

/// Final tools-disabled completion for the OpenAI (`/v1/chat/completions`) path.
///
/// `messages` is the already-trimmed list (caller uses `trim_for_summary`).
/// `accumulated` carries usage from the preceding tool-call rounds.
async fn final_summary_openai(
    client: &reqwest::Client,
    chat_url: &str,
    model: &str,
    api_key: Option<&str>,
    mut messages: Vec<serde_json::Value>,
    generation_policy: generation_policy::GenerationPolicy,
    cap: CapExit,
) -> anyhow::Result<(String, bool, Option<crate::TokenUsage>)> {
    let CapExit {
        max_tool_rounds,
        accumulated,
        wasted_calls,
        progress,
        observed,
        request_budget,
        calibration,
        estimation,
        ollama_num_ctx: _,
    } = cap;
    messages.push(serde_json::json!({
        "role": "user",
        "content": cap_exit_nudge(max_tool_rounds, progress.as_deref(), &observed),
    }));
    let messages = openai_chat_wire_messages(&messages)?;
    if preflight_full_message_request(
        &messages,
        None,
        request_budget,
        calibration,
        estimation,
        model,
    )
    .is_err()
    {
        return Ok((
            cap_exit_fallback(
                max_tool_rounds,
                accumulated,
                wasted_calls,
                progress.as_deref(),
            ),
            false,
            accumulated,
        ));
    }
    // Omit `tools` / `tool_choice` => the model cannot emit tool calls.
    let mut body = serde_json::json!({
        "model": model,
        "messages": &messages,
        "stream": false,
    });
    generation_policy.apply_to_chat_completions_body(&mut body);
    let retry = tui_retry_policy();
    let result = with_backoff_notify(
        &retry,
        || async {
            let mut req = client.post(chat_url).json(&body);
            if let Some(key) = api_key {
                req = req.bearer_auth(key);
            }
            let resp = req.send().await.map_err(|e| {
                // Typed classification at the source (W0 #1511).
                anyhow::Error::new(observability::DispatchError::from_reqwest(
                    "request failed",
                    e,
                ))
            })?;
            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                return Err(observability::DispatchError::http_status(format!(
                    "inference endpoint {status}: {text}"
                ))
                .into());
            }
            resp.json::<serde_json::Value>()
                .await
                .map_err(anyhow::Error::from)
        },
        |_, _| {},
    )
    .await;
    match result {
        Ok(json) => {
            // #385: strip inline <think>…</think> reasoning from the content.
            let (content, _reasoning) = crate::reasoning::split_reasoning(
                json["choices"][0]["message"]["content"]
                    .as_str()
                    .unwrap_or(""),
            );
            let total = merge_round_usage(accumulated, openai_usage(&json["usage"]));
            if content.is_empty() {
                Ok((
                    cap_exit_fallback(
                        max_tool_rounds,
                        accumulated,
                        wasted_calls,
                        progress.as_deref(),
                    ),
                    false,
                    accumulated,
                ))
            } else if cap_exit_summary_is_action_handoff(&content) {
                Ok((
                    cap_exit_action_handoff_fallback(
                        max_tool_rounds,
                        accumulated,
                        wasted_calls,
                        progress.as_deref(),
                    ),
                    false,
                    total,
                ))
            } else {
                Ok((content, false, total))
            }
        }
        Err(_) => Ok((
            cap_exit_fallback(
                max_tool_rounds,
                accumulated,
                wasted_calls,
                progress.as_deref(),
            ),
            false,
            accumulated,
        )),
    }
}

/// OpenAI-compatible variant of [`chat_complete`]: the same agentic tool-call
/// loop, but over `POST {endpoint}/v1/chat/completions` with bearer auth and
/// the OpenAI `tool_calls` / `tool_call_id` / `usage` shapes.
///
/// Non-streaming for now — the final answer is returned (and printed by the
/// caller) rather than streamed token-by-token. Token-by-token SSE streaming
/// is a follow-up; functionally the loop is complete, including tools.
pub async fn openai_chat_complete(
    ctx: ChatCtx<'_>,
    mcp: &mut dyn McpTools,
) -> anyhow::Result<(String, bool, Option<crate::TokenUsage>, u32)> {
    openai_chat_complete_with_prompt(ctx, None, None, mcp).await
}

/// Provenance-aware OpenAI Chat Completions loop. See
/// [`chat_complete_with_prompt`] for the compatibility contract.
pub async fn openai_chat_complete_with_prompt(
    ctx: ChatCtx<'_>,
    turn_prompt_context: Option<&crate::TurnPromptContext>,
    prompt_source: Option<&dyn PromptSource>,
    mcp: &mut dyn McpTools,
) -> anyhow::Result<(String, bool, Option<crate::TokenUsage>, u32)> {
    openai_chat_complete_with_prompt_and_artifacts(
        ctx,
        turn_prompt_context,
        prompt_source,
        None,
        None,
        mcp,
    )
    .await
}

async fn openai_chat_complete_with_prompt_and_artifacts(
    ctx: ChatCtx<'_>,
    turn_prompt_context: Option<&crate::TurnPromptContext>,
    prompt_source: Option<&dyn PromptSource>,
    artifact_source: Option<&dyn artifact_read::ArtifactSource>,
    artifact_sink: Option<&dyn artifact_read::PromptArtifactSink>,
    mcp: &mut dyn McpTools,
) -> anyhow::Result<(String, bool, Option<crate::TokenUsage>, u32)> {
    let ChatCtx {
        url,
        model,
        kind: _,
        api_key,
        messages: mem_messages,
        task,
        workspace,
        color,
        markdown: _,
        tool_offload,
        spill_store,
        compaction_store,
        scratchpad,
        scratchpad_store,
        code_search,
        where_is,
        nav,
        exposure,
        experience_store,
        step_ledger,
        caveats,
        persona_tools,
        // Chat Completions does not use the Responses-only `reasoning_effort`
        // field. Explicit endpoint capability data may instead project cognition
        // into a local generation policy.
        cognition,
        chat_completions_capability,
        reasoning_replay_scope,
        max_tool_rounds,
        workflow_grace_rounds,
        narration_nudge_cap,
        action_nudges,
        prompt_disposition,
        prompt_intake,
        tool_output_lines,
        debug,
        trace,
        num_ctx,
        connect_timeout_secs,
        inference_timeout_secs,
        mid_loop_trim_threshold,
        compaction_trigger_policy,
        mid_loop_trim_tokens,
        max_ok_input,
        build_check_cmd,
        safe_context,
        recover_cw_400,
        mut note_sink,
        mut note_nudge,
        recall_source,
        memory_source,
        summarizer,
        compress_state,
        mut tool_events,
        mut phantom_reaches,
        mut end_reason,
        mut solve_obs,
        mut permission_gate,
        mut on_round_usage,
        estimate_ratio,
        estimation,
        summary_input_cap_floor_chars,
        input_ceiling_pct,
        low_budget_pct,
        exec_floor,
        write_ledger,
        cancel,
        live_tool_output,
        git_tool,
        crew_runner,
        operating_mode_control,
        plan_mode_control,
    } = ctx;
    // See the Ollama path: a non-Act turn is allowed bounded reads but never
    // execution-pressure nudges.
    let action_nudges = action_nudges && prompt_disposition == PromptDisposition::Act;
    let max_tool_rounds = prompt_disposition.tool_round_limit(max_tool_rounds);
    let generation_policy = generation_policy::GenerationPolicy::resolve(
        cognition,
        chat_completions_capability,
        reasoning_replay_scope,
    );
    let reasoning_replay_scope = generation_policy.reasoning_replay_scope;
    // Headless callers may pass no session state (mirrors the Ollama path).
    let mut local_compress_state = CompressState::new();
    let compress_state = match compress_state {
        Some(s) => s,
        None => &mut local_compress_state,
    };
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(connect_timeout_secs))
        .timeout(std::time::Duration::from_secs(inference_timeout_secs))
        .build()?;
    let chat_url = format!("{}/v1/chat/completions", url.trim_end_matches('/'));
    let retry = tui_retry_policy();
    // The save_note tool is advertised only when a sink exists (Step 19.3);
    // recall only when a source exists (Step 17.5); memory_fetch only when a
    // memory source exists (#319) — mirrors the Ollama path.
    let advertise_save_note = note_sink.is_some();
    let advertise_recall = recall_source.is_some();
    let advertise_memory_fetch = memory_source.is_some();
    // Step 26.4 (#583): state tools only when the feature is on AND a store exists.
    let advertise_scratchpad = scratchpad_store.is_some() && scratchpad;
    // Step 26.5.5 (#582): the code_search tool when a searcher is present.
    let advertise_code_search = code_search.is_some();
    // Step 26.6a (#585): the experiential tools when a store is present.
    let advertise_experiential = experience_store.is_some();
    // Step 26.6b (#586): the scheduled plan tools when a ledger is present.
    let advertise_scheduled = step_ledger.is_some();
    let advertise_git = git_tool.is_some();
    let advertise_team = crew_runner.is_some();
    let advertise_operating_mode = operating_mode_control.is_some();
    let advertise_plan_mode = plan_mode_control.is_some();
    let advertise_plan_mode_active =
        plan_mode_control.is_some_and(|control| control.is_plan_mode());

    let mut messages: Vec<serde_json::Value> = mem_messages
        .iter()
        .map(|m| {
            let content = if m.role == crate::Role::Assistant
                && reasoning_replay_scope != crate::model_card::ReasoningReplayScope::FullHistory
            {
                crate::reasoning::split_reasoning(&m.content).0
            } else {
                m.content.clone()
            };
            serde_json::json!({"role": m.role.as_str(), "content": content})
        })
        .collect();
    let ephemeral_prompt = turn_prompt_context.is_none().then(|| {
        crate::TurnPromptContext::ephemeral_operator(
            "ephemeral-headless",
            task.as_bytes().to_vec(),
            task.as_bytes().to_vec(),
        )
    });
    let turn_prompt_context = turn_prompt_context.or(ephemeral_prompt.as_ref());
    let prompt_context =
        prompt_read::PromptReadContext::new(turn_prompt_context, task, prompt_source);
    let artifact_context = turn_prompt_context
        .map(|turn| artifact_read::ArtifactReadContext::from_turn(turn, artifact_source));
    let active_task = prompt_context.active_text();
    if let Some(intake) = prompt_intake {
        prompt_read::ensure_active_prompt_card_with_intake(&mut messages, prompt_context, intake);
    } else {
        prompt_read::ensure_active_prompt_card(&mut messages, prompt_context);
    }

    // In-band memory nudge (Step 19.3) — mirrors the Ollama path.
    if note_sink.is_some() {
        if let Some(line) = note_nudge.as_deref_mut().and_then(NoteNudge::begin_turn) {
            append_nudge_line(&mut messages, &line);
        }
    }

    let mut accumulated_usage: Option<crate::TokenUsage> = None;
    let mut hallucination_count: u32 = 0;
    // Step 27.3/#771: guard against exact-repeat tool loops this run.
    let mut repeat_calls = RepeatCallGuard::default();
    // At most one reasoning-only length-stop continuation per user turn. The
    // signal index lets the next response record whether that bounded recovery
    // produced visible content or an executable call.
    let mut reasoning_continuation_attempted = false;
    let mut reasoning_overflow_signal_index: Option<usize> = None;
    // Hard context-window 400s recovered (parse limit → trim → retry). See #223.
    let mut cw_retries: u32 = 0;
    // No-tools recovery (mirrors the Ollama path): a model that rejects the
    // `tools` field 400s even on "hello"; drop tools and retry, notice once.
    let mut tools_supported = true;
    let mut tools_unsupported_notified = false;
    // Pre-send token budget gate; tightened mid-turn by a recovered 400
    // (#223). `num_ctx` is not sent on this wire, but an operator-declared
    // local endpoint window still provides an authoritative input ceiling.
    // Cloud endpoints leave it unset and continue to fail open on proven-good
    // evidence alone.
    let mut effective_input_ceiling = num_ctx_input_ceiling(
        num_ctx,
        input_ceiling_pct,
        generation_policy.max_output_tokens,
    );
    let mut send_budget: Option<usize> =
        initial_send_budget(max_ok_input, safe_context, effective_input_ceiling);
    let mut send_budget_authoritative = safe_context.is_some() || effective_input_ceiling.is_some();
    // Tool schemas ride along in every request body; count them once (18.1).
    let tools = merged_tool_definitions(
        mcp,
        advertise_save_note,
        advertise_recall,
        advertise_memory_fetch,
        advertise_git,
        advertise_team,
        advertise_scratchpad,
        advertise_code_search,
        advertise_experiential,
        advertise_scheduled,
        advertise_operating_mode,
        advertise_plan_mode,
        advertise_plan_mode_active,
    );
    // FR-1 part 2 (#997): scope the advertised catalog to the active persona's
    // `tools:` allow-list (no-op when `persona_tools` is `None`). The executor
    // enforces the same set, so what the model sees and what it may run agree.
    let tools = filter_advertised_tools(tools, persona_tools);
    let tools = filter_tools_for_disposition(tools, prompt_disposition);
    // #TEC Pass 1: exposure stage — clip the authorized catalog to the live
    // usable budget (identity under `ExposureProfile::Full`). See the Ollama
    // path for the full rationale.
    let tools = crate::agentic::tools::select_exposed(
        tools,
        &exposure,
        exposure_budget_tokens(send_budget, safe_context),
        &std::collections::BTreeSet::new(),
        estimation,
    );
    let tool_tokens = estimate_value_tokens(&tools, estimation);
    // Phase 20 §2.3: per-turn calibration ratio + real-token schema overhead
    // (mirrors the Ollama path).
    let cal = sanitize_estimate_ratio(estimate_ratio);
    let tool_tokens_real = calibrate_up(tool_tokens, cal);
    preflight_irreducible_request(
        &messages,
        Some(&tools),
        authoritative_request_budget(send_budget, send_budget_authoritative, mid_loop_trim_tokens),
        cal,
        estimation,
        model,
    )?;
    // Truthful context-size tracker (prompt-tokens-preferred, Step 18.1).
    let mut prompt_tracker = PromptTracker::new();
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();

    // #867 Part A: observed-paths ledger (matches the Ollama path).
    let mut observed_paths = claim_check::ObservedPaths::default();
    let observed_resolver = claim_check::workspace_resolver(workspace);
    // #1214: HEAD at turn start (mirror of the Ollama path).
    let turn_start_head = claim_check::git_head(workspace);

    // Narrate-then-stop rescue counter (mirror of the Ollama path).
    let mut narration_nudges: usize = 0;
    // Self-verify gate (#23): times we've handed the model a round to run the
    // verification the workspace ships before letting it conclude. Capped so a
    // model that refuses to verify still ends the turn.
    let mut self_verify_nudges: usize = 0;
    const SELF_VERIFY_CAP: usize = 2;
    // Pending-plan final-answer gate counter (mirror of the Ollama path).
    let mut pending_plan_nudges: usize = 0;
    // Unverified stale-file blocker rescue counter (mirror of the Ollama path).
    let mut stale_file_nudges: usize = 0;
    let nudge_classifier = crate::NudgeClassifier::load_default();
    // #1152/#1162: same intent gate as the primary loop — see the comment there.
    let action_turn = action_nudges && crate::classifiers::user_turn_invites_action(active_task);
    let workflow_steerer = crate::WorkflowSteerer::load_default();
    let mut workflow_runtime = WorkflowRuntimeState {
        tenacity: crate::tenacity::effective_tenacity(),
        ..Default::default()
    };
    // See the Ollama path: a matching workflow's grace-horizon override.
    workflow_runtime.set_progress_horizon(
        workflow_steerer.progress_horizon(&workflow_classifier_text(&messages, "")),
    );

    // Agentic loop — up to `max_tool_rounds` tool-call rounds (matches the
    // Ollama path), plus a configurable workflow grace window when the normal
    // cap would stop during active workflow progress.
    let hard_tool_rounds = max_tool_rounds.saturating_add(workflow_grace_rounds);
    let mut workflow_grace_active = false;
    let mut current_tool_round_limit = max_tool_rounds;
    'round_loop: for round in 0..hard_tool_rounds {
        if round >= current_tool_round_limit {
            if workflow_grace_active {
                break;
            }
            if !action_nudges {
                break;
            }
            let Some(nudge) = workflow_runtime.cap_grace_nudge(
                step_ledger,
                max_tool_rounds,
                workflow_grace_rounds,
            ) else {
                break;
            };
            workflow_grace_active = true;
            current_tool_round_limit = hard_tool_rounds;
            if debug {
                print_debug(
                    "workflow progress at soft round cap — granting configured grace window",
                    color,
                );
            }
            messages.push(serde_json::json!({ "role": "user", "content": nudge }));
        }
        // Interrupt checkpoint (Esc / Ctrl-C), same contract as the Ollama path:
        // bail at the round boundary with an empty reply; tool dispatches below
        // are also raced so an interrupt can stop a hung command mid-round.
        if is_cancelled(cancel) {
            return Ok((String::new(), false, accumulated_usage, hallucination_count));
        }
        if round > 0 && color {
            execute!(
                io::stdout(),
                SetForegroundColor(CtColor::DarkGrey),
                Print("…\n"),
                ResetColor
            )
            .ok();
        }

        // Conditional plan re-seat (#630 b) — mirror of the Ollama path: re-show
        // the active step each round so a multi-step plan doesn't go stale.
        if round > 0 && action_nudges {
            if let Some(ptr) = step_ledger.and_then(plan_reseat_pointer) {
                messages.push(serde_json::json!({ "role": "user", "content": ptr }));
            }
            if let Some(nudge) = workflow_runtime.round_start_nudge(step_ledger) {
                messages.push(serde_json::json!({ "role": "user", "content": nudge }));
            }
            // Tenacity action-forcing (#tenacity): the OpenAI-chat loop had no
            // read-only action nudge (only the Ollama loop did), so a model
            // driven here could read/plan its whole budget without ever editing.
            // Fire the tenacity nudge once the read-only budget is spent.
            let remaining = current_tool_round_limit.saturating_sub(round + 1);
            if let Some(nudge) = workflow_runtime.action_forcing_nudge(remaining, step_ledger, None)
            {
                if debug {
                    print_debug(
                        &format!(
                            "tenacity[{}]: forcing action, read-only budget spent (round {round})",
                            workflow_runtime.tenacity
                        ),
                        color,
                    );
                }
                messages.push(serde_json::json!({ "role": "user", "content": nudge }));
            }
        }

        // Context compression (Step 18.4, #247 — mirrors the Ollama path):
        // the shared prune → boundary → redacted summary → marker pipeline
        // serves the mid-loop trigger and the pre-send budget guard.
        {
            // Phase 20 §2.3: calibrated `current` (real-token space) —
            // mirrors the Ollama path.
            let current = prompt_tracker.current(&messages, Some(&tools), cal, estimation);
            // Count-only budget priced in message-token space (F1) — mirrors
            // the Ollama path.
            let message_tokens = estimate_tokens(&messages, estimation);
            let has_authoritative_headroom = authoritative_request_budget(
                send_budget,
                send_budget_authoritative,
                mid_loop_trim_tokens,
            )
            .is_some();
            let reasoning_tail_len =
                compress::protected_reasoning_tail_len(&messages, reasoning_replay_scope);
            if let Some(trigger) = compression_trigger(
                compress::compression_message_count(&messages, reasoning_tail_len),
                current,
                message_tokens,
                CompressionTriggerLimits {
                    count_threshold: mid_loop_trim_threshold,
                    token_threshold: mid_loop_trim_tokens,
                    send_budget,
                    tool_tokens: tool_tokens_real,
                    policy: compaction_trigger_policy,
                    has_authoritative_headroom,
                },
            ) {
                // Hard budgets are real-token currency → pipeline chars/4
                // (Phase 20 §2.3); count-only budgets pass through (F1).
                let pipeline_budget = if trigger.hard_budget {
                    calibrate_down(trigger.budget, cal)
                } else {
                    trigger.budget
                };
                // Step 20.3: authoritative iff a token threshold fired or the
                // send budget rests on a believed ceiling (mirrors the Ollama
                // loop). A lone-HWM guard is non-authoritative → fails open.
                let token_fired = mid_loop_trim_tokens.is_some_and(|t| t > 0 && current > t);
                let outcome = compress(
                    CompressRequest {
                        messages: &messages,
                        budget: pipeline_budget,
                        // A current-turn reasoning transcript is one atomic
                        // logical item for count pressure. Token pressure still
                        // applies to its real size, but a physical count cap
                        // must not split away the plan the endpoint requires.
                        max_messages: if reasoning_tail_len > 0 {
                            None
                        } else {
                            trigger.max_messages
                        },
                        replay_protected_tail_len: reasoning_tail_len,
                        task: active_task,
                        hard_budget: trigger.hard_budget,
                        authoritative: token_fired || send_budget_authoritative,
                        focus: None,
                        est: estimation,
                        summary_input_cap_floor_chars,
                        compaction_store,
                    },
                    summarizer,
                    compress_state,
                )
                .await;
                if let Some(notice) = outcome.notice {
                    print_harness_notice(&notice, color);
                }
                if outcome.action == CompressAction::Refused {
                    anyhow::bail!(
                        "context (~{current} tokens) exceeds the model's input budget and \
                         auto-compression is disabled after repeated ineffective passes — \
                         start a new conversation or ask a more focused question, or run \
                         `newt tunings reset {model}` if this model's learned budget looks wrong"
                    );
                }
                if outcome.fired {
                    // N2 (mirrors the Ollama path): flag a still-over-budget
                    // assembly before the full-request preflight refuses its
                    // dispatch — compared in the pipeline's own chars/4
                    // currency (Phase 20 §2.3).
                    let suffix = if trigger.hard_budget && outcome.tokens_after > pipeline_budget {
                        ", still over budget"
                    } else {
                        ""
                    };
                    emit_compression_notice(
                        color,
                        outcome.tokens_before,
                        outcome.tokens_after,
                        outcome.action,
                        suffix,
                    );
                    if debug {
                        print_debug(
                            &format!(
                                "compression: {} → {} messages (budget ~{} tokens, \
                                 +~{tool_tokens} tool-schema tokens ride along)",
                                messages.len(),
                                outcome.messages.len(),
                                pipeline_budget,
                            ),
                            color,
                        );
                    }
                    messages = outcome.messages;
                    prompt_tracker.invalidate();
                    apply_post_compaction_continuation(
                        &mut messages,
                        &mut narration_nudges,
                        outcome.action,
                        step_ledger,
                        prompt_context,
                        round > 0,
                        action_nudges,
                    );
                    record_compaction_artifact(
                        artifact_sink,
                        artifact_context,
                        outcome.action,
                        outcome.tokens_before,
                        outcome.tokens_after,
                        pipeline_budget,
                        round,
                        trigger.primary_cause.artifact_reason(),
                        Some(&trigger),
                        send_budget_authoritative,
                        color,
                    );
                }
            }
        }

        let wire_messages = openai_chat_wire_messages(&messages)?;

        // Mirror the Ollama full-request gate. A known authoritative ceiling
        // means the harness must not dispatch an impossible request,
        // especially after a giant exact prompt_read result.
        preflight_full_message_request(
            &wire_messages,
            tools_supported.then_some(&tools),
            authoritative_request_budget(
                send_budget,
                send_budget_authoritative,
                mid_loop_trim_tokens,
            ),
            cal,
            estimation,
            model,
        )?;

        // Phase 20 §2.2: chars/4 estimate of exactly the request about to be
        // dispatched — mirrors the Ollama path.
        let round_est_raw = estimate_request_tokens(
            &wire_messages,
            tools_supported.then_some(&tools),
            estimation,
        );

        // OpenAI-compatible endpoints don't use Ollama's `options.num_ctx` —
        // context limits are configured server-side (vLLM --max-model-len).
        let mut body = serde_json::json!({
            "model": model,
            "messages": wire_messages,
            "tools": tools.clone(),
            "tool_choice": "auto",
            "stream": false,
        });
        generation_policy.apply_to_chat_completions_body(&mut body);
        // Drop tools (and the now-meaningless tool_choice) for a model that
        // rejected them on a prior "does not support tools" 400.
        if !tools_supported {
            if let Some(o) = body.as_object_mut() {
                o.remove("tools");
                o.remove("tool_choice");
            }
        }
        // OpenAI-compatible endpoints do not accept a `num_ctx` request field,
        // but the operator-declared local window still bounds Newt's pre-send
        // budget. These endpoints reject oversize requests rather than silently
        // truncating them, so no wire field is needed to enforce the local cap.
        let dispatch = with_backoff_notify(
            &retry,
            || async {
                let mut req = client.post(&chat_url).json(&body);
                if let Some(key) = api_key {
                    req = req.bearer_auth(key);
                }
                // W0 (#1511): classify while the error is TYPED — the
                // DispatchError keeps the historical message text and carries
                // the structural class to the driver boundary.
                let resp = req.send().await.map_err(|e| {
                    anyhow::Error::new(observability::DispatchError::from_reqwest(
                        "request failed",
                        e,
                    ))
                })?;
                if !resp.status().is_success() {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    return Err(observability::DispatchError::http_status(format!(
                        "inference endpoint {status}: {text}"
                    ))
                    .into());
                }
                resp.json::<serde_json::Value>()
                    .await
                    .map_err(anyhow::Error::from)
            },
            |attempt, delay| print_retry_indicator(attempt, delay, color),
        )
        .await;
        let json: serde_json::Value = match dispatch {
            Ok(j) => j,
            Err(e) => {
                // No-tools recovery: a model that rejects the `tools` field
                // (deepseek-r1) 400s even on "hello". Drop tools, notice once,
                // and re-dispatch the same round — self-limiting (the rebuilt
                // body omits tools) and session-persistent.
                if tools_supported && is_tools_unsupported_error(&e) {
                    tools_supported = false;
                    if !tools_unsupported_notified {
                        tools_unsupported_notified = true;
                        print_newt(
                            &format!(
                                "{model} does not support tools — tools disabled for this session"
                            ),
                            color,
                            false,
                        );
                    }
                    continue 'round_loop;
                }
                // Graceful context-window overflow recovery: parse the model's
                // real limit, tighten the budget, compress, and retry once (#223;
                // compress-not-trim since Step 18.4). When the endpoint carries
                // NO parseable limit — llama.cpp's numberless `500 "Context size
                // has been exceeded"` — fall back to deriving a tightened cap
                // from the current send budget. The shared parse-only hook reads
                // both LiteLLM and vLLM numbered forms in interactive and
                // headless drivers; this fallback is only for numberless errors.
                // It remains capped by the operator-declared local window even
                // though no `num_ctx` field rides on this wire.
                if cw_retries < 2 {
                    let recovered_window = recover_cw_400.and_then(|f| f(&e, model, &today));
                    if let Some(recovered_budget) = recovered_window
                        .map(|context_window| {
                            recovered_input_budget(
                                context_window,
                                input_ceiling_pct,
                                generation_policy.max_output_tokens,
                                effective_input_ceiling,
                            )
                        })
                        .or_else(|| {
                            cw_overflow::core_recover_overflow(&e.to_string(), send_budget, None)
                                .map(|cap| cap as usize)
                        })
                    {
                        if let Some(context_window) = recovered_window {
                            emit_context_window_400(&mut on_round_usage, context_window);
                        }
                        // The callback returns the endpoint's full hard window,
                        // not an already-discounted input cap. Reserve this
                        // request's maximum output against the actual window,
                        // then retain any tighter declared-window ceiling.
                        let new_budget = effective_input_ceiling
                            .map_or(recovered_budget, |c| recovered_budget.min(c));
                        emit_overflow_notice(
                            color,
                            accumulated_usage.as_ref(),
                            Some(new_budget.min(u32::MAX as usize) as u32),
                            model,
                            cw_retries + 1,
                        );
                        send_budget = Some(new_budget);
                        effective_input_ceiling = Some(new_budget);
                        // The endpoint's parsed hard limit is authoritative
                        // from here on (Step 20.3; mirrors the Ollama path).
                        send_budget_authoritative = true;
                        let outcome = compress(
                            CompressRequest {
                                // Real-token cap minus real-token schema
                                // overhead → pipeline chars/4 currency
                                // (Phase 20 §2.3; mirrors the Ollama path).
                                messages: &messages,
                                budget: calibrate_down(
                                    new_budget.saturating_sub(tool_tokens_real),
                                    cal,
                                ),
                                max_messages: None,
                                replay_protected_tail_len: compress::protected_reasoning_tail_len(
                                    &messages,
                                    reasoning_replay_scope,
                                ),
                                task: active_task,
                                hard_budget: true,
                                authoritative: true,
                                focus: None,
                                est: estimation,
                                summary_input_cap_floor_chars,
                                compaction_store,
                            },
                            summarizer,
                            compress_state,
                        )
                        .await;
                        if let Some(notice) = outcome.notice {
                            print_harness_notice(&notice, color);
                        }
                        if outcome.action == CompressAction::Refused {
                            // Refuse the resend; surface the endpoint's 400.
                            return Err(e);
                        }
                        if outcome.fired {
                            messages = outcome.messages;
                            prompt_tracker.invalidate();
                            apply_post_compaction_continuation(
                                &mut messages,
                                &mut narration_nudges,
                                outcome.action,
                                step_ledger,
                                prompt_context,
                                round > 0,
                                action_nudges,
                            );
                            record_compaction_artifact(
                                artifact_sink,
                                artifact_context,
                                outcome.action,
                                outcome.tokens_before,
                                outcome.tokens_after,
                                calibrate_down(new_budget.saturating_sub(tool_tokens_real), cal),
                                round,
                                "context_window_400",
                                None,
                                false,
                                color,
                            );
                        }
                        cw_retries += 1;
                        continue 'round_loop;
                    }
                }
                return Err(e);
            }
        };
        // Merge per-round token usage (input = max single prompt, output =
        // sum — Step 18.1) and anchor the context-size tracker.
        let round_usage = openai_usage(&json["usage"]);
        if let Some(u) = round_usage {
            prompt_tracker.record(u.input_tokens, messages.len());
        }
        accumulated_usage = merge_round_usage(accumulated_usage, round_usage);

        // Phase 20 §2.2: no `num_ctx` on this wire, so there is no silent
        // head-truncation mode to suspect (oversize requests get a parseable
        // 400 instead). The declared local `num_ctx` still caps Newt's input
        // budget even though that field is not sent on this wire: accepting a
        // prompt with a short reply does not prove the same prompt leaves room
        // for the configured maximum output.
        let truncation_suspect = false;
        if let (Some(u), Some(budget), false) = (round_usage, send_budget, truncation_suspect) {
            let raised = capped_accepted_prompt_tokens(u.input_tokens, effective_input_ceiling);
            if raised > budget {
                send_budget = Some(raised);
                if debug {
                    print_debug(
                        &format!(
                            "send budget raised to ~{raised} tokens (backend accepted \
                             {}-token prompt)",
                            u.input_tokens
                        ),
                        color,
                    );
                }
            }
        }

        let message = &json["choices"][0]["message"];

        // #857: split the reasoning OFF the answer. A `<think>` block (reasoning
        // parser off) must never be returned or fed to the content-scrape recovery,
        // and the separate `reasoning_content` channel (reasoning parser on) is read
        // but never concatenated into the reply. Normal replies (no reasoning) are
        // unchanged: `split_reasoning` returns the content verbatim.
        let (oa_content, inline_reasoning) =
            crate::reasoning::split_reasoning(message["content"].as_str().unwrap_or(""));
        let separate_reasoning = message["reasoning_content"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if debug && (separate_reasoning.is_some() || inline_reasoning.is_some()) {
            let n = separate_reasoning
                .map(str::len)
                .or_else(|| inline_reasoning.as_deref().map(str::len))
                .unwrap_or(0);
            print_debug(
                &format!("reasoning ({n} chars) surfaced to the trace, not the answer"),
                color,
            );
        }
        let native_calls = message["tool_calls"].as_array();
        // Recover tool calls emitted as content instead of the native field —
        // the #1 weak-model failure (see `tool_recovery`). Mirror of the Ollama
        // loop: a local vLLM/llama.cpp server reports OpenAI-wire, so weak models
        // there drop content-emitted calls too. Recovered calls are native-shaped
        // and flow into the executor + is_hallucination path below.
        let recovered = if native_calls.map(|t| t.is_empty()).unwrap_or(true) {
            tool_recovery::recover_tool_calls(&oa_content)
        } else {
            tool_recovery::Recovery::default()
        };
        let tool_calls: Option<&Vec<serde_json::Value>> = match native_calls {
            Some(t) if !t.is_empty() => Some(t),
            _ if !recovered.calls.is_empty() => Some(&recovered.calls),
            _ => None,
        };
        let has_tools = tool_calls.map(|tc| !tc.is_empty()).unwrap_or(false);
        let finish_reason = json["choices"][0]["finish_reason"].as_str();
        if let Some(obs) = solve_obs.as_deref_mut() {
            obs.behavior_signals
                .push(observability::BehaviorSignal::ChatCompletionFinish {
                    round,
                    finish_reason: finish_reason.map(str::to_string),
                });
        }
        let reasoning_text = separate_reasoning.or(inline_reasoning.as_deref());
        let reasoning_overflow = observability::reasoning_overflow_signature(
            finish_reason,
            oa_content.is_empty(),
            reasoning_text.is_some(),
            has_tools,
        );

        // Resolve the pending telemetry record on the first response after a
        // continuation. A tool call or visible answer is a successful recovery;
        // another reasoning-only/empty response remains an honest failure.
        if reasoning_continuation_attempted && !reasoning_overflow {
            if has_tools || !oa_content.is_empty() {
                if let (Some(obs), Some(index)) =
                    (solve_obs.as_deref_mut(), reasoning_overflow_signal_index)
                {
                    if let Some(signal) = obs.behavior_signals.get_mut(index) {
                        signal.mark_continuation_succeeded();
                    }
                }
            }
            reasoning_overflow_signal_index = None;
        }

        if reasoning_overflow {
            let has_round_budget = round + 1 < current_tool_round_limit;
            let can_continue = generation_policy
                .allows_reasoning_continuation(reasoning_continuation_attempted, has_round_budget);

            let resolving_existing_continuation =
                reasoning_continuation_attempted && reasoning_overflow_signal_index.is_some();
            if !resolving_existing_continuation {
                if let Some(obs) = solve_obs.as_deref_mut() {
                    let index = obs.behavior_signals.len();
                    obs.behavior_signals
                        .push(observability::BehaviorSignal::ReasoningOverflow {
                            round,
                            reasoning_overflow_detected: true,
                            continuation_attempted: can_continue,
                            continuation_succeeded: false,
                            finish_reason: "length".into(),
                            reasoning_tokens_estimate: estimation.tokens_for_chars(
                                reasoning_text
                                    .map(|reasoning| reasoning.chars().count())
                                    .unwrap_or(0),
                            ),
                        });
                    reasoning_overflow_signal_index = Some(index);
                }
            }

            if can_continue {
                print_newt(
                    "reasoning reached the output limit before an answer — continuing once",
                    color,
                    false,
                );
                messages.push(prepare_openai_assistant_replay(
                    message,
                    &oa_content,
                    reasoning_replay_scope,
                    true,
                ));
                reasoning_continuation_attempted = true;
                continue 'round_loop;
            }

            let reason = if resolving_existing_continuation {
                "the bounded continuation also reached the output limit"
            } else if reasoning_continuation_attempted {
                "the turn already used its bounded continuation"
            } else if !generation_policy.one_bounded_reasoning_continuation {
                "the endpoint does not advertise bounded continuation"
            } else if reasoning_replay_scope == crate::model_card::ReasoningReplayScope::Never {
                "the endpoint does not allow current-turn reasoning replay"
            } else {
                "the turn has no remaining round budget"
            };
            print_newt(
                &format!("reasoning overflow detected — {reason}"),
                color,
                false,
            );
        }
        // W0 (#1511): served-model + parse-status observation for the solve
        // contract — mirror of the Ollama loop above.
        if let Some(obs) = solve_obs.as_deref_mut() {
            if let Some(m) = json["model"].as_str().filter(|m| !m.is_empty()) {
                obs.served_model = Some(m.to_string());
            }
            let native = native_calls.is_some_and(|t| !t.is_empty());
            if let Some(sig) = observability::round_parse_signal(
                round,
                !oa_content.is_empty(),
                native,
                recovered.dialect,
            ) {
                obs.parse_signals.push(sig);
            }
        }
        if debug && !recovered.calls.is_empty() {
            print_debug(
                &format!(
                    "recovered {} tool call(s) from content (non-native emission)",
                    recovered.calls.len()
                ),
                color,
            );
        }

        if debug {
            let excerpt: String = oa_content.chars().take(80).collect();
            let tc_count = tool_calls.map(|tc| tc.len()).unwrap_or(0);
            let usage_str = match round_usage {
                Some(u) => format!("{} in / {} out", u.input_tokens, u.output_tokens),
                None => "no usage".into(),
            };
            print_debug(
                &format!(
                    "round {round}: tool_calls={tc_count} usage=[{usage_str}] content={excerpt:?}"
                ),
                color,
            );
        }

        if !has_tools {
            // Format-hallucination tracker (mirror of the Ollama loop): content
            // that looked like a tool call but couldn't be recovered is counted.
            if recovered.tool_shaped {
                hallucination_count += 1;
                if debug {
                    print_debug(
                        "format-hallucination: tool call emitted as unrecoverable text",
                        color,
                    );
                }
            }
            let content = oa_content.clone();
            if content.is_empty() && debug {
                print_debug(
                    "empty content with no tool calls — model produced nothing",
                    color,
                );
            }
            // Narrate-then-stop rescue (mirror of the Ollama loop): non-empty
            // prose with no tool call. Nudge once and continue instead of ending
            // the turn — bounded by the configured narration_nudge_cap + the round budget.
            let nudge_classification =
                (!content.is_empty()).then(|| nudge_classifier.classify(&content));
            let workflow_classifier_text = workflow_classifier_text(&messages, &content);
            let workflow_hint = nudge_classification
                .as_ref()
                .filter(|classification| classification.is_plan_update())
                .and_then(|_| workflow_steerer.plan_update_hint(&workflow_classifier_text));
            let classifier_plan_direction = nudge_classification
                .as_ref()
                .filter(|classification| classification.is_plan_update())
                .and_then(|_| nudge_classifier.direction_for(crate::NudgeClass::PlanUpdate));
            let plan_nudge_hint =
                combine_nudge_hints(classifier_plan_direction, workflow_hint.as_deref());
            if !content.is_empty() && round + 1 < current_tool_round_limit && action_turn {
                if let Some(nudge) = workflow_runtime.rediscovery_nudge(
                    nudge_classification.as_ref(),
                    &content,
                    step_ledger,
                ) {
                    if debug {
                        print_debug(
                            "workflow evidence rediscovery — nudging toward active repair",
                            color,
                        );
                    }
                    messages.push(serde_json::json!({ "role": "assistant", "content": content }));
                    messages.push(serde_json::json!({
                        "role": "user",
                        "content": format!("{} {}", compress::LOOP_GUIDANCE_PREFIX, nudge)
                    }));
                    continue 'round_loop;
                }
            }
            if !content.is_empty()
                && pending_plan_nudges < PENDING_PLAN_NUDGE_CAP
                && round + 1 < current_tool_round_limit
                && action_turn
            {
                let needs_plan_update = nudge_classification
                    .as_ref()
                    .is_some_and(|c| c.is_plan_update());
                if let Some(nudge) = pending_plan_completion_nudge(
                    step_ledger,
                    needs_plan_update,
                    plan_nudge_hint.as_deref(),
                ) {
                    if debug {
                        print_debug(
                            "active plan has unfinished steps — nudging before final answer",
                            color,
                        );
                    }
                    messages.push(serde_json::json!({ "role": "assistant", "content": content }));
                    messages.push(serde_json::json!({
                        "role": "user",
                        "content": format!("{} {}", compress::LOOP_GUIDANCE_PREFIX, nudge)
                    }));
                    pending_plan_nudges += 1;
                    continue 'round_loop;
                }
            }
            if !content.is_empty()
                && stale_file_nudges < STALE_FILE_NUDGE_CAP
                && round + 1 < current_tool_round_limit
                && action_nudges
                && looks_like_unverified_stale_file_blocker(&content)
            {
                if debug {
                    print_debug(
                        "unverified stale-file blocker — nudging to check ground truth",
                        color,
                    );
                }
                messages.push(serde_json::json!({ "role": "assistant", "content": content }));
                messages.push(serde_json::json!({
                    "role": "user",
                    "content": format!(
                        "{} {}",
                        compress::LOOP_GUIDANCE_PREFIX,
                        stale_file_ground_truth_nudge()
                    ),
                }));
                stale_file_nudges += 1;
                continue 'round_loop;
            }
            if !content.is_empty()
                && narration_nudges < narration_nudge_cap
                && round + 1 < current_tool_round_limit
                && action_turn
                && nudge_classification
                    .as_ref()
                    .is_some_and(|c| c.is_pending_action())
            {
                if debug {
                    print_debug(
                        "narrated intent with no tool call — nudging to act and continuing",
                        color,
                    );
                }
                // #1158: ephemeral — replace the prior nudge, don't stack.
                strip_trailing_nudge_exchange(&mut messages);
                messages.push(serde_json::json!({ "role": "assistant", "content": content }));
                // First nudge: the (tunable) classifier direction. Later
                // nudges (cap > 1) escalate — name the active step, demand a
                // bare tool call (mirrors the Ollama loop).
                let direction = if narration_nudges == 0 {
                    nudge_classification
                        .as_ref()
                        .and_then(|classification| {
                            nudge_classifier.direction_for(classification.class)
                        })
                        .map(str::to_string)
                        .unwrap_or_else(narration_action_nudge)
                } else {
                    escalated_narration_action_nudge(
                        narration_nudges + 1,
                        narration_nudge_cap,
                        step_ledger,
                    )
                };
                messages.push(serde_json::json!({
                    "role": "user",
                    "content": format!("{} {}", compress::LOOP_GUIDANCE_PREFIX, direction),
                }));
                narration_nudges += 1;
                continue 'round_loop;
            }
            // Self-verify gate (#23): the model is concluding with usable content.
            // If the workspace ships a verification (tests / a make·just·npm·cargo
            // target / a command the instruction names) it NEVER ran this turn,
            // hand it one more round to run it before we accept the finish — the
            // measured #1 capability lever (models declare done on broken
            // solutions). Gated by the action-nudge switch (`/nudge off`) and
            // capped so a model that won't verify still ends the turn. The
            // `round + 1 < current_tool_round_limit` guard (cursor[bot], #1483)
            // mirrors the narration/stale-file nudges: on the FINAL round a
            // verify nudge would burn the pending answer into a cap-exit with
            // zero rounds left to actually run anything — step aside and accept.
            if self_verify::enabled()
                && action_nudges
                && self_verify_nudges < SELF_VERIFY_CAP
                && round + 1 < current_tool_round_limit
                && !content.is_empty()
            {
                let entries = self_verify::workspace_entries(std::path::Path::new(workspace));
                let checks = self_verify::detect_checks(&entries, active_task);
                let cmds = self_verify::commands_from_messages(&messages);
                if let Some(nudge) = self_verify::verify_gate_nudge(&checks, &cmds) {
                    if debug {
                        print_debug(
                            "concluding with unrun verification — self-verify nudge",
                            color,
                        );
                    }
                    strip_trailing_nudge_exchange(&mut messages);
                    messages.push(serde_json::json!({ "role": "assistant", "content": content }));
                    messages.push(serde_json::json!({
                        "role": "user",
                        "content": format!("{} {}", compress::LOOP_GUIDANCE_PREFIX, nudge),
                    }));
                    self_verify_nudges += 1;
                    continue 'round_loop;
                }
            }
            // Phase 20 §2.2: non-empty final content is usable output —
            // report the accepted prompt before returning.
            if !content.is_empty() {
                emit_accepted(
                    &mut on_round_usage,
                    round_usage,
                    truncation_suspect,
                    round_est_raw,
                );
            }
            // Acceptance forensics (mirrors the Ollama macro): record WHY
            // this no-tool reply ends the turn. #1261: includes the gate's
            // `action_turn` guard — where the rescue can never arm (non-Act
            // turn / non-action-inviting prompt), prose is the legitimate
            // ending, never a "budget spent" anomaly.
            let accepted_reason = if content.is_empty() {
                crate::TurnEndReason::Empty
            } else if nudge_classification
                .as_ref()
                .is_some_and(|c| c.is_pending_action())
                && action_turn
            {
                if round + 1 >= current_tool_round_limit {
                    crate::TurnEndReason::NarrationFinalRound
                } else {
                    crate::TurnEndReason::NarrationCapExhausted
                }
            } else {
                crate::TurnEndReason::Completed
            };
            if debug && accepted_reason != crate::TurnEndReason::Completed {
                print_debug(
                    &format!("no-tool reply accepted as final answer ({accepted_reason:?})"),
                    color,
                );
            }
            if let Some(slot) = &mut end_reason {
                **slot = Some(accepted_reason);
            }
            let out = if content.is_empty() {
                "(model returned an empty response — try rephrasing, or check the model with `newt doctor`)".to_string()
            } else {
                content
            };
            return Ok((out, false, accumulated_usage, hallucination_count));
        }

        // Record the assistant turn (it carries the tool_calls), then run each call
        // and feed the result back keyed by its tool_call_id.
        // Phase 20 §2.2: tool calls are usable output (mirrors the Ollama path).
        emit_accepted(
            &mut on_round_usage,
            round_usage,
            truncation_suspect,
            round_est_raw,
        );
        // #857: unknown endpoints keep the historical clean replay (no inline
        // <think> or reasoning_content). A capability-profiled reasoning backend
        // may instead retain the assistant's current-turn plan across tool rounds.
        // Some proxies omit the otherwise-required role; preparation also
        // canonicalizes that field before compression can inspect tool pairs.
        let assistant_turn =
            prepare_openai_assistant_replay(message, &oa_content, reasoning_replay_scope, true);
        messages.push(assistant_turn);
        let mut round_modified_workspace = false;
        let mut round_progress = false;
        let tcs = tool_calls.unwrap();
        // Phase 1 (invariant #3, BATCH level): validate the ENTIRE batch before
        // any side effect. Every OpenAI `tool_call` must have a non-empty, UNIQUE
        // `id` and a valid name/object-args; a bad sibling rejects the WHOLE batch
        // — echo the reason for each call (keyed by its id) and execute nothing,
        // so no valid call mutates the workspace ahead of an unvalidated batch.
        //
        // Some API proxies (NVIDIA inference → Anthropic backend) wrap
        // Anthropic-native tool-use blocks in the OpenAI `tool_calls` array
        // without converting the inner schema, so fall back to `name`/`input`
        // when the `function` key is absent.
        let extracted: Vec<(Option<&str>, Option<&str>, &serde_json::Value)> = tcs
            .iter()
            .map(|tc| {
                if tc["function"].is_null() {
                    (tc["id"].as_str(), tc["name"].as_str(), &tc["input"])
                } else {
                    (
                        tc["id"].as_str(),
                        tc["function"]["name"].as_str(),
                        &tc["function"]["arguments"],
                    )
                }
            })
            .collect();
        let validated = match tools::validate_tool_call_batch(&extracted, true) {
            Ok(v) => Some(v),
            Err(reason) => {
                for tc in tcs {
                    let id = tc["id"].as_str().unwrap_or("");
                    print_synthetic_tool_result(
                        "(rejected tool-call batch)",
                        &serde_json::Value::Null,
                        workspace,
                        &reason,
                        color,
                    );
                    if let Some(rec) = tool_events.as_deref_mut() {
                        rec.push(crate::ToolEvent::from_call(
                            "(rejected tool-call batch)",
                            &serde_json::Value::Null,
                            false,
                            Some(0),
                        ));
                    }
                    messages.push(serde_json::json!({
                        "role": "tool",
                        "tool_call_id": id,
                        "content": format!("tool-call batch rejected before execution: {reason}"),
                    }));
                }
                None
            }
        };
        // Phase 2: every call is valid — execute in order (empty when rejected).
        for (tc, vc) in tcs.iter().zip(validated.iter().flatten()) {
            let id = vc.call_id.as_str();
            let name = vc.name.as_str();
            let args = vc.args.clone();
            if debug && tc["function"].is_null() {
                print_debug(
                    &format!(
                        "tool call in Anthropic-native format inside tool_calls array \
                         (no `function` key) — name={name:?}"
                    ),
                    color,
                );
            }
            if trace {
                print_trace(
                    &format!(
                        "raw tool_call element: {}",
                        serde_json::to_string(tc).unwrap_or_else(|_| "?".into())
                    ),
                    color,
                );
            }
            let mcp_handles = mcp.handles(name);
            if debug {
                print_debug(
                    &format!("dispatching tool name={name:?} mcp_handles={mcp_handles}"),
                    color,
                );
            }
            if is_hallucination(name, &args) {
                hallucination_count += 1;
            }
            // Step 27.3/#771: short-circuit selected exact repeats (mirrors the
            // Ollama path; Responses uses function_call_output). Counted as a
            // hallucination above first when applicable.
            if let Some(steer) = repeat_calls.repeat_steer(name, &args) {
                print_synthetic_tool_result(name, &args, workspace, &steer, color);
                if let Some(rec) = tool_events.as_deref_mut() {
                    rec.push(crate::ToolEvent::from_call(name, &args, false, Some(0)));
                }
                messages.push(serde_json::json!({
                    "role": "tool",
                    "tool_call_id": id,
                    "content": steer,
                }));
                continue;
            }
            // Organic save_note use resets the memory-nudge counter (mirrors
            // the Ollama path).
            if name == "save_note" && note_sink.is_some() {
                if let Some(n) = note_nudge.as_deref_mut() {
                    n.note_saved();
                }
            }
            // retry technique: snapshot the file's pre-write bytes before the
            // write tool runs, so the post-turn gate can revert exactly newt's writes.
            ledger_note_write(write_ledger, name, &args, workspace);
            let tool_t0 = std::time::Instant::now();
            // #727: intercept the read-only budget self-read (see the Ollama path).
            // OpenAI-compatible endpoints do not receive `num_ctx`, but a local
            // endpoint's operator-declared window still provides the displayed
            // input ceiling. Cloud endpoints normally leave it unset.
            let result = if tools::is_context_remaining_call(name) {
                let report = budget::render_context_budget(
                    prompt_tracker.current(&messages, Some(&tools), cal, estimation),
                    effective_input_ceiling,
                    num_ctx,
                    input_ceiling_pct,
                    low_budget_pct,
                );
                print_synthetic_tool_result(name, &args, workspace, &report, color);
                report
            } else {
                let Some(result) = tools::execute_tool_with_collaborators(
                    name,
                    &args,
                    workspace,
                    color,
                    tool_output_lines,
                    caveats,
                    mcp,
                    tools::ToolCollaborators {
                        build_check_cmd: build_check_cmd.as_deref(),
                        // Reborrow + re-coerce: shortens the trait-object
                        // lifetime to this call (Option<&mut dyn _> is
                        // invariant, so the longer ChatCtx lifetime can't
                        // unify directly).
                        note_sink: note_sink
                            .as_deref_mut()
                            .map(|s| &mut *s as &mut dyn NoteSink),
                        recall_source,
                        memory_source,
                        prompt_context: Some(prompt_context),
                        artifact_context,
                        artifact_sink,
                        // #263 prompted grants — same reborrow pattern.
                        permission_gate: permission_gate
                            .as_deref_mut()
                            .map(|g| &mut *g as &mut dyn PermissionGate),
                        exec_floor,
                        git_tool,
                        crew_runner,
                        scratchpad_store,
                        code_search,
                        where_is,
                        nav,
                        experience_store,
                        step_ledger,
                        operating_mode_control,
                        plan_mode_control,
                        spill_store,
                        persona_tools,
                        live_tool_output: live_tool_output.clone(),
                    },
                    tool_offload,
                    prompt_disposition,
                    cancel,
                )
                .await
                else {
                    return Ok((String::new(), false, accumulated_usage, hallucination_count));
                };
                result
            };
            if debug {
                let excerpt: String = result.chars().take(120).collect();
                print_debug(&format!("tool result: {excerpt:?}"), color);
            }
            // 17.6: record the call for the turn's events column (mirrors
            // the Ollama path) — digested args, duration as a display claim.
            // Step 27.3/#771: classify once; remember repeat-steered outcomes
            // (mirrors Ollama path).
            let ok = tools::tool_result_ok(&result);
            if ok && is_workspace_write_call(name) {
                round_modified_workspace = true;
            }
            if ok && meaningful_workflow_progress(name, &result) {
                round_progress = true;
            }
            repeat_calls.record(name, &args, ok, &result);
            if workflow_runtime.record_tool_result(&result) {
                round_progress = true;
            }
            if let Some(rec) = tool_events.as_deref_mut() {
                rec.push(crate::ToolEvent::from_call(
                    name,
                    &args,
                    ok,
                    u64::try_from(tool_t0.elapsed().as_millis()).ok(),
                ));
            }
            // #717: record any phantom/capability reach (alias / hallucination
            // / real-tool empty miss) for the alias-seam telemetry. #479 (G4)
            // composes the gated-off seam here, where `advertise_team` is known:
            // a `crew`/`compose_roster` reach with the surface OFF is a real name
            // (so `classify_phantom_reach` never flags it) but exactly the
            // delegation signal we want to mine for the common OFF default.
            if let Some(pr) = phantom_reaches.as_deref_mut() {
                if let Some(resolution) = tools::classify_phantom_reach(name, &args, &result, ok)
                    .or_else(|| tools::classify_gated_off_reach(name, advertise_team))
                {
                    pr.push(crate::PhantomReach {
                        name_as_called: name.to_string(),
                        resolution,
                        active_context_features: Vec::new(),
                    });
                }
            }
            // #867 Part A: ledger verified paths (see the Ollama path).
            observed_paths.record(&result, &observed_resolver);
            messages.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": id,
                // Step 26.3 (#584): see the Ollama path.
                "content": maybe_offload_tool_result(name, result, tool_offload, spill_store),
            }));
        }
        workflow_runtime.record_round_outcome(round_modified_workspace, round_progress);
    }

    // Reached the round cap. Trim the message list and make ONE final
    // tools-disabled completion (matches the Ollama path).
    let protected_head = protected_prompt_head_len(&messages, prompt_read::ACTIVE_PROMPT_PREFIX);
    let replay_protected_tail_len =
        compress::protected_reasoning_tail_len(&messages, reasoning_replay_scope);
    let trimmed = trim_for_summary(&messages, protected_head, 6.max(replay_protected_tail_len));
    // Step 27.5: salvage progress + failed-call count (matches the Ollama path).
    let progress = cap_exit_progress(step_ledger, scratchpad_store);
    let (text, streamed, usage) = final_summary_openai(
        &client,
        &chat_url,
        model,
        api_key,
        trimmed,
        generation_policy,
        CapExit {
            max_tool_rounds,
            accumulated: accumulated_usage,
            wasted_calls: repeat_calls.total_failures(),
            progress,
            observed: observed_paths.into_vec(),
            request_budget: authoritative_request_budget(
                send_budget,
                send_budget_authoritative,
                mid_loop_trim_tokens,
            ),
            calibration: cal,
            estimation,
            ollama_num_ctx: None,
        },
    )
    .await?;
    // #867: same path-claim refutation as the Ollama cap exit.
    let text = claim_check::annotate_against_workspace(text, workspace);
    // #1214: the sibling check for claimed ACTIONS (commits, branches, pushes,
    // passing tests) — refuted against the workspace's real git state across
    // this turn. Fail-quiet off-repo (no evidence, no annotation).
    let text = claim_check::annotate_action_claims(
        text,
        claim_check::collect_git_evidence(workspace, turn_start_head.as_deref()).as_ref(),
    );
    if let Some(slot) = &mut end_reason {
        **slot = Some(crate::TurnEndReason::RoundCap);
    }
    Ok((text, streamed, usage, hallucination_count))
}

// ── OpenAI Responses API (`POST /v1/responses`) ────────────────────────────
//
// The newer OpenAI surface. Models like `gpt-5-codex` are served ONLY here and
// 404 on `/v1/chat/completions`. The request/response shapes differ (input vs
// messages, instructions vs system message, a flatter tool schema, `output[]`
// items vs `choices`, `input_tokens`/`output_tokens` usage), so this is a
// parallel — deliberately leaner — loop. Selected per backend via
// `api = "responses"` (surfaced to the loop as `NEWT_OPENAI_API`). Non-streaming
// in v1 (matching the chat path's UX); the chat path's budget / cw-400 recovery
// is intentionally not duplicated here yet (opt-in path) — tracked.

/// `true` when the active OpenAI backend selected the Responses API
/// (`[backends].api = "responses"`, surfaced to the loop as `NEWT_OPENAI_API`).
fn responses_api_selected() -> bool {
    std::env::var("NEWT_OPENAI_API")
        .ok()
        .is_some_and(|v| v.eq_ignore_ascii_case("responses"))
}

/// The Responses-wire `reasoning` object for a cognition level, or `None` to omit
/// it. The **single owner** of how the psyche `cognition` dial becomes a wire
/// field: the value mapping lives in [`Cognition::reasoning_effort`], the Responses
/// *shape* (`{"effort": …}`) lives here, so the chat-shape projection
/// (`reasoning_effort: …`, a follow-up once the chat bodies are consolidated)
/// reuses the same value without duplicating the ladder. `None` → the field is
/// never added, leaving the request bit-for-bit unchanged for non-opt-in callers.
fn responses_reasoning_field(
    cognition: Option<crate::role_profile::Cognition>,
) -> Option<serde_json::Value> {
    cognition.map(|c| serde_json::json!({ "effort": c.reasoning_effort() }))
}

/// Translate the chat/completions tool array (`{type:function,
/// function:{name,…}}` elements, as returned by `merged_tool_definitions`) to
/// the Responses API's flatter `{type:function, name, description, parameters}`.
/// An already-flat (or unknown) element passes through.
fn tools_to_responses(tools: &serde_json::Value) -> Vec<serde_json::Value> {
    tools
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|t| {
                    let f = &t["function"];
                    if f.is_object() {
                        // `parameters` is copied wholesale, so `required` /
                        // `additionalProperties` / nested schemas keep their exact
                        // validation semantics.
                        let mut tool = serde_json::json!({
                            "type": "function",
                            "name": f["name"],
                            "description": f["description"],
                            "parameters": f["parameters"],
                        });
                        // #1526 (invariant #6): a schema conversion must not
                        // silently change validation semantics. Chat Completions
                        // puts `strict` on the `function` object; the Responses API
                        // puts it at the tool's TOP level. Dropping it would
                        // downgrade a strict schema (additionalProperties:false +
                        // all-required enforced) to permissive — so the model could
                        // send args the strict schema rejects. Carry it through
                        // verbatim; absent stays absent (no spurious strictness).
                        if let Some(strict) = f.get("strict").filter(|s| !s.is_null()) {
                            tool["strict"] = strict.clone();
                        }
                        tool
                    } else {
                        t.clone()
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Extract `(assistant_text, function_call_items, echo_items)` from a Responses
/// reply's `output[]`. `text` is the concatenation of `output_text` parts inside
/// `message` items. `function_call_items` are the calls the loop executes.
/// `echo_items` is the ordered subsequence of `reasoning` AND `function_call`
/// items that must be echoed VERBATIM into the next request's `input`: a
/// reasoning model (gpt-5.6-sol, gpt-5-codex) pairs each `function_call` with a
/// preceding `reasoning` item (`rs_…`) and 400s ("function_call … without its
/// required 'reasoning' item") if the call is sent back without it. Preserving
/// original order keeps each reasoning item adjacent to its call. Falls back to a
/// flattened top-level `output_text` if the structured walk found no text.
/// The OpenAI **Responses API** agentic loop (`POST {endpoint}/v1/responses`).
/// Parallel to [`openai_chat_complete`] but over the Responses shapes, for
/// models served only there (`gpt-5-codex`). Non-streaming; selected via
/// `api = "responses"`. The chat path's budget / cw-400 recovery is not yet
/// mirrored here (opt-in path) — tracked as a follow-up.
pub async fn openai_responses_complete(
    ctx: ChatCtx<'_>,
    mcp: &mut dyn McpTools,
) -> anyhow::Result<(String, bool, Option<crate::TokenUsage>, u32)> {
    openai_responses_complete_with_prompt(ctx, None, None, mcp).await
}

pub async fn openai_responses_complete_with_prompt(
    ctx: ChatCtx<'_>,
    turn_prompt_context: Option<&crate::TurnPromptContext>,
    prompt_source: Option<&dyn PromptSource>,
    mcp: &mut dyn McpTools,
) -> anyhow::Result<(String, bool, Option<crate::TokenUsage>, u32)> {
    openai_responses_complete_with_prompt_and_artifacts(
        ctx,
        turn_prompt_context,
        prompt_source,
        None,
        None,
        mcp,
    )
    .await
}

async fn openai_responses_complete_with_prompt_and_artifacts(
    ctx: ChatCtx<'_>,
    turn_prompt_context: Option<&crate::TurnPromptContext>,
    prompt_source: Option<&dyn PromptSource>,
    artifact_source: Option<&dyn artifact_read::ArtifactSource>,
    artifact_sink: Option<&dyn artifact_read::PromptArtifactSink>,
    mcp: &mut dyn McpTools,
) -> anyhow::Result<(String, bool, Option<crate::TokenUsage>, u32)> {
    let ChatCtx {
        url,
        model,
        kind: _,
        api_key,
        messages: mem_messages,
        task,
        workspace,
        color,
        markdown: _,
        tool_offload,
        spill_store,
        compaction_store,
        scratchpad,
        scratchpad_store,
        code_search,
        where_is,
        nav,
        exposure,
        experience_store,
        step_ledger,
        caveats,
        persona_tools,
        cognition,
        chat_completions_capability: _,
        reasoning_replay_scope: _,
        max_tool_rounds,
        workflow_grace_rounds: _,
        narration_nudge_cap: _,
        action_nudges: _,
        prompt_disposition,
        prompt_intake,
        tool_output_lines,
        debug,
        trace,
        // #727: bound (not `_`) so get_context_remaining can report the budget;
        // on the Responses wire num_ctx is normally unset (cloud), so the report
        // is honestly ceiling-less.
        num_ctx,
        connect_timeout_secs,
        inference_timeout_secs,
        mid_loop_trim_threshold: _,
        compaction_trigger_policy: _,
        mid_loop_trim_tokens,
        max_ok_input,
        build_check_cmd,
        safe_context,
        recover_cw_400: _,
        mut note_sink,
        mut note_nudge,
        recall_source,
        memory_source,
        summarizer: _,
        compress_state: _,
        mut tool_events,
        mut phantom_reaches,
        end_reason: _,
        solve_obs: _,
        mut permission_gate,
        on_round_usage: _,
        estimate_ratio,
        // #727: bound for the get_context_remaining used-token estimate.
        estimation,
        summary_input_cap_floor_chars: _,
        input_ceiling_pct,
        low_budget_pct,
        exec_floor,
        write_ledger,
        cancel,
        live_tool_output,
        git_tool,
        crew_runner,
        operating_mode_control,
        plan_mode_control,
    } = ctx;
    let max_tool_rounds = prompt_disposition.tool_round_limit(max_tool_rounds);
    // The OpenAI-Responses loop offloads tool output (spill_store) but does not
    // run the compressor, so it never stores compaction spans.
    let _ = compaction_store;

    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(connect_timeout_secs))
        .timeout(std::time::Duration::from_secs(inference_timeout_secs))
        .build()?;
    let responses_url = format!("{}/v1/responses", url.trim_end_matches('/'));
    let retry = tui_retry_policy();
    let advertise_save_note = note_sink.is_some();
    let advertise_recall = recall_source.is_some();
    let advertise_memory_fetch = memory_source.is_some();
    // Step 26.4 (#583): state tools only when the feature is on AND a store exists.
    let advertise_scratchpad = scratchpad_store.is_some() && scratchpad;
    // Step 26.5.5 (#582): the code_search tool when a searcher is present.
    let advertise_code_search = code_search.is_some();
    // Step 26.6a (#585): the experiential tools when a store is present.
    let advertise_experiential = experience_store.is_some();
    // Step 26.6b (#586): the scheduled plan tools when a ledger is present.
    let advertise_scheduled = step_ledger.is_some();
    let advertise_git = git_tool.is_some();
    let advertise_team = crew_runner.is_some();
    let advertise_operating_mode = operating_mode_control.is_some();
    let advertise_plan_mode = plan_mode_control.is_some();
    let advertise_plan_mode_active =
        plan_mode_control.is_some_and(|control| control.is_plan_mode());

    let mut msgs_json: Vec<serde_json::Value> = mem_messages
        .iter()
        .map(|m| serde_json::json!({"role": m.role.as_str(), "content": m.content}))
        .collect();
    let ephemeral_prompt = turn_prompt_context.is_none().then(|| {
        crate::TurnPromptContext::ephemeral_operator(
            "ephemeral-headless",
            task.as_bytes().to_vec(),
            task.as_bytes().to_vec(),
        )
    });
    let turn_prompt_context = turn_prompt_context.or(ephemeral_prompt.as_ref());
    let prompt_context =
        prompt_read::PromptReadContext::new(turn_prompt_context, task, prompt_source);
    let artifact_context = turn_prompt_context
        .map(|turn| artifact_read::ArtifactReadContext::from_turn(turn, artifact_source));
    if let Some(intake) = prompt_intake {
        prompt_read::ensure_active_prompt_card_with_intake(&mut msgs_json, prompt_context, intake);
    } else {
        prompt_read::ensure_active_prompt_card(&mut msgs_json, prompt_context);
    }
    let (instructions, mut input) = crate::responses_wire::build_responses_input(&msgs_json);
    let tools_chat = merged_tool_definitions(
        mcp,
        advertise_save_note,
        advertise_recall,
        advertise_memory_fetch,
        advertise_git,
        advertise_team,
        advertise_scratchpad,
        advertise_code_search,
        advertise_experiential,
        advertise_scheduled,
        advertise_operating_mode,
        advertise_plan_mode,
        advertise_plan_mode_active,
    );
    // FR-1 part 2 (#997): scope the advertised catalog to the active persona
    // (Responses wire). No-op when `persona_tools` is `None`.
    let tools_chat = filter_advertised_tools(tools_chat, persona_tools);
    let tools_chat = filter_tools_for_disposition(tools_chat, prompt_disposition);
    // #TEC Pass 1: exposure stage on the chat-shaped catalog before it is
    // projected to Responses tools, so the estimate and the wire agree.
    // Identity under `ExposureProfile::Full`. The send budget is computed just
    // below on this wire, so derive the live budget inline here.
    // #1526 (invariant #4): the configured context window is a LOCAL safety
    // limit even though the Responses wire never carries `num_ctx` (provider-side
    // limits). Derive its input ceiling here and thread it through every local
    // budget below, so a request that would overflow a declared window is refused
    // pre-dispatch instead of relying solely on a reactive 400 / silent
    // truncation. `None` num_ctx (the cloud default) resolves to `None` and
    // leaves the prior behaviour unchanged. No output reserve on this wire (it
    // sends no `max_output_tokens`), so the ceiling is the percentage ceiling.
    let responses_input_ceiling = num_ctx_input_ceiling(num_ctx, input_ceiling_pct, None);
    let tools_chat = crate::agentic::tools::select_exposed(
        tools_chat,
        &exposure,
        exposure_budget_tokens(
            initial_send_budget(max_ok_input, safe_context, responses_input_ceiling),
            safe_context,
        ),
        &std::collections::BTreeSet::new(),
        estimation,
    );
    let tools = tools_to_responses(&tools_chat);
    let tools_for_estimate = serde_json::Value::Array(tools.clone());
    let cal = sanitize_estimate_ratio(estimate_ratio);
    let send_budget = initial_send_budget(max_ok_input, safe_context, responses_input_ceiling);
    // A declared window is authoritative just like a cached `safe_context`: both
    // are local safety limits the preflight may refuse against.
    let send_budget_authoritative = safe_context.is_some() || responses_input_ceiling.is_some();
    let authoritative_budget =
        authoritative_request_budget(send_budget, send_budget_authoritative, mid_loop_trim_tokens);
    preflight_irreducible_request(
        &msgs_json,
        Some(&tools_for_estimate),
        authoritative_budget,
        cal,
        estimation,
        model,
    )?;

    let mut accumulated_usage: Option<crate::TokenUsage> = None;
    let mut hallucination_count: u32 = 0;
    // Step 27.3/#771: guard against exact-repeat tool loops this run.
    let mut repeat_calls = RepeatCallGuard::default();
    let mut tools_supported = true;
    let mut tools_unsupported_notified = false;

    let reasoning = responses_reasoning_field(cognition);
    let build_body = |input: &[serde_json::Value], with_tools: bool| {
        // `store` is set EXPLICITLY (#1526, invariant #5): the Responses API
        // defaults it to `true` (server-side retention). Newt is stateless — it
        // replays the full history here and never uses `previous_response_id` —
        // so retention buys nothing and would leave an unaudited copy of the
        // operator's prompts/source/reasoning on the provider. Policy lives in
        // one place (`responses_wire::STORE_RESPONSE_SERVER_SIDE`).
        let mut body = serde_json::json!({
            "model": model,
            "input": input,
            "stream": false,
            "store": crate::responses_wire::STORE_RESPONSE_SERVER_SIDE,
        });
        if let Some(ins) = &instructions {
            body["instructions"] = serde_json::json!(ins);
        }
        // Psyche cognition → `reasoning.effort` (omitted entirely when unset).
        if let Some(reasoning) = &reasoning {
            body["reasoning"] = reasoning.clone();
        }
        if with_tools && !tools.is_empty() {
            body["tools"] = serde_json::json!(tools);
            body["tool_choice"] = serde_json::json!("auto");
        }
        body
    };

    for round in 0..max_tool_rounds {
        if is_cancelled(cancel) {
            return Ok((String::new(), false, accumulated_usage, hallucination_count));
        }
        preflight_responses_request(
            instructions.as_deref(),
            &input,
            tools_supported.then_some(tools.as_slice()),
            authoritative_budget,
            cal,
            estimation,
            model,
        )?;
        let body = build_body(&input, tools_supported);
        let dispatch = with_backoff_notify(
            &retry,
            || async {
                let mut req = client.post(&responses_url).json(&body);
                if let Some(key) = api_key {
                    req = req.bearer_auth(key);
                }
                // Typed classification at the source (W0 #1511).
                let resp = req.send().await.map_err(|e| {
                    anyhow::Error::new(observability::DispatchError::from_reqwest(
                        "request failed",
                        e,
                    ))
                })?;
                if !resp.status().is_success() {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    return Err(observability::DispatchError::http_status(format!(
                        "inference endpoint {status}: {text}"
                    ))
                    .into());
                }
                resp.json::<serde_json::Value>()
                    .await
                    .map_err(anyhow::Error::from)
            },
            |attempt, delay| print_retry_indicator(attempt, delay, color),
        )
        .await;

        let json = match dispatch {
            Ok(j) => j,
            Err(e) => {
                if tools_supported && is_tools_unsupported_error(&e) {
                    tools_supported = false;
                    if !tools_unsupported_notified {
                        tools_unsupported_notified = true;
                        print_newt(
                            &format!(
                                "{model} does not support tools — tools disabled for this session"
                            ),
                            color,
                            false,
                        );
                    }
                    continue;
                }
                return Err(e);
            }
        };

        // Fail-closed decode (invariant #2). A `200 OK` is NOT a completed turn:
        // only affirmative success output decodes to `Ok`. A refusal is the
        // model's final answer for this turn; every other error (provider error,
        // failed / incomplete / non-terminal status, malformed/empty body) is
        // surfaced — never mistaken for a benign empty reply.
        let decoded = match crate::responses_wire::decode_response(&json) {
            Ok(d) => d,
            Err(crate::responses_wire::ResponseDecodeError::Refused { message, usage }) => {
                accumulated_usage = merge_round_usage(accumulated_usage, usage);
                return Ok((
                    format!("(the model refused the request) {message}"),
                    false,
                    accumulated_usage,
                    hallucination_count,
                ));
            }
            Err(e) => return Err(anyhow::anyhow!("Responses turn not usable: {e}")),
        };
        accumulated_usage = merge_round_usage(accumulated_usage, decoded.usage);
        let (text, calls, echo) = (decoded.text, decoded.tool_calls, decoded.echo);

        if debug {
            let excerpt: String = text.chars().take(80).collect();
            print_debug(
                &format!(
                    "responses round {round}: function_calls={} content={excerpt:?}",
                    calls.len()
                ),
                color,
            );
        }

        if calls.is_empty() {
            // The decoder guarantees affirmative output here (text or calls); with
            // no calls, `text` is non-empty — return it as the turn's answer.
            return Ok((text, false, accumulated_usage, hallucination_count));
        }

        // Echo the model's reasoning + function_call items back into the running
        // input (in output order, so each call keeps its required reasoning item),
        // then run each call and append its function_call_output.
        for item in &echo {
            input.push(item.clone());
        }
        // Phase 1 (invariant #3, BATCH level): validate the ENTIRE batch before
        // any side effect. A malformed / idless / duplicate-id sibling rejects the
        // WHOLE batch — echo the reason as every call's `function_call_output` and
        // execute NOTHING, so no valid mutating call runs ahead of the untrusted
        // batch being known good. The function_call items were already echoed into
        // `input` above, so pairing each with an output keeps the wire well-formed.
        let extracted: Vec<(Option<&str>, Option<&str>, &serde_json::Value)> = calls
            .iter()
            .map(|c| {
                (
                    c["call_id"].as_str().or_else(|| c["id"].as_str()),
                    c["name"].as_str(),
                    &c["arguments"],
                )
            })
            .collect();
        let validated = match tools::validate_tool_call_batch(&extracted, true) {
            Ok(v) => v,
            Err(reason) => {
                for call in &calls {
                    let call_id = call["call_id"]
                        .as_str()
                        .or_else(|| call["id"].as_str())
                        .unwrap_or("");
                    print_synthetic_tool_result(
                        "(rejected tool-call batch)",
                        &serde_json::Value::Null,
                        workspace,
                        &reason,
                        color,
                    );
                    if let Some(rec) = tool_events.as_deref_mut() {
                        rec.push(crate::ToolEvent::from_call(
                            "(rejected tool-call batch)",
                            &serde_json::Value::Null,
                            false,
                            Some(0),
                        ));
                    }
                    input.push(serde_json::json!({
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": format!("tool-call batch rejected before execution: {reason}"),
                    }));
                }
                continue;
            }
        };
        // Phase 2: every call in the batch is valid — execute in order.
        for (call, vc) in calls.iter().zip(validated.iter()) {
            let call_id = vc.call_id.as_str();
            let name = vc.name.as_str();
            let args = vc.args.clone();
            if trace {
                print_trace(
                    &format!(
                        "raw function_call: {}",
                        serde_json::to_string(call).unwrap_or_else(|_| "?".into())
                    ),
                    color,
                );
            }
            if is_hallucination(name, &args) {
                hallucination_count += 1;
            }
            // Step 27.3/#771: short-circuit selected exact repeats (Responses
            // shape: echo a function_call_output with the steer).
            // Counted as a hallucination above first when applicable.
            if let Some(steer) = repeat_calls.repeat_steer(name, &args) {
                print_synthetic_tool_result(name, &args, workspace, &steer, color);
                if let Some(rec) = tool_events.as_deref_mut() {
                    rec.push(crate::ToolEvent::from_call(name, &args, false, Some(0)));
                }
                input.push(serde_json::json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": steer,
                }));
                continue;
            }
            if name == "save_note" && note_sink.is_some() {
                if let Some(n) = note_nudge.as_deref_mut() {
                    n.note_saved();
                }
            }
            ledger_note_write(write_ledger, name, &args, workspace);
            let tool_t0 = std::time::Instant::now();
            // #727: intercept the read-only budget self-read (see the Ollama path).
            // The Responses loop has no PromptTracker, so `used` is the chars/4
            // estimate of the running `input` plus tool schemas; num_ctx is normally
            // unset here, so the report is honestly ceiling-less.
            let result = if tools::is_context_remaining_call(name) {
                let report = budget::render_context_budget(
                    estimate_request_tokens(&input, Some(&tools_chat), estimation),
                    num_ctx_input_ceiling(num_ctx, input_ceiling_pct, None),
                    num_ctx,
                    input_ceiling_pct,
                    low_budget_pct,
                );
                print_synthetic_tool_result(name, &args, workspace, &report, color);
                report
            } else {
                let Some(result) = tools::execute_tool_with_collaborators(
                    name,
                    &args,
                    workspace,
                    color,
                    tool_output_lines,
                    caveats,
                    mcp,
                    tools::ToolCollaborators {
                        build_check_cmd: build_check_cmd.as_deref(),
                        // Reborrow + re-coerce: shortens the trait-object
                        // lifetime to this call (Option<&mut dyn _> is
                        // invariant, so the longer ChatCtx lifetime can't
                        // unify directly).
                        note_sink: note_sink
                            .as_deref_mut()
                            .map(|s| &mut *s as &mut dyn NoteSink),
                        recall_source,
                        memory_source,
                        prompt_context: Some(prompt_context),
                        artifact_context,
                        artifact_sink,
                        // #263 prompted grants — same reborrow pattern.
                        permission_gate: permission_gate
                            .as_deref_mut()
                            .map(|g| &mut *g as &mut dyn PermissionGate),
                        exec_floor,
                        git_tool,
                        crew_runner,
                        scratchpad_store,
                        code_search,
                        where_is,
                        nav,
                        experience_store,
                        step_ledger,
                        operating_mode_control,
                        plan_mode_control,
                        spill_store,
                        persona_tools,
                        live_tool_output: live_tool_output.clone(),
                    },
                    tool_offload,
                    prompt_disposition,
                    cancel,
                )
                .await
                else {
                    return Ok((String::new(), false, accumulated_usage, hallucination_count));
                };
                result
            };
            if debug {
                let excerpt: String = result.chars().take(120).collect();
                print_debug(&format!("tool result: {excerpt:?}"), color);
            }
            // Step 27.3/#771: classify once; remember repeat-steered outcomes
            // (mirrors Ollama path).
            let ok = tools::tool_result_ok(&result);
            repeat_calls.record(name, &args, ok, &result);
            if let Some(rec) = tool_events.as_deref_mut() {
                rec.push(crate::ToolEvent::from_call(
                    name,
                    &args,
                    ok,
                    u64::try_from(tool_t0.elapsed().as_millis()).ok(),
                ));
            }
            // #717: record any phantom/capability reach (alias / hallucination
            // / real-tool empty miss) for the alias-seam telemetry. #479 (G4)
            // composes the gated-off seam here, where `advertise_team` is known:
            // a `crew`/`compose_roster` reach with the surface OFF is a real name
            // (so `classify_phantom_reach` never flags it) but exactly the
            // delegation signal we want to mine for the common OFF default.
            if let Some(pr) = phantom_reaches.as_deref_mut() {
                if let Some(resolution) = tools::classify_phantom_reach(name, &args, &result, ok)
                    .or_else(|| tools::classify_gated_off_reach(name, advertise_team))
                {
                    pr.push(crate::PhantomReach {
                        name_as_called: name.to_string(),
                        resolution,
                        active_context_features: Vec::new(),
                    });
                }
            }
            input.push(serde_json::json!({
                "type": "function_call_output",
                "call_id": call_id,
                // Step 26.3 (#584): see the Ollama path (Responses output shape).
                "output": maybe_offload_tool_result(name, result, tool_offload, spill_store),
            }));
        }
    }

    // Round cap: one final tools-disabled call for a summary answer (mirrors
    // the chat path's final_summary, in the Responses shape).
    preflight_responses_request(
        instructions.as_deref(),
        &input,
        None,
        authoritative_budget,
        cal,
        estimation,
        model,
    )?;
    let body = build_body(&input, false);
    let mut req = client.post(&responses_url).json(&body);
    if let Some(key) = api_key {
        req = req.bearer_auth(key);
    }
    // Bare `?` keeps the raw typed reqwest error in the chain (the boundary's
    // `error_class` classifies it as a fallback); the status bail is typed.
    let resp = req.send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(observability::DispatchError::http_status(format!(
            "inference endpoint {status}: {text}"
        ))
        .into());
    }
    let json: serde_json::Value = resp.json().await?;
    // Fail-closed decode (invariant #2): this text-only follow-up must not return
    // a failed, truncated, or empty body's text as a successful reply. A refusal
    // is the model's final answer; every other error is surfaced.
    let decoded = match crate::responses_wire::decode_response(&json) {
        Ok(d) => d,
        Err(crate::responses_wire::ResponseDecodeError::Refused { message, usage }) => {
            accumulated_usage = merge_round_usage(accumulated_usage, usage);
            return Ok((
                format!("(the model refused the request) {message}"),
                false,
                accumulated_usage,
                hallucination_count,
            ));
        }
        Err(e) => return Err(anyhow::anyhow!("Responses turn not usable: {e}")),
    };
    accumulated_usage = merge_round_usage(accumulated_usage, decoded.usage);
    Ok((decoded.text, false, accumulated_usage, hallucination_count))
}

/// Whether the reasoning spinner is enabled: `NEWT_THINKING` (set by
/// `/thinking`) overrides `[tui] thinking`; default on.
fn thinking_stream_enabled() -> bool {
    match std::env::var("NEWT_THINKING").ok().as_deref() {
        Some("off") => return false,
        Some("on" | "stream") => return true,
        _ => {}
    }
    crate::Config::resolve()
        .ok()
        .and_then(|c| c.tui)
        .map(|t| t.thinking == crate::ThinkingMode::Stream)
        .unwrap_or(true)
}

/// Stream an Ollama NDJSON response, printing tokens as they arrive.
/// Returns `(accumulated_text, token_usage)`.
/// Token usage is extracted from the final chunk (`done: true`).
/// `show_thinking` opts into the cargo-style reasoning spinner (TTY only).
async fn stream_response(
    resp: reqwest::Response,
    color: bool,
    show_thinking: bool,
    leading_reasoning: bool,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    markdown: bool,
) -> anyhow::Result<(String, Option<crate::TokenUsage>)> {
    // The ONE spinner (`newt_core::tty`). `legacy_caps` preserves today's
    // gating exactly; the shared 100ms OS-thread ticker replaces the old
    // advance-only-on-a-reasoning-chunk clock, so a model STALL now shows a
    // live glyph instead of a frozen one — the "looks hung" signature.
    let mut spinner = crate::tty::Spinner::start_with_caps(
        legacy_caps(show_thinking),
        "thinking…",
        crate::tty::Sink::Stdout,
        color,
    );
    let mut full = String::new();
    let mut started = false;
    let mut usage: Option<crate::TokenUsage> = None;
    // Step 25.3 (#568): when markdown is active, route the *visible* token stream
    // through the block-aware writer (inline lines render per completed line;
    // fences/tables hold until they close). The accumulated `full` stays RAW —
    // it is persisted and re-sent to the model, so it must carry no ANSI. The
    // caller gates `markdown` on `color`, so the writer renders with `color: true`.
    let cols = display::term_cols();
    let mut md =
        markdown.then(|| MarkdownStreamWriter::new(io::stdout(), RenderOpts { color: true, cols }));
    // #385: suppress inline <think>…</think> reasoning from the live stream + the
    // accumulated reply, even when a tag is split across token boundaries.
    // #528: models that emit a lone leading `</think>` (no opener) start the
    // filter *inside* the reasoning block so the closer + reasoning don't leak.
    let mut think = if leading_reasoning {
        crate::reasoning::ThinkFilter::with_leading_reasoning()
    } else {
        crate::reasoning::ThinkFilter::new()
    };

    let mut resp = resp;
    // Race each chunk read against the interrupt flag so Esc stops the token
    // stream promptly; on interrupt, stop reading and return what we have.
    while let Some(chunk) = match cancellable(cancel, resp.chunk()).await {
        Some(c) => c?,
        None => None,
    } {
        let text = String::from_utf8_lossy(&chunk);
        for line in text.lines() {
            if line.is_empty() {
                continue;
            }
            let Ok(json) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let raw = json["message"]["content"].as_str().unwrap_or("");
            let (token, reasoning) = think.feed_split(raw);
            // Surface reasoning live (cargo-style) — both the inline `<think>`
            // span the filter just split out AND any separate `thinking` field.
            if let Some(sp) = spinner.as_ref() {
                if !reasoning.is_empty() {
                    sp.detail(&reasoning);
                }
                if let Some(t) = json["message"]["thinking"].as_str() {
                    if !t.is_empty() {
                        sp.detail(t);
                    }
                }
            }
            let token = token.as_str();
            if !token.is_empty() {
                if !started {
                    // The answer is starting — tear the spinner down first.
                    drop(spinner.take());
                    if color {
                        execute!(
                            io::stdout(),
                            SetForegroundColor(NEWT_ORANGE_CT),
                            Print("▸  "),
                            ResetColor,
                        )
                        .ok();
                    } else {
                        print!("▸  ");
                    }
                    started = true;
                }
                if let Some(w) = md.as_mut() {
                    w.push(token).ok();
                } else {
                    print!("{token}");
                    io::stdout().flush().ok();
                }
                full.push_str(token);
            }
            if json["done"].as_bool().unwrap_or(false) {
                // Extract token counts from the final Ollama chunk.
                let input = json["prompt_eval_count"].as_u64().map(|n| n as u32);
                let output = json["eval_count"].as_u64().map(|n| n as u32);
                usage = input.zip(output).map(|(i, o)| crate::TokenUsage {
                    input_tokens: i,
                    output_tokens: o,
                });
                break;
            }
        }
    }
    // #385: flush any clean tail the filter held back (a trailing run that turned out
    // not to be the start of a `<think>` tag).
    let tail = think.finish();
    if !tail.is_empty() {
        if !started {
            drop(spinner.take());
            print!("▸  ");
            started = true;
        }
        if let Some(w) = md.as_mut() {
            w.push(&tail).ok();
        } else {
            print!("{tail}");
            io::stdout().flush().ok();
        }
        full.push_str(&tail);
    }
    // All-reasoning response (no clean content): tear the spinner down anyway so
    // the terminal isn't left mid-spinner. (Belt-and-braces: `Drop` covers every
    // path out of this function, INCLUDING the `?` on a mid-stream transport
    // error above, which used to skip every hand-placed `finish()` and leave a
    // glyph on screen with no live owner.)
    drop(spinner.take());
    // The markdown writer newline-terminates each line it emits, so it owns the
    // trailing newline; only the raw path needs the closing `println!`.
    if let Some(w) = md.as_mut() {
        w.finish().ok();
    }
    if started && md.is_none() {
        println!();
    }
    Ok((full, usage))
}

#[cfg(test)]
mod repeat_call_guard_tests {
    use super::*;

    #[test]
    fn loop_owned_tool_results_share_one_complete_spill_block() {
        let mut repeated = Vec::new();
        {
            let mut display = display::ToolDisplay::new(&mut repeated, false, 80, 3);
            present_synthetic_tool_result(
                &mut display,
                "find",
                &serde_json::json!({"path": ".", "name": "*.rs", "type": "f"}),
                std::path::Path::new("."),
                "a.rs\nb.rs\nc.rs\nd.rs",
            );
        }
        assert_eq!(
            String::from_utf8(repeated).unwrap(),
            "⚙  find: . (name=*.rs, type=f)\n\
             ▲ 1 more lines above · /spill N raises this view\n\
             ▒ b.rs\n\
             ▒ c.rs\n\
             ▓ d.rs\n\
             …\n"
        );

        let mut budget = Vec::new();
        {
            let mut display = display::ToolDisplay::new(&mut budget, false, 80, 3);
            present_synthetic_tool_result(
                &mut display,
                "tokens_left",
                &serde_json::json!({}),
                std::path::Path::new("."),
                "context budget: 75% remaining",
            );
        }
        assert_eq!(
            String::from_utf8(budget).unwrap(),
            "⚙  get_context_remaining: \n\
             ▒ context budget: 75% remaining\n\
             …\n"
        );
    }

    #[test]
    fn short_circuits_exact_repeat_and_escalates() {
        let mut g = RepeatCallGuard::default();
        let args = serde_json::json!({"command": "mkdir x"});
        // First sight of the call → let it run (no steer).
        assert!(g.repeat_steer("run_command", &args).is_none());
        // After a failure, an exact repeat is steered, quoting the prior error.
        g.record("run_command", &args, false, "error: shell unavailable");
        let s = g.repeat_steer("run_command", &args).expect("repeat steers");
        assert!(s.contains("already called"), "{s}");
        assert!(s.contains("error: shell unavailable"), "{s}");
        assert!(
            !s.contains("stop using"),
            "one failure → no escalation yet: {s}"
        );
        // A second (distinct-args) failure of the same tool crosses ESCALATE_AFTER.
        g.record(
            "run_command",
            &serde_json::json!({"command": "ls"}),
            false,
            "error: denied",
        );
        let s2 = g.repeat_steer("run_command", &args).expect("still steers");
        assert!(s2.contains("stop using"), "escalates: {s2}");
    }

    #[test]
    fn ignores_successes_and_distinct_calls() {
        let mut g = RepeatCallGuard::default();
        let a = serde_json::json!({"path": "f.rs"});
        g.record("read_file", &a, true, "file contents"); // success → not remembered
        assert!(g.repeat_steer("read_file", &a).is_none());
        // A failure under different args does not short-circuit a distinct call.
        let b = serde_json::json!({"path": "g.rs"});
        g.record("read_file", &b, false, "error reading g.rs");
        assert!(
            g.repeat_steer("read_file", &a).is_none(),
            "distinct args still run"
        );
        assert!(g.repeat_steer("read_file", &b).is_some());
    }

    #[test]
    fn steers_no_result_repeats_on_second_issuance() {
        // #718: a success-shaped no-result that the model re-issues byte-for-byte
        // is steered on its 2nd call — distinct from a hard failure (no escalation),
        // distinct from a genuine success (which is never steered).
        let mut g = RepeatCallGuard::default();

        // recall "no matches" — first sight runs; record it; the identical 2nd
        // issuance is steered before re-execution.
        let q = serde_json::json!({"query": "newt-tui PyO3 bindings"});
        assert!(
            g.repeat_steer("recall", &q).is_none(),
            "first recall must run"
        );
        g.record(
            "recall",
            &q,
            true,
            "no matches in past conversations for \"newt-tui PyO3 bindings\" — try different keywords.",
        );
        let s = g
            .repeat_steer("recall", &q)
            .expect("2nd identical recall steers");
        assert!(s.contains("no matches"), "{s}");
        assert!(
            s.contains("resume_context"),
            "recall steer points at resume_context: {s}"
        );
        assert!(
            !s.contains("stop using"),
            "a no-result is not a hard failure — no escalation: {s}"
        );

        // state_get "no such key" — same: 2nd identical probe is steered.
        let k = serde_json::json!({"key": "current_task"});
        assert!(g.repeat_steer("state_get", &k).is_none());
        g.record("state_get", &k, true, "no such key: current_task");
        assert!(
            g.repeat_steer("state_get", &k).is_some(),
            "2nd identical state_get steers"
        );

        // plan_get empty ledger — same: the second identical read is steered
        // toward creating the missing plan instead of polling the empty ledger.
        let empty_plan_args = serde_json::json!({});
        assert!(g.repeat_steer("plan_get", &empty_plan_args).is_none());
        g.record(
            "plan_get",
            &empty_plan_args,
            true,
            "no active plan — if this is multi-step work, call update_plan next",
        );
        let plan_steer = g
            .repeat_steer("plan_get", &empty_plan_args)
            .expect("2nd identical empty plan_get steers");
        assert!(plan_steer.contains("update_plan"), "{plan_steer}");

        // A genuine success with content is still NEVER steered on repeat.
        let f = serde_json::json!({"path": "f.rs"});
        g.record("read_file", &f, true, "file contents");
        assert!(g.repeat_steer("read_file", &f).is_none());

        // A no-result under DIFFERENT args is a distinct call — let it run.
        let q2 = serde_json::json!({"query": "something else entirely"});
        assert!(
            g.repeat_steer("recall", &q2).is_none(),
            "distinct recall args still run"
        );
    }

    #[test]
    fn steers_duplicate_successful_web_fetch() {
        let mut g = RepeatCallGuard::default();
        let issue = serde_json::json!({
            "url": "https://github.com/Gilamonster-Foundation/newt-agent/issues/771"
        });

        assert!(
            g.repeat_steer("web_fetch", &issue).is_none(),
            "first fetch must run"
        );
        g.record("web_fetch", &issue, true, "# Issue\n\nbody");
        let steer = g
            .repeat_steer("web_fetch", &issue)
            .expect("2nd identical successful fetch steers");
        assert!(steer.contains("already observed"), "{steer}");
        assert!(steer.contains("`web_fetch`"), "{steer}");
        assert!(
            steer.contains("https://github.com/Gilamonster-Foundation/newt-agent/issues/771"),
            "{steer}"
        );
        assert!(
            g.repeat_steer(
                "web_fetch",
                &serde_json::json!({"url": "https://github.com/hartsock/scrybe"})
            )
            .is_none(),
            "distinct URLs still run"
        );

        let file = serde_json::json!({"path": "src/lib.rs"});
        g.record("read_file", &file, true, "file contents");
        assert!(
            g.repeat_steer("read_file", &file).is_none(),
            "ordinary successful reads are still not steered"
        );
    }

    #[test]
    fn steers_duplicate_successful_read_only_run_command() {
        let mut g = RepeatCallGuard::default();
        let args = serde_json::json!({
            "command": "grep -n 'help_lines' /Users/shawnhartsock/workspaces/newt-agent/newt-tui/src/lib.rs"
        });

        assert!(
            g.repeat_steer("run_command", &args).is_none(),
            "first grep should run"
        );
        g.record(
            "run_command",
            &args,
            true,
            "9439:fn help_lines() -> &'static [&'static str] {",
        );

        let steer = g
            .repeat_steer("run_command", &args)
            .expect("second identical grep should steer");
        assert!(steer.contains("already observed"), "{steer}");
        assert!(steer.contains("read-only shell probe"), "{steer}");
        assert!(steer.contains("`run_command`"), "{steer}");
        assert!(steer.contains("grep -n"), "{steer}");
        assert!(steer.contains("Do NOT repeat"), "{steer}");
    }

    #[test]
    fn does_not_steer_successful_write_capable_run_command() {
        let mut g = RepeatCallGuard::default();
        let args = serde_json::json!({"command": "cargo test -p newt-tui"});

        g.record("run_command", &args, true, "test result: ok");

        assert!(
            g.repeat_steer("run_command", &args).is_none(),
            "successful build/test commands are still repeatable"
        );
    }

    #[test]
    fn classifier_leaves_ordinary_successes_repeatable() {
        let file = serde_json::json!({"path": "src/lib.rs"});
        assert_eq!(
            RepeatCallGuard::classify_repeat_memo("read_file", &file, true, "file contents"),
            None
        );

        let tests = serde_json::json!({"command": "cargo test -p newt-core"});
        assert_eq!(
            RepeatCallGuard::classify_repeat_memo("run_command", &tests, true, "test result: ok"),
            None
        );

        let mut g = RepeatCallGuard::default();
        g.record("read_file", &file, true, "file contents");
        g.record("run_command", &tests, true, "test result: ok");
        assert!(
            g.repeat_memos.is_empty(),
            "ordinary successful calls must stay repeatable"
        );
    }

    #[test]
    fn workflow_error_fingerprint_captures_cargo_location() {
        let output = r#"
error[E0425]: cannot find value `SECTION_PROMPT_TOKENS` in this scope
   --> newt-tui/src/help_sections.rs:523:22
    |
523 |         lines: SECTION_PROMPT_TOKENS,
    |                ^^^^^^^^^^^^^^^^^^^^^ help: a static with a similar name exists: `SECTION_PROMPT`
"#;

        let fp = build_error_fingerprint(output).expect("cargo error should fingerprint");

        assert!(fp.contains("newt-tui/src/help_sections.rs:523:22"), "{fp}");
        assert!(fp.contains("error[E0425]"), "{fp}");
        assert!(fp.contains("SECTION_PROMPT_TOKENS"), "{fp}");
    }

    #[test]
    fn tenacity_action_forcing_nudge_fires_at_the_budget_and_resets_on_a_write() {
        // #tenacity: the action-forcing nudge fires once the model has spent the
        // tenacity level's budget of consecutive read-only rounds, and a
        // workspace write resets the counter. This is what gives the OpenAI-chat
        // loop (which had no read-only nudge) a push from reading to acting.
        let mut state = WorkflowRuntimeState {
            tenacity: crate::tenacity::Tenacity::Relentless, // budget 1
            ..Default::default()
        };
        // Nothing spent yet → no nudge.
        assert!(state.action_forcing_nudge(5, None, None).is_none());
        // One read-only round → at the Relentless budget → fires.
        state.record_round_outcome(false, false);
        let nudge = state
            .action_forcing_nudge(5, None, None)
            .expect("relentless tenacity must force action after one read-only round");
        assert!(nudge.contains("edit_file or write_file"), "{nudge}");
        // Firing resets the counter; a follow-up read-only round re-accumulates.
        assert!(state.action_forcing_nudge(5, None, None).is_none());
        state.record_round_outcome(false, false);
        assert!(state.action_forcing_nudge(5, None, None).is_some());
        // A workspace-write round clears the counter entirely.
        state.record_round_outcome(true, true);
        assert!(
            state.action_forcing_nudge(5, None, None).is_none(),
            "a write must reset the read-only streak"
        );

        // Standard tenacity preserves the historical budget of 3.
        let mut standard = WorkflowRuntimeState::default();
        for _ in 0..2 {
            standard.record_round_outcome(false, false);
        }
        assert!(
            standard.action_forcing_nudge(5, None, None).is_none(),
            "standard must not fire before 3 read-only rounds"
        );
        standard.record_round_outcome(false, false);
        assert!(standard.action_forcing_nudge(5, None, None).is_some());
    }

    #[test]
    fn workflow_runtime_nudges_after_error_without_writes() {
        let output = r#"
error[E0425]: cannot find value `SECTION_PROMPT_TOKENS` in this scope
   --> newt-tui/src/help_sections.rs:523:22
"#;
        let mut state = WorkflowRuntimeState::default();

        state.record_tool_result(output);
        state.record_round_outcome(false, false);

        let nudge = state
            .round_start_nudge(None)
            .expect("read-only round after evidence should lock the active repair");
        assert!(nudge.contains("<workflow_state>"), "{nudge}");
        assert!(
            nudge.contains("newt-tui/src/help_sections.rs:523:22"),
            "{nudge}"
        );
        assert!(nudge.contains("next_allowed_actions"), "{nudge}");
        assert!(nudge.contains("disallowed_actions"), "{nudge}");

        let classification = crate::NudgeClassification {
            class: crate::NudgeClass::PlanUpdate,
            score: 1.0,
        };
        let rediscovery = state
            .rediscovery_nudge(
                Some(&classification),
                "Summary of Findings\nRoot Cause: the build failure is still present.",
                None,
            )
            .expect("classified summary should be steered toward action");
        assert!(
            rediscovery.contains("Do not restate findings"),
            "{rediscovery}"
        );
        assert!(
            rediscovery.contains("newt-tui/src/help_sections.rs:523:22"),
            "{rediscovery}"
        );
    }

    #[test]
    fn workflow_runtime_tracks_failed_edit_as_unresolved_evidence() {
        let output = "error: old_string not found in newt-tui/src/help_sections.rs";
        let mut state = WorkflowRuntimeState::default();

        state.record_tool_result(output);
        state.record_round_outcome(false, false);

        let nudge = state
            .round_start_nudge(None)
            .expect("failed edit should remain unresolved repair evidence");
        assert!(nudge.contains("old_string not found"), "{nudge}");

        let grace = state
            .cap_grace_nudge(None, 25, 5)
            .expect("cap after failed edit/read-only recovery should grant an action round");
        assert!(
            grace.contains("configured_workflow_grace_rounds = 5"),
            "{grace}"
        );
        assert!(
            grace.contains("call edit_file or write_file now"),
            "{grace}"
        );
        assert!(
            state.cap_grace_nudge(None, 25, 0).is_none(),
            "configured zero grace disables soft cap extension"
        );

        state.record_round_outcome(true, true);
        let verify = state
            .cap_grace_nudge(None, 25, 3)
            .expect("a successful edit at the cap should get a verification window");
        assert!(verify.contains("focused verification"), "{verify}");
        assert!(
            verify.contains("configured_workflow_grace_rounds = 3"),
            "{verify}"
        );
    }

    #[test]
    fn workflow_runtime_grants_configured_grace_for_recent_plan_progress() {
        let ledger = SessionStepLedger::default();
        ledger.set_plan(&["finish round-cap grace".to_string(), "verify".to_string()]);
        let mut state = WorkflowRuntimeState::default();

        state.record_round_outcome(false, true);

        let nudge = state
            .cap_grace_nudge(Some(&ledger), 2, 4)
            .expect("recent active-plan progress should activate configured grace");
        assert!(
            nudge.contains("configured_workflow_grace_rounds = 4"),
            "{nudge}"
        );
        assert!(nudge.contains("finish round-cap grace"), "{nudge}");
        assert!(
            state.cap_grace_nudge(Some(&ledger), 2, 0).is_none(),
            "zero configured grace keeps the cap hard"
        );
    }

    /// #<issue>: a diagnostic workflow (e.g. `diagnose_failure.toml`,
    /// `progress_horizon_rounds = 6`) legitimately spends more read-only
    /// rounds between plan checkpoints than a routine edit does. Without a
    /// horizon override, 4 rounds since the last checkpoint already exceeds
    /// the shared default (`WORKFLOW_RECENT_PROGRESS_ROUNDS = 3`) and grace
    /// does NOT activate — RED on the pre-fix behavior. Setting the override
    /// widens the window so the same 4-rounds-stale state still counts as
    /// "recent" — GREEN.
    #[test]
    fn progress_horizon_override_widens_the_recent_progress_window() {
        let ledger = SessionStepLedger::default();
        ledger.set_plan(&["diagnose the failure".to_string(), "fix it".to_string()]);

        let mut default_horizon = WorkflowRuntimeState::default();
        default_horizon.record_round_outcome(false, true); // a checkpoint...
        for _ in 0..4 {
            default_horizon.record_round_outcome(false, false); // ...then 4 idle rounds
        }
        assert!(
            default_horizon
                .cap_grace_nudge(Some(&ledger), 2, 4)
                .is_none(),
            "4 rounds since the last checkpoint exceeds the default 3-round horizon"
        );

        let mut widened = WorkflowRuntimeState::default();
        widened.set_progress_horizon(Some(6));
        widened.record_round_outcome(false, true);
        for _ in 0..4 {
            widened.record_round_outcome(false, false);
        }
        assert!(
            widened.cap_grace_nudge(Some(&ledger), 2, 4).is_some(),
            "a widened 6-round horizon still treats 4-rounds-stale as recent progress"
        );
    }

    #[test]
    fn workspace_write_classifier_is_narrow() {
        assert!(is_workspace_write_call("edit_file"));
        assert!(is_workspace_write_call("write_file"));
        assert!(!is_workspace_write_call("run_command"));
        assert!(!is_workspace_write_call("read_file"));
    }

    #[test]
    fn no_result_reason_classifies_and_routes() {
        // recall / state_get no-result prefixes classify…
        assert!(RepeatCallGuard::no_result_reason(
            "recall",
            "no matches in past conversations for \"x\" — try different keywords."
        )
        .is_some_and(|r| r.contains("no matches") && r.contains("resume_context")));
        assert!(
            RepeatCallGuard::no_result_reason("state_get", "no such key: current_task")
                .is_some_and(|r| r.contains("not set"))
        );
        assert!(
            RepeatCallGuard::no_result_reason("plan_get", "no active plan — call update_plan")
                .is_some_and(|r| r.contains("update_plan"))
        );
        // …a real success with content does not.
        assert!(
            RepeatCallGuard::no_result_reason("recall", "3 match(es) in past conversations")
                .is_none()
        );
        assert!(RepeatCallGuard::no_result_reason("read_file", "file contents").is_none());

        // A recall ERROR (ok=false) goes through the FAILURE path, not no-result
        // classification: it lands in repeat_memos as escalation-eligible.
        let mut g = RepeatCallGuard::default();
        let q = serde_json::json!({"query": "x"});
        g.record("recall", &q, false, "error: index unavailable");
        assert!(matches!(
            g.repeat_memos.get(&RepeatCallGuard::key("recall", &q)),
            Some(RepeatMemo::Failure { first_line }) if first_line == "error: index unavailable"
        ));
    }

    #[test]
    fn first_line_caps_and_takes_first() {
        assert_eq!(first_line("one\ntwo\nthree"), "one");
        assert_eq!(first_line(""), "");
        assert_eq!(first_line(&"x".repeat(500)).chars().count(), 200);
    }
}

#[cfg(test)]
mod cap_exit_unit_tests {
    use super::*;

    #[test]
    fn cap_exit_nudge_names_the_limit_and_folds_in_progress() {
        let nudge = cap_exit_nudge(5, None, &[]);
        assert!(nudge.contains("5 rounds"), "got: {nudge}");
        assert!(nudge.contains("Do NOT call any more tools"));
        // #867: the grounding constraint — the trim just deleted the evidence,
        // so the nudge must forbid reconstructing paths from memory.
        assert!(
            nudge.contains("Cite only file paths that appear verbatim"),
            "got: {nudge}"
        );
        assert!(nudge.contains("say so plainly"), "got: {nudge}");
        assert!(
            !nudge.contains("progress so far"),
            "no block when None: {nudge}"
        );
        assert!(
            !nudge.contains("actually observed"),
            "no manifest block when the ledger is empty: {nudge}"
        );
        // Step 27.5: the <plan>/<state> progress is folded into the nudge.
        let with = cap_exit_nudge(5, Some("<plan>1. [x] foo</plan>"), &[]);
        assert!(with.contains("Your progress so far"), "got: {with}");
        assert!(with.contains("<plan>1. [x] foo</plan>"), "got: {with}");
    }

    /// #867 Part A: the observed-paths manifest survives the trim and is
    /// handed to the model as the citable ground truth.
    #[test]
    fn cap_exit_nudge_folds_in_the_observed_paths_manifest() {
        let observed = vec![
            "newt-tui/src/lib.rs".to_string(),
            "newt-core/src/agentic/mod.rs".to_string(),
        ];
        let nudge = cap_exit_nudge(5, Some("<state>k=v</state>"), &observed);
        assert!(
            nudge.contains("File paths actually observed in tool results"),
            "got: {nudge}"
        );
        assert!(nudge.contains("- newt-tui/src/lib.rs"), "got: {nudge}");
        assert!(
            nudge.contains("- newt-core/src/agentic/mod.rs"),
            "got: {nudge}"
        );
        // Manifest precedes the progress block; both survive together.
        let manifest_at = nudge.find("actually observed").unwrap();
        let progress_at = nudge.find("Your progress so far").unwrap();
        assert!(manifest_at < progress_at, "got: {nudge}");
    }

    #[test]
    fn cap_exit_progress_renders_plan_and_state_or_none() {
        use crate::agentic::scheduled::{SessionStepLedger, StepLedger};
        use crate::agentic::scratchpad::{ScratchpadStore, SessionScratchpadStore};
        let ledger = SessionStepLedger::default();
        let pad = SessionScratchpadStore::default();
        // Both empty → nothing to salvage.
        assert!(cap_exit_progress(Some(&ledger), Some(&pad)).is_none());
        assert!(cap_exit_progress(None, None).is_none());
        // Populated → a combined block naming both.
        ledger.set_plan(&["build it".to_string(), "test it".to_string()]);
        pad.set("cwd", "/work".to_string());
        let p = cap_exit_progress(
            Some(&ledger as &dyn StepLedger),
            Some(&pad as &dyn ScratchpadStore),
        )
        .expect("non-empty progress");
        assert!(p.contains("build it"), "{p}");
        assert!(p.contains("cwd"), "{p}");
    }

    #[test]
    fn cap_exit_fallback_usage_advice_and_salvage() {
        // wasted_calls < rounds → the standard "raise max_tool_rounds" advice.
        let with = cap_exit_fallback(
            4,
            Some(crate::TokenUsage {
                input_tokens: 12,
                output_tokens: 34,
            }),
            0,
            None,
        );
        assert!(with.contains("12 in / 34 out tokens"), "got: {with}");
        assert!(with.contains("max_tool_rounds"), "got: {with}");

        let without = cap_exit_fallback(4, None, 0, None);
        assert!(!without.contains("tokens consumed"), "got: {without}");
        assert!(without.contains("tool-call limit of 4"), "got: {without}");

        // Step 27.5: a thrash run (≥ one failed call per round) gets HONEST
        // advice — a tooling problem, not "raise the cap".
        let thrash = cap_exit_fallback(4, None, 6, None);
        assert!(thrash.contains("tool calls that failed"), "got: {thrash}");
        assert!(
            !thrash.contains("raise [tui].max_tool_rounds"),
            "thrash advice must not blame the cap: {thrash}"
        );

        // Step 27.5: progress is salvaged even when the summary failed.
        let salvaged = cap_exit_fallback(4, None, 0, Some("<state>cwd=/x</state>"));
        assert!(salvaged.contains("Progress captured"), "got: {salvaged}");
        assert!(
            salvaged.contains("<state>cwd=/x</state>"),
            "got: {salvaged}"
        );
    }

    #[test]
    fn cap_exit_summary_action_handoff_is_rejected() {
        let handoff = "I have two issues: duplicate topic_has_rollups and a stray brace. Let me fix both — read around 490 to see what needs removing, then verify with a build check.";
        assert!(cap_exit_summary_is_action_handoff(handoff));
        assert!(!cap_exit_summary_is_action_handoff(
            "The duplicate helper definitions and stray brace were removed, and the build check passed."
        ));

        let fallback = cap_exit_action_handoff_fallback(
            25,
            None,
            2,
            Some("<plan>1. [ ] fix duplicate helper definitions</plan>"),
        );
        assert!(fallback.contains("tool-call limit of 25"), "{fallback}");
        assert!(
            fallback.contains("described future tool actions"),
            "{fallback}"
        );
        assert!(
            fallback.contains("preserved the verified progress"),
            "{fallback}"
        );
        assert!(
            !fallback.contains("final summarization request also failed"),
            "{fallback}"
        );
        assert!(
            fallback.contains("Progress captured at the tool-call limit"),
            "{fallback}"
        );
    }

    #[test]
    fn read_only_tools_classified_correctly() {
        // save_note writes memory, not the workspace: a round that only
        // saved a note must still count toward the read-only write-nudge.
        for name in &[
            "list_dir",
            "read_file",
            "find",
            "search",
            "web_fetch",
            "use_skill",
            "save_note",
            "prompt_read",
        ] {
            assert!(is_read_only_tool(name), "{name} should be read-only");
        }
    }

    #[test]
    fn prompt_read_exact_recovery_is_never_spilled() {
        let store = spill::SessionSpillStore::default();
        let exact = "x".repeat(spill::TOOL_RESULT_SPILL_CAP + 1);
        let output = maybe_offload_tool_result("prompt_read", exact.clone(), true, Some(&store));
        assert_eq!(output, exact);
        assert_eq!(spill::SpillStore::spills(&store), 0);
    }

    #[test]
    fn write_tools_not_read_only() {
        for name in &["edit_file", "write_file", "run_command"] {
            assert!(!is_read_only_tool(name), "{name} should NOT be read-only");
        }
    }

    #[test]
    fn read_only_call_classifies_simple_shell_probes() {
        assert!(is_read_only_call(
            "run_command",
            &serde_json::json!({"command": "grep -n 'help_lines' newt-tui/src/lib.rs"})
        ));
        assert!(is_read_only_call(
            "run_command",
            &serde_json::json!({"command": "rg -n format_help newt-tui/src"})
        ));
        assert!(is_read_only_call(
            "run_command",
            &serde_json::json!({"command": "sed -n '1,20p' newt-tui/src/lib.rs"})
        ));

        assert!(!is_read_only_call(
            "run_command",
            &serde_json::json!({"command": "cargo test -p newt-tui"})
        ));
        assert!(!is_read_only_call(
            "run_command",
            &serde_json::json!({"command": "sed -i 's/a/b/' file.txt"})
        ));
        assert!(!is_read_only_call(
            "run_command",
            &serde_json::json!({"command": "grep x file > out.txt"})
        ));
    }

    #[test]
    fn read_only_action_nudge_names_edit_permission_and_blocker_paths() {
        let nudge = read_only_action_nudge(3, 4, None, None);
        assert!(nudge.contains("read-only rounds so far"), "{nudge}");
        assert!(nudge.contains("edit_file"), "{nudge}");
        assert!(nudge.contains("write_file"), "{nudge}");
        assert!(nudge.contains("request_permissions"), "{nudge}");
        assert!(nudge.contains("exact blocker"), "{nudge}");
    }

    #[test]
    fn read_only_action_nudge_mentions_active_plan_when_present() {
        use crate::agentic::scheduled::{SessionStepLedger, StepLedger};

        let ledger = SessionStepLedger::default();
        ledger.restore(&PlanSnapshot {
            steps: vec![
                Step {
                    description: "inspect".to_string(),
                    status: StepStatus::Done,
                },
                Step {
                    description: "edit".to_string(),
                    status: StepStatus::Active,
                },
            ],
        });
        let nudge = read_only_action_nudge(3, 2, Some(&ledger as &dyn StepLedger), None);
        assert!(nudge.contains("active multi-step plan"), "{nudge}");
        assert!(nudge.contains("ACTIVE step"), "{nudge}");
    }

    /// #<issue>: when a `WorkflowSteerer` match offers a delegate hint (e.g.
    /// the built-in `diagnose_failure` workflow, and `crew`/`team` dispatch is
    /// available this session), the read-only nudge surfaces it — sustained
    /// read-only exploration on that task shape is exactly what delegation is
    /// for, not just "stop reading, edit it yourself".
    #[test]
    fn read_only_action_nudge_includes_a_delegate_hint_when_offered() {
        let nudge = read_only_action_nudge(3, 4, None, Some("consider calling crew or team"));
        assert!(nudge.contains("consider calling crew or team"), "{nudge}");
        // Still carries the original inline-action guidance too — delegation
        // is offered ALONGSIDE continuing directly, never in place of it.
        assert!(nudge.contains("edit_file"), "{nudge}");
    }

    #[test]
    fn read_only_action_nudge_omits_delegate_clause_when_none_offered() {
        let nudge = read_only_action_nudge(3, 4, None, None);
        assert!(!nudge.contains("crew"), "{nudge}");
        assert!(!nudge.contains("team"), "{nudge}");
    }

    #[test]
    fn pending_plan_completion_nudge_is_state_driven() {
        use crate::agentic::scheduled::{SessionStepLedger, StepLedger};

        assert!(pending_plan_completion_nudge(None, false, None).is_none());

        let ledger = SessionStepLedger::default();
        ledger.restore(&PlanSnapshot {
            steps: vec![
                Step {
                    description: "already done".to_string(),
                    status: StepStatus::Done,
                },
                Step {
                    description: "keep working".to_string(),
                    status: StepStatus::Active,
                },
            ],
        });
        let nudge = pending_plan_completion_nudge(Some(&ledger as &dyn StepLedger), false, None)
            .expect("open plan produces a nudge");
        assert!(nudge.contains("1/2 unfinished step"), "{nudge}");
        assert!(nudge.contains("Active step: 'keep working'"), "{nudge}");
        assert!(nudge.contains("update_plan"), "{nudge}");
        assert!(nudge.contains("call the next tool"), "{nudge}");
        assert!(nudge.contains("concrete blocker"), "{nudge}");

        let plan_update_nudge = pending_plan_completion_nudge(
            Some(&ledger as &dyn StepLedger),
            true,
            Some(
                "Configured workflow 'github_pr' is active. Workflow steps:\n- commit_step: Commit the verified step",
            ),
        )
        .expect("open plan produces a plan-update nudge");
        assert!(
            plan_update_nudge.contains("findings/next-steps summary"),
            "{plan_update_nudge}"
        );
        assert!(
            plan_update_nudge.contains("Call update_plan now"),
            "{plan_update_nudge}"
        );
        assert!(
            plan_update_nudge.contains("make the immediate blocker repair the active step"),
            "{plan_update_nudge}"
        );
        assert!(
            plan_update_nudge.contains("Do not repeat the findings summary"),
            "{plan_update_nudge}"
        );
        assert!(
            plan_update_nudge.contains("github_pr"),
            "{plan_update_nudge}"
        );
        assert!(
            plan_update_nudge.contains("commit_step"),
            "{plan_update_nudge}"
        );

        ledger.restore(&PlanSnapshot {
            steps: vec![Step {
                description: "complete".to_string(),
                status: StepStatus::Done,
            }],
        });
        assert!(
            pending_plan_completion_nudge(Some(&ledger as &dyn StepLedger), false, None).is_none()
        );
    }

    #[test]
    fn workflow_classifier_text_keeps_recent_user_issue_context() {
        let messages = vec![
            serde_json::json!({
                "role": "user",
                "content": "Take a look at https://github.com/Gilamonster-Foundation/newt-agent/issues/548 and get me a PR."
            }),
            serde_json::json!({
                "role": "assistant",
                "content": "I will inspect the issue and repo state."
            }),
        ];
        let text = workflow_classifier_text(
            &messages,
            "Summary of Findings\n\nCurrent Status: the build is broken. Next Steps Required: update the plan.",
        );
        let hint = crate::WorkflowSteerer::builtin()
            .plan_update_hint(&text)
            .expect("GitHub issue context should select the PR workflow");
        assert!(hint.contains("github_pr"), "{hint}");
        assert!(hint.contains("read_issue"), "{hint}");
        assert!(hint.contains("open_pr"), "{hint}");
    }
}

// ---------------------------------------------------------------------------
// Tool-call round cap + graceful cap-exit (issue: configurable max_tool_rounds)
// ---------------------------------------------------------------------------
//
// These tests exercise both agentic loops (`chat_complete` -> Ollama path and
// `openai_chat_complete`) against a wiremock backend. The mock returns tool
// calls while `tools` are present in the request and a real text answer once
// they are absent — letting us assert that:
//   (1) the loop honours the configured `max_tool_rounds` cap, and
//   (2) on hitting the cap newt issues ONE final tools-disabled completion and
//       returns its text (NOT the `(reached tool-call limit)` placeholder).
//
// Hard context-window recovery is covered both by the headless driver tests
// and by a TUI-side integration test that grounds capability-cache persistence.

/// Token weight of the builtin tool catalog the loop advertises at
/// `disposition` (default advertise flags, no MCP) — the same
/// `merged_tool_definitions` → `filter_tools_for_disposition` →
/// `estimate_value_tokens` pipeline `chat_complete` runs over its advertised
/// tools each turn (see the loop setup near the top of `chat_complete`).
///
/// Token-budget fixtures size their `safe_context` / `num_ctx` / trim
/// thresholds RELATIVE to this live figure plus a scenario-specific,
/// catalog-INDEPENDENT message/headroom offset — so adding a tool or a schema
/// property shifts the catalog and the derived budget together, preserving each
/// fixture's fit-vs-refuse intent instead of tipping it over a pinned magic
/// number.
#[cfg(test)]
pub(crate) fn builtin_catalog_tokens(disposition: PromptDisposition) -> usize {
    let tools = filter_tools_for_disposition(
        merged_tool_definitions(
            &NoMcp, false, false, false, false, false, false, false, false, false, false, false,
            false,
        ),
        disposition,
    );
    estimate_value_tokens(&tools, crate::tokens::TokenEstimation::default())
}

#[cfg(test)]
mod tool_round_cap_tests {
    use super::*;
    use crate::caveats::Caveats;
    use crate::{BackendKind, MemMessage};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    /// Was the `"tools"` key present on this request body?
    fn request_has_tools(req: &Request) -> bool {
        serde_json::from_slice::<serde_json::Value>(&req.body)
            .ok()
            .map(|v| v.get("tools").is_some())
            .unwrap_or(false)
    }

    /// Ollama-shaped responder: returns a tool call whenever `tools` are
    /// offered, and a plain text answer once they are withheld. Counts the
    /// number of tool-offering requests it served.
    struct OllamaResponder {
        tool_rounds_served: Arc<AtomicUsize>,
        final_answer: String,
    }

    impl Respond for OllamaResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if request_has_tools(req) {
                self.tool_rounds_served.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {
                        "content": "",
                        "tool_calls": [{
                            "function": { "name": "definitely_not_a_real_tool", "arguments": {} }
                        }]
                    }
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "content": self.final_answer }
                }))
            }
        }
    }

    /// OpenAI-shaped responder: same logic, OpenAI `choices[0].message` shape.
    struct OpenAiResponder {
        tool_rounds_served: Arc<AtomicUsize>,
        final_answer: String,
    }

    impl Respond for OpenAiResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if request_has_tools(req) {
                self.tool_rounds_served.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{ "message": {
                        "content": null,
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": { "name": "definitely_not_a_real_tool", "arguments": "{}" }
                        }]
                    }}]
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{ "message": { "content": self.final_answer } }]
                }))
            }
        }
    }

    struct ProtectedCapResponder {
        openai: bool,
        exact_task: String,
        pair_seen_on_final: Arc<std::sync::atomic::AtomicBool>,
        omission_seen_on_final: Arc<std::sync::atomic::AtomicBool>,
    }

    impl Respond for ProtectedCapResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if request_has_tools(req) {
                if self.openai {
                    return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "choices": [{"message": {
                            "content": null,
                            "tool_calls": [{
                                "id": "call_cap",
                                "type": "function",
                                "function": {
                                    "name": "definitely_not_a_real_tool",
                                    "arguments": "{}"
                                }
                            }]
                        }}]
                    }));
                }
                return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {
                        "content": "",
                        "tool_calls": [{"function": {
                            "name": "definitely_not_a_real_tool",
                            "arguments": {}
                        }}]
                    }
                }));
            }

            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap_or_default();
            let messages = body["messages"].as_array().cloned().unwrap_or_default();
            let system_count = messages
                .iter()
                .filter(|message| message["role"].as_str() == Some("system"))
                .count();
            let pair_seen = messages.windows(2).any(|pair| {
                let card = pair[0]["role"].as_str() == Some("system")
                    && pair[0]["content"].as_str().is_some_and(|content| {
                        (if self.openai {
                            content.contains(prompt_read::ACTIVE_PROMPT_PREFIX)
                        } else {
                            content.starts_with(prompt_read::ACTIVE_PROMPT_PREFIX)
                        }) && content.contains("address: prompt:")
                            && !content.contains("<ephemeral-unrecorded>")
                    });
                card && pair[1]["role"].as_str() == Some("user")
                    && pair[1]["content"].as_str() == Some(self.exact_task.as_str())
            });
            self.pair_seen_on_final.store(
                pair_seen && (!self.openai || system_count == 1),
                Ordering::SeqCst,
            );
            self.omission_seen_on_final.store(
                messages.iter().any(|message| {
                    message["content"]
                        .as_str()
                        .is_some_and(|content| content.contains("messages omitted"))
                }),
                Ordering::SeqCst,
            );

            if self.openai {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{"message": {"content": "cap summary"}}]
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"content": "cap summary"}
                }))
            }
        }
    }

    struct OpenAiReasoningCapResponder {
        round: AtomicUsize,
        first_plan_seen_on_final: Arc<std::sync::atomic::AtomicBool>,
        policy_seen_on_final: Arc<std::sync::atomic::AtomicBool>,
    }

    impl Respond for OpenAiReasoningCapResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if request_has_tools(req) {
                let round = self.round.fetch_add(1, Ordering::SeqCst);
                return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{"message": {
                        "content": null,
                        "reasoning_content": format!("persistent plan round {round}"),
                        "tool_calls": [{
                            "id": "call_cap",
                            "type": "function",
                            "function": {
                                "name": "definitely_not_a_real_tool",
                                "arguments": "{}"
                            }
                        }]
                    }}]
                }));
            }

            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap_or_default();
            let first_plan_seen = body["messages"].as_array().is_some_and(|messages| {
                messages.iter().any(|message| {
                    message["reasoning_content"].as_str() == Some("persistent plan round 0")
                })
            });
            self.first_plan_seen_on_final
                .store(first_plan_seen, Ordering::SeqCst);
            self.policy_seen_on_final.store(
                body["max_tokens"] == 10_000
                    && body["temperature"] == 0.6
                    && body["top_p"] == 0.95
                    && body["chat_template_kwargs"]["enable_thinking"] == true
                    && body.get("parallel_tool_calls").is_none(),
                Ordering::SeqCst,
            );
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "cap summary"}}]
            }))
        }
    }

    fn msgs() -> Vec<MemMessage> {
        vec![
            MemMessage::system("you are a test"),
            MemMessage::user("do the thing"),
        ]
    }

    fn hard_budget_ctx<'a>(
        url: &'a str,
        messages: &'a [MemMessage],
        caveats: &'a Caveats,
        task: &'a str,
        kind: BackendKind,
    ) -> ChatCtx<'a> {
        ChatCtx {
            url,
            model: "tiny-context-model",
            kind,
            api_key: (kind == BackendKind::Openai).then_some("sk-test"),
            messages,
            task,
            workspace: ".",
            color: false,
            markdown: false,
            tool_offload: false,
            spill_store: None,
            compaction_store: None,
            scratchpad: false,
            scratchpad_store: None,
            code_search: None,
            where_is: None,
            nav: None,
            exposure: Default::default(),
            experience_store: None,
            step_ledger: None,
            caveats,
            persona_tools: None,
            cognition: None,
            chat_completions_capability: Default::default(),
            reasoning_replay_scope: crate::model_card::ReasoningReplayScope::Never,
            max_tool_rounds: 1,
            narration_nudge_cap: 1,
            action_nudges: true,
            prompt_disposition: PromptDisposition::Act,
            prompt_intake: None,
            workflow_grace_rounds: 0,
            tool_output_lines: 20,
            debug: false,
            trace: false,
            num_ctx: None,
            input_ceiling_pct: 80,
            low_budget_pct: 15,
            connect_timeout_secs: 5,
            inference_timeout_secs: 5,
            mid_loop_trim_threshold: 40,
            compaction_trigger_policy: crate::CompactionTriggerPolicy::HeadroomAware,
            mid_loop_trim_tokens: None,
            max_ok_input: None,
            build_check_cmd: None,
            safe_context: Some(256),
            recover_cw_400: None,
            note_sink: None,
            note_nudge: None,
            recall_source: None,
            memory_source: None,
            summarizer: None,
            compress_state: None,
            tool_events: None,
            phantom_reaches: None,
            end_reason: None,
            solve_obs: None,
            permission_gate: None,
            on_round_usage: None,
            estimate_ratio: None,
            estimation: crate::tokens::TokenEstimation::default(),
            summary_input_cap_floor_chars: 8_192,
            exec_floor: None,
            write_ledger: None,
            cancel: None,
            live_tool_output: None,
            git_tool: None,
            crew_runner: None,
            operating_mode_control: None,
            plan_mode_control: None,
        }
    }

    async fn assert_no_requests(server: &MockServer) {
        assert!(
            server
                .received_requests()
                .await
                .expect("wiremock request journal")
                .is_empty(),
            "irreducible-prompt refusal must happen before HTTP dispatch"
        );
    }

    #[tokio::test]
    async fn ollama_cap_trim_keeps_headless_active_pair_after_more_than_six_trailing_messages() {
        let server = MockServer::start().await;
        let task = "CURRENT-B: keep this exact prompt through cap trim";
        let pair_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let omission_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ProtectedCapResponder {
                openai: false,
                exact_task: task.to_string(),
                pair_seen_on_final: pair_seen.clone(),
                omission_seen_on_final: omission_seen.clone(),
            })
            .mount(&server)
            .await;
        let messages = vec![
            MemMessage::system("base"),
            MemMessage::user("historical A"),
            MemMessage::assistant("A done"),
            MemMessage::user(task),
        ];
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut context = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Ollama);
        context.safe_context = None;
        context.max_tool_rounds = 4;
        let (reply, _, _, _) = chat_complete(context, &mut NoMcp)
            .await
            .expect("cap exit succeeds");
        assert_eq!(reply, "cap summary");
        assert!(pair_seen.load(Ordering::SeqCst));
        assert!(
            omission_seen.load(Ordering::SeqCst),
            "four tool rounds create >6 trailing messages and force a real trim"
        );
    }

    #[tokio::test]
    async fn openai_cap_trim_keeps_headless_active_pair_after_more_than_six_trailing_messages() {
        let server = MockServer::start().await;
        let task = "CURRENT-B: keep this exact OpenAI prompt through cap trim";
        let pair_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let omission_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ProtectedCapResponder {
                openai: true,
                exact_task: task.to_string(),
                pair_seen_on_final: pair_seen.clone(),
                omission_seen_on_final: omission_seen.clone(),
            })
            .mount(&server)
            .await;
        let messages = vec![
            MemMessage::system("base"),
            MemMessage::user("historical A"),
            MemMessage::assistant("A done"),
            MemMessage::user(task),
        ];
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut context = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
        context.safe_context = None;
        context.max_tool_rounds = 4;
        let (reply, _, _, _) = openai_chat_complete(context, &mut NoMcp)
            .await
            .expect("cap exit succeeds");
        assert_eq!(reply, "cap summary");
        assert!(pair_seen.load(Ordering::SeqCst));
        assert!(
            omission_seen.load(Ordering::SeqCst),
            "four tool rounds create >6 trailing messages and force a real trim"
        );
    }

    #[tokio::test]
    async fn openai_cap_exit_preserves_the_full_current_turn_reasoning_tail() {
        let server = MockServer::start().await;
        let first_plan_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let policy_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(OpenAiReasoningCapResponder {
                round: AtomicUsize::new(0),
                first_plan_seen_on_final: first_plan_seen.clone(),
                policy_seen_on_final: policy_seen.clone(),
            })
            .mount(&server)
            .await;
        let task = "keep the active plan through cap exit";
        let messages = vec![MemMessage::system("base"), MemMessage::user(task)];
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut context = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
        context.safe_context = None;
        context.max_tool_rounds = 4;
        context.reasoning_replay_scope = crate::model_card::ReasoningReplayScope::CurrentUserTurn;
        context.cognition = Some(crate::role_profile::Cognition::Deliberating);
        context.chat_completions_capability = crate::model_card::ChatCompletionsCapability {
            cognition: Some(true),
            chat_template_kwargs: Some(true),
            parallel_tool_calls: Some(false),
            bounded_reasoning_continuation: Some(true),
        };

        let (reply, _, _, _) = openai_chat_complete(context, &mut NoMcp)
            .await
            .expect("cap exit succeeds");
        assert_eq!(reply, "cap summary");
        assert!(
            first_plan_seen.load(Ordering::SeqCst),
            "the tools-disabled cap-exit request must retain the first current-turn plan"
        );
        assert!(
            policy_seen.load(Ordering::SeqCst),
            "the cap-exit request must retain cognition policy and omit tool-only fields"
        );
    }

    #[tokio::test]
    async fn ollama_loop_honors_configured_cap_and_returns_real_final_answer() {
        let server = MockServer::start().await;
        let served = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(OllamaResponder {
                tool_rounds_served: served.clone(),
                final_answer: "here is my partial summary".into(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let cap = 3;
        let mut end_reason: Option<crate::TurnEndReason> = None;
        let (reply, streamed, _usage, _hallu) = chat_complete(
            ChatCtx {
                url: &server.uri(),
                model: "test-model",
                kind: BackendKind::Ollama,
                api_key: None,
                messages: &messages,
                task: "do the thing",
                workspace: ".",
                color: false,
                markdown: false,
                tool_offload: false,
                spill_store: None,
                compaction_store: None,
                scratchpad: false,
                scratchpad_store: None,
                code_search: None,
                where_is: None,
                nav: None,
                exposure: Default::default(),
                experience_store: None,
                step_ledger: None,
                caveats: &caveats,
                persona_tools: None,
                cognition: None,
                chat_completions_capability: Default::default(),
                reasoning_replay_scope: crate::model_card::ReasoningReplayScope::Never,
                max_tool_rounds: cap,
                narration_nudge_cap: 1,
                action_nudges: true,
                prompt_disposition: PromptDisposition::Act,
                prompt_intake: None,
                workflow_grace_rounds: 0,
                tool_output_lines: 20,
                debug: false,
                trace: false,
                num_ctx: None,
                input_ceiling_pct: 80,
                low_budget_pct: 15,
                connect_timeout_secs: 5,
                inference_timeout_secs: 120,
                mid_loop_trim_threshold: 40,
                compaction_trigger_policy: crate::CompactionTriggerPolicy::HeadroomAware,
                mid_loop_trim_tokens: None,
                max_ok_input: None,
                build_check_cmd: None,
                safe_context: None,
                recover_cw_400: None,
                note_sink: None,
                note_nudge: None,
                recall_source: None,
                memory_source: None,
                summarizer: None,
                compress_state: None,
                tool_events: None,
                phantom_reaches: None,
                end_reason: Some(&mut end_reason),
                solve_obs: None,
                permission_gate: None,
                on_round_usage: None,
                estimate_ratio: None,
                estimation: crate::tokens::TokenEstimation::default(),
                summary_input_cap_floor_chars: 8_192,
                exec_floor: None,
                write_ledger: None,
                cancel: None,
                live_tool_output: None,
                git_tool: None,
                crew_runner: None,
                operating_mode_control: None,
                plan_mode_control: None,
            },
            &mut NoMcp,
        )
        .await
        .expect("chat_complete should succeed");

        // The cap was honoured: exactly `cap` tool-offering rounds were served.
        assert_eq!(served.load(Ordering::SeqCst), cap);
        // The cap-exit issued a final tools-disabled completion and returned
        // its text — NOT the dead placeholder.
        assert_eq!(reply, "here is my partial summary");
        assert_ne!(reply, "(reached tool-call limit)");
        assert!(!streamed);
        // The cap exit reports itself (acceptance forensics, commit 4).
        assert_eq!(end_reason, Some(crate::TurnEndReason::RoundCap));
    }

    #[tokio::test]
    async fn ollama_cap_exit_rejects_action_intent_summary() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {
                    "content": "I have two issues: duplicate topic_has_rollups and a stray brace. Let me fix both — read around 490 to see what needs removing, then verify with a build check."
                }
            })))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let chat_url = format!("{}/api/chat", server.uri());
        let (reply, streamed, _usage) = final_summary_ollama(
            &client,
            &chat_url,
            "test-model",
            Vec::new(),
            CapExit {
                max_tool_rounds: 25,
                accumulated: None,
                wasted_calls: 0,
                progress: Some("<plan>1. [ ] fix duplicate helper definitions</plan>".to_string()),
                observed: Vec::new(),
                request_budget: None,
                calibration: 1.0,
                estimation: crate::tokens::TokenEstimation::default(),
                ollama_num_ctx: Some(4_096),
            },
        )
        .await
        .expect("final summary helper should return a fallback");

        assert!(!streamed);
        assert!(reply.contains("tool-call limit of 25"), "{reply}");
        assert!(reply.contains("described future tool actions"), "{reply}");
        assert!(reply.contains("preserved the verified progress"), "{reply}");
        assert!(
            !reply.contains("Let me fix both"),
            "must not accept action-intent cap summary: {reply}"
        );
        assert!(
            !reply.contains("final summarization request also failed"),
            "{reply}"
        );
        assert!(
            reply.contains("Progress captured at the tool-call limit"),
            "{reply}"
        );
        let requests = server
            .received_requests()
            .await
            .expect("wiremock request journal");
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["options"]["num_ctx"], 4_096);
    }

    #[tokio::test]
    async fn ollama_cap_exit_refuses_giant_fresh_result_before_dispatch() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": { "content": "must not be dispatched" }
            })))
            .mount(&server)
            .await;

        let exact_task = "CURRENT-B: preserve this exact active operator prompt";
        let mut messages = vec![
            serde_json::json!({
                "role": "system",
                "content": format!("{}\naddress: prompt:test", prompt_read::ACTIVE_PROMPT_PREFIX),
            }),
            serde_json::json!({"role": "user", "content": exact_task}),
            serde_json::json!({
                "role": "assistant",
                "tool_calls": [{"function": {"name": "read_file", "arguments": {"path": "huge.txt"}}}],
            }),
            serde_json::json!({"role": "tool", "content": "x".repeat(32_000)}),
        ];
        let head = protected_prompt_head_len(&messages, prompt_read::ACTIVE_PROMPT_PREFIX);
        messages = trim_for_summary(&messages, head, 6);
        let client = reqwest::Client::new();
        let (reply, streamed, usage) = final_summary_ollama(
            &client,
            &format!("{}/api/chat", server.uri()),
            "tiny-model",
            messages,
            CapExit {
                max_tool_rounds: 1,
                accumulated: None,
                wasted_calls: 0,
                progress: None,
                observed: Vec::new(),
                request_budget: Some(2_000),
                calibration: 1.0,
                estimation: crate::tokens::TokenEstimation::default(),
                ollama_num_ctx: Some(2_500),
            },
        )
        .await
        .expect("oversized cap exit returns deterministic fallback");
        assert!(!streamed);
        assert!(usage.is_none());
        assert!(reply.contains("tool-call limit of 1"), "{reply}");
        assert!(
            reply.contains("final summarization request also failed"),
            "{reply}"
        );
        assert!(
            server
                .received_requests()
                .await
                .expect("wiremock request journal")
                .is_empty(),
            "the oversized cap-exit request must never reach the backend"
        );
    }

    #[tokio::test]
    async fn openai_cap_exit_refuses_giant_fresh_result_before_dispatch() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "must not be dispatched"}}]
            })))
            .mount(&server)
            .await;

        let messages = vec![
            serde_json::json!({
                "role": "system",
                "content": format!("{}\naddress: prompt:test", prompt_read::ACTIVE_PROMPT_PREFIX),
            }),
            serde_json::json!({"role": "user", "content": "CURRENT-B exact task"}),
            serde_json::json!({"role": "tool", "content": "x".repeat(32_000)}),
        ];
        let client = reqwest::Client::new();
        let (reply, streamed, usage) = final_summary_openai(
            &client,
            &format!("{}/v1/chat/completions", server.uri()),
            "tiny-model",
            None,
            messages,
            generation_policy::GenerationPolicy::default(),
            CapExit {
                max_tool_rounds: 1,
                accumulated: None,
                wasted_calls: 0,
                progress: None,
                observed: Vec::new(),
                request_budget: Some(2_000),
                calibration: 1.0,
                estimation: crate::tokens::TokenEstimation::default(),
                ollama_num_ctx: None,
            },
        )
        .await
        .expect("oversized cap exit returns deterministic fallback");
        assert!(!streamed);
        assert!(usage.is_none());
        assert!(reply.contains("tool-call limit of 1"), "{reply}");
        assert!(
            server
                .received_requests()
                .await
                .expect("wiremock request journal")
                .is_empty(),
            "the oversized cap-exit request must never reach the backend"
        );
    }

    /// UAT (Step 27.3 + 27.5, simulated integration): a thrash run — a DISTINCT
    /// failing tool call every round (so the failed-call count climbs to the
    /// cap) AND a final summary that also errors. The cap-exit must be HONEST:
    /// name the tooling problem, never advise "raise max_tool_rounds".
    struct ThrashResponder {
        round: AtomicUsize,
    }

    impl Respond for ThrashResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if request_has_tools(req) {
                let n = self.round.fetch_add(1, Ordering::SeqCst);
                // A distinct unknown tool each round → each fails and is NOT a
                // repeat, so the guard records every one (wasted_calls climbs to
                // the cap, which is what flips the cap-exit to honest advice).
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {
                        "content": "",
                        "tool_calls": [{
                            "function": { "name": format!("bogus_tool_{n}"), "arguments": {} }
                        }]
                    }
                }))
            } else {
                // The final tools-disabled summary request ALSO fails (500),
                // forcing the cap_exit_fallback path.
                ResponseTemplate::new(500).set_body_string("model exploded")
            }
        }
    }

    #[tokio::test]
    async fn uat_thrash_run_gets_honest_cap_exit_not_raise_the_limit() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ThrashResponder {
                round: AtomicUsize::new(0),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let cap = 3;
        let (reply, _streamed, _usage, hallu) = chat_complete(
            ChatCtx {
                url: &server.uri(),
                model: "test-model",
                kind: BackendKind::Ollama,
                api_key: None,
                messages: &messages,
                task: "do the thing",
                workspace: ".",
                color: false,
                markdown: false,
                tool_offload: false,
                spill_store: None,
                compaction_store: None,
                scratchpad: false,
                scratchpad_store: None,
                code_search: None,
                where_is: None,
                nav: None,
                exposure: Default::default(),
                experience_store: None,
                step_ledger: None,
                caveats: &caveats,
                persona_tools: None,
                cognition: None,
                chat_completions_capability: Default::default(),
                reasoning_replay_scope: crate::model_card::ReasoningReplayScope::Never,
                max_tool_rounds: cap,
                narration_nudge_cap: 1,
                action_nudges: true,
                prompt_disposition: PromptDisposition::Act,
                prompt_intake: None,
                workflow_grace_rounds: 0,
                tool_output_lines: 20,
                debug: false,
                trace: false,
                num_ctx: None,
                input_ceiling_pct: 80,
                low_budget_pct: 15,
                connect_timeout_secs: 5,
                inference_timeout_secs: 120,
                mid_loop_trim_threshold: 40,
                compaction_trigger_policy: crate::CompactionTriggerPolicy::HeadroomAware,
                mid_loop_trim_tokens: None,
                max_ok_input: None,
                build_check_cmd: None,
                safe_context: None,
                recover_cw_400: None,
                note_sink: None,
                note_nudge: None,
                recall_source: None,
                memory_source: None,
                summarizer: None,
                compress_state: None,
                tool_events: None,
                phantom_reaches: None,
                end_reason: None,
                solve_obs: None,
                permission_gate: None,
                on_round_usage: None,
                estimate_ratio: None,
                estimation: crate::tokens::TokenEstimation::default(),
                summary_input_cap_floor_chars: 8_192,
                exec_floor: None,
                write_ledger: None,
                cancel: None,
                live_tool_output: None,
                git_tool: None,
                crew_runner: None,
                operating_mode_control: None,
                plan_mode_control: None,
            },
            &mut NoMcp,
        )
        .await
        .expect("chat_complete should succeed even when the summary fails");

        // Every round emitted a (distinct) bogus call → counted as a hallucination.
        assert_eq!(hallu, cap as u32, "each round hallucinated a tool");
        // Step 27.5: the cap-exit is HONEST — a tooling problem, NOT "raise the cap".
        assert!(
            reply.contains("tool calls that failed"),
            "honest advice expected, got: {reply}"
        );
        assert!(
            !reply.contains("raise [tui].max_tool_rounds"),
            "must not blame the round cap on a thrash run: {reply}"
        );
    }

    #[tokio::test]
    async fn a_set_cancel_flag_abandons_the_turn_before_any_network_call() {
        // The interrupt checkpoint at the round-loop top runs before the first
        // request, so a pre-tripped flag returns instantly — the bogus URL
        // (a closed port) is never contacted. If the checkpoint regressed,
        // the dispatch would try to connect and this would not return empty.
        let messages = msgs();
        let caveats = Caveats::top();
        let flag = std::sync::atomic::AtomicBool::new(true);
        let (reply, streamed, usage, hallu) = chat_complete(
            ChatCtx {
                url: "http://127.0.0.1:1",
                model: "test-model",
                kind: BackendKind::Ollama,
                api_key: None,
                messages: &messages,
                task: "do the thing",
                workspace: ".",
                color: false,
                markdown: false,
                tool_offload: false,
                spill_store: None,
                compaction_store: None,
                scratchpad: false,
                scratchpad_store: None,
                code_search: None,
                where_is: None,
                nav: None,
                exposure: Default::default(),
                experience_store: None,
                step_ledger: None,
                caveats: &caveats,
                persona_tools: None,
                cognition: None,
                chat_completions_capability: Default::default(),
                reasoning_replay_scope: crate::model_card::ReasoningReplayScope::Never,
                max_tool_rounds: 5,
                narration_nudge_cap: 1,
                action_nudges: true,
                prompt_disposition: PromptDisposition::Act,
                prompt_intake: None,
                workflow_grace_rounds: 0,
                tool_output_lines: 20,
                debug: false,
                trace: false,
                num_ctx: None,
                input_ceiling_pct: 80,
                low_budget_pct: 15,
                connect_timeout_secs: 5,
                inference_timeout_secs: 120,
                mid_loop_trim_threshold: 40,
                compaction_trigger_policy: crate::CompactionTriggerPolicy::HeadroomAware,
                mid_loop_trim_tokens: None,
                max_ok_input: None,
                build_check_cmd: None,
                safe_context: None,
                recover_cw_400: None,
                note_sink: None,
                note_nudge: None,
                recall_source: None,
                memory_source: None,
                summarizer: None,
                compress_state: None,
                tool_events: None,
                phantom_reaches: None,
                end_reason: None,
                solve_obs: None,
                permission_gate: None,
                on_round_usage: None,
                estimate_ratio: None,
                estimation: crate::tokens::TokenEstimation::default(),
                summary_input_cap_floor_chars: 8_192,
                exec_floor: None,
                write_ledger: None,
                cancel: Some(&flag),
                live_tool_output: None,
                git_tool: None,
                crew_runner: None,
                operating_mode_control: None,
                plan_mode_control: None,
            },
            &mut NoMcp,
        )
        .await
        .expect("an interrupted turn still returns Ok, just empty");
        assert!(reply.is_empty(), "interrupted before any model output");
        assert!(!streamed);
        assert!(usage.is_none());
        assert_eq!(hallu, 0);
    }

    #[test]
    fn responses_keeps_exact_active_prompt_at_user_priority() {
        let exact = "operator text must remain user data";
        let mut messages = vec![
            serde_json::json!({"role": "system", "content": "base policy"}),
            serde_json::json!({"role": "user", "content": "historical ask"}),
        ];
        prompt_read::ensure_active_prompt_card(
            &mut messages,
            prompt_read::PromptReadContext::new(None, exact, None),
        );

        let (instructions, input) = crate::responses_wire::build_responses_input(&messages);
        let instructions = instructions.expect("base and metadata instructions");
        assert!(instructions.contains(prompt_read::ACTIVE_PROMPT_PREFIX));
        assert!(
            !instructions.contains(exact),
            "operator content must not be promoted to Responses instructions"
        );
        assert!(input
            .iter()
            .any(|item| { item["role"] == "user" && item["content"].as_str() == Some(exact) }));
    }

    #[test]
    fn tools_flatten_to_responses_shape() {
        let chat = serde_json::json!([{
            "type": "function",
            "function": {
                "name": "git",
                "description": "run git",
                "parameters": {"type": "object"}
            }
        }]);
        let out = tools_to_responses(&chat);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["type"], "function");
        assert_eq!(
            out[0]["name"], "git",
            "name hoisted out of the function wrapper"
        );
        assert_eq!(out[0]["description"], "run git");
        assert!(out[0]["function"].is_null(), "no nested function wrapper");
        // A non-strict tool stays non-strict — no strictness is invented.
        assert!(
            out[0].get("strict").is_none(),
            "absent strict must not become present"
        );
    }

    #[test]
    fn tools_to_responses_preserves_strictness_semantics() {
        // #1526 (invariant #6): a strict Chat Completions schema must stay strict
        // after conversion. `strict` moves from the `function` object to the
        // Responses tool's TOP level, and the parameters' `additionalProperties` /
        // `required` are carried through wholesale (not silently relaxed).
        let chat = serde_json::json!([{
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "write a file",
                "strict": true,
                "parameters": {
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"],
                    "additionalProperties": false
                }
            }
        }]);
        let out = tools_to_responses(&chat);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0]["strict"], true,
            "strict must survive at the Responses tool's top level"
        );
        // Validation-semantic fields inside `parameters` are unchanged.
        assert_eq!(
            out[0]["parameters"]["required"],
            serde_json::json!(["path"])
        );
        assert_eq!(out[0]["parameters"]["additionalProperties"], false);
    }

    #[test]
    fn cognition_maps_to_the_responses_reasoning_field_or_is_omitted() {
        use crate::role_profile::Cognition;
        // Opt-in: each level projects to the Responses `reasoning.effort` value.
        assert_eq!(
            responses_reasoning_field(Some(Cognition::Contemplating)),
            Some(serde_json::json!({ "effort": "high" }))
        );
        assert_eq!(
            responses_reasoning_field(Some(Cognition::Glancing)),
            Some(serde_json::json!({ "effort": "minimal" }))
        );
        assert_eq!(
            responses_reasoning_field(Some(Cognition::Deliberating)),
            Some(serde_json::json!({ "effort": "medium" }))
        );
        // Not opted in → the field is omitted entirely (request unchanged).
        assert_eq!(responses_reasoning_field(None), None);
    }

    #[test]
    fn responses_loop_consumes_the_shared_decoder_for_text_calls_and_usage() {
        // The agentic loop now shares ONE decoder with the inference transport
        // (`crate::responses_wire`). This grounds that the loop's consumption
        // path gets text, calls, echo (reasoning + function_call in order), and
        // usage from that single decoder — no second hand-rolled parser.
        let json = serde_json::json!({
            "status": "completed",
            "output": [
                {"type": "reasoning", "summary": "…"},
                {"type": "message", "role": "assistant",
                 "content": [{"type": "output_text", "text": "the answer"}]},
                {"type": "function_call", "call_id": "call_1", "name": "git",
                 "arguments": "{\"op\":\"status\"}"}
            ],
            "usage": {"input_tokens": 100, "output_tokens": 20}
        });
        let d = crate::responses_wire::decode_response(&json).expect("a completed tool-call turn");
        assert_eq!(d.text, "the answer");
        assert_eq!(d.tool_calls.len(), 1);
        assert_eq!(d.tool_calls[0]["call_id"], "call_1");
        // The echo re-sends the reasoning item AND the function_call in output
        // order, so a reasoning model (gpt-5.6-sol) does not 400 on the follow-up
        // turn for a function_call missing its required reasoning item.
        assert_eq!(d.echo.len(), 2, "reasoning + function_call are echoed");
        assert_eq!(d.echo[0]["type"], "reasoning");
        assert_eq!(d.echo[1]["type"], "function_call");
        assert_eq!(d.echo[1]["call_id"], "call_1");
        let usage = d.usage.unwrap();
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 20);
    }

    fn giant_prompt_messages(task: &str) -> Vec<MemMessage> {
        vec![MemMessage::system("base policy"), MemMessage::user(task)]
    }

    fn mid_sized_pair_task(label: &str) -> String {
        format!("{label} {}", "x".repeat(6_000))
    }

    #[test]
    fn accepted_prompt_cannot_raise_budget_past_declared_ceiling() {
        assert_eq!(capped_accepted_prompt_tokens(61_221, Some(54_394)), 54_394);
        assert_eq!(capped_accepted_prompt_tokens(8_734, None), 8_734);
    }

    #[test]
    fn authoritative_zero_input_budget_is_not_erased() {
        assert_eq!(authoritative_request_budget(Some(0), true, None), Some(0));
        assert_eq!(authoritative_request_budget(Some(0), false, None), None);
    }

    /// Prove the regression fixture isolates the live-tail duplicate — the
    /// protected recovery copy and schemas fit, but the irreducible complete
    /// request (recovery copy + newest user presentation) does not — and
    /// RETURN the `safe_context` budget the run should use.
    ///
    /// The budget is DERIVED from the live catalog, not pinned: both `one_copy`
    /// (protected head + advertised schemas) and `complete` (+ the duplicated
    /// live-tail presentation) already track the catalog, and the gap between
    /// them is one catalog-independent ~1.5k-token task copy. Sizing the budget
    /// at their midpoint keeps `one_copy <= budget < complete` under any catalog
    /// growth, so the fixture always exercises "one copy fits, the irreducible
    /// pair does not → refuse". Returning it makes the guard below and the
    /// actual `chat_complete` run agree on the same number.
    fn mid_sized_pair_budget(task: &str, responses_wire: bool) -> usize {
        let mut messages = vec![
            serde_json::json!({"role": "system", "content": "base policy"}),
            serde_json::json!({"role": "user", "content": task}),
        ];
        prompt_read::ensure_active_prompt_card(
            &mut messages,
            prompt_read::PromptReadContext::new(None, task, None),
        );
        let head = protected_prompt_head_len(&messages, prompt_read::ACTIVE_PROMPT_PREFIX);
        let chat_tools = merged_tool_definitions(
            &NoMcp, false, false, false, false, false, false, false, false, false, false, false,
            false,
        );
        let tools = if responses_wire {
            serde_json::Value::Array(tools_to_responses(&chat_tools))
        } else {
            chat_tools
        };
        let estimation = crate::tokens::TokenEstimation::default();
        let one_copy = estimate_request_tokens(&messages[..head], Some(&tools), estimation);
        let complete = estimate_request_tokens(&messages, Some(&tools), estimation);
        // Strictly between one_copy and complete (their gap is the ~1.5k-token
        // live-tail task copy), so one protected copy fits but the pair cannot.
        let budget = (one_copy + complete) / 2;
        assert!(
            one_copy <= budget,
            "fixture invalid: one protected copy needs {one_copy} tokens, budget {budget}"
        );
        assert!(
            complete > budget,
            "fixture invalid: the irreducible pair needs {complete} tokens, budget {budget}"
        );
        budget
    }

    fn assert_irreducible_refusal(error: &anyhow::Error) {
        let message = error.to_string();
        assert!(
            message.contains("refusing before inference dispatch"),
            "{message}"
        );
        assert!(
            message.contains("operator prompt was not truncated"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn ollama_giant_exact_prompt_refuses_before_zero_wire_dispatches() {
        let server = MockServer::start().await;
        let task = format!("OLLAMA-GIANT {}", "x".repeat(20_000));
        let messages = giant_prompt_messages(&task);
        let caveats = Caveats::top();
        let error = chat_complete(
            hard_budget_ctx(
                &server.uri(),
                &messages,
                &caveats,
                &task,
                BackendKind::Ollama,
            ),
            &mut NoMcp,
        )
        .await
        .expect_err("giant exact prompt is irreducible");
        assert_irreducible_refusal(&error);
        assert_no_requests(&server).await;
    }

    #[tokio::test]
    async fn openai_chat_giant_exact_prompt_refuses_before_zero_wire_dispatches() {
        let server = MockServer::start().await;
        let task = format!("OPENAI-CHAT-GIANT {}", "x".repeat(20_000));
        let messages = giant_prompt_messages(&task);
        let caveats = Caveats::top();
        let error = openai_chat_complete(
            hard_budget_ctx(
                &server.uri(),
                &messages,
                &caveats,
                &task,
                BackendKind::Openai,
            ),
            &mut NoMcp,
        )
        .await
        .expect_err("giant exact prompt is irreducible");
        assert_irreducible_refusal(&error);
        assert_no_requests(&server).await;
    }

    #[tokio::test]
    async fn responses_giant_exact_prompt_refuses_before_zero_wire_dispatches() {
        let server = MockServer::start().await;
        let task = format!("RESPONSES-GIANT {}", "x".repeat(20_000));
        let messages = giant_prompt_messages(&task);
        let caveats = Caveats::top();
        let error = openai_responses_complete(
            hard_budget_ctx(
                &server.uri(),
                &messages,
                &caveats,
                &task,
                BackendKind::Openai,
            ),
            &mut NoMcp,
        )
        .await
        .expect_err("giant exact prompt is irreducible");
        assert_irreducible_refusal(&error);
        assert_no_requests(&server).await;
    }

    #[tokio::test]
    async fn ollama_mid_sized_irreducible_prompt_pair_refuses_before_dispatch() {
        let server = MockServer::start().await;
        let task = mid_sized_pair_task("OLLAMA-MID-PAIR");
        let budget = mid_sized_pair_budget(&task, false);
        let messages = giant_prompt_messages(&task);
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, &task, BackendKind::Ollama);
        ctx.safe_context = Some(budget as u32);
        let error = chat_complete(ctx, &mut NoMcp)
            .await
            .expect_err("the two irreducible prompt presentations exceed the window");
        assert_irreducible_refusal(&error);
        assert_no_requests(&server).await;
    }

    #[tokio::test]
    async fn openai_chat_mid_sized_irreducible_prompt_pair_refuses_before_dispatch() {
        let server = MockServer::start().await;
        let task = mid_sized_pair_task("OPENAI-CHAT-MID-PAIR");
        let budget = mid_sized_pair_budget(&task, false);
        let messages = giant_prompt_messages(&task);
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, &task, BackendKind::Openai);
        ctx.safe_context = Some(budget as u32);
        let error = openai_chat_complete(ctx, &mut NoMcp)
            .await
            .expect_err("the two irreducible prompt presentations exceed the window");
        assert_irreducible_refusal(&error);
        assert_no_requests(&server).await;
    }

    #[tokio::test]
    async fn openai_chat_declared_num_ctx_is_a_local_refusal_budget() {
        let server = MockServer::start().await;
        let task = mid_sized_pair_task("OPENAI-CHAT-NUM-CTX");
        let budget = mid_sized_pair_budget(&task, false);
        let messages = giant_prompt_messages(&task);
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, &task, BackendKind::Openai);
        ctx.safe_context = None;
        ctx.num_ctx = Some(((budget * 100).div_ceil(80)) as u32);

        let error = openai_chat_complete(ctx, &mut NoMcp)
            .await
            .expect_err("the declared local window must refuse the irreducible request");

        assert_irreducible_refusal(&error);
        assert_no_requests(&server).await;
    }

    #[tokio::test]
    async fn openai_chat_output_reserve_tightens_declared_window_before_dispatch() {
        let server = MockServer::start().await;
        let task = mid_sized_pair_task("OPENAI-CHAT-OUTPUT-RESERVE");
        let budget = mid_sized_pair_budget(&task, false);
        let messages = giant_prompt_messages(&task);
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, &task, BackendKind::Openai);
        ctx.safe_context = None;
        ctx.cognition = Some(crate::role_profile::Cognition::Contemplating);
        ctx.chat_completions_capability = crate::model_card::ChatCompletionsCapability {
            cognition: Some(true),
            ..Default::default()
        };
        let context_window = budget + 16_000;
        assert!(
            context_window * ctx.input_ceiling_pct as usize / 100 > budget,
            "fixture must be tightened by output reserve, not percentage"
        );
        ctx.num_ctx = Some(context_window as u32);

        let error = openai_chat_complete(ctx, &mut NoMcp)
            .await
            .expect_err("the 16K output reserve must refuse the irreducible input");

        assert_irreducible_refusal(&error);
        assert_no_requests(&server).await;
    }

    #[tokio::test]
    async fn responses_mid_sized_irreducible_prompt_pair_refuses_before_dispatch() {
        let server = MockServer::start().await;
        let task = mid_sized_pair_task("RESPONSES-MID-PAIR");
        let budget = mid_sized_pair_budget(&task, true);
        let messages = giant_prompt_messages(&task);
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, &task, BackendKind::Openai);
        ctx.safe_context = Some(budget as u32);
        let error = openai_responses_complete(ctx, &mut NoMcp)
            .await
            .expect_err("the two irreducible prompt presentations exceed the window");
        assert_irreducible_refusal(&error);
        assert_no_requests(&server).await;
    }

    #[tokio::test]
    async fn responses_never_sends_num_ctx_on_the_wire() {
        // A configured window is a LOCAL limit (see the refusal test below), but
        // it must NEVER be sent on the Responses wire (limits are provider-side).
        // Here the window is large enough to fit the small request, so it
        // succeeds AND the body carries no `num_ctx`.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "output": [{
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "provider accepted"}]
                }],
                "usage": {"input_tokens": 20, "output_tokens": 3}
            })))
            .mount(&server)
            .await;

        let task = "a normal Responses request";
        let messages = giant_prompt_messages(task);
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
        ctx.safe_context = None;
        ctx.max_ok_input = None;
        // A generous configured window: a local ceiling, but the small request
        // fits well under it, so nothing is refused.
        ctx.num_ctx = Some(1_000_000);

        let (reply, _, _, _) = openai_responses_complete(ctx, &mut NoMcp)
            .await
            .expect("the request fits the configured window");
        assert_eq!(reply, "provider accepted");
        let requests = server
            .received_requests()
            .await
            .expect("wiremock request journal");
        assert_eq!(requests.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert!(
            body.get("num_ctx").is_none(),
            "Responses must not send the ChatCtx num_ctx display hint"
        );
        assert!(
            body.get("reasoning").is_none(),
            "no cognition set → no reasoning.effort on the wire (request unchanged)"
        );
        // #1526 (invariant #5): storage is an EXPLICIT policy — the body opts out
        // of server-side retention rather than inheriting the API's `store: true`.
        assert_eq!(
            body.get("store"),
            Some(&serde_json::Value::Bool(false)),
            "Responses must explicitly set store:false (stateless, no retention)"
        );
    }

    #[tokio::test]
    async fn responses_refuses_locally_when_a_configured_window_cannot_fit() {
        // #1526 (invariant #4): a CONFIGURED context window is a local safety
        // limit even though it is never sent on the Responses wire. A window too
        // small to hold the irreducible request must be refused PRE-DISPATCH —
        // no request reaches the provider — rather than relying on a reactive
        // 400 or a silent truncation. (The previous contract wrongly let this
        // sail through; that assertion is now reversed.)
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0) // nothing may be dispatched
            .mount(&server)
            .await;

        let task = "a normal Responses request";
        let messages = giant_prompt_messages(task);
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
        ctx.safe_context = None;
        ctx.max_ok_input = None;
        // A 1-token window leaves zero input capacity → local refusal.
        ctx.num_ctx = Some(1);

        openai_responses_complete(ctx, &mut NoMcp)
            .await
            .expect_err("a 1-token configured window cannot fit the request");
        assert_no_requests(&server).await;
    }

    #[tokio::test]
    async fn responses_emits_cognition_as_reasoning_effort_on_the_wire() {
        // The psyche cognition dial must reach the real /v1/responses request as
        // `reasoning.effort` — grounds the pure mapping test against the full loop.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "output": [{
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "considered"}]
                }],
                "usage": {"input_tokens": 20, "output_tokens": 3}
            })))
            .mount(&server)
            .await;

        let task = "think hard about this";
        let messages = giant_prompt_messages(task);
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
        ctx.safe_context = None;
        ctx.max_ok_input = None;
        ctx.cognition = Some(crate::role_profile::Cognition::Contemplating);

        let (reply, _, _, _) = openai_responses_complete(ctx, &mut NoMcp)
            .await
            .expect("the request should dispatch");
        assert_eq!(reply, "considered");
        let requests = server
            .received_requests()
            .await
            .expect("wiremock request journal");
        assert_eq!(requests.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(
            body["reasoning"]["effort"], "high",
            "cognition=contemplating must ride the wire as reasoning.effort=high"
        );
    }

    struct GiantPromptReadResponder {
        openai: bool,
    }

    impl Respond for GiantPromptReadResponder {
        fn respond(&self, _req: &Request) -> ResponseTemplate {
            if self.openai {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{"message": {
                        "content": null,
                        "tool_calls": [{
                            "id": "call_prompt_read",
                            "type": "function",
                            "function": {
                                "name": "prompt_read",
                                "arguments": "{\"address\":\"previous\"}"
                            }
                        }]
                    }}]
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"content": "", "tool_calls": [{
                        "function": {
                            "name": "prompt_read",
                            "arguments": {"address": "previous"}
                        }
                    }]}
                }))
            }
        }
    }

    fn giant_previous_prompt_context(
        store: &SessionPromptStore,
        conversation_id: &str,
        task: &str,
    ) -> crate::TurnPromptContext {
        let giant = format!("GIANT PREVIOUS OPERATOR PROMPT\n{}", "z\n".repeat(25_000));
        store
            .begin_prompt(
                conversation_id,
                crate::NewPrompt::operator(giant.as_bytes(), giant.as_bytes()),
            )
            .unwrap();
        store
            .begin_prompt(
                conversation_id,
                crate::NewPrompt::operator(task.as_bytes(), task.as_bytes()),
            )
            .unwrap()
    }

    #[tokio::test]
    async fn ollama_giant_prompt_read_result_refuses_before_second_dispatch() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(GiantPromptReadResponder { openai: false })
            .mount(&server)
            .await;
        let task = "re-read the prior prompt, then explain it";
        let messages = giant_prompt_messages(task);
        let caveats = Caveats::top();
        let prompt_store = SessionPromptStore::default();
        let turn = giant_previous_prompt_context(&prompt_store, "ollama-prompt-read", task);
        let source = prompt_store.source("ollama-prompt-read");
        let uri = server.uri();
        let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Ollama);
        ctx.safe_context = Some(8_000);
        ctx.max_tool_rounds = 2;

        let error = chat_complete_with_prompt(ctx, Some(&turn), Some(&source), &mut NoMcp)
            .await
            .expect_err("the giant exact prompt_read result must block the next request");
        let message = error.to_string();
        assert!(
            message.contains("complete inference request needs"),
            "{message}"
        );
        assert!(
            message.contains("tool results were not truncated"),
            "{message}"
        );
        let requests = server
            .received_requests()
            .await
            .expect("wiremock request journal");
        assert_eq!(requests.len(), 1, "no over-budget second dispatch");
    }

    #[tokio::test]
    async fn openai_chat_giant_prompt_read_result_refuses_before_second_dispatch() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(GiantPromptReadResponder { openai: true })
            .mount(&server)
            .await;
        let task = "re-read the prior prompt, then explain it";
        let messages = giant_prompt_messages(task);
        let caveats = Caveats::top();
        let prompt_store = SessionPromptStore::default();
        let turn = giant_previous_prompt_context(&prompt_store, "openai-prompt-read", task);
        let source = prompt_store.source("openai-prompt-read");
        let uri = server.uri();
        let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
        ctx.safe_context = Some(8_000);
        ctx.max_tool_rounds = 2;

        let error = openai_chat_complete_with_prompt(ctx, Some(&turn), Some(&source), &mut NoMcp)
            .await
            .expect_err("the giant exact prompt_read result must block the next request");
        let message = error.to_string();
        assert!(
            message.contains("complete inference request needs"),
            "{message}"
        );
        assert!(
            message.contains("tool results were not truncated"),
            "{message}"
        );
        let requests = server
            .received_requests()
            .await
            .expect("wiremock request journal");
        assert_eq!(requests.len(), 1, "no over-budget second dispatch");
    }

    #[tokio::test]
    async fn responses_refuses_giant_function_output_before_second_dispatch() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "output": [{
                    "type": "function_call",
                    "call_id": "call_huge",
                    "name": "read_file",
                    "arguments": "{\"path\":\"huge.txt\"}"
                }],
                "usage": {"input_tokens": 100, "output_tokens": 10}
            })))
            .mount(&server)
            .await;

        let workspace = tempfile::TempDir::new().unwrap();
        std::fs::write(workspace.path().join("huge.txt"), "x".repeat(64_000)).unwrap();
        let task = "read the large fixture and report what it contains";
        let messages = giant_prompt_messages(task);
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
        ctx.workspace = workspace.path().to_str().unwrap();
        ctx.safe_context = Some(8_000);
        ctx.max_tool_rounds = 2;

        let error = openai_responses_complete(ctx, &mut NoMcp)
            .await
            .expect_err("giant function output must block the next request");
        let message = error.to_string();
        assert!(message.contains("Responses request needs"), "{message}");
        assert!(
            message.contains("function outputs were not truncated"),
            "{message}"
        );
        let requests = server
            .received_requests()
            .await
            .expect("wiremock request journal");
        assert_eq!(
            requests.len(),
            1,
            "the first tool call may dispatch, but its giant output must never be resent"
        );
    }

    #[tokio::test]
    async fn responses_durable_prompt_context_reaches_v1_responses_wire() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "output": [{
                        "type": "message", "role": "assistant",
                        "content": [{"type": "output_text", "text": "hello from responses"}]
                    }],
                    "usage": {"input_tokens": 12, "output_tokens": 4}
                })),
            )
            .mount(&server)
            .await;

        let store_root = tempfile::TempDir::new().unwrap();
        let store_workspace = tempfile::TempDir::new().unwrap();
        let store =
            crate::ConversationStore::new(store_root.path(), store_workspace.path(), 0).unwrap();
        let conversation_id = "responses-durable-wire";
        let exact_task = "do the thing through the durable Responses seam";
        let turn_prompt = store
            .begin_prompt(
                conversation_id,
                "Responses durable wire",
                None,
                crate::NewPrompt::operator(exact_task.as_bytes(), exact_task.as_bytes()),
            )
            .unwrap();
        let prompt_source = StorePromptSource::new(&store, conversation_id);
        let expected_address = turn_prompt.active().id().to_string();
        let messages = vec![
            MemMessage::system("you are a test"),
            MemMessage::user(exact_task),
        ];
        let caveats = Caveats::top();
        let uri = server.uri();
        let (reply, streamed, usage, _hallu) = openai_responses_complete_with_prompt(
            ChatCtx {
                url: &uri,
                model: "gpt-5-codex",
                kind: BackendKind::Openai,
                api_key: Some("sk-test"),
                messages: &messages,
                task: exact_task,
                workspace: ".",
                color: false,
                markdown: false,
                tool_offload: false,
                spill_store: None,
                compaction_store: None,
                scratchpad: false,
                scratchpad_store: None,
                code_search: None,
                where_is: None,
                nav: None,
                exposure: Default::default(),
                experience_store: None,
                step_ledger: None,
                caveats: &caveats,
                persona_tools: None,
                cognition: None,
                chat_completions_capability: Default::default(),
                reasoning_replay_scope: crate::model_card::ReasoningReplayScope::Never,
                max_tool_rounds: 5,
                narration_nudge_cap: 1,
                action_nudges: true,
                prompt_disposition: PromptDisposition::Act,
                prompt_intake: None,
                workflow_grace_rounds: 0,
                tool_output_lines: 20,
                debug: false,
                trace: false,
                num_ctx: None,
                input_ceiling_pct: 80,
                low_budget_pct: 15,
                connect_timeout_secs: 5,
                inference_timeout_secs: 120,
                mid_loop_trim_threshold: 40,
                compaction_trigger_policy: crate::CompactionTriggerPolicy::HeadroomAware,
                mid_loop_trim_tokens: None,
                max_ok_input: None,
                build_check_cmd: None,
                safe_context: None,
                recover_cw_400: None,
                note_sink: None,
                note_nudge: None,
                recall_source: None,
                memory_source: None,
                summarizer: None,
                compress_state: None,
                tool_events: None,
                phantom_reaches: None,
                end_reason: None,
                solve_obs: None,
                permission_gate: None,
                on_round_usage: None,
                estimate_ratio: None,
                estimation: crate::tokens::TokenEstimation::default(),
                summary_input_cap_floor_chars: 8_192,
                exec_floor: None,
                write_ledger: None,
                cancel: None,
                live_tool_output: None,
                git_tool: None,
                crew_runner: None,
                operating_mode_control: None,
                plan_mode_control: None,
            },
            Some(&turn_prompt),
            Some(&prompt_source),
            &mut NoMcp,
        )
        .await
        .expect("responses loop returns the message text");
        assert_eq!(reply, "hello from responses");
        assert!(!streamed);
        assert_eq!(usage.map(|u| u.input_tokens), Some(12));
        let requests = server
            .received_requests()
            .await
            .expect("wiremock request journal");
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        let instructions = body["instructions"].as_str().unwrap_or_default();
        assert!(instructions.contains(prompt_read::ACTIVE_PROMPT_PREFIX));
        assert!(instructions.contains(&format!("address: {expected_address}")));
        assert!(!instructions.contains("<ephemeral-unrecorded>"));
        assert!(body["input"].as_array().is_some_and(|input| input
            .iter()
            .any(|item| item["role"] == "user" && item["content"].as_str() == Some(exact_task))));
    }

    #[tokio::test]
    async fn openai_loop_honors_configured_cap_and_returns_real_final_answer() {
        let server = MockServer::start().await;
        let served = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(OpenAiResponder {
                tool_rounds_served: served.clone(),
                final_answer: "openai partial answer".into(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let cap = 2;
        let (reply, streamed, _usage, _hallu) = openai_chat_complete(
            ChatCtx {
                url: &server.uri(),
                model: "test-model",
                kind: BackendKind::Openai,
                api_key: Some("sk-test"),
                messages: &messages,
                task: "do the thing",
                workspace: ".",
                color: false,
                markdown: false,
                tool_offload: false,
                spill_store: None,
                compaction_store: None,
                scratchpad: false,
                scratchpad_store: None,
                code_search: None,
                where_is: None,
                nav: None,
                exposure: Default::default(),
                experience_store: None,
                step_ledger: None,
                caveats: &caveats,
                persona_tools: None,
                cognition: None,
                chat_completions_capability: Default::default(),
                reasoning_replay_scope: crate::model_card::ReasoningReplayScope::Never,
                max_tool_rounds: cap,
                narration_nudge_cap: 1,
                action_nudges: true,
                prompt_disposition: PromptDisposition::Act,
                prompt_intake: None,
                workflow_grace_rounds: 0,
                tool_output_lines: 20,
                debug: false,
                trace: false,
                num_ctx: None,
                input_ceiling_pct: 80,
                low_budget_pct: 15,
                connect_timeout_secs: 5,
                inference_timeout_secs: 120,
                mid_loop_trim_threshold: 40,
                compaction_trigger_policy: crate::CompactionTriggerPolicy::HeadroomAware,
                mid_loop_trim_tokens: None,
                max_ok_input: None,
                build_check_cmd: None,
                safe_context: None,
                recover_cw_400: None,
                note_sink: None,
                note_nudge: None,
                recall_source: None,
                memory_source: None,
                summarizer: None,
                compress_state: None,
                tool_events: None,
                phantom_reaches: None,
                end_reason: None,
                solve_obs: None,
                permission_gate: None,
                on_round_usage: None,
                estimate_ratio: None,
                estimation: crate::tokens::TokenEstimation::default(),
                summary_input_cap_floor_chars: 8_192,
                exec_floor: None,
                write_ledger: None,
                cancel: None,
                live_tool_output: None,
                git_tool: None,
                crew_runner: None,
                operating_mode_control: None,
                plan_mode_control: None,
            },
            &mut NoMcp,
        )
        .await
        .expect("openai_chat_complete should succeed");

        assert_eq!(served.load(Ordering::SeqCst), cap);
        assert_eq!(reply, "openai partial answer");
        assert_ne!(reply, "(reached tool-call limit)");
        assert!(!streamed);
    }

    /// 17.6: with a recorder lent in `ChatCtx.tool_events`, the Ollama loop
    /// records one event per executed tool call — name as invoked, digested
    /// args (keys + hash, never raw values), best-effort outcome, duration
    /// claim. Without a recorder (every other test here) nothing changes.
    #[tokio::test]
    async fn ollama_loop_records_tool_events_with_digested_args() {
        let server = MockServer::start().await;
        struct TwoToolResponder;
        impl Respond for TwoToolResponder {
            fn respond(&self, req: &Request) -> ResponseTemplate {
                if request_has_tools(req) {
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "message": { "content": "", "tool_calls": [
                            { "function": { "name": "list_dir",
                                            "arguments": {"path": "."} } },
                            { "function": { "name": "definitely_not_a_real_tool",
                                            "arguments": {"token": "tippy-top-secret"} } }
                        ]}
                    }))
                } else {
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "message": { "content": "done" }
                    }))
                }
            }
        }
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(TwoToolResponder)
            .mount(&server)
            .await;

        let ws = tempfile::TempDir::new().unwrap();
        let workspace = ws.path().to_string_lossy().into_owned();
        let messages = msgs();
        let caveats = Caveats::top();
        let mut events: Vec<crate::ToolEvent> = Vec::new();
        chat_complete(
            ChatCtx {
                url: &server.uri(),
                model: "test-model",
                kind: BackendKind::Ollama,
                api_key: None,
                messages: &messages,
                task: "do the thing",
                workspace: &workspace,
                color: false,
                markdown: false,
                tool_offload: false,
                spill_store: None,
                compaction_store: None,
                scratchpad: false,
                scratchpad_store: None,
                code_search: None,
                where_is: None,
                nav: None,
                exposure: Default::default(),
                experience_store: None,
                step_ledger: None,
                caveats: &caveats,
                persona_tools: None,
                cognition: None,
                chat_completions_capability: Default::default(),
                reasoning_replay_scope: crate::model_card::ReasoningReplayScope::Never,
                max_tool_rounds: 1,
                narration_nudge_cap: 1,
                action_nudges: true,
                prompt_disposition: PromptDisposition::Act,
                prompt_intake: None,
                workflow_grace_rounds: 0,
                tool_output_lines: 20,
                debug: false,
                trace: false,
                num_ctx: None,
                input_ceiling_pct: 80,
                low_budget_pct: 15,
                connect_timeout_secs: 5,
                inference_timeout_secs: 120,
                mid_loop_trim_threshold: 40,
                compaction_trigger_policy: crate::CompactionTriggerPolicy::HeadroomAware,
                mid_loop_trim_tokens: None,
                max_ok_input: None,
                build_check_cmd: None,
                safe_context: None,
                recover_cw_400: None,
                note_sink: None,
                note_nudge: None,
                recall_source: None,
                memory_source: None,
                summarizer: None,
                compress_state: None,
                tool_events: Some(&mut events),
                phantom_reaches: None,
                end_reason: None,
                solve_obs: None,
                permission_gate: None,
                on_round_usage: None,
                estimate_ratio: None,
                estimation: crate::tokens::TokenEstimation::default(),
                summary_input_cap_floor_chars: 8_192,
                exec_floor: None,
                write_ledger: None,
                cancel: None,
                live_tool_output: None,
                git_tool: None,
                crew_runner: None,
                operating_mode_control: None,
                plan_mode_control: None,
            },
            &mut NoMcp,
        )
        .await
        .expect("chat_complete should succeed");

        assert_eq!(events.len(), 2, "one event per tool call: {events:?}");
        assert_eq!(events[0].tool, "list_dir");
        assert!(events[0].ok, "a real listing reads as success");
        assert!(events[0].args_digest.contains("path"));
        assert!(events[0].duration_ms.is_some());
        assert_eq!(events[1].tool, "definitely_not_a_real_tool");
        assert!(!events[1].ok, "an unknown tool reads as failure");
        // Args are digested, never recorded raw.
        assert!(events[1].args_digest.contains("token"));
        assert!(
            !events[1].args_digest.contains("tippy-top-secret"),
            "raw arg value leaked: {}",
            events[1].args_digest
        );
    }

    /// 17.6: the OpenAI loop records the same per-call events (its tool
    /// arguments arrive as a JSON *string* — the digest must match the
    /// parsed-args digest the Ollama path produces for identical args).
    #[tokio::test]
    async fn openai_loop_records_tool_events_with_digested_args() {
        let server = MockServer::start().await;
        struct OneToolResponder;
        impl Respond for OneToolResponder {
            fn respond(&self, req: &Request) -> ResponseTemplate {
                if request_has_tools(req) {
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "choices": [{ "message": {
                            "content": null,
                            "tool_calls": [{
                                "id": "call_1",
                                "type": "function",
                                "function": { "name": "list_dir",
                                              "arguments": "{\"path\": \".\"}" }
                            }]
                        }}]
                    }))
                } else {
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "choices": [{ "message": { "content": "done" } }]
                    }))
                }
            }
        }
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(OneToolResponder)
            .mount(&server)
            .await;

        let ws = tempfile::TempDir::new().unwrap();
        let workspace = ws.path().to_string_lossy().into_owned();
        let messages = msgs();
        let caveats = Caveats::top();
        let mut events: Vec<crate::ToolEvent> = Vec::new();
        openai_chat_complete(
            ChatCtx {
                url: &server.uri(),
                model: "test-model",
                kind: BackendKind::Openai,
                api_key: Some("sk-test"),
                messages: &messages,
                task: "do the thing",
                workspace: &workspace,
                color: false,
                markdown: false,
                tool_offload: false,
                spill_store: None,
                compaction_store: None,
                scratchpad: false,
                scratchpad_store: None,
                code_search: None,
                where_is: None,
                nav: None,
                exposure: Default::default(),
                experience_store: None,
                step_ledger: None,
                caveats: &caveats,
                persona_tools: None,
                cognition: None,
                chat_completions_capability: Default::default(),
                reasoning_replay_scope: crate::model_card::ReasoningReplayScope::Never,
                max_tool_rounds: 1,
                narration_nudge_cap: 1,
                action_nudges: true,
                prompt_disposition: PromptDisposition::Act,
                prompt_intake: None,
                workflow_grace_rounds: 0,
                tool_output_lines: 20,
                debug: false,
                trace: false,
                num_ctx: None,
                input_ceiling_pct: 80,
                low_budget_pct: 15,
                connect_timeout_secs: 5,
                inference_timeout_secs: 120,
                mid_loop_trim_threshold: 40,
                compaction_trigger_policy: crate::CompactionTriggerPolicy::HeadroomAware,
                mid_loop_trim_tokens: None,
                max_ok_input: None,
                build_check_cmd: None,
                safe_context: None,
                recover_cw_400: None,
                note_sink: None,
                note_nudge: None,
                recall_source: None,
                memory_source: None,
                summarizer: None,
                compress_state: None,
                tool_events: Some(&mut events),
                phantom_reaches: None,
                end_reason: None,
                solve_obs: None,
                permission_gate: None,
                on_round_usage: None,
                estimate_ratio: None,
                estimation: crate::tokens::TokenEstimation::default(),
                summary_input_cap_floor_chars: 8_192,
                exec_floor: None,
                write_ledger: None,
                cancel: None,
                live_tool_output: None,
                git_tool: None,
                crew_runner: None,
                operating_mode_control: None,
                plan_mode_control: None,
            },
            &mut NoMcp,
        )
        .await
        .expect("openai_chat_complete should succeed");

        assert_eq!(events.len(), 1, "one event per tool call: {events:?}");
        assert_eq!(events[0].tool, "list_dir");
        assert!(events[0].ok);
        assert_eq!(
            events[0].args_digest,
            crate::ToolEvent::from_call("x", &serde_json::json!({"path": "."}), true, None)
                .args_digest,
            "string-encoded args must digest like parsed args"
        );
    }

    #[tokio::test]
    async fn cap_exit_fallback_when_final_summary_errors() {
        // No mock for the tools-disabled request would still 404 via the
        // tool-offering mock only matching when... actually both match the same
        // path, so instead we mount a server that always 500s the *second*
        // shape. Simpler: a server that returns tool calls for tools-present
        // and a 500 for tools-absent, forcing the fallback branch.
        let server = MockServer::start().await;
        let served = Arc::new(AtomicUsize::new(0));
        struct ErrOnFinal {
            served: Arc<AtomicUsize>,
        }
        impl Respond for ErrOnFinal {
            fn respond(&self, req: &Request) -> ResponseTemplate {
                if request_has_tools(req) {
                    self.served.fetch_add(1, Ordering::SeqCst);
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "message": { "content": "", "tool_calls": [{
                            "function": { "name": "definitely_not_a_real_tool", "arguments": {} }
                        }]}
                    }))
                } else {
                    ResponseTemplate::new(500).set_body_string("boom")
                }
            }
        }
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ErrOnFinal {
                served: served.clone(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let (reply, _streamed, _usage, _hallu) = chat_complete(
            ChatCtx {
                url: &server.uri(),
                model: "test-model",
                kind: BackendKind::Ollama,
                api_key: None,
                messages: &messages,
                task: "do the thing",
                workspace: ".",
                color: false,
                markdown: false,
                tool_offload: false,
                spill_store: None,
                compaction_store: None,
                scratchpad: false,
                scratchpad_store: None,
                code_search: None,
                where_is: None,
                nav: None,
                exposure: Default::default(),
                experience_store: None,
                step_ledger: None,
                caveats: &caveats,
                persona_tools: None,
                cognition: None,
                chat_completions_capability: Default::default(),
                reasoning_replay_scope: crate::model_card::ReasoningReplayScope::Never,
                max_tool_rounds: 2,
                narration_nudge_cap: 1,
                action_nudges: true,
                prompt_disposition: PromptDisposition::Act,
                prompt_intake: None,
                workflow_grace_rounds: 0,
                tool_output_lines: 20,
                debug: false,
                trace: false,
                num_ctx: None,
                input_ceiling_pct: 80,
                low_budget_pct: 15,
                connect_timeout_secs: 5,
                inference_timeout_secs: 120,
                mid_loop_trim_threshold: 40,
                compaction_trigger_policy: crate::CompactionTriggerPolicy::HeadroomAware,
                mid_loop_trim_tokens: None,
                max_ok_input: None,
                build_check_cmd: None,
                safe_context: None,
                recover_cw_400: None,
                note_sink: None,
                note_nudge: None,
                recall_source: None,
                memory_source: None,
                summarizer: None,
                compress_state: None,
                tool_events: None,
                phantom_reaches: None,
                end_reason: None,
                solve_obs: None,
                permission_gate: None,
                on_round_usage: None,
                estimate_ratio: None,
                estimation: crate::tokens::TokenEstimation::default(),
                summary_input_cap_floor_chars: 8_192,
                exec_floor: None,
                write_ledger: None,
                cancel: None,
                live_tool_output: None,
                git_tool: None,
                crew_runner: None,
                operating_mode_control: None,
                plan_mode_control: None,
            },
            &mut NoMcp,
        )
        .await
        .expect("chat_complete should succeed even when final summary errors");

        // Fallback names the limit + the knob — strictly better than the bare
        // placeholder.
        assert!(reply.contains("tool-call limit"));
        assert!(reply.contains("max_tool_rounds"));
    }

    /// `run_command` called with a tool name as the first word must return a
    /// corrective error message, not shell it through agent-bridle.
    #[tokio::test]
    async fn run_command_refuses_tool_name_as_shell_command() {
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = Caveats::top();
        for tool in [
            "list_dir",
            "read_file",
            "write_file",
            "use_skill",
            "web_fetch",
        ] {
            let args = serde_json::json!({ "command": format!("{tool} some/path") });
            let out = execute_tool(
                "run_command",
                &args,
                &ws.path().to_string_lossy(),
                false,
                20,
                &caveats,
                &mut NoMcp,
                None,
                None,
                None,
                None, // memory_source
                None,
                None,
                None, // git_tool
                None, // crew_runner
                None, // scratchpad_store
                None, // code_search
                None, // where_is
                None, // experience_store
                None, // step_ledger
            )
            .await;
            assert!(
                out.contains("is a tool, not a shell command"),
                "expected corrective message for '{tool}', got: {out}"
            );
        }
    }

    /// When the final summary 500s, the accumulated usage from the tool rounds
    /// must still be returned (not None), so usage.jsonl is not blank.
    #[tokio::test]
    async fn accumulated_usage_survives_summary_failure() {
        let server = MockServer::start().await;
        let served = Arc::new(AtomicUsize::new(0));

        struct UsageRoundsErrFinal {
            served: Arc<AtomicUsize>,
        }
        impl Respond for UsageRoundsErrFinal {
            fn respond(&self, req: &Request) -> ResponseTemplate {
                if request_has_tools(req) {
                    self.served.fetch_add(1, Ordering::SeqCst);
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "message": { "content": "", "tool_calls": [{
                            "function": { "name": "definitely_not_a_real_tool", "arguments": {} }
                        }]},
                        // Ollama reports per-round usage even in non-streaming mode.
                        "prompt_eval_count": 100,
                        "eval_count": 20,
                    }))
                } else {
                    ResponseTemplate::new(500).set_body_string("boom")
                }
            }
        }

        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(UsageRoundsErrFinal {
                served: served.clone(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let cap = 2;
        let (reply, _streamed, usage, hallu) = chat_complete(
            ChatCtx {
                url: &server.uri(),
                model: "test-model",
                kind: BackendKind::Ollama,
                api_key: None,
                messages: &messages,
                task: "do the thing",
                workspace: ".",
                color: false,
                markdown: false,
                tool_offload: false,
                spill_store: None,
                compaction_store: None,
                scratchpad: false,
                scratchpad_store: None,
                code_search: None,
                where_is: None,
                nav: None,
                exposure: Default::default(),
                experience_store: None,
                step_ledger: None,
                caveats: &caveats,
                persona_tools: None,
                cognition: None,
                chat_completions_capability: Default::default(),
                reasoning_replay_scope: crate::model_card::ReasoningReplayScope::Never,
                max_tool_rounds: cap,
                narration_nudge_cap: 1,
                action_nudges: true,
                prompt_disposition: PromptDisposition::Act,
                prompt_intake: None,
                workflow_grace_rounds: 0,
                tool_output_lines: 20,
                debug: false,
                trace: false,
                num_ctx: None,
                input_ceiling_pct: 80,
                low_budget_pct: 15,
                connect_timeout_secs: 5,
                inference_timeout_secs: 120,
                mid_loop_trim_threshold: 40,
                compaction_trigger_policy: crate::CompactionTriggerPolicy::HeadroomAware,
                mid_loop_trim_tokens: None,
                max_ok_input: None,
                build_check_cmd: None,
                safe_context: None,
                recover_cw_400: None,
                note_sink: None,
                note_nudge: None,
                recall_source: None,
                memory_source: None,
                summarizer: None,
                compress_state: None,
                tool_events: None,
                phantom_reaches: None,
                end_reason: None,
                solve_obs: None,
                permission_gate: None,
                on_round_usage: None,
                estimate_ratio: None,
                estimation: crate::tokens::TokenEstimation::default(),
                summary_input_cap_floor_chars: 8_192,
                exec_floor: None,
                write_ledger: None,
                cancel: None,
                live_tool_output: None,
                git_tool: None,
                crew_runner: None,
                operating_mode_control: None,
                plan_mode_control: None,
            },
            &mut NoMcp,
        )
        .await
        .expect("chat_complete must succeed even when final summary errors");

        // The fallback reply must contain accumulated token counts.
        assert!(reply.contains("tool-call limit"), "got: {reply}");
        assert!(
            reply.contains("in / ") && reply.contains("out tokens"),
            "fallback must include accumulated token counts, got: {reply}"
        );

        // The usage returned must be non-None and reflect the rounds.
        let u = usage.expect("usage must be Some even when final summary fails");
        // SEMANTICS CHANGED in Step 18.1: each round's 100-token prompt
        // contained the same history, so the turn input is the largest single
        // prompt (100), not the 200 sum that double-counted it.
        assert_eq!(
            u.input_tokens, 100,
            "largest single prompt across 2 rounds, not the sum"
        );
        assert_eq!(
            u.output_tokens, 40,
            "2 rounds × 20 output tokens each = 40 total"
        );

        // Unknown tool calls during cap rounds counted as hallucinations.
        assert_eq!(
            hallu, cap as u32,
            "each round had one hallucinated tool call"
        );
    }

    // -----------------------------------------------------------------------
    // Read-only nudge injection test
    //
    // Scenario: model keeps calling list_dir (read-only) for 3 rounds.
    // On round 4 the harness injects the nudge.  The responder detects the
    // nudge text in the message list and returns a final text answer instead
    // of another tool call, proving the nudge reached the model.
    // -----------------------------------------------------------------------

    struct ReadOnlyNudgeResponder {
        /// Flipped to true the first time the responder sees the nudge text.
        nudge_seen: Arc<std::sync::atomic::AtomicBool>,
    }

    impl Respond for ReadOnlyNudgeResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let body = serde_json::from_slice::<serde_json::Value>(&req.body).unwrap_or_default();
            let has_nudge = body["messages"]
                .as_array()
                .map(|msgs| {
                    msgs.iter().any(|m| {
                        m["content"]
                            .as_str()
                            .map(|c| c.contains("read-only rounds so far"))
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false);

            if has_nudge {
                self.nudge_seen
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                // Return a plain text answer — no more tool calls.
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "content": "nudge received, writing file now" }
                }))
            } else if request_has_tools(req) {
                // Keep returning list_dir calls until the nudge arrives.
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {
                        "content": "",
                        "tool_calls": [{ "function": {
                            "name": "list_dir",
                            "arguments": { "path": "." }
                        }}]
                    }
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "content": "final summary" }
                }))
            }
        }
    }

    #[tokio::test]
    async fn read_only_nudge_injected_after_three_rounds() {
        let server = MockServer::start().await;
        let nudge_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));

        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ReadOnlyNudgeResponder {
                nudge_seen: nudge_seen.clone(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let (reply, _streamed, _usage, _hallu) = chat_complete(
            ChatCtx {
                url: &server.uri(),
                model: "test-model",
                kind: BackendKind::Ollama,
                api_key: None,
                messages: &messages,
                task: "list all files",
                workspace: ".",
                color: false,
                markdown: false,
                tool_offload: false,
                spill_store: None,
                compaction_store: None,
                scratchpad: false,
                scratchpad_store: None,
                code_search: None,
                where_is: None,
                nav: None,
                exposure: Default::default(),
                experience_store: None,
                step_ledger: None,
                caveats: &caveats,
                persona_tools: None,
                cognition: None,
                chat_completions_capability: Default::default(),
                reasoning_replay_scope: crate::model_card::ReasoningReplayScope::Never,
                max_tool_rounds: 10,
                narration_nudge_cap: 1,
                action_nudges: true,
                prompt_disposition: PromptDisposition::Act,
                prompt_intake: None,
                workflow_grace_rounds: 0,
                tool_output_lines: 5,
                debug: false,
                trace: false,
                num_ctx: None,
                input_ceiling_pct: 80,
                low_budget_pct: 15,
                connect_timeout_secs: 5,
                inference_timeout_secs: 30,
                mid_loop_trim_threshold: 40,
                compaction_trigger_policy: crate::CompactionTriggerPolicy::HeadroomAware,
                mid_loop_trim_tokens: None,
                max_ok_input: None,
                build_check_cmd: None,
                safe_context: None,
                recover_cw_400: None,
                note_sink: None,
                note_nudge: None,
                recall_source: None,
                memory_source: None,
                summarizer: None,
                compress_state: None,
                tool_events: None,
                phantom_reaches: None,
                end_reason: None,
                solve_obs: None,
                permission_gate: None,
                on_round_usage: None,
                estimate_ratio: None,
                estimation: crate::tokens::TokenEstimation::default(),
                summary_input_cap_floor_chars: 8_192,
                exec_floor: None,
                write_ledger: None,
                cancel: None,
                live_tool_output: None,
                git_tool: None,
                crew_runner: None,
                operating_mode_control: None,
                plan_mode_control: None,
            },
            &mut NoMcp,
        )
        .await
        .expect("chat_complete should succeed");

        assert!(
            nudge_seen.load(std::sync::atomic::Ordering::SeqCst),
            "nudge was never injected after 3 consecutive read-only rounds"
        );
        assert_eq!(
            reply, "nudge received, writing file now",
            "model should have responded to the nudge with a final answer"
        );
    }
}

// ---------------------------------------------------------------------------
// HTTP-loop tests — streaming, overflow retry, mid-loop trim, and final
// summary, all against wiremock backends.
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "mod_tests/artifact_compaction_provenance.rs"]
mod artifact_compaction_provenance_tests;
#[cfg(test)]
#[path = "mod_tests/artifact_provenance.rs"]
mod artifact_provenance_tests;
#[cfg(test)]
#[path = "mod_tests/http_loop.rs"]
mod http_loop_tests;
// #1265: EPIC #1257's acceptance gate — the "10 largest Rust files" session
// replayed end-to-end (BAT tier: scripted backend, simulated workspace).
#[cfg(test)]
#[path = "mod_tests/bat_largest_files.rs"]
mod bat_largest_files_tests;

// ---------------------------------------------------------------------------
// save_note tool + memory nudge — loop integration (Step 19.3, #248)
// ---------------------------------------------------------------------------
//
// Wiremock-backed tests against both agentic loops, pinning:
//   (1) save_note is advertised iff a NoteSink is present, and a save_note
//       tool call routes through the sink with the result fed back;
//   (2) the in-band memory nudge is appended to the user message when due —
//       and ONLY when a sink exists;
//   (3) organic save_note use resets the nudge counter (the read-only-rounds
//       reset pattern, hermes's reset-on-memory-write).
#[cfg(test)]
mod save_note_loop_tests {
    use super::note_sink::tests::MockSink;
    use super::*;
    use crate::caveats::Caveats;
    use crate::{BackendKind, MemMessage};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    fn msgs() -> Vec<MemMessage> {
        vec![
            MemMessage::system("you are a test"),
            MemMessage::user("do the thing"),
        ]
    }

    fn ctx<'a>(
        server_uri: &'a str,
        messages: &'a [MemMessage],
        caveats: &'a Caveats,
    ) -> ChatCtx<'a> {
        ChatCtx {
            url: server_uri,
            model: "test-model",
            kind: BackendKind::Ollama,
            api_key: None,
            messages,
            task: "do the thing",
            workspace: ".",
            color: false,
            markdown: false,
            tool_offload: false,
            spill_store: None,
            compaction_store: None,
            scratchpad: false,
            scratchpad_store: None,
            code_search: None,
            where_is: None,
            nav: None,
            exposure: Default::default(),
            experience_store: None,
            step_ledger: None,
            caveats,
            persona_tools: None,
            cognition: None,
            chat_completions_capability: Default::default(),
            reasoning_replay_scope: crate::model_card::ReasoningReplayScope::Never,
            max_tool_rounds: 6,
            narration_nudge_cap: 1,
            action_nudges: true,
            prompt_disposition: PromptDisposition::Act,
            prompt_intake: None,
            workflow_grace_rounds: 0,
            tool_output_lines: 20,
            debug: false,
            trace: false,
            num_ctx: None,
            input_ceiling_pct: 80,
            low_budget_pct: 15,
            connect_timeout_secs: 5,
            inference_timeout_secs: 30,
            mid_loop_trim_threshold: 40,
            compaction_trigger_policy: crate::CompactionTriggerPolicy::HeadroomAware,
            mid_loop_trim_tokens: None,
            max_ok_input: None,
            build_check_cmd: None,
            safe_context: None,
            recover_cw_400: None,
            note_sink: None,
            note_nudge: None,
            recall_source: None,
            memory_source: None,
            summarizer: None,
            compress_state: None,
            tool_events: None,
            phantom_reaches: None,
            end_reason: None,
            solve_obs: None,
            permission_gate: None,
            on_round_usage: None,
            estimate_ratio: None,
            estimation: crate::tokens::TokenEstimation::default(),
            summary_input_cap_floor_chars: 8_192,
            exec_floor: None,
            write_ledger: None,
            cancel: None,
            live_tool_output: None,
            git_tool: None,
            crew_runner: None,
            operating_mode_control: None,
            plan_mode_control: None,
        }
    }

    fn body_json(req: &Request) -> serde_json::Value {
        serde_json::from_slice(&req.body).unwrap_or_default()
    }

    fn advertised_tool_names(body: &serde_json::Value) -> Vec<String> {
        body["tools"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|d| d["function"]["name"].as_str())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn messages_contain(body: &serde_json::Value, needle: &str) -> bool {
        body["messages"]
            .as_array()
            .map(|msgs| {
                msgs.iter().any(|m| {
                    m["content"]
                        .as_str()
                        .map(|c| c.contains(needle))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    }

    /// Ollama-shaped responder: issues one save_note tool call, then a final
    /// text answer once the "note saved:" tool result is visible in history.
    /// Also records whether save_note was advertised and whether the memory
    /// nudge line reached the model.
    struct SaveNoteResponder {
        save_note_advertised: Arc<AtomicBool>,
        nudge_seen: Arc<AtomicBool>,
        final_answer: String,
    }

    impl Respond for SaveNoteResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let body = body_json(req);
            if advertised_tool_names(&body).contains(&"save_note".to_string()) {
                self.save_note_advertised.store(true, Ordering::SeqCst);
            }
            if messages_contain(&body, "[system reminder:")
                && messages_contain(&body, "without a saved note")
            {
                self.nudge_seen.store(true, Ordering::SeqCst);
            }
            if messages_contain(&body, "note saved:") {
                // The tool result round-tripped — answer for real now.
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "content": self.final_answer }
                }))
            } else if body.get("tools").is_some() {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {
                        "content": "",
                        "tool_calls": [{ "function": {
                            "name": "save_note",
                            "arguments": {
                                "action": "add",
                                "text": "user prefers vi keybindings"
                            }
                        }}]
                    }
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "content": "final summary" }
                }))
            }
        }
    }

    #[tokio::test]
    async fn ollama_save_note_routes_to_sink_and_result_feeds_back() {
        let server = MockServer::start().await;
        let advertised = Arc::new(AtomicBool::new(false));
        let nudge_seen = Arc::new(AtomicBool::new(false));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(SaveNoteResponder {
                save_note_advertised: advertised.clone(),
                nudge_seen: nudge_seen.clone(),
                final_answer: "noted, moving on".into(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut sink = MockSink::default();
        let mut c = ctx(&uri, &messages, &caveats);
        c.note_sink = Some(&mut sink);
        let (reply, _streamed, _usage, hallu) = chat_complete(c, &mut NoMcp)
            .await
            .expect("chat_complete should succeed");

        assert!(
            advertised.load(Ordering::SeqCst),
            "save_note must be advertised when a sink is present"
        );
        assert_eq!(
            sink.calls,
            vec!["add:user prefers vi keybindings"],
            "the tool call must route through the sink"
        );
        assert_eq!(reply, "noted, moving on");
        assert_eq!(hallu, 0, "save_note is a real tool, not a hallucination");
        assert!(
            !nudge_seen.load(Ordering::SeqCst),
            "no nudge configured — none may be injected"
        );
    }

    /// Without a sink the tool must be absent from the advertised set, and a
    /// configured nudge must NOT be appended (absent-without-sink).
    struct NoSinkObserver {
        save_note_advertised: Arc<AtomicBool>,
        nudge_seen: Arc<AtomicBool>,
    }

    impl Respond for NoSinkObserver {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let body = body_json(req);
            if advertised_tool_names(&body).contains(&"save_note".to_string()) {
                self.save_note_advertised.store(true, Ordering::SeqCst);
            }
            if messages_contain(&body, "[system reminder:") {
                self.nudge_seen.store(true, Ordering::SeqCst);
            }
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": { "content": "plain answer" }
            }))
        }
    }

    #[tokio::test]
    async fn without_sink_no_tool_and_no_nudge_even_when_due() {
        let server = MockServer::start().await;
        let advertised = Arc::new(AtomicBool::new(false));
        let nudge_seen = Arc::new(AtomicBool::new(false));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(NoSinkObserver {
                save_note_advertised: advertised.clone(),
                nudge_seen: nudge_seen.clone(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        // A nudge that is overdue (interval 1, one quiet turn already counted)…
        let mut nudge = NoteNudge::new(1);
        let _ = nudge.begin_turn();
        let mut c = ctx(&uri, &messages, &caveats);
        // …but NO sink: the loop must neither advertise nor nudge.
        c.note_nudge = Some(&mut nudge);
        let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
            .await
            .expect("chat_complete should succeed");

        assert_eq!(reply, "plain answer");
        assert!(
            !advertised.load(Ordering::SeqCst),
            "save_note advertised without a sink"
        );
        assert!(
            !nudge_seen.load(Ordering::SeqCst),
            "nudge injected without a sink"
        );
    }

    #[tokio::test]
    async fn nudge_appended_to_user_message_when_due() {
        let server = MockServer::start().await;
        let advertised = Arc::new(AtomicBool::new(false));
        let nudge_seen = Arc::new(AtomicBool::new(false));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(NoSinkObserver {
                save_note_advertised: advertised.clone(),
                nudge_seen: nudge_seen.clone(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut sink = MockSink::default();
        // One quiet turn already elapsed → due on this (the next) turn.
        let mut nudge = NoteNudge::new(1);
        let _ = nudge.begin_turn();
        let mut c = ctx(&uri, &messages, &caveats);
        c.note_sink = Some(&mut sink);
        c.note_nudge = Some(&mut nudge);
        let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
            .await
            .expect("chat_complete should succeed");

        assert_eq!(reply, "plain answer");
        assert!(
            nudge_seen.load(Ordering::SeqCst),
            "the reminder line must reach the model on the due turn"
        );
    }

    #[tokio::test]
    async fn organic_save_resets_the_nudge_counter() {
        let server = MockServer::start().await;
        let advertised = Arc::new(AtomicBool::new(false));
        let nudge_seen = Arc::new(AtomicBool::new(false));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(SaveNoteResponder {
                save_note_advertised: advertised.clone(),
                nudge_seen: nudge_seen.clone(),
                final_answer: "done".into(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut sink = MockSink::default();
        let mut nudge = NoteNudge::new(1);
        let mut c = ctx(&uri, &messages, &caveats);
        c.note_sink = Some(&mut sink);
        c.note_nudge = Some(&mut nudge);
        let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
            .await
            .expect("chat_complete should succeed");
        assert_eq!(reply, "done");
        assert_eq!(sink.calls.len(), 1, "the model saved organically");

        // The turn included an organic save → the counter restarted, so the
        // next turn must NOT be nudged (without the save, interval=1 would
        // have made it due).
        assert!(
            nudge.begin_turn().is_none(),
            "organic save_note use must reset the nudge counter"
        );
    }

    /// OpenAI-shaped mirror: save_note advertised + routed, nudge appended.
    struct OpenAiSaveNoteResponder {
        save_note_advertised: Arc<AtomicBool>,
        nudge_seen: Arc<AtomicBool>,
    }

    impl Respond for OpenAiSaveNoteResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let body = body_json(req);
            if advertised_tool_names(&body).contains(&"save_note".to_string()) {
                self.save_note_advertised.store(true, Ordering::SeqCst);
            }
            if messages_contain(&body, "[system reminder:") {
                self.nudge_seen.store(true, Ordering::SeqCst);
            }
            if messages_contain(&body, "note saved:") {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{ "message": { "content": "openai noted" } }]
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{ "message": {
                        "content": null,
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "save_note",
                                "arguments": "{\"action\":\"add\",\"text\":\"CI gate is just check\"}"
                            }
                        }]
                    }}]
                }))
            }
        }
    }

    #[tokio::test]
    async fn openai_save_note_routes_and_nudge_appends() {
        let server = MockServer::start().await;
        let advertised = Arc::new(AtomicBool::new(false));
        let nudge_seen = Arc::new(AtomicBool::new(false));
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(OpenAiSaveNoteResponder {
                save_note_advertised: advertised.clone(),
                nudge_seen: nudge_seen.clone(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut sink = MockSink::default();
        let mut nudge = NoteNudge::new(1);
        let _ = nudge.begin_turn(); // due on this turn
        let mut c = ctx(&uri, &messages, &caveats);
        c.kind = BackendKind::Openai;
        c.note_sink = Some(&mut sink);
        c.note_nudge = Some(&mut nudge);
        let (reply, _, _, hallu) = chat_complete(c, &mut NoMcp)
            .await
            .expect("openai loop should succeed");

        assert_eq!(reply, "openai noted");
        assert_eq!(sink.calls, vec!["add:CI gate is just check"]);
        assert!(advertised.load(Ordering::SeqCst));
        assert!(nudge_seen.load(Ordering::SeqCst));
        assert_eq!(hallu, 0);
    }

    /// A sink error (here: the 19.1 over-budget curator error) must round-trip
    /// to the model verbatim as the tool result so it can replace/remove and
    /// retry — pinned end-to-end through the loop.
    struct ErrorEchoResponder {
        error_seen_by_model: Arc<AtomicBool>,
    }

    impl Respond for ErrorEchoResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let body = body_json(req);
            if messages_contain(&body, "Replace or remove existing entries first")
                && messages_contain(&body, "1. an existing entry")
            {
                self.error_seen_by_model.store(true, Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "content": "I will curate first" }
                }))
            } else if body.get("tools").is_some() {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {
                        "content": "",
                        "tool_calls": [{ "function": {
                            "name": "save_note",
                            "arguments": { "action": "add", "text": "too big" }
                        }}]
                    }
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "content": "final summary" }
                }))
            }
        }
    }

    #[tokio::test]
    async fn over_budget_error_round_trips_to_the_model() {
        let server = MockServer::start().await;
        let error_seen = Arc::new(AtomicBool::new(false));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ErrorEchoResponder {
                error_seen_by_model: error_seen.clone(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut sink = MockSink {
            fail_with: Some(
                "NOTES.md is full: this write needs 99/50 chars. \
                 Replace or remove existing entries first.\nCurrent entries:\n  1. an existing entry"
                    .into(),
            ),
            ..Default::default()
        };
        let mut c = ctx(&uri, &messages, &caveats);
        c.note_sink = Some(&mut sink);
        let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
            .await
            .expect("chat_complete should succeed");

        assert_eq!(reply, "I will curate first");
        assert!(
            error_seen.load(Ordering::SeqCst),
            "the curator error (full entry list + instruction) must reach the model verbatim"
        );
    }
}

// ---------------------------------------------------------------------------
// Compression v2 — summarize, don't discard (Step 18.4, #247)
// ---------------------------------------------------------------------------
//
// End-to-end wiremock tests for the compression pipeline wired into both
// loops. The headline property is B5's acceptance criterion from the context
// baseline (docs/testing/results/context-baseline-f0f4f6e.md): a long
// tool-heavy conversation crosses the token budget, compression fires, and
// the ORIGINAL TASK still reaches the next request — where the baseline
// measured 9/10 silently wrong answers because truncation discarded it (B6).
#[cfg(test)]
mod compression_loop_tests {
    use super::*;
    use crate::caveats::Caveats;
    use crate::{BackendKind, MemMessage};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    const TASK: &str =
        "ACTIVE TASK GAUNTLET-7f3d9c: read big.txt until told to stop, then restate this marker";
    const CANNED_SUMMARY: &str =
        "## Active Task\nACTIVE TASK GAUNTLET-7f3d9c (canned summary)\n## Completed Actions\n1. read big.txt";

    fn msgs() -> Vec<MemMessage> {
        vec![MemMessage::system("you are a test"), MemMessage::user(TASK)]
    }

    fn ctx<'a>(
        server_uri: &'a str,
        messages: &'a [MemMessage],
        caveats: &'a Caveats,
        workspace: &'a str,
    ) -> ChatCtx<'a> {
        ChatCtx {
            url: server_uri,
            model: "test-model",
            kind: BackendKind::Ollama,
            api_key: None,
            messages,
            task: TASK,
            workspace,
            color: false,
            markdown: false,
            tool_offload: false,
            spill_store: None,
            compaction_store: None,
            scratchpad: false,
            scratchpad_store: None,
            code_search: None,
            where_is: None,
            nav: None,
            exposure: Default::default(),
            experience_store: None,
            step_ledger: None,
            caveats,
            persona_tools: None,
            cognition: None,
            chat_completions_capability: Default::default(),
            reasoning_replay_scope: crate::model_card::ReasoningReplayScope::Never,
            max_tool_rounds: 12,
            narration_nudge_cap: 1,
            action_nudges: true,
            prompt_disposition: PromptDisposition::Act,
            prompt_intake: None,
            workflow_grace_rounds: 0,
            tool_output_lines: 2,
            debug: false,
            trace: false,
            num_ctx: None,
            input_ceiling_pct: 80,
            low_budget_pct: 15,
            connect_timeout_secs: 5,
            inference_timeout_secs: 30,
            mid_loop_trim_threshold: 40,
            compaction_trigger_policy: crate::CompactionTriggerPolicy::HeadroomAware,
            // The token trigger under test: well below what a few 4 KB
            // tool results accumulate to.
            mid_loop_trim_tokens: Some(5_000),
            max_ok_input: None,
            build_check_cmd: None,
            safe_context: None,
            recover_cw_400: None,
            note_sink: None,
            note_nudge: None,
            recall_source: None,
            memory_source: None,
            summarizer: None,
            compress_state: None,
            tool_events: None,
            phantom_reaches: None,
            end_reason: None,
            solve_obs: None,
            permission_gate: None,
            on_round_usage: None,
            estimate_ratio: None,
            estimation: crate::tokens::TokenEstimation::default(),
            summary_input_cap_floor_chars: 8_192,
            exec_floor: None,
            write_ledger: None,
            cancel: None,
            live_tool_output: None,
            git_tool: None,
            crew_runner: None,
            operating_mode_control: None,
            plan_mode_control: None,
        }
    }

    /// Workspace with one ~4 KB file the mock model reads over and over.
    fn gauntlet_workspace() -> tempfile::TempDir {
        let ws = tempfile::TempDir::new().unwrap();
        let line = "the quick brown newt compresses context without discarding it\n";
        std::fs::write(ws.path().join("big.txt"), line.repeat(64)).unwrap();
        ws
    }

    fn body_json(req: &Request) -> serde_json::Value {
        serde_json::from_slice(&req.body).unwrap_or_default()
    }

    fn messages_contain(body: &serde_json::Value, needle: &str) -> bool {
        body["messages"]
            .as_array()
            .map(|msgs| {
                msgs.iter().any(|m| {
                    m["content"]
                        .as_str()
                        .map(|c| c.contains(needle))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    }

    /// chars/4 estimate of a request's message list (mirrors the loop's
    /// fallback estimator) — used to measure the reclaim across requests.
    fn body_message_tokens(body: &serde_json::Value) -> usize {
        body["messages"]
            .as_array()
            .map(|msgs| {
                msgs.iter()
                    .map(|m| {
                        crate::tokens::TokenEstimation::default()
                            .tokens_for_chars(m.to_string().chars().count())
                    })
                    .sum()
            })
            .unwrap_or(0)
    }

    /// chars/4 estimate of the complete model input: messages plus the tool
    /// schemas that ride on every tools-enabled request.
    fn body_request_tokens(body: &serde_json::Value) -> usize {
        let estimation = crate::tokens::TokenEstimation::default();
        body_message_tokens(body)
            + body
                .get("tools")
                .map(|tools| estimate_value_tokens(tools, estimation))
                .unwrap_or(0)
    }

    /// A summarizer that records every request it receives and returns the
    /// canned summary.
    fn canned_summarizer(prompts: Arc<Mutex<Vec<String>>>) -> Summarizer {
        Box::new(move |prompt: String| {
            let prompts = prompts.clone();
            Box::pin(async move {
                prompts.lock().unwrap().push(prompt);
                Ok(CANNED_SUMMARY.to_string())
            })
        })
    }

    /// Ollama-shaped gauntlet responder: keeps demanding `read_file` of the
    /// big fixture until the compaction marker shows up in the request, then
    /// answers. Records per-request observations the assertions need.
    struct GauntletResponder {
        final_answer: String,
        /// `(had_marker, est_message_tokens, est_full_request_tokens)` per
        /// non-streaming request.
        log: Arc<Mutex<Vec<(bool, usize, usize)>>>,
        task_in_marker_request: Arc<AtomicBool>,
        summary_in_marker_request: Arc<AtomicBool>,
        old_placeholder_seen: Arc<AtomicBool>,
        static_marker_instead: bool,
    }

    impl Respond for GauntletResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let body = body_json(req);
            let has_marker = messages_contain(&body, SUMMARY_PREFIX);
            if !body["stream"].as_bool().unwrap_or(false) {
                self.log.lock().unwrap().push((
                    has_marker,
                    body_message_tokens(&body),
                    body_request_tokens(&body),
                ));
            }
            if messages_contain(&body, "earlier tool-call messages omitted") {
                self.old_placeholder_seen.store(true, Ordering::SeqCst);
            }
            if has_marker {
                if messages_contain(&body, TASK) {
                    self.task_in_marker_request.store(true, Ordering::SeqCst);
                }
                let summary_needle = if self.static_marker_instead {
                    "Summary generation was unavailable."
                } else {
                    CANNED_SUMMARY
                };
                if messages_contain(&body, summary_needle) {
                    self.summary_in_marker_request.store(true, Ordering::SeqCst);
                }
                return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "content": self.final_answer }
                }));
            }
            if body.get("tools").is_some() {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "content": "", "tool_calls": [{
                        "function": { "name": "read_file", "arguments": { "path": "big.txt" } }
                    }]}
                }))
            } else {
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "message": { "content": "cap exit" } }))
            }
        }
    }

    /// THE B5 acceptance property: compression fires on a long tool-heavy
    /// conversation and the original task text still reaches the next
    /// request — summarized, not discarded.
    #[tokio::test]
    async fn active_task_survives_compression() {
        let server = MockServer::start().await;
        let log = Arc::new(Mutex::new(Vec::new()));
        let task_in_marker = Arc::new(AtomicBool::new(false));
        let summary_in_marker = Arc::new(AtomicBool::new(false));
        let old_placeholder = Arc::new(AtomicBool::new(false));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(GauntletResponder {
                final_answer: "the marker is GAUNTLET-7f3d9c".into(),
                log: log.clone(),
                task_in_marker_request: task_in_marker.clone(),
                summary_in_marker_request: summary_in_marker.clone(),
                old_placeholder_seen: old_placeholder.clone(),
                static_marker_instead: false,
            })
            .mount(&server)
            .await;

        let ws = gauntlet_workspace();
        let workspace = ws.path().to_string_lossy().to_string();
        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let summarizer = canned_summarizer(prompts.clone());
        let mut compress_state = CompressState::new();
        let mut c = ctx(&uri, &messages, &caveats, &workspace);
        // The trigger is a complete-request ceiling, including the expanded
        // builtin tool catalog. Derive it as the live catalog weight plus ~5k
        // message tokens of headroom (a catalog-INDEPENDENT offset) so the
        // first zero-cost prune remains observable on wire before the
        // summarizing pass; catalog growth shifts the threshold with it. The
        // >40% reclaim assertion below still measures actual dispatched
        // requests.
        c.mid_loop_trim_tokens = Some(
            builtin_catalog_tokens(PromptDisposition::Act)
                + prompt_read::response_repository_policy_tokens()
                + 5_600,
        );
        c.summarizer = Some(&*summarizer);
        c.compress_state = Some(&mut compress_state);
        let (reply, _streamed, _usage, hallu) = chat_complete(c, &mut NoMcp)
            .await
            .expect("chat_complete should succeed");

        // The turn completed with a real answer, not a cap/diagnostic exit.
        assert_eq!(reply, "the marker is GAUNTLET-7f3d9c");
        assert_eq!(hallu, 0);

        // The summarizer ran exactly once and its request carried the
        // original task verbatim (the verbatim-Active-Task anchor).
        let prompts = prompts.lock().unwrap();
        assert_eq!(prompts.len(), 1, "one compression, one summary request");
        assert!(
            prompts[0].contains(TASK),
            "summary request must quote the task verbatim"
        );

        // The post-compression request still carried the task AND the
        // summary, wrapped in the marker — summarize, don't discard.
        assert!(task_in_marker.load(Ordering::SeqCst), "B5 property");
        assert!(summary_in_marker.load(Ordering::SeqCst));
        assert!(
            !old_placeholder.load(Ordering::SeqCst),
            "the old amputation placeholder must never be dispatched"
        );

        // Reclaim numbers: the compressed request must be materially smaller
        // than the largest pre-compression request.
        let log = log.lock().unwrap();
        let before = log
            .iter()
            .filter(|(m, ..)| !m)
            .map(|&(_, t, _)| t)
            .max()
            .expect("pre-compression requests were dispatched");
        let after = log
            .iter()
            .find(|(m, ..)| *m)
            .map(|&(_, t, _)| t)
            .expect("a compressed request was dispatched");
        println!("e2e reclaim: ~{before} -> ~{after} est. message tokens");
        let fixed = prompt_read::response_repository_policy_tokens();
        let reclaimable_before = before.saturating_sub(fixed);
        let reclaimable_after = after.saturating_sub(fixed);
        assert!(
            reclaimable_after < reclaimable_before * 6 / 10,
            "compression must reclaim >40% of the non-policy messages here \
             (got {before} -> {after}, fixed policy ~{fixed})"
        );
    }

    /// THE B6 regression (#282): a FIRST-turn request on a fresh capability
    /// cache (no `max_ok_input`, no `safe_context`) whose history exceeds the
    /// `num_ctx` ceiling must compress BEFORE dispatch and dispatch under the
    /// ceiling. Pre-fix, `send_budget` was `None` here — the after-benchmark
    /// measured all 10 B6 runs shipping ~41k-token requests into a forced
    /// 6,144 window with zero compression events (8/10 silently wrong),
    /// because the `num_ctx` newt itself sent fed into nothing.
    #[tokio::test]
    async fn first_turn_over_num_ctx_ceiling_compresses_before_dispatch() {
        let server = MockServer::start().await;
        let log = Arc::new(Mutex::new(Vec::new()));
        let task_in_marker = Arc::new(AtomicBool::new(false));
        let summary_in_marker = Arc::new(AtomicBool::new(false));
        let old_placeholder = Arc::new(AtomicBool::new(false));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(GauntletResponder {
                final_answer: "the marker is GAUNTLET-7f3d9c".into(),
                log: log.clone(),
                task_in_marker_request: task_in_marker.clone(),
                summary_in_marker_request: summary_in_marker.clone(),
                old_placeholder_seen: old_placeholder.clone(),
                static_marker_instead: false,
            })
            .mount(&server)
            .await;

        // The B6 shape, condensed: turn 1 of a fresh process already carries
        // a history far over the forced window (a restored conversation whose
        // assistant replies dumped file contents) — ~34k chars ≈ 9k estimated
        // tokens against a 6,144 num_ctx. The task itself is small and sits
        // up front (the protected head), the bulk is summarizable middle, and
        // the recent tail is small — compression CAN reach the budget here,
        // so a still-over-budget dispatch would be a wiring failure, not an
        // incompressibility artifact.
        let filler = "the quick brown newt reads three fifty-kilobyte fixtures\n".repeat(50);
        let mut messages = vec![MemMessage::system("you are a test"), MemMessage::user(TASK)];
        for _ in 0..12 {
            messages.push(MemMessage::assistant(format!("file contents: {filler}")));
            messages.push(MemMessage::user("continue"));
        }

        let ws = gauntlet_workspace();
        let workspace = ws.path().to_string_lossy().to_string();
        let caveats = Caveats::top();
        let uri = server.uri();
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let summarizer = canned_summarizer(prompts.clone());
        let mut compress_state = CompressState::new();
        let mut c = ctx(&uri, &messages, &caveats, &workspace);
        // First turn of a fresh session: NO capability-cache numbers, NO
        // token threshold — pre-#282 nothing armed the trigger. The only
        // ceiling in play is the num_ctx the loop itself is about to send.
        c.max_ok_input = None;
        c.safe_context = None;
        c.mid_loop_trim_tokens = None;
        // The expanded always-on catalog plus the exact prompt/card needs
        // ~3.5k tokens by itself. Derive the window from the live catalog:
        // reserve ~1,130 tokens of headroom above it (a catalog-INDEPENDENT
        // figure sized for the compressed head/card/summary/tail) as the input
        // ceiling, then back out the num_ctx that yields it. So the truthful
        // input budget tracks catalog growth while still forcing this ~12k-token
        // full request through compression before its first dispatch. (At
        // today's catalog this reproduces the historical 6,144 num_ctx /
        // 4,915-token ceiling.)
        let input_ceiling = builtin_catalog_tokens(PromptDisposition::Act)
            + prompt_read::response_repository_policy_tokens()
            + 1_130;
        let num_ctx = (input_ceiling * 100).div_ceil(c.input_ceiling_pct as usize) as u32;
        // The actual ceiling the loop derives (`num_ctx_input_ceiling`), reused
        // by the fit assertion below so budget and check stay in lockstep.
        let ceiling = num_ctx as usize * c.input_ceiling_pct as usize / 100;
        c.num_ctx = Some(num_ctx);
        c.summarizer = Some(&*summarizer);
        c.compress_state = Some(&mut compress_state);
        let (reply, _streamed, _usage, _hallu) = chat_complete(c, &mut NoMcp)
            .await
            .expect("the first turn must complete");

        // The turn produced the real answer (visibly degraded, not silently
        // wrong: compression ran and the model answered from the summary).
        assert_eq!(reply, "the marker is GAUNTLET-7f3d9c");

        // Compression fired BEFORE the first dispatch: the summarizer ran,
        // and the VERY FIRST request the backend ever saw already carried
        // the compaction marker. Step 24.4 (#559): this ~34k-char middle now
        // exceeds the per-request cap, so the ONE compression event issues
        // several BOUNDED chunk + reduce summary requests instead of a single
        // truncated one — assert ≥1 (the before-dispatch guarantee is the
        // `first_had_marker` check below), not exactly one.
        assert!(
            !prompts.lock().unwrap().is_empty(),
            "compression ran before the first dispatch (≥1 bounded summary request)"
        );
        let log = log.lock().unwrap();
        let (first_had_marker, first_message_tokens, first_request_tokens) =
            *log.first().expect("at least one request dispatched");
        assert!(
            first_had_marker,
            "B6: the first dispatched request must already be compressed — \
             pre-#282 it went out raw at ~9k tokens"
        );

        // And the COMPLETE request dispatched under the ceiling
        // (`input_ceiling_pct`% of the derived num_ctx). Counting only messages
        // here would hide the catalog cost.
        assert!(
            first_request_tokens <= ceiling,
            "first dispatch must fit the num_ctx input ceiling \
             (got ~{first_request_tokens} est. full-request tokens, \
              including ~{first_message_tokens} message tokens, > {ceiling})"
        );

        // Summarize-don't-discard still holds on the turn-1 path.
        assert!(task_in_marker.load(Ordering::SeqCst), "task survives");
        assert!(summary_in_marker.load(Ordering::SeqCst), "summary present");
        assert!(!old_placeholder.load(Ordering::SeqCst));
    }

    /// Summarizer endpoint returns 500 → the static marker is dispatched
    /// instead and the turn still completes (never aborts).
    #[tokio::test]
    async fn summarizer_500_degrades_to_static_marker_and_turn_completes() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/summarize"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let log = Arc::new(Mutex::new(Vec::new()));
        let task_in_marker = Arc::new(AtomicBool::new(false));
        let static_in_marker = Arc::new(AtomicBool::new(false));
        let old_placeholder = Arc::new(AtomicBool::new(false));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(GauntletResponder {
                final_answer: "completed despite summarizer outage".into(),
                log: log.clone(),
                task_in_marker_request: task_in_marker.clone(),
                summary_in_marker_request: static_in_marker.clone(),
                old_placeholder_seen: old_placeholder.clone(),
                static_marker_instead: true,
            })
            .mount(&server)
            .await;

        // A summarizer that really performs the HTTP call — and gets a 500.
        let attempts = Arc::new(AtomicUsize::new(0));
        let summarize_url = format!("{}/summarize", server.uri());
        let attempts_in = attempts.clone();
        let summarizer: Summarizer = Box::new(move |prompt: String| {
            let url = summarize_url.clone();
            let attempts = attempts_in.clone();
            Box::pin(async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                let resp = reqwest::Client::new()
                    .post(&url)
                    .body(prompt)
                    .send()
                    .await?;
                if !resp.status().is_success() {
                    anyhow::bail!("summarizer endpoint {}", resp.status());
                }
                Ok(resp.text().await?)
            })
        });

        let ws = gauntlet_workspace();
        let workspace = ws.path().to_string_lossy().to_string();
        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut compress_state = CompressState::new();
        let mut c = ctx(&uri, &messages, &caveats, &workspace);
        // The complete-request gate now honestly includes the advertised
        // schemas. Derive the threshold as the live catalog weight plus a
        // ~1.8k-token sliver (a catalog-INDEPENDENT offset) that holds the task
        // plus the static fallback marker, so this test keeps isolating a
        // summarizer failure rather than tipping into an irreducible-window
        // refusal as the catalog grows. (Reproduces the historical 5,600 at
        // today's catalog size.)
        c.mid_loop_trim_tokens = Some(
            builtin_catalog_tokens(PromptDisposition::Act)
                + prompt_read::response_repository_policy_tokens()
                + 1_815,
        );
        c.summarizer = Some(&*summarizer);
        c.compress_state = Some(&mut compress_state);
        let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
            .await
            .expect("a summarizer failure must never abort the turn");

        assert_eq!(reply, "completed despite summarizer outage");
        assert!(
            attempts.load(Ordering::SeqCst) >= 1,
            "the summarizer endpoint must have been attempted"
        );
        assert!(
            static_in_marker.load(Ordering::SeqCst),
            "the static fallback marker must reach the model"
        );
        assert!(task_in_marker.load(Ordering::SeqCst), "task still anchored");
        assert!(!old_placeholder.load(Ordering::SeqCst));
    }

    /// OpenAI-path mirror: the same pipeline serves the second loop — the
    /// marker + anchored task reach the post-compression request.
    struct OpenAiGauntletResponder {
        final_answer: String,
        task_in_marker_request: Arc<AtomicBool>,
        summary_in_marker_request: Arc<AtomicBool>,
        directive_in_marker_request: Arc<AtomicBool>,
    }

    impl Respond for OpenAiGauntletResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let body = body_json(req);
            if messages_contain(&body, SUMMARY_PREFIX) {
                if messages_contain(&body, TASK) {
                    self.task_in_marker_request.store(true, Ordering::SeqCst);
                }
                if messages_contain(&body, CANNED_SUMMARY) {
                    self.summary_in_marker_request.store(true, Ordering::SeqCst);
                }
                if messages_contain(&body, compress::CONTINUATION_PREFIX) {
                    self.directive_in_marker_request
                        .store(true, Ordering::SeqCst);
                }
                return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{ "message": { "content": self.final_answer } }]
                }));
            }
            if body.get("tools").is_some() {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{ "message": {
                        "content": null,
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": { "name": "read_file", "arguments": "{\"path\":\"big.txt\"}" }
                        }]
                    }}]
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{ "message": { "content": "cap exit" } }]
                }))
            }
        }
    }

    #[tokio::test]
    async fn openai_loop_compresses_with_the_same_pipeline() {
        let server = MockServer::start().await;
        let task_in_marker = Arc::new(AtomicBool::new(false));
        let summary_in_marker = Arc::new(AtomicBool::new(false));
        let directive_in_marker = Arc::new(AtomicBool::new(false));
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(OpenAiGauntletResponder {
                final_answer: "openai: marker is GAUNTLET-7f3d9c".into(),
                task_in_marker_request: task_in_marker.clone(),
                summary_in_marker_request: summary_in_marker.clone(),
                directive_in_marker_request: directive_in_marker.clone(),
            })
            .mount(&server)
            .await;

        let ws = gauntlet_workspace();
        let workspace = ws.path().to_string_lossy().to_string();
        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let summarizer = canned_summarizer(prompts.clone());
        let mut compress_state = CompressState::new();
        let mut c = ctx(&uri, &messages, &caveats, &workspace);
        c.kind = BackendKind::Openai;
        c.api_key = Some("sk-test");
        c.summarizer = Some(&*summarizer);
        c.compress_state = Some(&mut compress_state);
        // Drive the count trigger: structural pruning shortens read results but
        // cannot reduce message cardinality, so the shared summary stage must
        // run on this wire too.
        c.mid_loop_trim_tokens = None;
        c.mid_loop_trim_threshold = 8;
        let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
            .await
            .expect("openai loop should succeed");

        assert_eq!(reply, "openai: marker is GAUNTLET-7f3d9c");
        assert!(!prompts.lock().unwrap().is_empty(), "summarizer engaged");
        assert!(task_in_marker.load(Ordering::SeqCst));
        assert!(summary_in_marker.load(Ordering::SeqCst));
        // This compaction fired MID-TURN (tool rounds preceded it), so the
        // real pipeline outcome must also carry the act-now continuation
        // directive to the wire (commit 2's seam, exercised end to end).
        assert!(
            directive_in_marker.load(Ordering::SeqCst),
            "the post-compaction act-now directive must reach the wire"
        );
    }

    // -----------------------------------------------------------------------
    // Multi-compression long-haul regressions (review of PR #267, F1/F2/N3):
    // the original suite never exercised a SECOND compression — the gap that
    // let the self-poisoning boundary bug through.
    // -----------------------------------------------------------------------

    /// Per-request long-haul observations: `(dispatched message count,
    /// length of the last tool-role message — the freshest result)`.
    type HaulLog = Arc<Mutex<Vec<(usize, Option<usize>)>>>;

    /// Endless-work responder: each round calls a (hallucinated) write-ish
    /// tool and then `read_file` of `path` while tools are offered (the loop
    /// runs to its round cap), answering only the cap-exit tools-disabled
    /// completion. The non-read-only call keeps the loop's read-only nudge
    /// quiet — no intervening user messages, the regime the reviewer's F1
    /// traces locked up in (a periodic nudge would hand the boundary a fresh
    /// anchor and mask the bug). Logs, per tool-offering request, the
    /// dispatched message count and the length of the LAST tool-role message
    /// — the freshest result the model is about to read.
    struct LongHaulResponder {
        path: &'static str,
        log: HaulLog,
    }

    impl Respond for LongHaulResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let body = body_json(req);
            if body.get("tools").is_some() {
                let empty = Vec::new();
                let msgs = body["messages"].as_array().unwrap_or(&empty);
                let last_tool_len = msgs
                    .iter()
                    .rev()
                    .find(|m| m["role"].as_str() == Some("tool"))
                    .and_then(|m| m["content"].as_str())
                    .map(|c| c.chars().count());
                self.log.lock().unwrap().push((msgs.len(), last_tool_len));
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "content": "", "tool_calls": [
                        { "function": { "name": "apply_patch", "arguments": {} } },
                        { "function": { "name": "read_file", "arguments": { "path": self.path } } }
                    ]}
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({ "message": { "content": "long haul done" } }),
                )
            }
        }
    }

    /// Three prior turns, then the active task — the reviewer's multi-turn
    /// shape (the last REAL user message sits deep before the tool rounds).
    fn multi_turn_msgs() -> Vec<MemMessage> {
        vec![
            MemMessage::system("you are a test"),
            MemMessage::user("prior turn: inspect the workspace"),
            MemMessage::assistant("inspected — looks healthy"),
            MemMessage::user("prior turn: run the linters"),
            MemMessage::assistant("linters are green"),
            MemMessage::user("prior turn: sketch a fix"),
            MemMessage::assistant("sketched in my head"),
            MemMessage::user(TASK),
        ]
    }

    /// Leave exactly one estimated token of room for the initial request. This
    /// keeps the hard-budget regressions coupled to the live advertised tool
    /// catalog rather than a stale numeric snapshot of its schema overhead.
    fn initial_request_budget(messages: &[MemMessage], task: &str) -> usize {
        let tools = merged_tool_definitions(
            &NoMcp, false, false, false, false, false, false, false, false, false, false, false,
            false,
        );
        let mut wire_messages: Vec<serde_json::Value> = messages
            .iter()
            .map(|message| {
                serde_json::json!({"role": message.role.as_str(), "content": message.content})
            })
            .collect();
        let receipt = crate::TurnPromptContext::ephemeral_operator(
            "ephemeral-headless",
            task.as_bytes().to_vec(),
            task.as_bytes().to_vec(),
        );
        prompt_read::ensure_active_prompt_card(
            &mut wire_messages,
            prompt_read::PromptReadContext::new(Some(&receipt), task, None),
        );
        estimate_request_tokens(
            &wire_messages,
            Some(&tools),
            crate::tokens::TokenEstimation::default(),
        )
        .saturating_add(1)
    }

    /// Drive `rounds` tool rounds under count-only compression pressure.
    /// Returns `(per-request log, summarizer invocations, reply, latched)`.
    async fn run_long_haul(
        mem_messages: Vec<MemMessage>,
        rounds: usize,
        threshold: usize,
        file: &'static str,
        content: &str,
    ) -> (Vec<(usize, Option<usize>)>, usize, String, bool) {
        let server = MockServer::start().await;
        let log = Arc::new(Mutex::new(Vec::new()));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(LongHaulResponder {
                path: file,
                log: log.clone(),
            })
            .mount(&server)
            .await;
        let ws = tempfile::TempDir::new().unwrap();
        std::fs::write(ws.path().join(file), content).unwrap();
        let workspace = ws.path().to_string_lossy().to_string();
        let caveats = Caveats::top();
        let uri = server.uri();
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let summarizer = canned_summarizer(prompts.clone());
        let mut compress_state = CompressState::new();
        let mut c = ctx(&uri, &mem_messages, &caveats, &workspace);
        c.max_tool_rounds = rounds;
        c.mid_loop_trim_threshold = threshold;
        c.mid_loop_trim_tokens = None; // count-only: the F1/F2 regime
        c.summarizer = Some(&*summarizer);
        c.compress_state = Some(&mut compress_state);
        let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
            .await
            .expect("the long haul must complete");
        let log = log.lock().unwrap().clone();
        let calls = prompts.lock().unwrap().len();
        (log, calls, reply, compress_state.is_disabled())
    }

    /// F1 regression (i) — the reviewer's single-turn trace: 4 KB read_file
    /// results to round 40 under the count-only trigger. Pre-fix, the second
    /// compression anchored on its own summary: the count never shrank again
    /// (65 messages at round 40), the summarizer re-fired per round, and the
    /// fresh 4 KB result was one-lined before every dispatch from round ~20
    /// — the model could never read anything for the rest of the turn.
    #[tokio::test]
    async fn forty_rounds_single_turn_stay_bounded_with_fresh_results_intact() {
        let line = "the quick brown newt compresses context without discarding it\n";
        let threshold = 15usize;
        let (log, summarizer_calls, reply, latched) =
            run_long_haul(msgs(), 40, threshold, "big.txt", &line.repeat(64)).await;

        assert_eq!(reply, "long haul done");
        assert!(!latched, "count-only pressure must never latch anti-thrash");
        assert_eq!(log.len(), 40, "all 40 tool rounds dispatched");
        for (round, (len, last_tool)) in log.iter().enumerate() {
            assert!(
                *len <= threshold + 6,
                "round {round}: dispatched {len} messages — the count must stay \
                 bounded after every compression (threshold {threshold} + slack)"
            );
            if let Some(n) = last_tool {
                assert!(
                    *n > 1_000,
                    "round {round}: the fresh tool result was destroyed before \
                     dispatch ({n} chars — a one-liner)"
                );
            }
        }
        assert!(summarizer_calls >= 2, "the long haul compresses repeatedly");
        assert!(
            summarizer_calls <= 16,
            "summarizer invocations must be bounded, not per-round \
             (got {summarizer_calls} in 40 rounds)"
        );
        let max_len = log.iter().map(|(l, _)| *l).max().unwrap();
        println!(
            "forty-round trace: max dispatched len {max_len}, \
             summarizer calls {summarizer_calls}"
        );
    }

    /// F1 regression (ii) — the reviewer's multi-turn trace: 3 prior turns,
    /// then 30 tool rounds. Pre-fix the regime locked in at round ~9 (the
    /// anchor pinned the current task, the middle shrank to the previous
    /// summary alone, nothing could ever shrink) and the summarizer re-ran
    /// almost every round (71 invocations in 80 rounds).
    #[tokio::test]
    async fn thirty_rounds_multi_turn_stay_bounded_with_fresh_results_intact() {
        let line = "the quick brown newt compresses context without discarding it\n";
        let threshold = 15usize;
        let (log, summarizer_calls, reply, latched) = run_long_haul(
            multi_turn_msgs(),
            30,
            threshold,
            "big.txt",
            &line.repeat(64),
        )
        .await;

        assert_eq!(reply, "long haul done");
        assert!(!latched, "count-only pressure must never latch anti-thrash");
        assert_eq!(log.len(), 30, "all 30 tool rounds dispatched");
        for (round, (len, last_tool)) in log.iter().enumerate() {
            assert!(
                *len <= threshold + 6,
                "round {round}: dispatched {len} messages — bounded"
            );
            if let Some(n) = last_tool {
                assert!(
                    *n > 1_000,
                    "round {round}: fresh tool result destroyed pre-dispatch ({n} chars)"
                );
            }
        }
        assert!(summarizer_calls >= 2);
        assert!(
            summarizer_calls <= 14,
            "summarizer invocations must be bounded, not per-round \
             (got {summarizer_calls} in 30 rounds)"
        );
        let max_len = log.iter().map(|(l, _)| *l).max().unwrap();
        println!(
            "thirty-round multi-turn trace: max dispatched len {max_len}, \
             summarizer calls {summarizer_calls}"
        );
    }

    /// F2 regression — the reviewer's 600-char-results multi-turn shape:
    /// count-only compressions whose per-pass reclaim is small must neither
    /// latch anti-thrash (silently killing the VRAM guard) nor escalate to
    /// the Refused bail — pre-fix this errored the whole turn at round 25
    /// with "context exceeds the model's input budget" while the actual
    /// context was ~3-5k tokens and NO token threshold was configured.
    #[tokio::test]
    async fn count_only_low_reclaim_never_latches_or_bails() {
        let (log, _calls, reply, latched) =
            run_long_haul(multi_turn_msgs(), 30, 15, "small.txt", &"x".repeat(600)).await;
        assert_eq!(reply, "long haul done", "the turn must complete — no bail");
        assert!(
            !latched,
            "count-only passes must never latch the disable switch"
        );
        assert_eq!(log.len(), 30, "all 30 rounds ran (no Refused early-exit)");
    }

    // -----------------------------------------------------------------------
    // Trailing-group long-hauls (#270 / #285): the read-only-nudge regime the
    // #267 re-verifier flagged as untested, and the oversized-single-round
    // residual #284's gauntlet measured.
    // -----------------------------------------------------------------------

    /// Per-request observations for the nudged haul: `(message count, nudge
    /// text present, per-result content lengths of the trailing tool group)`.
    type NudgedLog = Arc<Mutex<Vec<(usize, bool, Vec<usize>)>>>;

    /// Read-only-only responder: every round reads big1 + big2 + small (three
    /// read-only calls, no writes), so the loop's read-only nudge fires every
    /// few rounds — the regime #270's probe ran in. The #267 long-haul
    /// responder deliberately added a write-ish call to keep that nudge quiet
    /// and hand the boundary no fresh anchors; this one deliberately does the
    /// opposite. Logs, per tool-offering request, the content length of every
    /// tool result in the trailing group (everything after the last
    /// assistant-with-`tool_calls`).
    struct NudgedHaulResponder {
        log: NudgedLog,
    }

    impl Respond for NudgedHaulResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let body = body_json(req);
            if body.get("tools").is_some() {
                let empty = Vec::new();
                let msgs = body["messages"].as_array().unwrap_or(&empty);
                let group_start = msgs
                    .iter()
                    .rposition(|m| m["tool_calls"].as_array().is_some_and(|t| !t.is_empty()))
                    .map(|i| i + 1)
                    .unwrap_or(msgs.len());
                let group_lens: Vec<usize> = msgs[group_start..]
                    .iter()
                    .filter(|m| m["role"].as_str() == Some("tool"))
                    .filter_map(|m| m["content"].as_str())
                    .map(|c| c.chars().count())
                    .collect();
                let nudged = messages_contain(&body, "read-only rounds so far");
                self.log
                    .lock()
                    .unwrap()
                    .push((msgs.len(), nudged, group_lens));
                // `role` present like a real Ollama reply — the loop appends
                // this object verbatim and the prune pairing reads the role.
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "role": "assistant", "content": "", "tool_calls": [
                        { "function": { "name": "read_file", "arguments": { "path": "big1.txt" } } },
                        { "function": { "name": "read_file", "arguments": { "path": "big2.txt" } } },
                        { "function": { "name": "read_file", "arguments": { "path": "small.txt" } } }
                    ]}
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({ "message": { "content": "nudged haul done" } }),
                )
            }
        }
    }

    /// #270 e2e — the test the #267 re-verifier said was missing: a
    /// nudge-active long session (read-only rounds only, so the loop injects
    /// its stop-exploring user message right before the compression call
    /// site every few rounds) under a hard token trigger. Pre-fix, the
    /// nudge zeroed the trailing `role == "tool"` count, `keep_last`
    /// floored at 2, and BOTH big fresh results were one-lined pre-dispatch
    /// on every nudge round. Post-fix the group derives from the assistant
    /// turn that issued the calls: the newest member is always whole and
    /// the middle member survives every round (#285's within-group reclaim
    /// may one-line only the oldest, oldest-first, and only while over
    /// budget).
    #[tokio::test]
    async fn nudged_long_haul_keeps_fresh_group_results_intact() {
        let server = MockServer::start().await;
        let log: NudgedLog = Arc::new(Mutex::new(Vec::new()));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(NudgedHaulResponder { log: log.clone() })
            .mount(&server)
            .await;
        // Distinct contents per file: identical results would engage the
        // dedupe pass, which is not what this test pins.
        let ws = tempfile::TempDir::new().unwrap();
        std::fs::write(
            ws.path().join("big1.txt"),
            "the first big fixture keeps unseen results intact\n".repeat(200),
        )
        .unwrap();
        std::fs::write(
            ws.path().join("big2.txt"),
            "the second big fixture must survive every nudge round\n".repeat(200),
        )
        .unwrap();
        std::fs::write(
            ws.path().join("small.txt"),
            "the small newest fixture stays whole\n".repeat(5),
        )
        .unwrap();
        let workspace = ws.path().to_string_lossy().to_string();
        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let summarizer = canned_summarizer(prompts.clone());
        let mut compress_state = CompressState::new();
        let mut c = ctx(&uri, &messages, &caveats, &workspace);
        c.max_tool_rounds = 12;
        // Derive the trigger from the live Always-on catalog (#1387 grew it)
        // plus a catalog-independent headroom for one complete fresh result
        // group — same shape as the other compression-loop fixtures.
        c.mid_loop_trim_tokens = Some(
            builtin_catalog_tokens(PromptDisposition::Act)
                + prompt_read::response_repository_policy_tokens()
                + 4_600,
        );
        c.summarizer = Some(&*summarizer);
        c.compress_state = Some(&mut compress_state);
        let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
            .await
            .expect("the nudged haul must complete");

        assert_eq!(reply, "nudged haul done");
        assert!(
            !compress_state.is_disabled(),
            "real reclaims every round must never latch anti-thrash"
        );
        let log = log.lock().unwrap();
        for (round, entry) in log.iter().enumerate() {
            println!("nudged haul round {round}: {entry:?}");
        }
        assert_eq!(log.len(), 12, "all 12 tool rounds dispatched");
        let nudged_rounds = log.iter().filter(|(_, n, _)| *n).count();
        assert!(
            nudged_rounds >= 2,
            "the read-only nudge regime must actually be active \
             (got {nudged_rounds} nudged requests)"
        );
        for (round, (len, nudged, group_lens)) in log.iter().enumerate() {
            if group_lens.is_empty() {
                continue; // round 0: no tool group yet
            }
            assert_eq!(group_lens.len(), 3, "round {round}: pairing intact");
            // The newest member is ALWAYS whole.
            assert!(
                group_lens[2] > 150,
                "round {round} (nudged={nudged}): the newest fresh result \
                 was destroyed pre-dispatch ({} chars)",
                group_lens[2]
            );
            // The middle member survives too: within-group reclaim is
            // oldest-first and stops at the budget — pre-#270 every nudge
            // round one-lined it ({len} msgs dispatched).
            assert!(
                group_lens[1] > 1_000,
                "round {round} (nudged={nudged}, {len} msgs): the middle \
                 fresh result was destroyed pre-dispatch ({} chars)",
                group_lens[1]
            );
        }
        let max_tokens = log.iter().map(|(l, _, _)| *l).max().unwrap();
        println!(
            "nudged haul trace: {nudged_rounds} nudged rounds, \
             max dispatched len {max_tokens}, group lens e.g. {:?}",
            log.last().unwrap().2
        );
    }

    /// Per-request observations for the oversized-round haul: `(est message
    /// tokens, a one-lined, b one-lined, c payload intact, task present)`.
    type OversizedLog = Arc<Mutex<Vec<(usize, bool, bool, bool, bool)>>>;

    /// One round reads three files whose results TOGETHER exceed the model
    /// window; answers as soon as the dispatch shows a.txt one-lined.
    struct OversizedRoundResponder {
        log: OversizedLog,
    }

    impl Respond for OversizedRoundResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let body = body_json(req);
            let a_onelined = messages_contain(&body, "[read_file] read 'a.txt'");
            if !body["stream"].as_bool().unwrap_or(false) {
                self.log.lock().unwrap().push((
                    body_request_tokens(&body),
                    a_onelined,
                    messages_contain(&body, "[read_file] read 'b.txt'"),
                    messages_contain(&body, "NEWEST-PAYLOAD-C-INTACT"),
                    messages_contain(&body, TASK),
                ));
            }
            if a_onelined {
                return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "content": "the three files are summarized" }
                }));
            }
            if body.get("tools").is_some() {
                // `role` present like a real Ollama reply — the prune
                // pairing needs it to name the file in each one-liner.
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "role": "assistant", "content": "", "tool_calls": [
                        { "function": { "name": "read_file", "arguments": { "path": "a.txt" } } },
                        { "function": { "name": "read_file", "arguments": { "path": "b.txt" } } },
                        { "function": { "name": "read_file", "arguments": { "path": "c.txt" } } }
                    ]}
                }))
            } else {
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "message": { "content": "cap exit" } }))
            }
        }
    }

    /// #285 e2e — the B6 residual measured in #284's gauntlet: ONE round's
    /// tool group alone exceeds the `num_ctx` ceiling (the only budget in
    /// play on a fresh capability cache, #282/#284 wiring untouched).
    /// Pre-fix the F1c protection made the group unreclaimable: compression
    /// honestly reported "still over budget" and the dispatch shipped
    /// over-window into a silent backend truncation. Post-fix the dispatch
    /// fits the ceiling: a.txt / b.txt one-lined (each naming its file for
    /// re-read), c.txt — the newest — intact, the task still present, and
    /// the model returns the real answer.
    #[tokio::test]
    async fn oversized_single_round_dispatches_within_the_window() {
        let server = MockServer::start().await;
        let log: OversizedLog = Arc::new(Mutex::new(Vec::new()));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(OversizedRoundResponder { log: log.clone() })
            .mount(&server)
            .await;
        // Three ~7 KB results plus Always-on schemas must exceed the input
        // ceiling before reclaim (B6's shape). Derive num_ctx from the live
        // catalog (#1387 grew Always-on tools) plus fixed headroom so the
        // relative property stays stable as the catalog changes.
        // Distinct contents per file: identical results would engage the
        // dedupe pass, which is not what this test pins.
        let catalog = builtin_catalog_tokens(PromptDisposition::Act);
        // ~3.2k tokens of message/result headroom after reclaim (matches the
        // pre-#1387 6,553 − ~3.4k catalog gap).
        let input_ceiling = catalog + prompt_read::response_repository_policy_tokens() + 3_200;
        let num_ctx = ((input_ceiling as f64) / 0.8).ceil() as u32;
        let ws = tempfile::TempDir::new().unwrap();
        std::fs::write(
            ws.path().join("a.txt"),
            "fixture a arrives first in the one giant trailing group\n".repeat(125),
        )
        .unwrap();
        std::fs::write(
            ws.path().join("b.txt"),
            "fixture b arrives second and is older than the newest\n".repeat(130),
        )
        .unwrap();
        std::fs::write(
            ws.path().join("c.txt"),
            format!(
                "{}NEWEST-PAYLOAD-C-INTACT\n",
                "fixture c arrives last and must reach the model whole\n".repeat(130)
            ),
        )
        .unwrap();
        let workspace = ws.path().to_string_lossy().to_string();
        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let summarizer = canned_summarizer(prompts.clone());
        let mut compress_state = CompressState::new();
        let mut c = ctx(&uri, &messages, &caveats, &workspace);
        // Fresh capability cache, no token threshold: the num_ctx the loop
        // itself sends is the only ceiling (#284's regime, untouched).
        c.max_ok_input = None;
        c.safe_context = None;
        c.mid_loop_trim_tokens = None;
        c.num_ctx = Some(num_ctx);
        c.summarizer = Some(&*summarizer);
        c.compress_state = Some(&mut compress_state);
        let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
            .await
            .expect("the oversized round must complete");

        let log = log.lock().unwrap();
        for (i, entry) in log.iter().enumerate() {
            println!("oversized round request {i}: {entry:?}");
        }
        // Never a silent wrong answer: the model answered from real data.
        assert_eq!(reply, "the three files are summarized");

        // THE #285 property: no dispatch ships over the window.
        for (i, &(tokens, ..)) in log.iter().enumerate() {
            assert!(
                tokens <= input_ceiling,
                "request {i} dispatched over the window: ~{tokens} est. \
                 full-request tokens > {input_ceiling}"
            );
        }

        let (tokens, _, b_onelined, c_intact, task_present) = *log
            .iter()
            .find(|(_, a, ..)| *a)
            .expect("a dispatch with a.txt one-lined must have happened");
        assert!(
            b_onelined,
            "older members one-lined in order: b.txt one-liner missing"
        );
        assert!(
            c_intact,
            "#285: the NEWEST result must reach the model whole"
        );
        assert!(task_present, "the task survives the within-group reclaim");
        assert!(
            tokens <= input_ceiling,
            "#285: the reclaimed dispatch must fit the window \
             (got ~{tokens} est. full-request tokens > {input_ceiling})"
        );
        println!(
            "#285 e2e trace: reclaimed dispatch ~{tokens} est. tokens \
             (full-request ceiling {input_ceiling}, num_ctx {num_ctx}), \
             a/b one-lined, c intact"
        );
    }

    /// N3 — the loop-level hard-budget refusal path end-to-end. The older
    /// regression intentionally dispatched repeated known-over-budget
    /// requests until anti-thrash latched. The authoritative full-request
    /// gate now supersedes that unsafe route: after one valid tool round, an
    /// incompressible follow-up is refused before its wire dispatch.
    #[tokio::test]
    async fn hard_budget_thrash_latches_then_bails_with_named_error() {
        let server = MockServer::start().await;
        let log = Arc::new(Mutex::new(Vec::new()));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(LongHaulResponder {
                path: "tiny.txt",
                log: log.clone(),
            })
            .mount(&server)
            .await;
        let ws = tempfile::TempDir::new().unwrap();
        // The protected prompt + catalog fits the configured budget, so the
        // first request is valid. The newest tool result does not fit in the
        // remaining message budget and cannot be discarded.
        std::fs::write(ws.path().join("tiny.txt"), "ok").unwrap();
        let workspace = ws.path().to_string_lossy().to_string();
        // Incompressible: the system prompt dominates and no structural
        // pass or boundary can reduce it.
        let messages = vec![
            MemMessage::system(format!("you are a test. {}", "rule. ".repeat(7_000))),
            MemMessage::user(TASK),
        ];
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut compress_state = CompressState::new();
        let mut c = ctx(&uri, &messages, &caveats, &workspace);
        c.mid_loop_trim_tokens = Some(initial_request_budget(&messages, TASK));
        c.compress_state = Some(&mut compress_state);
        let err = chat_complete(c, &mut NoMcp)
            .await
            .expect_err("the first known-over-budget follow-up must refuse the send");
        let msg = err.to_string();
        assert!(msg.contains("complete inference request needs"), "{msg}");
        assert!(msg.contains("tool results were not truncated"), "{msg}");
        assert!(
            !compress_state.is_disabled(),
            "preflight refuses before thrashing"
        );
        assert_eq!(
            log.lock().unwrap().len(),
            1,
            "no known-over-budget follow-up may reach the wire"
        );
    }

    /// Step 20.3 — the loop-level fail-open path (the gpt-4.1 bug). Same
    /// incompressible over-budget shape as the bail test, but the budget rests
    /// on the proven-good high-water mark ALONE (`max_ok_input`, no
    /// `safe_context`, no `num_ctx`, no token threshold) — the cloud /
    /// no-`/api/show` case. Anti-thrash still latches, but the latched
    /// over-budget rounds must NOT refuse: refusing here is the death spiral
    /// the user hit. The loop keeps dispatching (fails open) and never bails
    /// with the named error.
    #[tokio::test]
    async fn lone_hwm_budget_fails_open_and_does_not_bail() {
        let server = MockServer::start().await;
        let log = Arc::new(Mutex::new(Vec::new()));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(LongHaulResponder {
                path: "tiny.txt",
                log: log.clone(),
            })
            .mount(&server)
            .await;
        let ws = tempfile::TempDir::new().unwrap();
        std::fs::write(ws.path().join("tiny.txt"), "ok").unwrap();
        let workspace = ws.path().to_string_lossy().to_string();
        let messages = vec![
            MemMessage::system(format!("you are a test. {}", "rule. ".repeat(700))),
            MemMessage::user(TASK),
        ];
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut compress_state = CompressState::new();
        let mut c = ctx(&uri, &messages, &caveats, &workspace);
        // The cloud shape: a starved proven-good HWM and NOTHING authoritative.
        c.mid_loop_trim_tokens = None;
        c.max_ok_input = Some(50); // largest prompt "seen" — a floor, not a cap
        c.safe_context = None; // no /api/show seed
        c.num_ctx = None; // no per-request window ceiling
        c.compress_state = Some(&mut compress_state);
        let result = chat_complete(c, &mut NoMcp).await;
        // The session must NOT die on the budget bail — it fails open.
        if let Err(e) = &result {
            let msg = e.to_string();
            assert!(
                !msg.contains("exceeds the model's input budget"),
                "a lone-HWM budget must never refuse the send: {msg}"
            );
        }
        assert!(
            compress_state.is_disabled(),
            "anti-thrash still latches on the poor passes"
        );
        assert!(
            log.lock().unwrap().len() > 3,
            "the loop kept dispatching past the latch instead of bailing"
        );
    }
}

// ---------------------------------------------------------------------------
// Per-round observation hook + mid-turn budget raise (Phase 20,
// docs/design/model-self-tuning.md §2.2) — wiremock e2e against both gates.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod observation_hook_tests {
    use super::*;
    use crate::caveats::Caveats;
    use crate::{BackendKind, MemMessage};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    fn ctx<'a>(
        server_uri: &'a str,
        messages: &'a [MemMessage],
        caveats: &'a Caveats,
    ) -> ChatCtx<'a> {
        ChatCtx {
            url: server_uri,
            model: "test-model",
            kind: BackendKind::Ollama,
            api_key: None,
            messages,
            task: "do the thing",
            workspace: ".",
            color: false,
            markdown: false,
            tool_offload: false,
            spill_store: None,
            compaction_store: None,
            scratchpad: false,
            scratchpad_store: None,
            code_search: None,
            where_is: None,
            nav: None,
            exposure: Default::default(),
            experience_store: None,
            step_ledger: None,
            caveats,
            persona_tools: None,
            cognition: None,
            chat_completions_capability: Default::default(),
            reasoning_replay_scope: crate::model_card::ReasoningReplayScope::Never,
            max_tool_rounds: 8,
            narration_nudge_cap: 1,
            action_nudges: true,
            prompt_disposition: PromptDisposition::Act,
            prompt_intake: None,
            workflow_grace_rounds: 0,
            tool_output_lines: 20,
            debug: false,
            trace: false,
            num_ctx: None,
            input_ceiling_pct: 80,
            low_budget_pct: 15,
            connect_timeout_secs: 5,
            inference_timeout_secs: 30,
            mid_loop_trim_threshold: 40,
            compaction_trigger_policy: crate::CompactionTriggerPolicy::HeadroomAware,
            mid_loop_trim_tokens: None,
            max_ok_input: None,
            build_check_cmd: None,
            safe_context: None,
            recover_cw_400: None,
            note_sink: None,
            note_nudge: None,
            recall_source: None,
            memory_source: None,
            summarizer: None,
            compress_state: None,
            tool_events: None,
            phantom_reaches: None,
            end_reason: None,
            solve_obs: None,
            permission_gate: None,
            on_round_usage: None,
            estimate_ratio: None,
            estimation: crate::tokens::TokenEstimation::default(),
            summary_input_cap_floor_chars: 8_192,
            // #307: test ChatCtx carries no preset exec floor (headless default).
            exec_floor: None,
            write_ledger: None,
            cancel: None,
            live_tool_output: None,
            git_tool: None,
            crew_runner: None,
            operating_mode_control: None,
            plan_mode_control: None,
        }
    }

    /// Set a hard gate immediately above the live initial wire request. The
    /// following tool result must then exercise the preflight refusal rather
    /// than making this regression depend on a frozen catalog size.
    fn initial_request_budget(messages: &[MemMessage], task: &str) -> usize {
        let tools = merged_tool_definitions(
            &NoMcp, false, false, false, false, false, false, false, false, false, false, false,
            false,
        );
        let mut wire_messages: Vec<serde_json::Value> = messages
            .iter()
            .map(|message| {
                serde_json::json!({"role": message.role.as_str(), "content": message.content})
            })
            .collect();
        let receipt = crate::TurnPromptContext::ephemeral_operator(
            "ephemeral-headless",
            task.as_bytes().to_vec(),
            task.as_bytes().to_vec(),
        );
        prompt_read::ensure_active_prompt_card(
            &mut wire_messages,
            prompt_read::PromptReadContext::new(Some(&receipt), task, None),
        );
        estimate_request_tokens(
            &wire_messages,
            Some(&tools),
            crate::tokens::TokenEstimation::default(),
        )
        .saturating_add(1)
    }

    fn body_json(req: &Request) -> serde_json::Value {
        serde_json::from_slice(&req.body).unwrap_or_default()
    }

    fn is_stream(req: &Request) -> bool {
        body_json(req)["stream"].as_bool().unwrap_or(false)
    }

    fn ndjson(lines: &[serde_json::Value]) -> ResponseTemplate {
        let body: String = lines
            .iter()
            .map(|l| format!("{l}\n"))
            .collect::<Vec<_>>()
            .join("");
        ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "application/x-ndjson")
    }

    /// Tool calls for the first two tools-offering requests (each reporting
    /// the backend ACCEPTED an 8,734-token prompt), then a final answer.
    struct AcceptsLargePrompts {
        tools_rounds: Arc<AtomicUsize>,
    }
    impl Respond for AcceptsLargePrompts {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if is_stream(req) {
                return ndjson(&[serde_json::json!({
                    "message": {"content": "budget raised, here is the answer"},
                    "done": true, "prompt_eval_count": 8_700, "eval_count": 12
                })]);
            }
            let n = self.tools_rounds.fetch_add(1, Ordering::SeqCst);
            if n < 2 {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"content": "", "tool_calls": [{
                        "function": {"name": "definitely_not_a_real_tool", "arguments": {}}
                    }]},
                    "prompt_eval_count": 8_734, "eval_count": 10,
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"content": "budget raised, here is the answer"},
                    "prompt_eval_count": 8_700, "eval_count": 12,
                }))
            }
        }
    }

    /// THE trace-class regression (the motivating failure): a poisoned-low
    /// `max_ok_input` (the largest prompt SEEN, not accepted) used to refuse
    /// sends the backend was happily evaluating. Now: the over-budget
    /// acceptance (a) reaches the caller as an `Accepted` observation with
    /// the backend's real prompt size, and (b) raises the in-turn send
    /// budget, so the turn completes instead of latching anti-thrash into
    /// the Refused bail across the following rounds.
    #[tokio::test]
    async fn poisoned_low_budget_recovers_via_accepted_observation_and_raise() {
        let server = MockServer::start().await;
        let tools_rounds = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(AcceptsLargePrompts {
                tools_rounds: tools_rounds.clone(),
            })
            .mount(&server)
            .await;

        // A task big enough (~12k chars ≈ 3k est. tokens) to sit over the
        // poisoned 2,000-token budget but far under what the backend accepts.
        let big_task = "study the workspace and report. ".repeat(380);
        let messages = vec![
            MemMessage::system("you are a test"),
            MemMessage::user(&big_task),
        ];
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut observations: Vec<RoundObservation> = Vec::new();
        let mut hook = |obs: RoundObservation| observations.push(obs);
        let mut c = ctx(&uri, &messages, &caveats);
        c.max_ok_input = Some(2_000); // the poisoned ratchet
        c.on_round_usage = Some(&mut hook);
        let (reply, _streamed, _usage, _hallu) = chat_complete(c, &mut NoMcp)
            .await
            .expect("the turn must complete — no Refused bail after the raise");

        assert_eq!(reply, "budget raised, here is the answer");
        assert!(
            observations.iter().any(|o| matches!(
                o,
                RoundObservation::Accepted {
                    prompt_tokens: 8_734,
                    ..
                }
            )),
            "the accepted 8,734-token prompt must reach the hook: {observations:?}"
        );
        // Every accepted round carried a non-zero chars/4 estimate for
        // calibration pairing.
        for o in &observations {
            if let RoundObservation::Accepted {
                estimated_tokens, ..
            } = o
            {
                assert!(*estimated_tokens > 0, "estimate rides along: {o:?}");
            }
        }
    }

    /// Always tool calls (with usage) — drives the anti-thrash latch under an
    /// unreachable hard token budget so the turn ends in the Refused Err.
    struct ToolCallsWithUsage;
    impl Respond for ToolCallsWithUsage {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if body_json(req).get("tools").is_some() {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"content": "", "tool_calls": [{
                        "function": {"name": "definitely_not_a_real_tool", "arguments": {}}
                    }]},
                    "prompt_eval_count": 14_000, "eval_count": 5,
                }))
            } else {
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"message": {"content": "cap exit"}}))
            }
        }
    }

    /// A turn that ends `Err` at the authoritative full-request preflight
    /// STILL delivered the earlier round's `Accepted` observation first —
    /// evidence at the moment of observation, not in an epilogue the error
    /// skips (the spec's headline property).
    #[tokio::test]
    async fn err_turn_still_delivered_accepted_observations_first() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ToolCallsWithUsage)
            .mount(&server)
            .await;

        // Incompressible context + hard token budget: the initial request is
        // valid and accepted, then its fresh result makes the follow-up
        // impossible. The full gate refuses that follow-up before the wire.
        let messages = vec![
            MemMessage::system(format!("you are a test. {}", "rule. ".repeat(7_000))),
            MemMessage::user("do the thing"),
        ];
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut compress_state = CompressState::new();
        let mut observations: Vec<RoundObservation> = Vec::new();
        let mut hook = |obs: RoundObservation| observations.push(obs);
        let mut c = ctx(&uri, &messages, &caveats);
        c.mid_loop_trim_tokens = Some(initial_request_budget(&messages, "do the thing"));
        c.compress_state = Some(&mut compress_state);
        c.on_round_usage = Some(&mut hook);
        let err = chat_complete(c, &mut NoMcp)
            .await
            .expect_err("the known-over-budget follow-up must refuse the send");

        let msg = err.to_string();
        assert!(msg.contains("complete inference request needs"), "{msg}");
        assert!(msg.contains("tool results were not truncated"), "{msg}");
        assert!(
            observations.iter().any(|o| matches!(
                o,
                RoundObservation::Accepted {
                    prompt_tokens: 14_000,
                    ..
                }
            )),
            "accepted rounds before the bail must have been reported: {observations:?}"
        );
    }

    /// Probe 1: thinking-only (empty content, non-empty `thinking`, generated
    /// tokens); the corrective retry then recovers. The hook must see exactly
    /// one `ThinkingOnly` (once per turn) plus the recovery's `Accepted`.
    struct ThinkingOnlyThenRecover {
        probes: Arc<AtomicUsize>,
    }
    impl Respond for ThinkingOnlyThenRecover {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if is_stream(req) {
                if self.probes.load(Ordering::SeqCst) <= 1 {
                    ndjson(&[serde_json::json!({
                        "message": {"content": ""}, "done": true,
                        "prompt_eval_count": 9, "eval_count": 4
                    })])
                } else {
                    ndjson(&[serde_json::json!({
                        "message": {"content": "recovered after thinking-only"},
                        "done": true, "prompt_eval_count": 12, "eval_count": 3
                    })])
                }
            } else {
                let n = self.probes.fetch_add(1, Ordering::SeqCst) + 1;
                if n == 1 {
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "message": {
                            "content": "",
                            "thinking": "all reasoning, no final text"
                        },
                        "prompt_eval_count": 10, "eval_count": 2559,
                    }))
                } else {
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "message": {"content": "recovered after thinking-only"},
                        "prompt_eval_count": 12, "eval_count": 3,
                    }))
                }
            }
        }
    }

    #[tokio::test]
    async fn thinking_only_response_emits_one_thinking_only_observation() {
        let server = MockServer::start().await;
        let probes = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ThinkingOnlyThenRecover {
                probes: probes.clone(),
            })
            .mount(&server)
            .await;

        let messages = vec![
            MemMessage::system("you are a test"),
            MemMessage::user("do the thing"),
        ];
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut observations: Vec<RoundObservation> = Vec::new();
        let mut hook = |obs: RoundObservation| observations.push(obs);
        let mut c = ctx(&uri, &messages, &caveats);
        c.on_round_usage = Some(&mut hook);
        let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
            .await
            .expect("the corrective retry recovers the turn");

        assert_eq!(reply, "recovered after thinking-only");
        let thinking = observations
            .iter()
            .filter(|o| matches!(o, RoundObservation::ThinkingOnly))
            .count();
        assert_eq!(thinking, 1, "exactly once per turn: {observations:?}");
        assert!(
            observations
                .iter()
                .any(|o| matches!(o, RoundObservation::Accepted { .. })),
            "the recovered round is usable output: {observations:?}"
        );
    }

    /// Tool round + final round both reporting a prompt at ≥95% of the
    /// request's `num_ctx` — Ollama may have silently dropped the head, so
    /// the rounds are window evidence of NOTHING: no `Accepted` observation,
    /// no budget raise.
    struct TruncationSuspectResponder {
        tools_rounds: Arc<AtomicUsize>,
        /// Reported prompt size for every round — set ≥95% of the request's
        /// `num_ctx` so each round reads as truncation-suspect.
        suspect_prompt: u32,
    }
    impl Respond for TruncationSuspectResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let suspect_prompt = self.suspect_prompt;
            if is_stream(req) {
                return ndjson(&[serde_json::json!({
                    "message": {"content": "suspect answer"}, "done": true,
                    "prompt_eval_count": suspect_prompt, "eval_count": 5
                })]);
            }
            let n = self.tools_rounds.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"content": "", "tool_calls": [{
                        "function": {"name": "definitely_not_a_real_tool", "arguments": {}}
                    }]},
                    // ≥95% of num_ctx — truncation suspect.
                    "prompt_eval_count": suspect_prompt, "eval_count": 5,
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"content": "suspect answer"},
                    "prompt_eval_count": suspect_prompt, "eval_count": 5,
                }))
            }
        }
    }

    #[tokio::test]
    async fn truncation_suspect_rounds_emit_nothing() {
        let server = MockServer::start().await;
        // Derive the window from the live catalog. The exact prompt + schemas
        // must fit the input ceiling (input_ceiling_pct% of num_ctx), so reserve
        // ~311 tokens of headroom above the catalog (a catalog-INDEPENDENT
        // figure for the tiny system/card/user messages) and back out num_ctx.
        // The reported prompt is then pinned at ≥95% of that num_ctx, so every
        // round stays truncation-suspect no matter how the catalog grows.
        // (Reproduces the historical 5,120 num_ctx / 4,096 ceiling / ~5,000
        // report at today's catalog size.)
        const INPUT_CEILING_PCT: usize = 80; // matches ctx() default below
        let input_ceiling = builtin_catalog_tokens(PromptDisposition::Act)
            + prompt_read::response_repository_policy_tokens()
            + 311;
        let num_ctx = (input_ceiling * 100).div_ceil(INPUT_CEILING_PCT) as u32;
        let suspect_prompt = num_ctx * 98 / 100; // ≥95% of num_ctx → suspect
        let tools_rounds = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(TruncationSuspectResponder {
                tools_rounds: tools_rounds.clone(),
                suspect_prompt,
            })
            .mount(&server)
            .await;

        let messages = vec![
            MemMessage::system("you are a test"),
            MemMessage::user("do the thing"),
        ];
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut observations: Vec<RoundObservation> = Vec::new();
        let mut hook = |obs: RoundObservation| observations.push(obs);
        let mut c = ctx(&uri, &messages, &caveats);
        assert_eq!(
            c.input_ceiling_pct as usize, INPUT_CEILING_PCT,
            "derived num_ctx assumes the ctx() input-ceiling percentage"
        );
        c.num_ctx = Some(num_ctx);
        c.on_round_usage = Some(&mut hook);
        let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
            .await
            .expect("suspect rounds still complete the turn");

        assert_eq!(reply, "suspect answer");
        assert!(
            observations.is_empty(),
            "a possibly head-truncated prompt is evidence of nothing: \
             {observations:?}"
        );
    }

    /// OpenAI-path mirror: tool round then final content, both with usage —
    /// the hook receives `Accepted` for both (no `num_ctx` on this wire, so
    /// no truncation gate), and an absent hook stays a no-op.
    struct OpenAiAcceptsResponder;
    impl Respond for OpenAiAcceptsResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if body_json(req).get("tools").is_some()
                && !body_json(req)["messages"]
                    .as_array()
                    .map(|m| m.iter().any(|x| x["role"] == "tool"))
                    .unwrap_or(false)
            {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{"message": {
                        "content": null,
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": {"name": "definitely_not_a_real_tool", "arguments": "{}"}
                        }]
                    }}],
                    "usage": {"prompt_tokens": 5_120, "completion_tokens": 9},
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{"message": {"content": "openai accepted"}}],
                    "usage": {"prompt_tokens": 5_200, "completion_tokens": 11},
                }))
            }
        }
    }

    #[tokio::test]
    async fn openai_loop_reports_accepted_rounds() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(OpenAiAcceptsResponder)
            .mount(&server)
            .await;

        let messages = vec![
            MemMessage::system("you are a test"),
            MemMessage::user("do the thing"),
        ];
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut observations: Vec<RoundObservation> = Vec::new();
        let mut hook = |obs: RoundObservation| observations.push(obs);
        let mut c = ctx(&uri, &messages, &caveats);
        c.kind = BackendKind::Openai;
        c.api_key = Some("sk-test");
        c.on_round_usage = Some(&mut hook);
        let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
            .await
            .expect("openai loop should succeed");

        assert_eq!(reply, "openai accepted");
        let accepted: Vec<u32> = observations
            .iter()
            .filter_map(|o| match o {
                RoundObservation::Accepted { prompt_tokens, .. } => Some(*prompt_tokens),
                _ => None,
            })
            .collect();
        assert_eq!(
            accepted,
            vec![5_120, 5_200],
            "both usable rounds reported, in order: {observations:?}"
        );
    }

    /// Persistent empties (probe AND stream return empty content, no tool
    /// calls) at a prompt ≥85% of the configured `safe_context`, with no
    /// generated tokens — so the suspicious-empty corrective retry is NOT
    /// taken (that path needs `eval_count > 0`). The loop exhausts its two
    /// `overflow_retries`, then on the next persistent empty falls through to
    /// the silent-overflow exit and must emit exactly one
    /// `SuspectedOverflow { prompt_tokens }` carrying the merged (largest
    /// single) prompt size — the loop-emission seam that the dispatch-seam
    /// `record_overflow` tests at probe.rs cannot reach.
    struct PersistentEmptyOverflow;
    impl Respond for PersistentEmptyOverflow {
        fn respond(&self, _req: &Request) -> ResponseTemplate {
            if is_stream(_req) {
                // Stream re-issue: empty content, no tokens generated, but the
                // round still reports a large evaluated prompt.
                return ndjson(&[serde_json::json!({
                    "message": {"content": ""}, "done": true,
                    "prompt_eval_count": 8_734, "eval_count": 0
                })]);
            }
            // Probe (non-stream): empty content, no tool calls, no generated
            // tokens, large evaluated prompt.
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": ""},
                "prompt_eval_count": 8_734, "eval_count": 0,
            }))
        }
    }

    #[tokio::test]
    async fn persistent_empty_over_safe_context_emits_suspected_overflow() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(PersistentEmptyOverflow)
            .mount(&server)
            .await;

        let messages = vec![
            MemMessage::system("you are a test"),
            MemMessage::user("do the thing"),
        ];
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut observations: Vec<RoundObservation> = Vec::new();
        let mut hook = |obs: RoundObservation| observations.push(obs);
        let mut c = ctx(&uri, &messages, &caveats);
        // Derive the window from the live catalog: catalog weight plus ~215
        // tokens (a catalog-INDEPENDENT offset covering the tiny system/card/
        // user messages plus headroom) so the exact request keeps fitting as
        // the catalog grows. The reported 8_734-token prompt stays far above
        // 85% of this window, so the silent-overflow gate still fires.
        // (Reproduces the historical 4_000 at today's catalog size.)
        c.safe_context = Some(
            (builtin_catalog_tokens(PromptDisposition::Act)
                + prompt_read::response_repository_policy_tokens()
                + 215) as u32,
        );
        c.on_round_usage = Some(&mut hook);
        let (_reply, streamed, _usage, _hallu) = chat_complete(c, &mut NoMcp)
            .await
            .expect("persistent empties return the empty-response message, not Err");

        // Diagnostic exit returns non-streamed placeholder text.
        assert!(
            !streamed,
            "the silent-overflow exit is not a streamed reply"
        );
        // Exactly one SuspectedOverflow, carrying the merged (largest single)
        // prompt size — emitted once at the exit, never per retry.
        let overflow: Vec<u32> = observations
            .iter()
            .filter_map(|o| match o {
                RoundObservation::SuspectedOverflow { prompt_tokens } => Some(*prompt_tokens),
                _ => None,
            })
            .collect();
        assert_eq!(
            overflow,
            vec![8_734],
            "one SuspectedOverflow at the merged prompt size: {observations:?}"
        );
        // No Accepted: empty content is never usable output, so the window
        // evidence must not ratchet a success.
        assert!(
            !observations
                .iter()
                .any(|o| matches!(o, RoundObservation::Accepted { .. })),
            "empty rounds are not Accepted evidence: {observations:?}"
        );
    }
}
