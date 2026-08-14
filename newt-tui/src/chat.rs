use super::*;
use newt_core::agentic::chat_complete_with_prompt_and_artifacts;

#[cfg(feature = "rich-tui")]
fn block_open_delim(input: &str) -> Option<&'static str> {
    match input.lines().next().unwrap_or("").trim() {
        "\"\"\"" => Some("\"\"\""),
        "'''" => Some("'''"),
        _ => None,
    }
}

#[cfg(feature = "rich-tui")]
fn block_is_closed(input: &str, delim: &str) -> bool {
    input.lines().skip(1).any(|l| l.trim() == delim)
}

/// Whether the input wants another line — the multi-line continuation classifier
/// the rich surface ([`rich_input::RichSurface`]) uses to decide whether Enter
/// submits or adds a line:
/// - a **triple-quote block** (`"""`/`'''` alone on the first line) stays open
///   until a matching closing fence — Enter adds lines, the closing fence
///   submits. The fences are kept and flow to the model as a fenced block.
/// - a **`! …` host-shell line** continues on a trailing `\` so multi-line shell
///   commands work. A chat line submits on Enter even if it ends with `\` (that
///   backslash is literal text) — `\`-continuation is bang-only.
#[cfg(feature = "rich-tui")]
pub(crate) fn footer_continues(input: &str) -> bool {
    if let Some(delim) = block_open_delim(input) {
        return !block_is_closed(input, delim);
    }
    input.trim_start().starts_with('!') && input.ends_with('\\')
}

/// The result of reading one turn from an [`InputSurface`].
///
/// This is the widget-agnostic vocabulary the chat loop speaks: it never sees a
/// surface's native error type, so each surface (the lean crossterm box, the
/// ratatui inline rich input — issue #416) can satisfy the same contract without
/// leaking its own error types into `run_chat`.
pub(crate) enum ReadOutcome {
    /// A submitted line. May contain `\`-continued newlines the loop rejoins.
    Line(String),
    /// Ctrl-C — interrupt; the loop exits cleanly.
    Interrupted,
    /// Ctrl-D / EOF — end of input; the loop exits cleanly.
    Eof,
    /// vi `:wq` after its turn ran — exit cleanly AND end the active
    /// conversation (mark `end_reason`) so the next launch starts fresh.
    /// Only the rich surface produces this; the lean surface never does, so
    /// in the lean (`--no-default-features`) build it is constructed nowhere.
    #[cfg_attr(not(feature = "rich-tui"), allow(dead_code))]
    EndAndQuit,
    /// The terminal degraded (EMFILE, or a readline panic from fd exhaustion).
    /// Carries a ready-to-print, multi-line message; the loop prints it and
    /// breaks **without** a clean exit (no close-time network round-trip on a
    /// broken terminal). Raw mode is already disabled by the surface.
    Fatal(String),
}

/// Provenance attached to a line entering the model-input branch.
///
/// A corrective retry is deliberately not represented as another human line:
/// it carries the prompt context whose operator root authorized the original
/// turn. That distinction keeps retries out of readline history and prevents a
/// harness-generated string such as `/exit` or `! rm ...` from being interpreted
/// as operator input by the TUI interceptors.
#[derive(Debug, Clone)]
enum ModelInputOrigin {
    Operator,
    /// A direct operator answer to a bounded harness clarification. It is a
    /// fresh operator receipt, but keeps the original objective root through
    /// an explicit semantic parent rather than silently becoming a new task.
    OperatorContinuation {
        parent: Box<newt_core::TurnPromptContext>,
    },
    HarnessRetry {
        parent: Box<newt_core::TurnPromptContext>,
    },
    /// A prompt injected from an ATTACH surface (newt-web, A3/W6): the operator
    /// typed it into a web/phone tab attached to this running session. Like
    /// `HarnessRetry`, it is deliberately NOT operator input to the TUI — see
    /// `is_operator` — so an injected `/exit` or `! rm ...` is inert model text,
    /// never a host-shell escape, slash command, or readline-history entry. The
    /// running REPL still mints the turn (D2 sole-writer); the web only enqueued
    /// it in the store inbox. `inbox_id` back-links the delivered turn.
    WebInjected {
        inbox_id: String,
    },
}

impl ModelInputOrigin {
    fn is_operator(&self) -> bool {
        matches!(self, Self::Operator | Self::OperatorContinuation { .. })
    }
}

/// bug/steering-regressions iteration #2: upgrade a fresh operator prompt to
/// an [`ModelInputOrigin::OperatorContinuation`] when the previous agentic
/// turn was interrupted by the round cap AND the input is a bare continuation
/// nudge ("continue", "keep going", "1: proceed"). Minted fresh, such a nudge
/// becomes the active operator prompt itself — the compression-immune card
/// then protects the word "continue" while the real task drifts into the
/// summarizable middle (observed live 2026-07-27). Linking it re-enters the
/// interrupted objective's lineage, so fix #1's authority walk keeps the real
/// task active. A substantive new ask never upgrades — the classifier is
/// conservative by construction ([`newt_core::classifiers::is_bare_continuation`]).
fn upgrade_origin_for_interrupted_objective(
    origin: ModelInputOrigin,
    task: &str,
    interrupted: Option<&newt_core::TurnPromptContext>,
) -> ModelInputOrigin {
    match (&origin, interrupted) {
        (ModelInputOrigin::Operator, Some(parent))
            if newt_core::classifiers::is_bare_continuation(task) =>
        {
            ModelInputOrigin::OperatorContinuation {
                parent: Box::new(parent.clone()),
            }
        }
        _ => origin,
    }
}

#[cfg(test)]
mod origin_upgrade_tests {
    use super::*;

    fn ctx() -> newt_core::TurnPromptContext {
        newt_core::TurnPromptContext::ephemeral_operator(
            "conv",
            b"extract the module and open a PR".to_vec(),
            b"extract the module and open a PR".to_vec(),
        )
    }

    #[test]
    fn bare_continue_after_round_cap_links_to_the_interrupted_objective() {
        let parent = ctx();
        let got = upgrade_origin_for_interrupted_objective(
            ModelInputOrigin::Operator,
            "continue",
            Some(&parent),
        );
        match got {
            ModelInputOrigin::OperatorContinuation { parent: linked } => assert_eq!(
                linked.submitted_prompt().id(),
                parent.submitted_prompt().id(),
                "the nudge must re-enter the interrupted objective's lineage"
            ),
            other => panic!("bare continue must link, got {other:?}"),
        }
    }

    #[test]
    fn substantive_input_stays_fresh_even_with_an_interrupted_objective() {
        let parent = ctx();
        let got = upgrade_origin_for_interrupted_objective(
            ModelInputOrigin::Operator,
            "now refactor newt-tui/src/lib.rs instead and open a PR",
            Some(&parent),
        );
        assert!(
            matches!(got, ModelInputOrigin::Operator),
            "a new ask must never be silently chained to a stale objective"
        );
    }

    #[test]
    fn no_interrupted_objective_means_no_upgrade() {
        let got =
            upgrade_origin_for_interrupted_objective(ModelInputOrigin::Operator, "continue", None);
        assert!(matches!(got, ModelInputOrigin::Operator));
    }

    #[test]
    fn pending_clarification_continuations_are_left_untouched() {
        let parent = ctx();
        let pending = ModelInputOrigin::OperatorContinuation {
            parent: Box::new(ctx()),
        };
        let before_id = match &pending {
            ModelInputOrigin::OperatorContinuation { parent } => parent.submitted_prompt().id(),
            _ => unreachable!(),
        };
        let got = upgrade_origin_for_interrupted_objective(pending, "continue", Some(&parent));
        match got {
            ModelInputOrigin::OperatorContinuation { parent: kept } => assert_eq!(
                kept.submitted_prompt().id(),
                before_id,
                "a pending-clarification link outranks the round-cap link"
            ),
            other => panic!("existing continuation must be preserved, got {other:?}"),
        }
    }
}

/// Preserve the unit boundary between a backend's full context window and an
/// already-derived input cap. OpenAI-compatible loops need the former so core
/// can reserve the active generation policy; Ollama keeps using the latter as
/// its conservative `num_ctx` KV-allocation fallback.
fn context_window_for_core(
    kind: newt_core::BackendKind,
    full_context_window: Option<u32>,
    safe_context: Option<u32>,
) -> Option<u32> {
    match kind {
        // Hosted APIs (OpenAI-compatible and Anthropic) get the full declared
        // window: core reserves the active generation policy itself.
        newt_core::BackendKind::Openai | newt_core::BackendKind::Anthropic => full_context_window,
        newt_core::BackendKind::Ollama | newt_core::BackendKind::Embedded => safe_context,
    }
}

/// Resolve the selected model's full window from strongest to weakest
/// declaration. The caller performs the exact model lookup for configured and
/// community profiles, so switching models naturally produces a new value.
fn selected_model_context_window(
    live: Option<u32>,
    configured: Option<u32>,
    community: Option<u32>,
) -> Option<u32> {
    live.or(configured).or(community)
}

/// The canonical [`probe::CapKey`] for the active serving principal — the ONE
/// place the serving-default rule lives (three-Cs: the broken "key by bare
/// model" call is unrepresentable once every site takes this key).
///
/// `serving` comes from the resolved [`BackendChoice`] (`Some` after a
/// successful adopt). When it is `None` — an offline endpoint that could not be
/// probed, or a not-yet-adopted route — we default to `Multiplexer`, which keys
/// by bare model exactly as the pre-`cap_key` code did, so the fallback is
/// byte-for-byte backward compatible with existing `model-capabilities.json`
/// and never *newly* backend-keys anything. An instance backend that wants
/// isolation even while offline should declare `serving = "instance"` in its
/// backend TOML so adopt resolves it to `Some(Instance)`.
fn session_cap_id(
    serving: Option<newt_core::Serving>,
    backend_name: &str,
    model: &str,
) -> probe::CapKey {
    probe::cap_key(
        serving.unwrap_or(newt_core::Serving::Multiplexer),
        backend_name,
        model,
    )
}

/// A numbered server rejection is an authoritative upper bound on later
/// turns. It may tighten an explicit/session window but never raise a tighter
/// operator choice. An ordinary discovered window is deliberately not passed
/// here, so experimental raises remain possible until the server rejects one.
fn cap_context_window_by_recovery(
    requested: Option<u32>,
    recovered_hard_window: Option<u32>,
) -> Option<u32> {
    match (requested, recovered_hard_window) {
        (Some(requested), Some(recovered)) => Some(requested.min(recovered)),
        (requested, recovered) => requested.or(recovered),
    }
}

/// Match the agentic loop's initial send-budget calculation for the visible
/// next-turn gauge. A hard 400 may discover a smaller full window during the
/// turn, so retain both the original declared ceiling and the newly observed
/// one; like core's recovery path, the tighter result wins.
#[allow(clippy::too_many_arguments)]
fn context_gauge_budget(
    kind: newt_core::BackendKind,
    api: newt_core::OpenAiApi,
    declared_context_window: Option<u32>,
    observed_context_window: Option<u32>,
    input_ceiling_pct: u32,
    cognition: Option<newt_core::role_profile::Cognition>,
    chat_capability: newt_core::model_card::ChatCompletionsCapability,
    reasoning_replay_scope: newt_core::model_card::ReasoningReplayScope,
    max_ok_input: Option<u32>,
    safe_context: Option<u32>,
) -> Option<u32> {
    let budget_for = |window| {
        newt_core::agentic::initial_context_input_budget(
            kind,
            api,
            window,
            input_ceiling_pct,
            cognition,
            chat_capability,
            reasoning_replay_scope,
            max_ok_input,
            safe_context,
        )
    };
    match (
        budget_for(declared_context_window),
        budget_for(observed_context_window),
    ) {
        (Some(declared), Some(observed)) => Some(declared.min(observed)),
        (declared, observed) => declared.or(observed),
    }
}

#[cfg(test)]
mod context_window_handoff_tests {
    use super::*;

    #[test]
    fn hard_recovery_caps_future_declared_windows_without_raising_tighter_ones() {
        assert_eq!(
            cap_context_window_by_recovery(Some(65_536), Some(32_768)),
            Some(32_768),
        );
        assert_eq!(
            cap_context_window_by_recovery(Some(16_384), Some(32_768)),
            Some(16_384),
        );
        assert_eq!(
            cap_context_window_by_recovery(Some(65_536), None),
            Some(65_536),
            "an ordinary probe is not a hard cap on an explicit override",
        );
        assert_eq!(
            cap_context_window_by_recovery(None, Some(32_768)),
            Some(32_768),
        );

        let capable = newt_core::model_card::ChatCompletionsCapability {
            cognition: Some(true),
            ..Default::default()
        };
        let next_chat_window = cap_context_window_by_recovery(Some(65_536), Some(32_768));
        assert_eq!(
            newt_core::agentic::initial_context_input_budget(
                newt_core::BackendKind::Openai,
                newt_core::OpenAiApi::ChatCompletions,
                next_chat_window,
                90,
                Some(newt_core::role_profile::Cognition::Contemplating),
                capable,
                newt_core::model_card::ReasoningReplayScope::CurrentUserTurn,
                Some(29_491),
                Some(29_491),
            ),
            Some(16_768),
            "the next 90%-configured Chat turn must retain the 32K hard window and 16K output reserve",
        );

        let next_ollama_window = cap_context_window_by_recovery(Some(50_000), Some(32_768));
        assert_eq!(
            newt_core::agentic::initial_context_input_budget(
                newt_core::BackendKind::Ollama,
                newt_core::OpenAiApi::ChatCompletions,
                next_ollama_window,
                80,
                None,
                Default::default(),
                newt_core::model_card::ReasoningReplayScope::Never,
                Some(50_000),
                Some(50_000),
            ),
            Some(26_214),
            "the next Ollama turn must cap a raised /context size at the recovered full window",
        );
    }

    #[test]
    fn openai_handoff_keeps_the_full_window_separate_from_the_input_cap() {
        assert_eq!(
            context_window_for_core(newt_core::BackendKind::Openai, Some(32_768), Some(26_214),),
            Some(32_768),
        );
        assert_eq!(
            newt_core::config::input_percentage_ceiling(32_768, 90),
            29_491,
            "the configured percentage, not a hardcoded 80%, seeds the OpenAI input cap",
        );
        assert_eq!(
            context_window_for_core(newt_core::BackendKind::Openai, None, Some(26_214)),
            None,
            "a cached input cap must not be reinterpreted as a full OpenAI window",
        );
        assert_eq!(
            context_window_for_core(newt_core::BackendKind::Ollama, Some(32_768), Some(26_214),),
            Some(26_214),
            "Ollama retains the conservative KV-allocation fallback",
        );
    }

    #[test]
    fn selected_model_switch_replaces_the_previous_context_window() {
        assert_eq!(
            selected_model_context_window(None, None, Some(1_000_000)),
            Some(1_000_000)
        );
        assert_eq!(
            selected_model_context_window(None, Some(131_072), None),
            Some(131_072)
        );
        assert_eq!(
            selected_model_context_window(Some(262_144), Some(131_072), Some(1_000_000)),
            Some(262_144),
            "fresh endpoint metadata wins for the newly selected model"
        );
    }

    #[test]
    fn openai_gauge_reports_the_output_reserved_send_budget() {
        let capability = newt_core::model_card::ChatCompletionsCapability {
            cognition: Some(true),
            ..Default::default()
        };
        assert_eq!(
            context_gauge_budget(
                newt_core::BackendKind::Openai,
                newt_core::OpenAiApi::ChatCompletions,
                Some(32_768),
                Some(32_768),
                80,
                Some(newt_core::role_profile::Cognition::Contemplating),
                capability,
                newt_core::model_card::ReasoningReplayScope::CurrentUserTurn,
                Some(26_214),
                Some(26_214),
            ),
            Some(16_768),
            "the visible gauge must match the contemplating request's actual input ceiling",
        );
        assert_eq!(
            context_gauge_budget(
                newt_core::BackendKind::Openai,
                newt_core::OpenAiApi::ChatCompletions,
                Some(65_536),
                None,
                80,
                Some(newt_core::role_profile::Cognition::Contemplating),
                capability,
                newt_core::model_card::ReasoningReplayScope::CurrentUserTurn,
                Some(26_214),
                Some(26_214),
            ),
            Some(26_214),
            "an ordinary 32K probe must not defeat an explicit 65K turn window",
        );
        assert_eq!(
            context_gauge_budget(
                newt_core::BackendKind::Openai,
                newt_core::OpenAiApi::ChatCompletions,
                Some(65_536),
                Some(32_768),
                80,
                Some(newt_core::role_profile::Cognition::Contemplating),
                capability,
                newt_core::model_card::ReasoningReplayScope::CurrentUserTurn,
                Some(26_214),
                Some(26_214),
            ),
            Some(16_768),
            "the same 32K value must tighten only after a numbered 400 observed it",
        );
        assert_eq!(
            context_gauge_budget(
                newt_core::BackendKind::Ollama,
                newt_core::OpenAiApi::ChatCompletions,
                Some(50_000),
                None,
                80,
                None,
                Default::default(),
                newt_core::model_card::ReasoningReplayScope::Never,
                Some(50_000),
                Some(50_000),
            ),
            Some(40_000),
            "an ordinary Ollama probe must not defeat /context size",
        );
    }
}

#[derive(Debug, Clone)]
struct PendingRetry {
    text: String,
    parent: Box<newt_core::TurnPromptContext>,
}

/// A harness-owned clarification handoff. The live copy avoids repeated store
/// reads during a session; durable restore deterministically rebuilds it from
/// the immutable prompt-receipt lineage. Its content-free projection is also
/// recorded as a prompt-rooted artifact, while the next direct operator answer
/// becomes a receipt whose parent preserves this objective's lineage.
#[derive(Debug, Clone)]
struct PendingClarification {
    parent: Box<newt_core::TurnPromptContext>,
    intake: newt_core::agentic::PromptIntake,
}

/// Rebuild an outstanding clarification from its durable operator-receipt
/// lineage. A prompt that reached model work cannot be pending: `Ask` exits
/// before inference, so every descendant while it remains pending must be an
/// explicit operator continuation. The bounded walk refuses malformed
/// ancestry instead of treating a resumed answer as a new action objective.
fn rehydrate_pending_clarification(
    store: &newt_core::ConversationStore,
    conversation_id: &str,
    parent: &newt_core::TurnPromptContext,
) -> anyhow::Result<Option<PendingClarification>> {
    const MAX_LINEAGE_DEPTH: usize = 256;

    let submitted = parent.submitted_prompt().receipt();
    if submitted.origin() != newt_core::PromptOrigin::Operator {
        return Ok(None);
    }

    let chain = store.prompt_chain(conversation_id)?;
    let by_id = chain
        .iter()
        .map(|receipt| (receipt.id(), receipt))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut lineage = Vec::new();
    let mut cursor = by_id
        .get(&submitted.id())
        .copied()
        .ok_or_else(|| anyhow::anyhow!("restored prompt is absent from its conversation"))?;

    for _ in 0..=MAX_LINEAGE_DEPTH {
        lineage.push(cursor);
        if cursor.id() == cursor.root_prompt_id() {
            break;
        }
        let parent_id = cursor.parent_prompt_id().ok_or_else(|| {
            anyhow::anyhow!("non-root clarification receipt lacks a semantic parent")
        })?;
        cursor = by_id.get(&parent_id).copied().ok_or_else(|| {
            anyhow::anyhow!("clarification parent is absent from its conversation")
        })?;
    }

    if lineage
        .last()
        .is_none_or(|receipt| receipt.id() != receipt.root_prompt_id())
    {
        anyhow::bail!("clarification receipt lineage exceeds its bounded depth");
    }
    lineage.reverse();

    let root = lineage
        .first()
        .copied()
        .ok_or_else(|| anyhow::anyhow!("clarification receipt lineage is empty"))?;
    if root.origin() != newt_core::PromptOrigin::Operator {
        anyhow::bail!("clarification objective root is not an operator prompt");
    }
    let root_text = root
        .model_text_utf8()
        .map_err(|_| anyhow::anyhow!("clarification objective root is not UTF-8"))?;
    let mut intake = newt_core::agentic::PromptIntake::analyze(root_text);
    if intake.disposition() != newt_core::agentic::PromptDisposition::Ask {
        return Ok(None);
    }

    for answer in lineage.into_iter().skip(1) {
        // A harness retry proves this objective left the Ask terminal path;
        // never replay arbitrary model text as a decision answer.
        if answer.origin() != newt_core::PromptOrigin::Operator {
            return Ok(None);
        }
        let answer_text = answer
            .model_text_utf8()
            .map_err(|_| anyhow::anyhow!("clarification answer is not UTF-8"))?;
        intake = intake.resolve_with_operator_answer(answer_text);
        if intake.disposition() != newt_core::agentic::PromptDisposition::Ask {
            return Ok(None);
        }
    }

    Ok(Some(PendingClarification {
        parent: Box::new(parent.clone()),
        intake,
    }))
}

fn restored_clarification_notice(pending: &PendingClarification) -> String {
    format!(
        "Restored pending prompt clarification:\n{}",
        pending.intake.clarification_batch()
    )
}

/// Mint the prompt receipt that must exist before inference or tool work.
///
/// Persistent sessions fail closed when the durable transaction fails. An
/// ephemeral session mints the same typed receipt/context in memory, without
/// constructing or touching a [`ConversationStore`](newt_core::ConversationStore).
#[derive(Clone, Copy)]
struct PromptIngress<'a> {
    durable: Option<&'a newt_core::ConversationStore>,
    ephemeral: &'a newt_core::agentic::SessionPromptStore,
}

fn begin_model_prompt(
    ingress: PromptIngress<'_>,
    conversation_id: &str,
    title: &str,
    persona: Option<&str>,
    raw: &[u8],
    model: &[u8],
    origin: &ModelInputOrigin,
) -> anyhow::Result<newt_core::TurnPromptContext> {
    match (ingress.durable, origin) {
        (Some(store), ModelInputOrigin::Operator) => store.begin_prompt(
            conversation_id,
            title,
            persona,
            newt_core::NewPrompt::operator(raw.to_vec(), model.to_vec()),
        ),
        (Some(store), ModelInputOrigin::OperatorContinuation { parent }) => store.begin_prompt(
            conversation_id,
            title,
            persona,
            newt_core::NewPrompt::operator_continuation(
                raw.to_vec(),
                model.to_vec(),
                parent.submitted_prompt().id(),
            ),
        ),
        (Some(store), ModelInputOrigin::HarnessRetry { parent }) => store.begin_prompt(
            conversation_id,
            title,
            persona,
            newt_core::NewPrompt::harness_retry(
                raw.to_vec(),
                model.to_vec(),
                parent.submitted_prompt().id(),
            ),
        ),
        (None, ModelInputOrigin::Operator) => ingress.ephemeral.begin_prompt(
            conversation_id,
            newt_core::NewPrompt::operator(raw.to_vec(), model.to_vec()),
        ),
        (None, ModelInputOrigin::OperatorContinuation { parent }) => {
            ingress.ephemeral.begin_prompt(
                conversation_id,
                newt_core::NewPrompt::operator_continuation(
                    raw.to_vec(),
                    model.to_vec(),
                    parent.submitted_prompt().id(),
                ),
            )
        }
        (None, ModelInputOrigin::HarnessRetry { parent }) => ingress.ephemeral.begin_prompt(
            conversation_id,
            newt_core::NewPrompt::harness_retry(
                raw.to_vec(),
                model.to_vec(),
                parent.submitted_prompt().id(),
            ),
        ),
        // A3/W6: a web-injected turn is minted by the RUNNING session (D2). The
        // durable receipt is written as `operator` on purpose — a first-class
        // `origin='web_injected'` would trip the prompt_receipts CHECK on every
        // existing db; the auditable "entered via web" proof is recorded
        // additively via `link_inbox_delivery` at the call site instead.
        (Some(store), ModelInputOrigin::WebInjected { .. }) => store.begin_prompt(
            conversation_id,
            title,
            persona,
            newt_core::NewPrompt::operator(raw.to_vec(), model.to_vec()),
        ),
        (None, ModelInputOrigin::WebInjected { .. }) => ingress.ephemeral.begin_prompt(
            conversation_id,
            newt_core::NewPrompt::operator(raw.to_vec(), model.to_vec()),
        ),
    }
}

/// Build the prompt-artifact ledger for an explicitly ephemeral session.
///
/// Persistent conversations use a [`StoreArtifactStore`] adapter per turn;
/// keeping that adapter out of this session slot makes the no-SQLite guarantee
/// of `--ephemeral` explicit. Calling this again after a conversation rotation
/// drops the prior in-memory ledger and creates a fresh conversation fence.
fn session_artifact_store(
    ephemeral_session: bool,
    conversation_id: &str,
) -> anyhow::Result<Option<newt_core::agentic::SessionArtifactStore>> {
    ephemeral_session
        .then(|| newt_core::agentic::SessionArtifactStore::new(conversation_id))
        .transpose()
}

/// Read-only repository identity observed at one edge of an inference turn.
///
/// This is evidence of a transition, never an attribution: the embedded git
/// tool, a shell command, or another process could all have moved HEAD. The
/// observation uses the turn's effective authority and does no worktree scan.
fn git_head_snapshot(
    tool: Option<&newt_git::LocalGitTool>,
    caveats: &newt_core::Caveats,
) -> Option<newt_git::HeadSnapshot> {
    let tool = tool?;
    let root = tool.root.to_string_lossy().into_owned();
    if !newt_core::ScopeExt::permits(&caveats.fs_read, &root) {
        return None;
    }
    let git_caveats = newt_core::git_caveats::GitCaveats::from_session(caveats);
    tool.head_snapshot(&git_caveats).ok()
}

/// Text that may be labelled as the active task in derived memory artifacts.
/// A harness retry is a submitted attempt, not new operator authority, so
/// summaries must remain anchored to the validated operator receipt.
fn active_operator_task<'a>(
    context: Option<&'a newt_core::TurnPromptContext>,
    submitted_task: &'a str,
) -> &'a str {
    context
        .and_then(|context| context.active_operator_prompt().model_text_utf8().ok())
        .unwrap_or(submitted_task)
}

/// Cloneable liveness state for work owned by the harness but rendered by an
/// [`InputSurface`]. Workers only flip the state; the active surface remains
/// the sole terminal writer.
#[cfg_attr(not(feature = "rich-tui"), allow(dead_code))]
#[derive(Clone, Debug)]
pub(crate) struct BackgroundJob {
    label: std::sync::Arc<str>,
    running: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[cfg_attr(not(feature = "rich-tui"), allow(dead_code))]
impl BackgroundJob {
    pub(crate) fn start(label: impl Into<String>) -> Self {
        Self {
            label: std::sync::Arc::from(label.into()),
            running: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }

    pub(crate) fn label(&self) -> &str {
        &self.label
    }

    pub(crate) fn is_running(&self) -> bool {
        self.running.load(std::sync::atomic::Ordering::Acquire)
    }

    pub(crate) fn finish(&self) {
        self.running
            .store(false, std::sync::atomic::Ordering::Release);
    }

    fn completion_guard(&self) -> BackgroundJobCompletion {
        BackgroundJobCompletion(self.clone())
    }
}

/// Marks a background job complete on success, cancellation, or unwind.
struct BackgroundJobCompletion(BackgroundJob);

impl Drop for BackgroundJobCompletion {
    fn drop(&mut self) {
        self.0.finish();
    }
}

/// The severable input boundary between the chat loop and the editor widget.
///
/// `run_chat` drives the conversation through this trait so the *input widget*
/// can change without the surrounding dispatch (bang-escape, slash commands,
/// chat, history) changing with it. Impls:
/// - [`lean_input::LeanSurface`] — the hand-rolled crossterm lean box. Used for
///   the non-TTY/piped path, the headless/wyvern tier, and any non-`rich-tui`
///   build. Always available.
/// - [`rich_input::RichSurface`] — a ratatui inline **rich** surface: TTY
///   multi-line input + status row, behind the `rich-tui` cargo feature
///   (issue #416).
pub(crate) trait InputSurface {
    /// Read one turn, given the per-turn `prompt` (built fresh by the caller so
    /// the rich default's timestamp is current). Returns a [`ReadOutcome`];
    /// only an *unexpected* editor error propagates as `Err`.
    fn read_line(&mut self, prompt: &str) -> anyhow::Result<ReadOutcome>;
    /// Record a submitted entry in history.
    fn add_history(&mut self, entry: &str);
    /// Persist history to disk (no-op when there is no history path).
    fn save_history(&mut self);
    /// Rebuild the editor from fresh config — used after a `/vi` · `/emacs`
    /// edit-mode switch so the next read reflects the new mode.
    fn reload(&mut self) -> anyhow::Result<()>;
    /// Update the runtime context (active model + endpoint + the context-budget
    /// gauge `(used, budget)` + the session name, #1671) shown in the rich
    /// status header (issues #527 / #559). Called once per turn before
    /// `read_line` so a `/model` switch, the latest fill, and a `/rename` are
    /// all reflected. Default no-op: only the rich surface renders it; the
    /// lean surface carries model in the prompt string (or not).
    fn set_runtime_context(
        &mut self,
        _model: &str,
        _endpoint: &str,
        _gauge: Option<(u32, u32)>,
        _session: &str,
    ) {
    }
    /// Replace the harness-owned jobs whose live state the input surface may
    /// render. Default no-op keeps lean/headless output free of ephemeral UI.
    fn set_background_jobs(&mut self, _jobs: Vec<BackgroundJob>) {}
}

pub(crate) fn run_chat(
    workspace: &str,
    color: bool,
    persona: Option<&str>,
    // FR-5 (#999): the session `--altitude` override, applied to `active_persona`.
    altitude: Option<newt_core::Altitude>,
    crew_runner: Option<&dyn newt_core::agentic::CrewRunner>,
    // Backend probe started at splash entry (splash-first startup): consumed
    // by adopt_backend_choice when the resolved choice still matches its URL.
    prewarm: Option<crate::Prewarm>,
) -> anyhow::Result<()> {
    let verbose = verbose_mode();

    // Header line — one-time print, then normal scroll from here.
    if color {
        execute!(
            io::stdout(),
            Print("\n"),
            SetForegroundColor(NEWT_ORANGE_CT),
            Print("newt"),
            ResetColor,
            SetForegroundColor(CtColor::DarkGrey),
            Print(format!("  ·  {workspace}\n")),
            ResetColor,
        )?;
    } else {
        println!("\nnewt  ·  {workspace}");
    }

    // Input history file and tokio runtime for async inference.
    let history_path = newt_core::Config::user_config_path().map(|p| p.with_file_name("history"));

    // Use the existing tokio runtime from main — block_in_place lets the input
    // surface block the thread while still allowing block_on() inside it.
    let rt = tokio::runtime::Handle::current();

    // Resolve config ONCE per session and reuse it for every read this turn.
    // It is re-read (`Config::resolve`) only after a slash command, the one
    // intentional refresh point — config.toml may have changed on disk.
    let mut cfg = newt_core::Config::resolve().unwrap_or_default();
    // The active profile is resolved just below, AFTER the model is known — a
    // `--bundle`/inferred bundle picks its profile from the model id.

    // 17.7: how this session treats conversation persistence, resolved ONCE.
    // Precedence: --ephemeral > NEWT_CONVERSATION_ID > [conversations] resume.
    let session_start = resolve_session_start(
        std::env::var("NEWT_EPHEMERAL").is_ok(),
        std::env::var("NEWT_CONVERSATION_ID").ok(),
        std::env::var("NEWT_RESUME").ok(),
        cfg.conversations.clone().unwrap_or_default().resume,
    );
    let ephemeral_session = session_start == SessionStart::Ephemeral;
    // Ephemeral sessions get NO store handle at all (17.7): nothing to
    // create rows, nothing to append turns, nothing to read past
    // conversations from — the cleanest possible "no persistence" seam.
    let mut conversation_store: Option<newt_core::ConversationStore> = if ephemeral_session {
        None
    } else {
        Some(conversation_store_for(workspace, &cfg)?)
    };
    // A session always has a conversation id, assigned up front so the
    // per-session plan path (`.scratch/sessions/<id>/plan.md`, issue #220) is
    // stable from the first turn. The durable conversation record adopts this
    // id when the first turn is saved.
    let mut active_conversation_id: String = newt_core::new_conversation_id();

    // Capability cache: loaded once per session, written back after each turn
    // that updates tuning state (context window discovery, success/overflow).
    let mut cap_cache = probe::load_cache();
    // Shareable, model-keyed declarations are resolved against the ACTIVE
    // model each turn. This matters for multiplexing gateways where `/model`
    // changes the window without changing the endpoint.
    let community_tunings = newt_core::tuning::load_community_tunings();
    // Negative cache for /api/show (Phase 20,
    // docs/design/model-self-tuning.md §3): models whose context-window
    // fetch has been ATTEMPTED this session — successful or not. Without it,
    // an endpoint that reports no context length was re-queried every single
    // turn (`ensure_context_window` only early-outs on success).
    let mut ctx_window_probed: std::collections::HashSet<probe::CapKey> =
        std::collections::HashSet::new();

    // The OPERATOR's backend baseline: what a `/persona clear` (or a switch to a
    // persona that declares no backend) reverts to — the operator's own latest
    // explicit choice, NOT the last persona's route. Seeded from startup
    // (`--backend` / loadout / sticky), then updated below whenever an operator
    // backend command (`/backends` / `/model` / `/backend`) runs — a persona's own
    // routing never touches it (review P1#2). `None` ⇒ the configured default.
    let mut base_provider = std::env::var("NEWT_PROVIDER").ok();
    let mut base_model = std::env::var("NEWT_DGX_MODEL").ok();
    // P2#4 visibility: warn ONCE if cognition is set on a backend that ignores it
    // (Responses-only) so the dial isn't silently dropped.
    let mut cognition_scope_noted = false;

    // Resolve the inference backend and permission caveats once at session
    // start.  Both are re-read after each slash command (config.toml on disk).
    let mut choice = resolve_backend_choice(&cfg);
    // #1126 C1b: the server dictates — adopt what the endpoint actually
    // serves (bounded ~1s; offline keeps the file hint + says so).
    for line in adopt_backend_choice(&mut choice, prewarm) {
        print_newt(&line, color, verbose);
    }
    let (mut inf_url, mut inf_model) = (choice.url.clone(), choice.model.clone());
    // The canonical capability identity for the ACTIVE serving principal — the
    // single key for every empirical-capability lookup/observation below
    // (multiplexer → model, instance → backend; see `probe::cap_key`). Recomputed
    // in lockstep with the route after every switch funnel (`refresh_backend` /
    // `apply_persona_backend`), so two vLLM instances serving the same model name
    // never share (or poison) each other's tuning evidence.
    let mut cap_id = session_cap_id(choice.serving, &choice.name, &inf_model);
    // #1139: attribute the resolved model's family so per-family `[tenacity]`
    // config defaults apply in CHAT, exactly as they do in solve. This was the
    // ACTIVE_FAMILY gap — `set_active_model_family` was called ONLY in solve, so
    // chat left the family None and a `[tenacity.families]` default silently never
    // applied to an interactive session.
    newt_core::tenacity::attribute_active_family(cfg.tenacity.as_ref(), &inf_model);
    // #1199: the server-declared window from adopt, fresh per session — feeds
    // the budget without the persisted cache.
    let mut inf_context_window: Option<u32> = choice.context_window;
    // Numbered hard-window rejections are stronger than ordinary probes and
    // must survive into later turns. Keep their provenance separate so a
    // normal discovered window does not defeat an explicit experimental raise.
    let mut recovered_context_windows = std::collections::HashMap::<probe::CapKey, u32>::new();

    // Resolve + validate the active profile against config, now that the model is
    // known. Precedence: --profile (explicit) > --bundle > a bundle inferred from
    // the model (`applies_to`) > none. An unknown bundle/profile — or a profile
    // naming an unknown technique / unmet presupposition — is a hard error; a
    // selector that silently did nothing would be a false claim. Held for the loop
    // to apply.
    let active_profile = {
        let profile_env = std::env::var("NEWT_PROFILE").ok();
        let bundle_env = std::env::var("NEWT_BUNDLE").ok();
        let pick = cfg
            .pick_active_profile(profile_env.as_deref(), bundle_env.as_deref(), &inf_model)
            .map_err(|e| anyhow::anyhow!(e))?;
        match pick {
            Some(p) => {
                let profile = cfg
                    .resolve_profile(&p.name)
                    .map_err(|e| anyhow::anyhow!("profile '{}': {e}", p.name))?
                    .clone();
                announce_profile(&p.name, &profile, &p.via, color);
                Some(profile)
            }
            None => None,
        }
    };

    // Hardware telemetry: best-effort, None on non-DGX backends.
    // GPU telemetry is a `--verbose`-only display, so set it up only then.
    // `try_connect` probes DCGM port 9400 (blocking); on success it becomes a
    // BACKGROUND sampler publishing snapshots on a `watch` channel, so the
    // per-turn read is instant and never blocks the prompt (issue #414).
    let mut dgx_rx = if verbose {
        dgx_probe::DgxTelemetry::try_connect(&inf_url).map(|d| d.into_sampler(2))
    } else {
        None
    };
    let mut inf_kind = choice.kind;
    let mut inf_key = choice.api_key.clone();
    // Step 24.10 (#559): dedicated summarizer config (`~/.newt/summarizer.toml`).
    // Absent/malformed → defaults that reuse the session backend, so behavior is
    // unchanged unless the user opts the summarizer onto its own backend.
    let sum_cfg = newt_core::SummarizerConfig::resolve().unwrap_or_else(|e| {
        tracing::warn!(error = %e, "failed to load ~/.newt/summarizer.toml — using defaults (reuse session backend)");
        newt_core::SummarizerConfig::default()
    });
    apply_openai_api_env(choice.api);
    let key_path = newt_identity::default_key_path().ok();
    let mut cap = SessionCapability::establish(resolve_tui(&cfg), key_path.as_deref(), workspace);
    // Session working style. This never grants authority; plan/diagnose only
    // narrow the existing prompt-disposition and caveat boundaries.
    let mut active_operating_mode = OperatingMode::Chat;
    // Model-selected working style while the human leaves `/mode auto`
    // active. Bound to the requesting conversation and never authority.
    let conversation_mode_states = ConversationModeStates::default();
    // #307: the active `/posture` preset clamp (an authority FLOOR), if any.
    // It persists across conversations for the life of this process.
    let mut active_posture: Option<ActivePosture> = None;
    // Step 25.4 (#568): per-session Markdown override set by `/markdown on|off`.
    // `None` defers to `[tui].markdown`; `Some(b)` forces it for the session.
    let mut markdown_override: Option<bool> = None;
    // #1235: per-session live spill height. `None` follows `[tui].spill_lines`;
    // `Some(0)` keeps completed output unbounded and disables the live frame.
    //
    // #1434: `--trace` / `[tui] trace` SEEDS this, rather than being consulted
    // separately at the point of use. One variable, so the launch flag and the
    // runtime control cannot disagree — see `initial_spill_override`, and pi's
    // `options.verbose || toolOutputExpanded` phase bug that motivated it.
    let mut spill_lines_override: Option<usize> =
        crate::initial_spill_override(crate::prompt::trace_mode(&cfg));
    // #1640 Layer 1: per-session committed-result mode. `None` follows the
    // surface default (rich collapses spilled results to a one-line summary,
    // lean shows full output); `/spill summary` / `/spill excerpt` override.
    let mut spill_summary_override: Option<bool> = None;
    // Human-only per-session override for the agentic loop's tool-call round
    // safety valve. `None` preserves config/model-tuning behavior exactly.
    let mut max_tool_rounds_override: Option<usize> = None;
    // Step 24.8 (#559): per-session context-manager override from
    // `/context manager <name>`. `None` defers to `[context].manager`.
    let mut context_manager_override: Option<newt_core::ContextManager> = None;
    // Per-session automatic-compaction trigger override from `/context
    // compaction <policy>`. `None` defers to `[context].compaction_trigger_policy`.
    let mut compaction_trigger_policy_override: Option<newt_core::CompactionTriggerPolicy> = None;
    // Step 26.1 (#588): per-session context-FEATURE overrides from
    // `/context feature <name> on|off`. Each `None` defers to `[context.features]`
    // then the `manager` preset default.
    let mut context_features_override = newt_core::ContextFeatures::default();
    // Step 24.6 (#559): the latest context-budget gauge `(used, budget)`, set
    // after each turn from the turn's input tokens + the resolved send budget,
    // and shown in the rich header for the NEXT prompt. `None` until known.
    let mut token_gauge: Option<(u32, u32)> = None;
    // `/context size <N>` session override (#588): clamps the per-turn send
    // budget (eff_safe_context / eff_max_ok_input) to a user-chosen ceiling so
    // a too-tight auto-sized window can be widened for experimentation without
    // editing config. `None` = use the probed / configured budget.
    let mut context_size_override: Option<u32> = None;
    // Prompted ocap grants (issue #263 + #721), resolved ONCE per session.
    // #721 flipped the default: an INTERACTIVE human (BOTH stdin and stdout are
    // real terminals) now prompts on a denial BY DEFAULT — a denial that asks
    // beats a dead-end denial the model can't recover from. A piped / captured /
    // headless stream stays DEFAULT-DENY (never blocks on a prompt no one can
    // answer). `--no-prompt-for-permissions` (env NEWT_NO_PROMPT_FOR_PERMISSIONS)
    // opts back out; `--prompt-for-permissions` / config still turns it on (now
    // redundant with the default for interactive, but honored).
    let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();
    let prompt_permissions_enabled = should_prompt_permissions(
        permission_prompting_configured(
            std::env::var("NEWT_PROMPT_FOR_PERMISSIONS").is_ok(),
            resolve_tui(&cfg).as_ref(),
        ),
        std::env::var("NEWT_NO_PROMPT_FOR_PERMISSIONS").is_ok(),
        interactive,
        // run_chat is the INTERACTIVE entry point; headless / eval / ACP build
        // their own loop with `permission_gate: None` and never reach here, so
        // the dual-TTY `interactive` check above is the operative guard.
        false,
    );
    // `[tui] allow_bang_escape` (default true): the human's `!` host shell-out.
    // The model can never reach it regardless; this only governs the keyboard.
    let bang_escape_enabled = resolve_tui(&cfg)
        .map(|t| t.allow_bang_escape)
        .unwrap_or(true);
    let permission_log_path =
        newt_core::Config::user_config_path().map(|p| p.with_file_name("permission-log.jsonl"));
    // #904: the durable denylist lives next to the log; load it into the session
    // state so `[P]ermanently deny` decisions from prior runs still hold.
    let permission_denials_path =
        newt_core::Config::user_config_path().map(|p| p.with_file_name("permission-denials.jsonl"));
    // #904: the user config file that `[A]llow permanently` appends a net host to.
    let permission_config_path = newt_core::Config::user_config_path();
    let mut permission_state =
        PermissionPromptState::with_persistent_denials(permission_denials_path.as_deref());
    // A4/W6 (opt-in via NEWT_WEB_DECISIONS): route permission decisions to the
    // attach surface (the web) through the store instead of the TTY — the gate
    // publishes the decision and polls for the operator's verdict. Off by
    // default, so the canonical TTY prompt is byte-for-byte unchanged. A durable
    // conversation store is required (an ephemeral session has nothing to attach
    // to); `clone` is cheap (shares the connection).
    if std::env::var("NEWT_WEB_DECISIONS").is_ok() {
        permission_state.web_store = conversation_store.clone();
    }
    // Track O (#1131): load the durable OCAP policy from `~/.newt/ocap/*.toml`
    // (beside the config file). The gate consults it before prompting — a
    // durable deny refuses, a durable approve pre-answers (danger-gated). A
    // missing store is an empty policy (no behavior change); a malformed file is
    // skipped loudly so a bad rule never bricks startup.
    //
    // #1207: durable approves must be SIGNED by the operator's root key — the
    // same key the session's operating authority is minted from (loaded here,
    // generated on first use, so an interactive session always has one).
    // Unsigned/tampered approve entries are dropped loudly at load, fail-closed
    // to the prompt; deny/ask load unsigned (narrowing is fail-safe).
    if let Some(config_path) = permission_config_path.as_deref() {
        let root_vk = key_path
            .as_deref()
            .and_then(|p| newt_identity::load_or_generate(p).ok())
            .map(|user| user.public().as_bytes());
        let (policy, warnings) = newt_core::ocap_store::load_store(config_path, root_vk);
        for w in warnings {
            print_newt(&format!("warning: OCAP policy: {w}"), color, verbose);
        }
        permission_state.ocap_policy = policy;
    }
    print_newt(
        &ready_line(VERSION, &inf_model, &inf_url, inf_kind),
        color,
        verbose,
    );
    // N7 (#261 review): the conversation store's WAL→DELETE fallback notice
    // must actually reach the user. Shown ONCE, here at session start — the
    // store is re-created after slash commands (config refresh) but only this
    // construction reports it, so the warning never repeats mid-session.
    if let Some(notice) = conversation_store
        .as_ref()
        .and_then(|store| wal_fallback_startup_notice(store.wal_fallback_notice()))
    {
        print_newt(&notice, color, verbose);
    }
    if ephemeral_session {
        print_newt(EPHEMERAL_SESSION_NOTICE, color, verbose);
    }
    if prompt_permissions_enabled {
        print_newt(
            "prompted permissions ON — capability denials will ask: allow once / session / deny / \
             permanently deny (a net host also offers allow-permanently, which adds it to \
             [tui.permissions] net). Decisions recorded; /permissions lists them; permanent \
             denials persist in ~/.newt/permission-denials.jsonl",
            color,
            verbose,
        );
    }
    // INTERIM (#297): --disable-ocap / --yolo / NEWT_DISABLE_OCAP=1 — surface
    // the open session loudly: an unmissable banner plus ONE `ocap-disabled`
    // line in the #263 permission log. The bypass itself lives at the
    // run_command dispatch (newt-core); exec never prompts under it
    // (--disable-ocap > --prompt-for-permissions for exec), while fs fencing
    // and fs prompting are unaffected. A log-write failure is reported but
    // never blocks the session — the record is a review artifact, not a gate.
    if newt_core::agentic::ocap_disabled() {
        print_newt(&ocap_disabled_banner(), color, verbose);
        if let Some(path) = permission_log_path.as_deref() {
            if let Err(e) = ocap_disabled_record(&active_conversation_id).append_jsonl(path) {
                print_newt(
                    &format!("warning: permission log write failed: {e}"),
                    color,
                    verbose,
                );
            }
        }
    }
    // --full-access / NEWT_FULL_ACCESS=1 — same loud-surfacing contract as the
    // ocap bypass: an unmissable banner plus ONE `full-access` line in the
    // #263 permission log. The override itself lives in `policy_for`.
    if newt_core::agentic::full_access_requested() {
        print_newt(&full_access_banner(), color, verbose);
        if let Some(path) = permission_log_path.as_deref() {
            if let Err(e) = full_access_record(&active_conversation_id).append_jsonl(path) {
                print_newt(
                    &format!("warning: permission log write failed: {e}"),
                    color,
                    verbose,
                );
            }
        }
    }

    // Connect to discovered MCP servers ONCE for the session (newt config +
    // Claude Code config). Failures are logged + skipped; their tools are added
    // to the agent's tool set, namespaced `server__tool`. `newt doctor` shows
    // the same discovery if a server is missing.
    let cfg_mcp_servers = cfg.mcp_servers.clone();
    let sanitize_mcp = cfg
        .tui
        .as_ref()
        .map(|t| t.sanitize_mcp_server_names)
        .unwrap_or(true);
    let allow_insecure_hosts = cfg
        .tui
        .as_ref()
        .map(|t| t.mcp_allow_insecure_hosts.clone())
        .unwrap_or_default();
    let mut mcp = tokio::task::block_in_place(|| {
        rt.block_on(Mcp::connect(
            workspace,
            &cfg_mcp_servers,
            sanitize_mcp,
            &allow_insecure_hosts,
            // #1243 Leg 3: the full session leash (not just its net axis) so a
            // spawned stdio MCP server is confined to the session's authority.
            cap.caveats(),
        ))
    });
    if !mcp.is_empty() {
        let summary = mcp
            .summary()
            .into_iter()
            .map(|(name, n)| format!("{name} ({n})"))
            .collect::<Vec<_>>()
            .join(", ");
        print_newt(&format!("MCP: {summary}"), color, verbose);
    }
    println!();

    // Whether the built-in default prompt is the rich one (timestamp + status
    // folded into the prompt line). An explicit `[tui] prompt` overrides it;
    // `footer_on` also gates the multi-line helper. The prompt itself is built
    // fresh each turn (below) so the timestamp is current.
    let footer_on = footer_rich_enabled(footer_mode(), io::stdout().is_terminal());
    // Input goes through the InputSurface seam so the chat dispatch below is
    // widget-agnostic. Two morphologies:
    //  - footer ON + TTY + `rich-tui` feature → the ratatui inline RICH surface
    //    (issue #416);
    //  - otherwise (footer OFF via `-n` / `--plain` / `NEWT_FOOTER=off`, piped /
    //    headless, or a non-`rich-tui` build) → the dead-simple LEAN crossterm
    //    text box (issue #527), the flight/wyvern morphology.
    // #1640: remember which morphology we chose. Only the RICH surface has an
    // interactive spill viewport, so only it should collapse committed tool
    // output into a truncated excerpt; the LEAN surface shows the whole thing
    // (see `committed_spill_lines`).
    let surface_is_rich;
    let mut surface: Box<dyn InputSurface> = {
        #[cfg(feature = "rich-tui")]
        {
            if footer_on && io::stdout().is_terminal() {
                surface_is_rich = true;
                Box::new(rich_input::RichSurface::new(history_path)?)
            } else {
                surface_is_rich = false;
                Box::new(lean_input::LeanSurface::new(history_path)?)
            }
        }
        #[cfg(not(feature = "rich-tui"))]
        {
            surface_is_rich = false;
            Box::new(lean_input::LeanSurface::new(history_path)?)
        }
    };
    // Mark all open fds (terminal, history file, sockets) as O_CLOEXEC so
    // subprocesses spawned by run_command don't inherit them. This is the
    // primary defence against EMFILE from cargo test / rustc worker floods.
    #[cfg(unix)]
    mark_fds_cloexec();

    // `mut` so a runtime `/vi` / `/emacs` switch is reflected in the next prompt.
    let mut is_vi = resolve_edit_mode() == newt_core::EditMode::Vi;

    // Human `/cd` session working directory — SEPARATE from the
    // OCAP-load-bearing `workspace`. `/cd` moves it (confined below `workspace`),
    // the prompt shows it; it never mutates `workspace` or the process cwd.
    let mut session_cwd = std::path::PathBuf::from(workspace);

    // system prompt is built AFTER initialize_all (see below) so soul files are loaded.
    // Placeholder until then.
    let mut system: String;
    // #1021: seed the shipped gila-personal-assistant skill (best-effort — a
    // seeding failure shouldn't block the session; the persona's declared
    // `skills:` binding would just warn as unresolved instead).
    if let Err(e) = ensure_default_skills() {
        print_newt(
            &format!("warning: could not seed default skills: {e}"),
            color,
            verbose,
        );
    }
    let persona_store = PersonaStore::default();
    let mut active_persona: Option<Persona> = match persona {
        Some(name) => Some(persona_store.load(name)?),
        None => None,
    };
    warn_on_missing_bound_skills(
        active_persona.as_ref(),
        &cfg.skill_search_dirs(),
        color,
        verbose,
    );
    // FR-5 (#999): apply the `--altitude` flag. It rides on `active_persona`
    // (already threaded through every prompt rebuild), so an explicit flag either
    // overrides a loaded persona's altitude or — with no `--persona` — synthesizes
    // a minimal altitude-only persona. Absent the flag, each persona's own
    // altitude governs (doer when unset).
    if let Some(alt) = altitude {
        match active_persona.as_mut() {
            Some(p) => p.profile.altitude = Some(alt),
            // No persona named: only a non-doer altitude needs a carrier — doer
            // is already the default identity when no persona is active.
            None if alt != newt_core::Altitude::Doer => {
                active_persona = Some(synthetic_altitude_persona(alt));
            }
            None => {}
        }
    }

    // P1#3 / review-2: install a `--persona`'s declared tenacity + cognition as
    // real resolution layers at startup too (below any `--tenacity` / `--cognition`,
    // above config/family), so the loop obeys them and status surfaces agree.
    newt_core::tenacity::set_persona_tenacity(
        active_persona.as_ref().and_then(|p| p.profile.tenacity),
    );
    newt_core::cognition::set_persona_cognition(
        active_persona.as_ref().and_then(|p| p.profile.cognition),
    );

    // Persona backend auto-route (startup): if `--persona` named a persona that
    // declares a `backend:`, repoint the session to it now — before the memory /
    // budget setup below reads inf_model. Follow-ups (both minor, cloud backends
    // like sol unaffected): the active_profile pick above used the pre-persona
    // model and is not recomputed; and this re-resolve re-probes the endpoint a
    // second time (the first was the default at session start).
    if active_persona.is_some() {
        let url_changed = apply_persona_backend(
            active_persona.as_ref(),
            &base_provider,
            &base_model,
            &cfg,
            &mut choice,
            &mut inf_url,
            &mut inf_model,
            &mut inf_kind,
            &mut inf_key,
            &mut inf_context_window,
            color,
            verbose,
        );
        if url_changed && verbose {
            dgx_rx = dgx_probe::DgxTelemetry::try_connect(&inf_url).map(|d| d.into_sampler(2));
        }
        // A startup persona may have switched the backend/model; re-derive the
        // capability identity so the budget block below keys the resolved route.
        cap_id = session_cap_id(choice.serving, &choice.name, &inf_model);
    }

    // Pluggable memory manager — replaces the old conv Vec.
    let mem_cfg = cfg.memory.clone().unwrap_or_default();
    // Memory/compression budget (Step 18.2, #247): the SAME empirical
    // capability numbers that gate the loop's send_budget guard feed the
    // memory providers (newt-core has no dependency on probe types).
    // Precedence: explicit `[memory] context_tokens` → selected model's live /
    // configured / community declaration → capability evidence → static
    // fallback. This seeds construction; the turn loop rebinds the providers
    // in place whenever the selected model's resolution changes.
    let mem_budget = {
        let entry = cap_cache.entry(cap_id.clone()).or_default();
        // Once per model per session, even on failure (Phase 20): the set
        // insert returning true means this is the first attempt.
        let updated = ctx_window_probed.insert(cap_id.clone())
            && probe::ensure_context_window(
                entry,
                &inf_url,
                &inf_model,
                !real_context_discovery(&cfg, &inf_model),
                inf_kind,
            );
        if updated {
            probe::save_cache(&cap_cache);
        }
        let declared_window = selected_model_context_window(
            inf_context_window,
            cfg.find_model_tuning(&inf_model)
                .and_then(|tuning| tuning.context_window),
            community_tunings
                .find(&inf_model)
                .and_then(|profile| profile.context_window),
        );
        probe::resolve_memory_budget(
            mem_cfg.context_tokens,
            declared_window,
            cap_cache.get(&cap_id),
        )
    };
    let mut memory = {
        let mut mgr = newt_core::MemoryManager::new();
        // Soul provider first — sets the frozen identity block.
        let soul_override = mem_cfg.soul_file.as_ref().map(std::path::PathBuf::from);
        mgr.add_provider(newt_core::SoulProvider::new(soul_override));
        // Project instructions (AGENTS.md / CLAUDE.md) — compose right after
        // the soul so the block lands in the frozen system prompt. CLI-env
        // overrides config: --no-agents-file forces off, --agents-file forces
        // on (and sets the search target); otherwise follow `[agents] enabled`.
        let agents_enabled = std::env::var("NEWT_NO_AGENTS_FILE").is_err()
            && (cfg.agents.enabled || std::env::var("NEWT_AGENTS_FILE").is_ok());
        let agents_path = std::env::var("NEWT_AGENTS_FILE")
            .ok()
            .or_else(|| cfg.agents.path.clone());
        mgr.add_provider(newt_core::AgentsProvider::new(agents_enabled, agents_path));
        // Profile technique: knowledge_base (R1) — inject the authoritative PyO3
        // import surface into the system prompt when the active profile lists it.
        // Rides the provider seam (survives system-prompt rebuilds); a no-op on a
        // non-PyO3 workspace. See docs/design/technique-library.md.
        if active_profile
            .as_ref()
            .is_some_and(|p| p.enables("knowledge_base"))
        {
            // The PyO3/FFI import surface (#74) + the general workspace API
            // surface (#669) — both stable bases in the protected system prompt.
            mgr.add_provider(newt_core::FfiSurfaceProvider::new());
            // Built-in language packs + any inline `[[context.api_surface.
            // language_packs]]`. (Drop-in `~/.newt/language-packs/*.toml`
            // auto-discovery uses the public load_packs_from_dir — wired next.)
            let api_cfg = cfg
                .context
                .as_ref()
                .map(|c| c.api_surface.clone())
                .unwrap_or_default();
            // #1283: the tier-2 surface budget scales with the discovered window
            // (SC-L2), replacing the starved fixed 3000-char cap. `w` = the same
            // resolved send budget the memory providers use (mem_budget); the
            // ratio is the static `[context.estimation]` value, so `b` is a pure
            // session-fixed function (never the live calibrated ratio).
            let surface_cpt = cfg
                .context
                .as_ref()
                .map(|c| c.estimation.chars_per_token)
                .unwrap_or(4);
            let surface_budget =
                newt_core::resolve_surface_budget(mem_budget as usize, surface_cpt, &api_cfg);
            let packs = resolved_language_packs(workspace, &api_cfg);
            mgr.add_provider(
                newt_core::ApiSurfaceProvider::new(packs, &api_cfg).with_budget(surface_budget),
            );
            // #1284: the untruncatable project map (crate/package units + curated
            // purposes) — the navigation floor of the "IDE for LLMs" spine. A
            // no-op on a non-project dir; drift-cached so a re-launch is cheap.
            mgr.add_provider(newt_core::ProjectMapProvider::new());
        }
        // History provider based on config.
        match mem_cfg.provider {
            newt_core::MemoryProviderKind::TokenBudget => {
                mgr.add_provider(newt_core::TokenBudget::new(mem_budget, 0.80));
            }
            newt_core::MemoryProviderKind::Summarizing => {
                // Step 18.5 (#247): the provider delegates to the shared 18.4
                // compression pipeline, so it takes the SAME async summarizer
                // the loop uses — one HTTP wiring, one redaction + marker
                // path. (The old sync closure here blocked inside `sync_turn`
                // — the contract violation this step deletes.) Captured at
                // session start; model switches apply on next session.
                let s =
                    // The same capability-derived context figure the provider
                    // budget uses — the summary request must not be silently
                    // truncated at Ollama's default window (F5).
                    newt_core::Summarizing::new(mem_budget).with_summarizer(build_session_summarizer(
                        &sum_cfg,
                        &cfg,
                        &inf_url,
                        &inf_model,
                        inf_kind,
                        &inf_key,
                        Some(mem_budget),
                        color,
                    ));
                mgr.add_provider(s);
            }
            _ => {
                mgr.add_provider(newt_core::RollingWindow::new(mem_cfg.window));
            }
        }
        // NoteStore is always active — manages system-prompt injection only.
        mgr.add_provider(newt_core::NoteStore::default_path());
        // Progressive-disclosure memory (Workstream A MVP, #319): under
        // `[memory] disclosure = "index"` ONLY, add the budgeted MemoryIndex
        // provider (note ids/titles in the prompt; bodies fetched on demand via
        // `memory_fetch`). Default (`frozen`) registers nothing — bit-for-bit
        // unchanged. System-prompt-only, so it never competes for the
        // build_messages slot.
        if mem_cfg.disclosure == newt_core::MemoryDisclosure::Index {
            mgr.add_provider(newt_core::MemoryIndex::default_path());
        }
        mgr
    };
    // Summarizer ownership (the split-brain fix): decide ONCE whether the memory
    // provider's summarizer INHERITS the session backend (degraded fallback or a
    // partial override that reuses `inf_url`) — in which case it must follow a
    // live `/model` / `/backend` switch — or is explicitly PINNED (a dedicated
    // endpoint / embedded engine) and stays fixed across switches. `last_route`
    // is the (url, model, kind) the current summarizer was built for, so a rebind
    // happens ONLY when the route actually changes (never per turn).
    let summarizer_follows_route =
        summarizer_follows_session(&sum_cfg, embedded_summarizer_default());
    let mut last_summarizer_route = (inf_url.clone(), inf_model.clone(), inf_kind);
    // Turn-counted memory nudge (Step 19.3, #248): owned per session, lent to
    // the loop each turn. `[memory] note_nudge_interval` (default 10, 0 = off).
    let mut note_nudge = newt_core::NoteNudge::new(mem_cfg.note_nudge_interval);
    // Compression anti-thrash state (Step 18.4, #247): owned per session,
    // lent to the loop each turn (same pattern as `note_nudge`). Two
    // consecutive <10% reclaims disable auto-compression until restart.
    let mut compress_state = newt_core::CompressState::new();
    // #1528 B3: mint ONE per-session nonce (16 CSPRNG bytes) at session start and
    // bind BOTH content-addressed stores to it. The spill handle is the BLAKE3 CID of
    // a session-scoped record, so the nonce seals the equality-leak (identical
    // plaintext addresses differently across sessions). Provenance (ToolOutput vs
    // CompactionSpan) separates the two stores' address spaces, so one nonce is safe
    // for both. `getrandom` is the OS CSPRNG already used for MCP tokens.
    let session_spill_nonce: [u8; 16] = {
        let mut nonce = [0u8; 16];
        getrandom::getrandom(&mut nonce)
            .map_err(|e| anyhow::anyhow!("failed to read OS randomness for spill nonce: {e}"))?;
        nonce
    };
    // Step 26.3 (#584): session-scoped store for offloaded tool payloads (the
    // `tool_offload` feature). Session-lived so `spill:` re-reads work across
    // rounds; pure in-memory, discarded at session end / `/new`.
    let spill_store = newt_core::SessionSpillStore::new(session_spill_nonce);
    // #661 group B: session-scoped compaction store — the compressor stores each
    // evicted (redacted) middle span here and names a `compaction:<cid>` handle so
    // the model can losslessly recover a dropped detail via memory_fetch. A
    // SEPARATE address space from `spill_store` (separated by provenance, not nonce).
    // Discarded at `/new`.
    let compaction_store = newt_core::SessionSpillStore::new(session_spill_nonce);
    // Ephemeral sessions still need durable-within-the-process prompt
    // provenance. This is the receipt minting authority and exact-text source
    // for the session; it never opens SQLite. Reads are bound to the current
    // conversation id, so `/new` cannot recover an earlier task's prompts.
    let ephemeral_prompt_store = newt_core::agentic::SessionPromptStore::default();
    // Ephemeral prompt artifacts have the same append/read semantics as their
    // persistent SQLite peers, but remain process-local. The store is rebound
    // whenever the active conversation rotates so artifacts cannot cross a
    // `/new` or persona-created task boundary.
    let mut ephemeral_artifact_store =
        session_artifact_store(ephemeral_session, &active_conversation_id)?;
    // Step 26.4 (#583): session-scoped scratchpad <state> store. Session-lived;
    // cleared on /new so a fresh task never inherits stale state.
    let scratchpad_store = newt_core::SessionScratchpadStore::default();
    // Step 26.5.4 (#582): session-scoped semantic index (embedding RAG). Built
    // lazily on the first semantic-active turn; cleared (re-indexed) on /new.
    // `semantic_indexed` records that indexing was ATTEMPTED (not that it found
    // chunks) so a total embed failure (e.g. the model isn't pulled) doesn't
    // re-walk + re-embed the repo every turn — reset on /new to re-index.
    let semantic_index = std::sync::Arc::new(newt_core::SessionSemanticIndex::default());
    let mut semantic_indexed = false;
    // Iteration #4 (bug/steering-regressions): corpus embedding runs in the
    // background; the turn NEVER waits on it (see spawn_semantic_indexing).
    let mut semantic_warmup: Option<SemanticIndexWarmup> = None;
    // #1387 Phase 1: session pin/exclude + lightweight index status + last
    // `/search` result (for preview/model/pin). Cleared on `/new`.
    let mut retrieval_steer = newt_core::RetrievalSteer::default();
    let mut index_status = newt_core::IndexStatus::default();
    let mut last_search: Option<newt_core::RetrievalResult> = None;
    let mut nav_session = newt_core::NavigatorSession::default();
    // #1285: the model-free `where_is` symbol index, built once per session on
    // the first turn (reset on /new). Independent of the embedder — structural
    // extraction needs no model, so the exact typed-verdict lookup rides every
    // session as the navigation floor.
    let mut where_is_index: Option<newt_core::WhereIsIndex> = None;
    // Warm the model-free repository navigator in parallel with the remaining
    // session startup. The first consumer joins this handle; a failed task
    // leaves the existing synchronous ensure path as the honest fallback.
    let mut nav_warmup = Some(spawn_nav_warmup(&rt, workspace, &cfg, &index_status));
    // Step 26.6a (#585): session-scoped experiential ledger. Unlike the others it
    // SURVIVES /new (cross-task reuse within the session) — see the /new handler.
    let experience_store = newt_core::SessionExperienceStore::default();
    // Step 26.6b (#586): session-scoped plan ledger for the scheduled view.
    // Task-specific → CLEARED on /new (like the scratchpad).
    let step_ledger = newt_core::SessionStepLedger::default();
    let ctx = newt_core::SessionContext {
        workspace: workspace.to_string(),
        session_id: format!(
            "{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        ),
    };
    tokio::task::block_in_place(|| rt.block_on(memory.initialize_all(&ctx)));

    // Build system prompt now that SoulProvider has loaded its soul file.
    system = rebuild_system_prompt(
        workspace,
        &memory,
        active_persona.as_ref(),
        &active_conversation_id,
    );

    // 17.7: `/new` opts the SESSION out of auto-resume. Every auto-resume
    // consult goes through `should_auto_resume`, so an explicit /new is
    // never undone — today the only resume point is the startup block
    // below (necessarily before any /new), and the flag keeps that
    // invariant load-bearing if a later refresh point is ever added.
    let mut session_opted_fresh = false;

    // 19.4 (#248): close-time note extraction. The flag is resolved once at
    // session start (like the nudge interval); the counter tracks turns
    // completed in THIS session for the active conversation — a resumed
    // conversation with zero new turns reads as 0 and skips extraction.
    let extract_on_close = mem_cfg.extract_notes_on_close;
    let mut turns_this_conversation: usize = 0;
    // Whether the loop below was left by a user-initiated exit (Ctrl-C/D,
    // `exit`, `/exit`) as opposed to the EMFILE/readline-panic crash paths —
    // only a clean exit runs the close-time extraction.
    let mut clean_exit = false;
    // Set by a `:wq` (ReadOutcome::EndAndQuit): on exit, mark the active
    // conversation ended so the next launch starts fresh — the same close-out
    // `/end` does, but folded into the quit.
    let mut end_conversation_on_exit = false;
    // #1030: the ids from the most recent `/resume` listing, so `/resume <n>`
    // selects by the number the user just saw. Rebuilt on every browse/search.
    let mut last_resume_listing: Vec<String> = Vec::new();
    // #1030: the roadmap this session is authoring/viewing (via /roadmap new|use);
    // /roadmap add and /tree operate on it. None until one is created or selected.
    let mut active_roadmap_id: Option<String> = None;
    // The most recently submitted receipt in the active conversation. Restore
    // rehydrates this metadata for addressability only; it never queues the
    // prompt for execution. An outstanding clarification is separately
    // reconstructed from this receipt's durable lineage below.
    let mut active_prompt_context: Option<newt_core::TurnPromptContext> = None;
    // bug/steering-regressions iteration #2: when an agentic turn ends at the
    // round cap, its objective stays linkable — the next bare "continue"
    // re-enters that lineage instead of becoming a goal-less fresh prompt.
    let mut interrupted_objective: Option<newt_core::TurnPromptContext> = None;
    let mut pending_clarification: Option<PendingClarification> = None;

    // 17.7 session-start resume. Both arms go through the SAME restore
    // implementation `/conversation restore` uses — one restore path.
    if let Some(store) = conversation_store.as_ref() {
        let mut resume_ctx = ConversationCommandContext {
            store,
            persona_store: &persona_store,
            workspace,
            memory: &mut memory,
            system: &mut system,
            active_persona: &mut active_persona,
            active_conversation_id: &mut active_conversation_id,
            compress_state: &mut compress_state,
            scratchpad: &scratchpad_store as &dyn newt_core::ScratchpadStore,
            step_ledger: &step_ledger as &dyn newt_core::StepLedger,
            active_prompt_context: &mut active_prompt_context,
            mode_states: &conversation_mode_states,
        };
        match &session_start {
            // NEWT_CONVERSATION_ID: an explicit override — errors are hard
            // (silently starting fresh would betray the operator's ask).
            SessionStart::ResumeExact(id) => {
                let banner = resume_exact_conversation(&mut resume_ctx, id)?;
                print_newt(&banner, color, verbose);
            }
            // #1671 `--resume <name>`: id/prefix first (the exact contract),
            // then title matching — then the SAME one restore path as above.
            // Errors stay hard, like every explicit resume ask.
            SessionStart::ResumeNamed(name) => {
                let id = match store.resolve_id(name) {
                    Ok(id) => id,
                    Err(_) => crate::resolve_conversation_by_name(&store.list()?, name)
                        .map_err(|e| anyhow::anyhow!(e))?,
                };
                let banner = resume_exact_conversation(&mut resume_ctx, &id)?;
                print_newt(&banner, color, verbose);
            }
            // [conversations] resume = true: latest by §6 activity tick.
            // Failure here degrades to a fresh conversation with a warning
            // — a corrupt record must not lock the user out of their TUI.
            SessionStart::ResumeLatest => {
                if should_auto_resume(&session_start, session_opted_fresh) {
                    match auto_resume_latest(&mut resume_ctx) {
                        Ok(Some(banner)) => print_newt(&banner, color, verbose),
                        Ok(None) => {} // no conversations yet — fresh, silent
                        Err(e) => print_newt(
                            &format!("warning: auto-resume failed ({e}) — starting fresh"),
                            color,
                            verbose,
                        ),
                    }
                }
            }
            SessionStart::Ephemeral | SessionStart::Fresh => {}
        }
        if let Some(parent) = active_prompt_context.as_ref() {
            if let Some(pending) =
                rehydrate_pending_clarification(store, &active_conversation_id, parent)?
            {
                print_newt(&restored_clarification_notice(&pending), color, verbose);
                pending_clarification = Some(pending);
            }
        }
    }

    // #1030: become the live owner of the active conversation. With
    // fresh-on-launch (the default) this id is brand-new so the claim is
    // granted; only a NEWT_CONVERSATION_ID / `resume = true` opt-in can point at
    // a conversation another live newt already holds — we refuse to attach (that
    // is the turn-interleaving bug) and start a fresh conversation instead.
    if let Some(store) = conversation_store.as_ref() {
        match store.claim(&active_conversation_id) {
            Ok(newt_core::ClaimOutcome::Claimed) => {}
            Ok(newt_core::ClaimOutcome::HeldBy { host, pid }) => {
                print_newt(
                    &format!(
                        "that conversation is open in another newt (pid {pid} on {host}) — \
                         starting a fresh one instead (/resume to pick another)"
                    ),
                    color,
                    verbose,
                );
                let mut reset_ctx = ConversationResetContext {
                    memory: &mut memory,
                    system: &mut system,
                    conversation_id: &mut active_conversation_id,
                    mode_states: &conversation_mode_states,
                };
                handle_new_conversation(
                    workspace,
                    active_persona.as_ref(),
                    &mut reset_ctx,
                    &mut compress_state,
                    &mut session_opted_fresh,
                );
                active_prompt_context = None;
                pending_clarification = None;
                {
                    use newt_core::ScratchpadStore;
                    scratchpad_store.clear();
                }
                {
                    use newt_core::StepLedger;
                    step_ledger.clear();
                }
                let _ = store.claim(&active_conversation_id);
            }
            Err(e) => print_newt(
                &format!("warning: could not claim the conversation ({e})"),
                color,
                verbose,
            ),
        }
    }

    // retry technique (increment 2b): the re-prompt budget for the *current* user
    // turn, and a queued corrective re-prompt. When `pending_retry` is `Some`, the
    // next loop iteration runs it instead of reading user input — so a fabricating
    // turn is reverted (2a) and then re-prompted up to `max_retries` times before an
    // honest give-up. `retry_max` is 0 when the profile does not enable `retry`, so
    // the queue is never primed and behavior is unchanged.
    let retry_max = active_profile
        .as_ref()
        .filter(|p| p.enables("retry"))
        .map(|p| p.retry_knobs().max_retries)
        .unwrap_or(0);
    let mut pending_retry: Option<PendingRetry> = None;
    let mut retry_budget: u32 = 0;

    // PR4 (#461): the embedded `git` tool. Built once per session and injected
    // into every turn's ChatCtx. It is now ALWAYS advertised — previously it was
    // gated behind a `GitEngine::open` repo probe and vanished in a non-repo
    // workspace, which led agents to hunt for a (non-existent) MCP git tool and
    // conclude they had "no git tool", giving up on committing. The tool carries
    // an `init` op, so it is useful even before a repo exists. The commit author
    // is the resolved AgentIdentity (`newt-agent` User default, overridable).
    let session_identity = newt_core::AgentIdentity::resolve().unwrap_or_default();
    let session_git_tool: Option<newt_git::LocalGitTool> = {
        Some(newt_git::LocalGitTool {
            root: std::path::PathBuf::from(workspace),
            author: newt_git::Author {
                name: session_identity.name.clone(),
                email: session_identity.email.clone(),
            },
            // Auto-sign commits with the AI credit (the tool owns this so it is
            // always present and correctly formatted — the model is told it's
            // automatic, see runtime_context_block). Model = the session's
            // model; email = the resolved harness identity.
            coauthor: Some(coauthor_trailer(&inf_model, &session_identity)),
        })
    };

    loop {
        // The input surface can panic (assertion `fd != -1`) when the terminal
        // file descriptor becomes invalid — most commonly from file-descriptor
        // exhaustion after spawning many subprocesses (e.g., `cargo test`
        // with multiple compile workers). Without this guard the panic
        // propagates through a non-unwindable tokio boundary and the process
        // aborts with no useful message.
        //
        // `catch_unwind` catches the panic before it reaches that boundary and
        // converts it into a clean exit. `AssertUnwindSafe` is safe here:
        // the surface's editor state may be inconsistent after a panic, but we
        // immediately `break` out of the loop and drop it rather than
        // continuing to use it.
        // Layer 2: probe for EMFILE before the surface tries to open /dev/tty.
        // Catching the panic (Layer 3 / PR #184) remains as a last resort, but
        // this check fires first and gives a cleaner message when the fd table
        // is already full before reading even starts.
        let (outcome, mut model_input_origin) = if let Some(retry) = pending_retry.take() {
            // retry technique (2b): run the queued corrective re-prompt as this
            // turn's input instead of reading from the user. The budget was already
            // decremented when it was queued.
            (
                ReadOutcome::Line(retry.text),
                ModelInputOrigin::HarnessRetry {
                    parent: retry.parent,
                },
            )
        } else if let Some(injected) = pending_clarification
            .is_none()
            .then(|| {
                // A3/W6: a prompt injected from an ATTACH surface (newt-web) runs
                // as this turn's input. take_injected_prompt is a bounded,
                // NON-BLOCKING store poll — it returns None instantly on an empty
                // inbox, so this never stalls the REPL. A BUSY/store error is
                // swallowed (`.ok()`) so a transient db lock can NEVER kill the
                // session; we simply fall through to the blocking read. Skipped
                // while a clarification is pending so an injection can't orphan
                // the bounded handoff (the row stays queued until it is answered).
                conversation_store.as_ref().and_then(|s| {
                    s.take_injected_prompt(&active_conversation_id)
                        .ok()
                        .flatten()
                })
            })
            .flatten()
        {
            // A fresh turn: reset the re-prompt budget (as the read path does).
            retry_budget = retry_max;
            // Phase 1b (req 3): a web/dock inject is NOT operator keystrokes.
            // Surface its provenance so the operator can see a remote prompt
            // arrived rather than mistake it for something they typed — it still
            // runs as D2 (this session stays the sole writer). Printed through
            // the same line path as every other newt notice, between turns.
            let preview: String = injected.body.chars().take(60).collect();
            let ellipsis = if injected.body.chars().count() > 60 {
                "…"
            } else {
                ""
            };
            print_newt(
                &format!("[web] injected prompt → \"{preview}{ellipsis}\" (runs now; D2)"),
                color,
                verbose,
            );
            (
                ReadOutcome::Line(injected.body),
                ModelInputOrigin::WebInjected {
                    inbox_id: injected.id,
                },
            )
        } else {
            // A fresh user turn: reset the re-prompt budget for it.
            retry_budget = retry_max;
            // Build the prompt FRESH for this turn so the rich default's
            // timestamp is current; the surface floats it at the bottom while
            // idle and it stays in scrollback (the per-turn log marker) on
            // submit — no region, no cursor games. The EMFILE probe and the
            // panic guard now live inside the surface (returned as `Fatal`).
            // Show the session cwd (its name) in the prompt — `cd` moves it.
            let cwd_display = session_cwd.to_string_lossy();
            let prompt = prompt_str(&cwd_display, is_vi, &inf_model, footer_on);
            // Refresh the rich status header's model @ endpoint each turn (#527)
            // so a mid-session `/model` switch is reflected (no-op for lean).
            // #1671: the session NAME rides the same refresh, so the footer
            // always says which conversation this is — the current title
            // (re-read each turn: /rename and /resume both change it), the
            // short id while untitled, or "ephemeral" with no persistence.
            let session_label = if ephemeral_session {
                "ephemeral".to_string()
            } else {
                conversation_store
                    .as_ref()
                    .and_then(|store| store.title(&active_conversation_id).ok().flatten())
                    .filter(|title| !title.trim().is_empty())
                    .unwrap_or_else(|| {
                        format!("#{}", short_conversation_id(&active_conversation_id))
                    })
            };
            surface.set_runtime_context(&inf_model, &inf_url, token_gauge, &session_label);
            surface.set_background_jobs(
                nav_warmup
                    .as_ref()
                    .map(|warmup| vec![warmup.job.clone()])
                    .unwrap_or_default(),
            );
            let origin =
                pending_clarification
                    .as_ref()
                    .map_or(ModelInputOrigin::Operator, |pending| {
                        ModelInputOrigin::OperatorContinuation {
                            parent: pending.parent.clone(),
                        }
                    });
            (surface.read_line(&prompt)?, origin)
        };
        match outcome {
            ReadOutcome::Line(line) => {
                // Rejoin `\`-continued lines (multi-line entry) into real
                // newlines; a no-op for single-line input.
                let task = line.replace("\\\n", "\n").trim().to_string();
                if task.is_empty() {
                    continue;
                }
                model_input_origin = upgrade_origin_for_interrupted_objective(
                    model_input_origin,
                    &task,
                    interrupted_objective.as_ref(),
                );
                if model_input_origin.is_operator() {
                    surface.add_history(&task);
                }
                println!();
                // `! <cmd>` — human-only host shell-escape (interactive, inherited
                // stdio: prompts + browser SAML work). Intercepted before the
                // slash/chat paths; the model can never reach this. When disabled
                // via `[tui] allow_bang_escape = false`, the line is caught and
                // refused with a notice — never silently sent to the model.
                //
                // Detect + run from the RAW line, not `task`: `task` collapsed
                // each `\`+newline to a bare newline, but the shell should do its
                // own line-continuation, so a multi-line `! cmd \` joins into one
                // command (`$SHELL -c` sees the backslash-newline intact).
                let human_bang = if model_input_origin.is_operator() {
                    bang_command(line.trim())
                } else {
                    None
                };
                if let Some(rest) = human_bang {
                    if bang_escape_enabled {
                        run_bang_escape(rest, color, verbose);
                    } else {
                        print_newt(
                            "! bang-escape is disabled ([tui] allow_bang_escape = false)",
                            color,
                            verbose,
                        );
                    }
                    println!();
                    continue;
                }
                if model_input_origin.is_operator() && task.starts_with('/') {
                    // Per-command help, intercepted before ANY command runs so
                    // every command answers `--help`/`-h`/`help` (and `/help
                    // <cmd>`) uniformly — even the ones handled inline below.
                    // A bare `/help` falls through to the full command list.
                    if let Some(topic) = help_request(&task) {
                        print_command_help(
                            &topic,
                            color,
                            verbose,
                            markdown_enabled(&cfg, color, markdown_override),
                        );
                        println!();
                        continue;
                    }
                    // `/cd [dir]` — move the session working dir (shown in the
                    // prompt), confined below the start dir; bare `/cd` returns to
                    // the root. Handled here, not in `dispatch_slash`, because it
                    // mutates `session_cwd`. This is the ONE navigation command:
                    // #1096 retired the bare `cd`/`pwd`/`ls`/`rm`/… verbs, so bare
                    // text is now a message to the model (like Claude Code) and
                    // `!` runs the shell explicitly (`!pwd`, `!ls`, `!rm x`).
                    if let Some(arg) = cd_command(&task) {
                        run_cd(arg, &mut session_cwd, workspace, color, verbose);
                        println!();
                        continue;
                    }
                    // `/mcp` — MCP management surface (#1149 + session mute):
                    // status table, session on/off (instant catalog filter), and
                    // durable enable/disable (config writeback). Handled here
                    // because it needs the live `mcp` instance.
                    {
                        let w = task.trim_start_matches('/');
                        let (c, rest) = w
                            .split_once(char::is_whitespace)
                            .map_or((w, ""), |(a, b)| (a, b.trim()));
                        if c == "mcp" {
                            let (verb, name) = rest
                                .split_once(char::is_whitespace)
                                .map_or((rest, ""), |(a, b)| (a, b.trim()));
                            match (verb, name) {
                                ("", _) => {
                                    if mcp.statuses.is_empty() {
                                        print_newt(
                                            "no MCP servers configured — add [[mcp_servers]] to ~/.newt/config.toml",
                                            color,
                                            verbose,
                                        );
                                    } else {
                                        print_newt("MCP servers:", color, verbose);
                                        for (n, st) in &mcp.statuses {
                                            let line = match st {
                                                crate::mcp::McpStatus::Connected {
                                                    tools,
                                                    confinement,
                                                    net,
                                                } => {
                                                    if mcp.is_muted(n) {
                                                        format!(
                                                            "  {n}  ⏸ muted this session ({tools} tools still connected — /mcp on {n}){}{}",
                                                            confinement.note(),
                                                            net.note()
                                                        )
                                                    } else {
                                                        format!(
                                                            "  {n}  ✓ connected ({tools} tools){}{}",
                                                            confinement.note(),
                                                            net.note()
                                                        )
                                                    }
                                                }
                                                crate::mcp::McpStatus::Skipped(r) => {
                                                    let hint = if r.contains("401")
                                                        || r.to_lowercase().contains("auth")
                                                    {
                                                        format!(" — `newt auth {n}` to re-authenticate")
                                                    } else {
                                                        String::new()
                                                    };
                                                    format!("  {n}  ✗ skipped: {r}{hint}")
                                                }
                                                crate::mcp::McpStatus::Disabled => {
                                                    format!("  {n}  ⏸ disabled in config (/mcp enable {n})")
                                                }
                                            };
                                            println!("{line}");
                                        }
                                        print_newt(
                                            "usage: /mcp [on|off|enable|disable|auth] [name]",
                                            color,
                                            verbose,
                                        );
                                    }
                                }
                                // Session-scoped mute — tools leave the catalog
                                // immediately; connection stays for instant /mcp on.
                                ("off", "") => {
                                    let muted = mcp.mute_all();
                                    if muted.is_empty() {
                                        print_newt("no connected MCP servers to mute", color, verbose);
                                    } else {
                                        print_newt(
                                            &format!(
                                                "muted {} — tools removed from this session (still connected; /mcp on to restore)",
                                                muted.join(", ")
                                            ),
                                            color,
                                            verbose,
                                        );
                                    }
                                }
                                ("off", n) if !n.is_empty() => {
                                    if mcp.mute(n) {
                                        print_newt(
                                            &format!(
                                                "{n} muted — tools removed from this session (still connected; /mcp on {n})"
                                            ),
                                            color,
                                            verbose,
                                        );
                                    } else {
                                        print_newt(
                                            &format!(
                                                "no connected MCP server `{n}` — try /mcp for status"
                                            ),
                                            color,
                                            verbose,
                                        );
                                    }
                                }
                                ("on", "") => {
                                    let unmuted = mcp.unmute_all();
                                    if unmuted.is_empty() {
                                        print_newt("no muted MCP servers", color, verbose);
                                    } else {
                                        print_newt(
                                            &format!(
                                                "unmuted {} — tools restored this session",
                                                unmuted.join(", ")
                                            ),
                                            color,
                                            verbose,
                                        );
                                    }
                                }
                                ("on", n) if !n.is_empty() => {
                                    if mcp.unmute(n) {
                                        print_newt(
                                            &format!("{n} unmuted — tools restored this session"),
                                            color,
                                            verbose,
                                        );
                                    } else {
                                        print_newt(
                                            &format!(
                                                "no connected MCP server `{n}` — config-disabled servers need `/mcp enable {n}` then relaunch (live reconnect: #1148)"
                                            ),
                                            color,
                                            verbose,
                                        );
                                    }
                                }
                                ("enable", n) | ("disable", n) if !n.is_empty() => {
                                    let on = verb == "enable";
                                    match newt_core::Config::user_config_path()
                                        .ok_or_else(|| anyhow::anyhow!("no config path"))
                                        .and_then(|p| {
                                            let text = std::fs::read_to_string(&p)?;
                                            let out =
                                                newt_core::Config::with_mcp_enabled(&text, n, on)
                                                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                                            std::fs::write(&p, out)?;
                                            Ok(())
                                        }) {
                                        Ok(()) if on => print_newt(
                                            &format!(
                                                "{n} enabled in config — connects at next launch (live connect: #1148). For this session use `/mcp on {n}` if already connected."
                                            ),
                                            color,
                                            verbose,
                                        ),
                                        Ok(()) => {
                                            // Drop the live server immediately (#1149):
                                            // its tools vanish from the surface now.
                                            mcp.drop_server(n);
                                            for st in &mut mcp.statuses {
                                                if st.0 == n {
                                                    st.1 = crate::mcp::McpStatus::Disabled;
                                                }
                                            }
                                            print_newt(
                                                &format!("{n} disabled in config — tools removed from this session"),
                                                color,
                                                verbose,
                                            );
                                        }
                                        Err(e) => print_newt(&format!("mcp {verb}: {e}"), color, verbose),
                                    }
                                }
                                ("auth", n) if !n.is_empty() => print_newt(
                                    &format!("run `newt auth {n}` in a shell to (re)authenticate, then relaunch"),
                                    color,
                                    verbose,
                                ),
                                _ => print_newt(
                                    "usage: /mcp [on|off|enable|disable|auth] [name]\n  on/off = this session (instant); enable/disable = config (durable)",
                                    color,
                                    verbose,
                                ),
                            }
                            println!();
                            continue;
                        }
                    }
                    // Commands that need direct access to `memory` are handled here
                    // before delegating to the generic slash dispatcher.
                    if task.trim_start_matches('/').starts_with("memory") {
                        let usage = memory.usage();
                        if usage.is_empty() {
                            print_newt("No memory usage data available.", color, verbose);
                        } else {
                            print_newt("Context window usage:", color, verbose);
                            for (label, cur, max) in &usage {
                                let pct = if *max > 0 { cur * 100 / max } else { 0 };
                                println!("  {label}: {cur}/{max}  ({pct}%)");
                            }
                        }
                        // Anti-thrash visibility (Step 18.6, #247): read-only
                        // surfacing of the session compression counters.
                        print_newt("Compression:", color, verbose);
                        println!("{}", memory_compress_section(&compress_state.counters()));
                        println!();
                        continue;
                    }
                    let slash_body = task.trim_start_matches('/');
                    // #1030: `/status` / `/info` quick state surfaces.
                    if slash_body == "status" || slash_body == "info" {
                        let mut lines = vec![
                            format!("workspace: {workspace}"),
                            format!("conversation: {active_conversation_id}"),
                            format!("backend: {inf_model} @ {inf_url} ({})", inf_kind.label()),
                            format!(
                                "prompt permissions: {}",
                                if prompt_permissions_enabled {
                                    "ON"
                                } else {
                                    "OFF"
                                }
                            ),
                        ];
                        if slash_body == "info" {
                            lines.push(format!("newt version: {VERSION}"));
                            lines.push(format!("active mode: {}", active_operating_mode.as_str()));
                            lines.push(format!(
                                "active posture: {}",
                                active_posture.as_ref().map_or("off", |p| p.name.as_str())
                            ));
                            if let Some(path) = permission_log_path.as_deref() {
                                lines.push(format!("permission log: {}", path.display()));
                            }
                        }
                        let mut it = lines.into_iter();
                        if let Some(first) = it.next() {
                            print_newt(&first, color, verbose);
                        }
                        for line in it {
                            println!("{line}");
                        }
                        println!();
                        continue;
                    }
                    if slash_body == "docs" {
                        print_newt("docs and help:", color, verbose);
                        println!("  https://github.com/Gilamonster-Foundation/newt-agent");
                        println!("  https://github.com/Gilamonster-Foundation/newt-agent/blob/main/README.md");
                        println!("  https://github.com/Gilamonster-Foundation/newt-agent/issues");
                        println!(
                            "  https://github.com/Gilamonster-Foundation/newt-agent/tree/main/docs"
                        );
                        println!("  /help (command list) · /help <cmd> (command detail)");
                        println!();
                        continue;
                    }
                    // req 7: the operator's dock kill-switch. `/dock disable`
                    // writes the `dock-exposure-disabled` marker in the config
                    // dir that the co-located newt-web + its mesh responder both
                    // check fail-closed, forcibly undocking THIS box from every
                    // remote hub at once. `/dock status` reports the switch and
                    // the signed approved-dock registry.
                    if slash_body == "dock" || slash_body.starts_with("dock ") {
                        let sub = slash_body.strip_prefix("dock").unwrap_or("").trim();
                        let cfg_dir = newt_core::Config::user_config_dir();
                        let marker = cfg_dir.as_ref().map(|d| d.join("dock-exposure-disabled"));
                        match sub {
                            "disable" | "off" => match marker.as_ref() {
                                Some(m) => match std::fs::write(m, b"") {
                                    Ok(()) => print_newt(
                                        "remote HTMX docking DISABLED — every hub is forcibly undocked (fail-closed until `/dock enable`)",
                                        color,
                                        verbose,
                                    ),
                                    Err(e) => print_newt(
                                        &format!("could not disable docking: {e}"),
                                        color,
                                        verbose,
                                    ),
                                },
                                None => print_newt(
                                    "no config dir; cannot set the dock kill-switch",
                                    color,
                                    verbose,
                                ),
                            },
                            "enable" | "on" => {
                                if let Some(m) = marker.as_ref() {
                                    let _ = std::fs::remove_file(m);
                                }
                                print_newt(
                                    "remote HTMX docking re-enabled — approved hubs may dock again",
                                    color,
                                    verbose,
                                );
                            }
                            "status" | "" => {
                                let disabled =
                                    marker.as_ref().is_some_and(|m| m.exists());
                                print_newt(
                                    &format!(
                                        "remote HTMX docking: {}",
                                        if disabled {
                                            "DISABLED (kill-switch on)"
                                        } else {
                                            "enabled"
                                        }
                                    ),
                                    color,
                                    verbose,
                                );
                                if let (Some(dir), Some(cfg)) =
                                    (cfg_dir.as_ref(), newt_core::Config::user_config_path())
                                {
                                    let identity = dir.join("identity.pem");
                                    let (reg, _warn) =
                                        newt_core::dock_registry::load_docks_with_identity(
                                            &cfg, &identity,
                                        );
                                    let live = reg.live();
                                    if live.is_empty() {
                                        println!(
                                            "  approved peers: none (approve with `newt dock approve`)"
                                        );
                                    } else {
                                        println!("  approved peers:");
                                        for d in live {
                                            println!(
                                                "    {} ({}…) {}",
                                                d.peer_label,
                                                &d.peer_agent_fingerprint
                                                    [..12.min(d.peer_agent_fingerprint.len())],
                                                d.scope.as_wire()
                                            );
                                        }
                                    }
                                    println!("  durable revoke: `newt dock revoke-all`");
                                }
                            }
                            other => print_newt(
                                &format!(
                                    "unknown /dock subcommand `{other}` — try disable | enable | status"
                                ),
                                color,
                                verbose,
                            ),
                        }
                        println!();
                        continue;
                    }
                    // #263: review surface for prompted permission decisions.
                    // Read-only by design — promoting an allow to a durable
                    // grant is a human editing [tui.permissions] in config.
                    if slash_body == "permissions"
                        || slash_body == "allow"
                        || slash_body.starts_with("permissions ")
                        || slash_body.starts_with("allow ")
                    {
                        let perm_tail = if let Some(tail) = slash_body.strip_prefix("permissions") {
                            tail.trim()
                        } else if let Some(tail) = slash_body.strip_prefix("allow") {
                            tail.trim()
                        } else {
                            ""
                        };
                        if let Some(tail) = perm_tail.strip_prefix("audit") {
                            let tail = tail.trim();
                            let limit = if tail.is_empty() {
                                50
                            } else {
                                match tail.parse::<usize>() {
                                    Ok(n) => n,
                                    Err(_) => {
                                        print_newt(
                                            "usage: /permissions audit [N] (N must be an integer)",
                                            color,
                                            verbose,
                                        );
                                        println!();
                                        continue;
                                    }
                                }
                            };
                            match permission_log_path.as_deref() {
                                Some(path) => {
                                    let mut lines = permission_audit_lines(path, limit).into_iter();
                                    if let Some(first) = lines.next() {
                                        print_newt(&first, color, verbose);
                                    }
                                    for line in lines {
                                        println!("{line}");
                                    }
                                    if perm_tail.trim() == "audit" {
                                        print_newt(
                                            "showing newest 50 (default). use /permissions audit N",
                                            color,
                                            verbose,
                                        );
                                    }
                                }
                                None => print_newt(
                                    "permission log not configured for this session yet",
                                    color,
                                    verbose,
                                ),
                            }
                            println!();
                            continue;
                        }
                        if !perm_tail.is_empty() {
                            print_newt("usage: /permissions [audit N]", color, verbose);
                            println!();
                            continue;
                        }
                        let mut lines = permissions_command_lines(
                            &permission_state,
                            prompt_permissions_enabled,
                            permission_log_path.as_deref(),
                            // #307: surface the active posture's preset clamp.
                            active_posture.as_ref(),
                        )
                        .into_iter();
                        if let Some(first) = lines.next() {
                            print_newt(&first, color, verbose);
                        }
                        for line in lines {
                            println!("{line}");
                        }
                        println!();
                        continue;
                    }
                    // #307: `/posture <name>` — atomically preload a skill body,
                    // apply a named permission preset (an authority floor), and
                    // carry its guidance into each live turn. All three or none.
                    let slash_command = task.trim_start_matches('/');
                    if slash_command == "posture" || slash_command.starts_with("posture ") {
                        let arg = slash_command.strip_prefix("posture").unwrap_or("").trim();
                        handle_posture_command(arg, &cfg, &mut active_posture, color, verbose);
                        surface.save_history();
                        println!();
                        continue;
                    }
                    // Operating modes guide how the harness works; they do not
                    // alter the authority posture above.
                    if slash_command == "mode" || slash_command.starts_with("mode ") {
                        let arg = slash_command.strip_prefix("mode").unwrap_or("").trim();
                        handle_operating_mode_command(
                            arg,
                            &mut active_operating_mode,
                            &conversation_mode_states,
                            color,
                            verbose,
                        );
                        surface.save_history();
                        println!();
                        continue;
                    }
                    let slash_word = task.trim_start_matches('/');
                    if slash_word == "compress"
                        || slash_word.starts_with("compress ")
                        || slash_word == "compact"
                        || slash_word.starts_with("compact ")
                    {
                        // Manual compression (Step 18.6, #247): the SAME
                        // prune → boundary → redacted summary → marker
                        // pipeline the loop's triggers call, run because the
                        // user asked — through the session compress_state and
                        // the same summarizer wiring the loop uses.
                        let focus = parse_compress_command(&task).unwrap_or(None);
                        let wire = session_wire_view(&memory, &system);
                        // Same capability-derived cap the Summarizing provider
                        // injects — the summary request must not be silently
                        // truncated (F5).
                        let summarizer = build_session_summarizer(
                            &sum_cfg,
                            &cfg,
                            &inf_url,
                            &inf_model,
                            inf_kind,
                            &inf_key,
                            Some(mem_budget),
                            color,
                        );
                        let active_task =
                            Some(active_operator_task(active_prompt_context.as_ref(), ""))
                                .filter(|task| !task.is_empty())
                                .or_else(|| {
                                    wire.iter()
                                        .rev()
                                        .find(|message| {
                                            message.get("role").and_then(serde_json::Value::as_str)
                                                == Some("user")
                                        })
                                        .and_then(|message| {
                                            message
                                                .get("content")
                                                .and_then(serde_json::Value::as_str)
                                        })
                                })
                                .unwrap_or("");
                        let outcome = tokio::task::block_in_place(|| {
                            rt.block_on(newt_core::compress_user_initiated_for_task(
                                &wire,
                                active_task,
                                focus.as_deref(),
                                Some(&*summarizer),
                                &mut compress_state,
                                cfg.context
                                    .as_ref()
                                    .map(|c| c.estimation)
                                    .unwrap_or_default(),
                                cfg.context
                                    .as_ref()
                                    .map(|c| c.summary_input_cap_floor_chars)
                                    .unwrap_or(8_192),
                            ))
                        });
                        if outcome.fired {
                            // Apply the compressed working set back through
                            // the existing in-memory replace seam so the next
                            // turn actually sends it — a notice claiming
                            // savings that the session never sees would be a
                            // false claim. The durable store keeps the raw
                            // turn record untouched.
                            memory.restore_turns(&wire_messages_to_turns(&outcome.messages));

                            // Manual compaction bypasses the provider loop, so
                            // root its checkpoint explicitly in the currently
                            // active operator receipt. The slash command itself
                            // is not model input and therefore does not mint a
                            // new prompt receipt.
                            let durable_artifact_store_owner =
                                conversation_store.as_ref().map(|store| {
                                    newt_core::agentic::StoreArtifactStore::new(
                                        store,
                                        active_conversation_id.clone(),
                                    )
                                });
                            let artifact_source: Option<&dyn newt_core::agentic::ArtifactSource> =
                                durable_artifact_store_owner
                                    .as_ref()
                                    .map(|store| store as &dyn newt_core::agentic::ArtifactSource)
                                    .or_else(|| {
                                        ephemeral_artifact_store.as_ref().map(|store| {
                                            store as &dyn newt_core::agentic::ArtifactSource
                                        })
                                    });
                            let artifact_sink: Option<&dyn newt_core::agentic::PromptArtifactSink> =
                                durable_artifact_store_owner
                                    .as_ref()
                                    .map(|store| {
                                        store as &dyn newt_core::agentic::PromptArtifactSink
                                    })
                                    .or_else(|| {
                                        ephemeral_artifact_store.as_ref().map(|store| {
                                            store as &dyn newt_core::agentic::PromptArtifactSink
                                        })
                                    });
                            match (active_prompt_context.as_ref(), artifact_sink) {
                                (Some(turn), Some(sink)) => {
                                    let context =
                                        newt_core::agentic::ArtifactReadContext::from_turn(
                                            turn,
                                            artifact_source,
                                        );
                                    if let Err(e) =
                                        newt_core::agentic::record_manual_compaction_checkpoint(
                                            sink, context, &outcome,
                                        )
                                    {
                                        print_newt(
                                            &format!(
                                                "warning: could not record manual compaction checkpoint: {e}"
                                            ),
                                            color,
                                            verbose,
                                        );
                                    }
                                }
                                (None, Some(_)) => print_newt(
                                    "warning: context was compacted without an active prompt receipt; no checkpoint artifact was recorded",
                                    color,
                                    verbose,
                                ),
                                _ => {}
                            }
                        }
                        if let Some(ref notice) = outcome.notice {
                            print_newt(notice, color, verbose);
                        }
                        print_newt(&compress_feedback_message(&outcome), color, verbose);
                        println!();
                        continue;
                    }
                    // Step 25.4 (#568): `/markdown [on|off|auto]` — session override
                    // of `[tui].markdown`. No arg reports the effective state.
                    let slash_md = task.trim_start_matches('/');
                    if slash_md == "markdown" || slash_md.starts_with("markdown ") {
                        let arg = slash_md.strip_prefix("markdown").unwrap_or("").trim();
                        if arg.is_empty() {
                            let on = markdown_enabled(&cfg, color, markdown_override);
                            let src = if markdown_override.is_some() {
                                "session"
                            } else {
                                "config"
                            };
                            print_newt(
                                &format!(
                                    "markdown is {} ({src}) — use /markdown on|off|auto",
                                    if on { "on" } else { "off" }
                                ),
                                color,
                                verbose,
                            );
                        } else if let Some(mode) = newt_core::MarkdownMode::from_keyword(arg) {
                            markdown_override = mode.forced();
                            let on = markdown_enabled(&cfg, color, markdown_override);
                            print_newt(
                                &format!(
                                    "markdown → {} (now {})",
                                    mode.keyword(),
                                    if on { "on" } else { "off" }
                                ),
                                color,
                                verbose,
                            );
                        } else {
                            print_newt(
                                &format!("unknown /markdown arg '{arg}' — use on|off|auto"),
                                color,
                                verbose,
                            );
                        }
                        surface.save_history();
                        println!();
                        continue;
                    }
                    // #1434: `/detail` flips the session detail level between the
                    // configured height and unbounded. It shares
                    // `spill_lines_override` with `/spill`, so the two controls
                    // cannot disagree — there is no second detail state.
                    //
                    // A slash command, not only a chord, because the chord path
                    // is terminal-dependent: on macOS Option is a compose key by
                    // default, so `Alt+t` needs a per-terminal setting. This
                    // always works. (The chord itself waits on #294's action
                    // table — binding it inline here is exactly the sprawl that
                    // issue exists to prevent.)
                    if slash_md == "detail" {
                        let configured = spill_lines(&cfg);
                        spill_lines_override =
                            crate::toggle_spill_detail(spill_lines_override, configured);
                        print_newt(
                            &spill_status(
                                configured,
                                spill_lines_override,
                                crate::effective_spill_summary(
                                    crate::summary_recovery_available(surface_is_rich),
                                    spill_summary_override,
                                ),
                                surface_is_rich,
                                live_spill_eligibility(),
                            ),
                            color,
                            verbose,
                        );
                        surface.save_history();
                        println!();
                        continue;
                    }
                    if slash_md == "spill" || slash_md.starts_with("spill ") {
                        let configured = spill_lines(&cfg);
                        match parse_spill_command(&task) {
                            // #1640 Layer 1: one pure transition for every
                            // form (crate::apply_spill_command, pinned by
                            // test) — Reset returns BOTH knobs to the surface
                            // defaults.
                            Ok(cmd) => {
                                crate::apply_spill_command(
                                    cmd,
                                    &mut spill_lines_override,
                                    &mut spill_summary_override,
                                );
                            }
                            Err(e) => {
                                print_newt(
                                    &format!(
                                        "error: {e} — use /spill [status|<rows>|reset|summary|excerpt]"
                                    ),
                                    color,
                                    verbose,
                                );
                                surface.save_history();
                                println!();
                                continue;
                            }
                        }
                        // One status line for every successful form — the same
                        // report, so the four arms cannot drift apart.
                        print_newt(
                            &spill_status(
                                configured,
                                spill_lines_override,
                                crate::effective_spill_summary(
                                    crate::summary_recovery_available(surface_is_rich),
                                    spill_summary_override,
                                ),
                                surface_is_rich,
                                live_spill_eligibility(),
                            ),
                            color,
                            verbose,
                        );
                        surface.save_history();
                        println!();
                        continue;
                    }

                    // #1387 Phases 2–4: structural nav + retrieval debug + impact.
                    if let Some(parsed) = crate::navigator_cmds::parse_nav_command(&task) {
                        match parsed {
                            Ok(cmd) => {
                                finish_nav_warmup(
                                    &rt,
                                    &mut nav_warmup,
                                    &mut where_is_index,
                                    &mut nav_session,
                                );
                                // Iteration #3: a still-building warm-up must not
                                // stall the command either — regex floor now,
                                // full index on a later invocation.
                                if nav_warmup.is_some() {
                                    print_newt(
                                        "repository index is still building — structural \
                                         answers use the regex floor until it finishes",
                                        color,
                                        verbose,
                                    );
                                }
                                if nav_warmup.is_none() {
                                    ensure_nav_indexes(
                                        workspace,
                                        &cfg,
                                        &mut where_is_index,
                                        &mut nav_session,
                                        &index_status,
                                    );
                                }
                                let msg = handle_nav_command(
                                    cmd,
                                    workspace,
                                    &mut nav_session,
                                    where_is_index.as_ref(),
                                    &index_status,
                                );
                                print_newt(&msg, color, verbose);
                            }
                            Err(e) => print_newt(&e, color, verbose),
                        }
                        surface.save_history();
                        println!();
                        continue;
                    }

                    // #1387 Phase 1: Code Navigator `/search` cockpit — same
                    // structured retrieve path as auto-inject + code_search.
                    if slash_md == "search" || slash_md.starts_with("search ") {
                        match parse_search_command(&task) {
                            Ok(SearchCommand::Help) => {
                                for line in search_help_text().lines() {
                                    print_newt(line, color, verbose);
                                }
                            }
                            Ok(SearchCommand::Status) => {
                                print_newt(
                                    &newt_core::format_index_status(
                                        &index_status,
                                        &retrieval_steer,
                                    ),
                                    color,
                                    verbose,
                                );
                            }
                            Ok(SearchCommand::Clear) => {
                                retrieval_steer.clear();
                                print_newt(
                                    "cleared session pins/exclusions (applies on next inject)",
                                    color,
                                    verbose,
                                );
                            }
                            Ok(SearchCommand::Preview(n)) => match last_search.as_ref() {
                                Some(r) => print_newt(
                                    &newt_core::format_search_preview(r, n),
                                    color,
                                    verbose,
                                ),
                                None => print_newt(
                                    "no search yet — run /search <query> first",
                                    color,
                                    verbose,
                                ),
                            },
                            Ok(SearchCommand::Model) => match last_search.as_ref() {
                                Some(r) => match newt_core::render_code_evidence(r) {
                                    Some(block) => {
                                        print_newt(
                                            "model view (exact evidence packet):",
                                            color,
                                            verbose,
                                        );
                                        println!("{block}");
                                    }
                                    None => print_newt(
                                        "last search selected no hits for the model packet",
                                        color,
                                        verbose,
                                    ),
                                },
                                None => print_newt(
                                    "no search yet — run /search <query> first",
                                    color,
                                    verbose,
                                ),
                            },
                            Ok(SearchCommand::Rejects) => match last_search.as_ref() {
                                Some(r) => {
                                    print_newt(
                                        &newt_core::format_search_rejects(r),
                                        color,
                                        verbose,
                                    );
                                }
                                None => print_newt(
                                    "no search yet — run /search <query> first",
                                    color,
                                    verbose,
                                ),
                            },
                            Ok(SearchCommand::Pin(n)) => match last_search.as_ref() {
                                Some(r) => match r.hits.get(n.saturating_sub(1)) {
                                    Some(hit) => {
                                        retrieval_steer.pin(hit.clone());
                                        print_newt(
                                            &format!(
                                                "pinned {} for next inject/tool retrieve",
                                                hit.loc_key()
                                            ),
                                            color,
                                            verbose,
                                        );
                                    }
                                    None => print_newt(
                                        &format!("no hit #{n} in last search"),
                                        color,
                                        verbose,
                                    ),
                                },
                                None => print_newt(
                                    "no search yet — run /search <query> first",
                                    color,
                                    verbose,
                                ),
                            },
                            Ok(SearchCommand::Exclude(n)) => match last_search.as_ref() {
                                Some(r) => match r.hits.get(n.saturating_sub(1)) {
                                    Some(hit) => {
                                        let path = hit.chunk.file.clone();
                                        retrieval_steer.exclude_path(path.clone());
                                        print_newt(
                                            &format!(
                                                "excluded path `{path}` from automatic retrieval"
                                            ),
                                            color,
                                            verbose,
                                        );
                                    }
                                    None => print_newt(
                                        &format!("no hit #{n} in last search"),
                                        color,
                                        verbose,
                                    ),
                                },
                                None => print_newt(
                                    "no search yet — run /search <query> first",
                                    color,
                                    verbose,
                                ),
                            },
                            Ok(SearchCommand::Query(query)) => {
                                let manager = context_manager(&cfg, context_manager_override);
                                let features = context_features(
                                    &cfg,
                                    manager,
                                    &context_features_override,
                                    inf_kind,
                                );
                                if !features.semantic {
                                    print_newt(
                                        "semantic feature is off — enable with /context feature semantic on",
                                        color,
                                        verbose,
                                    );
                                } else {
                                    let mut semantic_cfg = cfg
                                        .context
                                        .as_ref()
                                        .map(|c| c.semantic.clone())
                                        .unwrap_or_default();
                                    semantic_cfg.embedding_model_path =
                                        effective_embedding_model_path(
                                            semantic_cfg.embedding_model_path.take(),
                                            newt_inference::palette::embed_model_dir_if_present(),
                                        );
                                    if let Some(reason) =
                                        semantic_embedder_unavailable_reason(&semantic_cfg)
                                    {
                                        print_newt(&reason, color, verbose);
                                    } else {
                                        let embedder: std::sync::Arc<dyn newt_core::Embedder> =
                                            std::sync::Arc::from(build_semantic_embedder(
                                                &semantic_cfg,
                                                &inf_url,
                                                inf_kind,
                                                inf_key.as_deref(),
                                            ));
                                        if let Some(n) =
                                            poll_semantic_indexing(&rt, &mut semantic_warmup)
                                        {
                                            print_newt(
                                                &format!(
                                                    "semantic: indexed {n} code chunks (background)"
                                                ),
                                                color,
                                                verbose,
                                            );
                                        } else if semantic_warmup.is_some() {
                                            print_newt(
                                                "semantic index is still embedding in the \
                                                 background — results ride the lexical floor",
                                                color,
                                                verbose,
                                            );
                                        }
                                        if !semantic_indexed {
                                            semantic_indexed = true;
                                            let source_extensions =
                                                resolved_source_extensions(workspace, &cfg);
                                            let (files, manifest) = newt_core::gather_with_manifest(
                                                workspace,
                                                &source_extensions,
                                                newt_core::GatherCaps::default(),
                                            );
                                            let (git_head, dirty) = lightweight_git_meta(workspace);
                                            index_status.generation =
                                                index_status.generation.saturating_add(1);
                                            index_status.manifest = Some(manifest);
                                            index_status.git_head = git_head;
                                            index_status.dirty = dirty;
                                            if !files.is_empty() {
                                                print_newt(
                                                    &format!(
                                                        "embedding {} files for semantic \
                                                         retrieval in the background…",
                                                        files.len()
                                                    ),
                                                    color,
                                                    verbose,
                                                );
                                                semantic_warmup = Some(spawn_semantic_indexing(
                                                    &rt,
                                                    files,
                                                    std::sync::Arc::clone(&embedder),
                                                    std::sync::Arc::clone(&semantic_index),
                                                    semantic_cfg.on_embed_failure,
                                                ));
                                            }
                                        }
                                        match tokio::task::block_in_place(|| {
                                            rt.block_on(newt_core::retrieve_ranked(
                                                &query,
                                                embedder.as_ref(),
                                                semantic_index.as_ref(),
                                                semantic_cfg.top_k,
                                                Some(&retrieval_steer),
                                                Some(&index_status),
                                            ))
                                        }) {
                                            Some(result) => {
                                                print_newt(
                                                    &newt_core::format_search_hits(&result),
                                                    color,
                                                    verbose,
                                                );
                                                nav_session.turn_counter =
                                                    nav_session.turn_counter.saturating_add(1);
                                                let pins: Vec<_> = retrieval_steer
                                                    .pinned
                                                    .iter()
                                                    .map(|h| h.loc_key())
                                                    .collect();
                                                let ctx_hash =
                                                    newt_core::render_code_evidence(&result)
                                                        .map(|b| {
                                                            newt_core::hash_context(b.as_bytes())
                                                        })
                                                        .unwrap_or_else(|| {
                                                            newt_core::hash_context(
                                                                result
                                                                    .hits
                                                                    .iter()
                                                                    .map(|h| h.loc_key())
                                                                    .collect::<Vec<_>>()
                                                                    .join("\n")
                                                                    .as_bytes(),
                                                            )
                                                        });
                                                nav_session.ledger.record_semantic(
                                                    nav_session.turn_counter,
                                                    &query,
                                                    &result,
                                                    &pins,
                                                    &retrieval_steer.excluded_paths,
                                                    &ctx_hash,
                                                );
                                                nav_session.last_semantic = Some(result.clone());
                                                last_search = Some(result);
                                            }
                                            None => print_newt(
                                                "no code matched — index empty or embed failed",
                                                color,
                                                verbose,
                                            ),
                                        }
                                    }
                                }
                            }
                            Err(e) => print_newt(&format!("error: {e}"), color, verbose),
                        }
                        surface.save_history();
                        println!();
                        continue;
                    }
                    if tool_round_limit_command_arg(&task).is_some() {
                        let configured = cfg
                            .find_model_tuning(&inf_model)
                            .and_then(|t| t.max_tool_rounds)
                            .unwrap_or_else(|| max_tool_rounds(&cfg));
                        match parse_tool_round_limit_command(&task) {
                            Ok(ToolRoundLimitCommand::Show) => {
                                print_newt(
                                    &tool_round_limit_status(configured, max_tool_rounds_override),
                                    color,
                                    verbose,
                                );
                            }
                            Ok(ToolRoundLimitCommand::Set(n)) => {
                                max_tool_rounds_override = Some(n);
                                print_newt(
                                    &tool_round_limit_status(configured, max_tool_rounds_override),
                                    color,
                                    verbose,
                                );
                            }
                            Ok(ToolRoundLimitCommand::Double) => {
                                let current = effective_tool_round_limit(
                                    configured,
                                    max_tool_rounds_override,
                                );
                                max_tool_rounds_override = Some(double_tool_round_limit(current));
                                print_newt(
                                    &tool_round_limit_status(configured, max_tool_rounds_override),
                                    color,
                                    verbose,
                                );
                            }
                            Ok(ToolRoundLimitCommand::Reset) => {
                                max_tool_rounds_override = None;
                                print_newt(
                                    &format!(
                                        "tool-call round limit reset to {}",
                                        describe_tool_round_limit(configured)
                                    ),
                                    color,
                                    verbose,
                                );
                            }
                            Ok(ToolRoundLimitCommand::Unlimited) => {
                                max_tool_rounds_override = Some(EFFECTIVELY_UNLIMITED_TOOL_ROUNDS);
                                print_newt(
                                    &tool_round_limit_status(configured, max_tool_rounds_override),
                                    color,
                                    verbose,
                                );
                            }
                            Err(e) => print_newt(
                                &format!(
                                    "error: {e} — use /rounds [show|<n>|double|reset|unlimited]"
                                ),
                                color,
                                verbose,
                            ),
                        }
                        surface.save_history();
                        println!();
                        continue;
                    }
                    if slash_md == "context" || slash_md.starts_with("context ") {
                        // Step 24.8 (#559) / Step 26.1 (#588): the context-manager
                        // preset selector + composable feature toggles. Only
                        // `standard` / no features are implemented yet; the rest
                        // report "not yet available" (#546 / #582–#586). Dispatch
                        // is a pure, unit-tested helper.
                        let rest = slash_md.strip_prefix("context").unwrap_or("").trim();
                        if rest == "stats" {
                            // Step 26.2 (#588): the experimentation dashboard —
                            // needs runtime state (live gauge + compression
                            // counters), so it's handled here, not in the pure
                            // dispatch helper.
                            let manager = context_manager(&cfg, context_manager_override);
                            let compaction_policy =
                                compaction_trigger_policy(&cfg, compaction_trigger_policy_override);
                            let compaction_policy_source = compaction_trigger_policy_source(
                                &cfg,
                                compaction_trigger_policy_override,
                            );
                            let features = context_features(
                                &cfg,
                                manager,
                                &context_features_override,
                                inf_kind,
                            );
                            // Step 26.3: surface tool_offload's measured impact.
                            let impact = features.tool_offload.then(|| {
                                use newt_core::SpillStore;
                                (
                                    spill_store.unique_objects(),
                                    spill_store.unique_offloaded_chars(),
                                )
                            });
                            let scratch_impact = features.scratchpad.then(|| {
                                use newt_core::ScratchpadStore;
                                (
                                    scratchpad_store.keys_count(),
                                    scratchpad_store.state_chars(),
                                )
                            });
                            let sem_impact = features.semantic.then(|| {
                                use newt_core::SemanticIndex;
                                (
                                    semantic_index.chunks_indexed(),
                                    semantic_index.indexed_chars(),
                                )
                            });
                            let exp_impact = features.experiential.then(|| {
                                use newt_core::ExperienceStore;
                                (experience_store.count(), experience_store.total_chars())
                            });
                            let plan_impact = features.scheduled.then(|| {
                                use newt_core::StepLedger;
                                (step_ledger.count(), step_ledger.done_count())
                            });
                            for line in context_stats_text(
                                token_gauge,
                                &compress_state.counters(),
                                compaction_policy,
                                compaction_policy_source,
                                features,
                                impact,
                                scratch_impact,
                                sem_impact,
                                exp_impact,
                                plan_impact,
                            ) {
                                print_newt(&line, color, verbose);
                            }
                        } else if rest == "show" {
                            // Build the outbound message set fresh and render a
                            // compact per-message breakdown so the operator can
                            // see exactly what fills the window right now.
                            let msgs = memory.build_messages("", "");
                            let mut total = 0usize;
                            print_newt("context contents (freshly built):", color, verbose);
                            for (i, m) in msgs.iter().enumerate() {
                                let chars = m.content.chars().count();
                                total += chars;
                                let preview: String =
                                    m.content.chars().take(60).collect::<String>();
                                let preview = preview.replace('\n', " ");
                                print_newt(
                                    &format!(
                                        "  [{i:>2}] {:<9} {chars:>7} chars  {preview}",
                                        m.role.as_str()
                                    ),
                                    color,
                                    verbose,
                                );
                            }
                            print_newt(
                                &format!(
                                    "  total: {} messages, {total} chars (~{} tokens)",
                                    msgs.len(),
                                    total / 4
                                ),
                                color,
                                verbose,
                            );
                        } else {
                            let result = handle_context_command(
                                rest,
                                &cfg,
                                context_manager_override,
                                compaction_trigger_policy_override,
                                &context_features_override,
                                inf_kind,
                            );
                            for line in &result.lines {
                                print_newt(line, color, verbose);
                            }
                            if let Some(m) = result.set_manager {
                                context_manager_override = Some(m);
                            }
                            if let Some((f, on)) = result.set_feature {
                                context_features_override.set(f, Some(on));
                            }
                            if let Some(sz) = result.set_budget {
                                context_size_override = if sz == 0 { None } else { Some(sz) };
                            }
                            if let Some(policy) = result.set_compaction_trigger_policy {
                                match policy {
                                    CompactionTriggerPolicyOverride::Set(policy) => {
                                        compaction_trigger_policy_override = Some(policy);
                                    }
                                    CompactionTriggerPolicyOverride::Reset => {
                                        compaction_trigger_policy_override = None;
                                    }
                                }
                            }
                        }
                        surface.save_history();
                        println!();
                        continue;
                    }
                    if let Some(fact) = task.trim_start_matches('/').strip_prefix("remember ") {
                        // Route the fact through MemoryManager::add_note —
                        // the first note-capable provider (NoteStore) wins.
                        match memory.add_note(fact) {
                            Ok(()) => print_newt(&format!("Noted: {fact}"), color, verbose),
                            Err(e) => print_newt(&format!("error: {e}"), color, verbose),
                        }
                        println!();
                        continue;
                    }
                    // `/new` · `/end` · `/restart` finalize the current
                    // conversation and drop into a fresh one; `/start [title]`
                    // SWITCHES to a fresh one but leaves the outgoing OPEN and
                    // resumable (#1030). All stay in the session — `/exit` ·
                    // `/quit` · vi `:wq` leave. The `end_reason`, the message,
                    // and (for /start) an optional pre-title are what differ.
                    let slash_verb = task.trim_start_matches('/');
                    let (verb, verb_arg) = slash_verb
                        .split_once(char::is_whitespace)
                        .map_or((slash_verb, ""), |(v, a)| (v, a.trim()));
                    let close_word = match verb {
                        // `/clear` is the Claude-Code-parity alias for `/new`.
                        "new" | "clear" => Some("new"),
                        "end" => Some("end"),
                        "restart" => Some("restart"),
                        "start" => Some("start"),
                        _ => None,
                    };
                    if let Some(reason) = close_word {
                        // #1030: /start SWITCHES (leaves the outgoing conversation
                        // OPEN), so it does NOT finalize — skip close-time note
                        // extraction, which is a finalization action (running it on
                        // a still-open conversation also double-extracts when it is
                        // later /resumed and /ended). The finalizers (/new · /end ·
                        // /restart) extract as before; 19.4: extraction runs BEFORE
                        // the reset below wipes the history it reads, and failure
                        // never blocks the reset.
                        if reason != "start" {
                            let close_complete = build_session_summarizer(
                                &sum_cfg,
                                &cfg,
                                &inf_url,
                                &inf_model,
                                inf_kind,
                                &inf_key,
                                Some(mem_budget),
                                color,
                            );
                            if let Some(notice) = tokio::task::block_in_place(|| {
                                rt.block_on(run_close_extraction(
                                    extract_on_close,
                                    ephemeral_session,
                                    turns_this_conversation,
                                    &mut memory,
                                    &close_complete,
                                ))
                            }) {
                                print_newt(&notice, color, verbose);
                            }
                        }
                        let outgoing_id = active_conversation_id.clone();
                        // #1030: does the outgoing conversation have a durable
                        // row (an accepted prompt receipt, a completed turn, or
                        // an explicitly titled `/start`)? Prompt-only
                        // conversations survive failed/cancelled inference and
                        // remain resumable. Only an unrecorded or ephemeral
                        // conversation has no row.
                        let outgoing_durable = conversation_store
                            .as_ref()
                            .is_some_and(|store| store.exists(&outgoing_id).unwrap_or(false));
                        if let Some(store) = conversation_store.as_ref() {
                            // `/start` leaves the outgoing conversation OPEN so it
                            // stays resumable via `/resume`; the finalizers mark it
                            // ended (`end_reason`, shown as ✓ in `/resume`). Only
                            // when durable content exists — a truly untouched
                            // conversation has no row to resolve, while a
                            // prompt-only conversation does.
                            if reason != "start" && outgoing_durable {
                                if let Err(e) = store.end_conversation(&outgoing_id, reason) {
                                    print_newt(
                                        &format!("warning: could not mark conversation ended: {e}"),
                                        color,
                                        verbose,
                                    );
                                }
                            }
                            // #1030: hand the outgoing conversation back — this
                            // process is no longer its live owner, so another newt
                            // (or a later /resume) may take it.
                            let _ = store.release(&outgoing_id);
                        }
                        turns_this_conversation = 0;
                        let mut reset_ctx = ConversationResetContext {
                            memory: &mut memory,
                            system: &mut system,
                            conversation_id: &mut active_conversation_id,
                            mode_states: &conversation_mode_states,
                        };
                        let started = handle_new_conversation(
                            workspace,
                            active_persona.as_ref(),
                            &mut reset_ctx,
                            &mut compress_state,
                            &mut session_opted_fresh,
                        );
                        active_prompt_context = None;
                        pending_clarification = None;
                        ephemeral_artifact_store =
                            session_artifact_store(ephemeral_session, &active_conversation_id)?;
                        // #1030: pre-title a `/start <title>` conversation by
                        // creating its (empty) record up front, so it appears in
                        // `/resume` with that title immediately; the first turn
                        // then appends rather than deriving a title. Then claim the
                        // new conversation for THIS process (a brand-new id — the
                        // claim is always granted).
                        if let Some(store) = conversation_store.as_ref() {
                            if reason == "start" && !verb_arg.is_empty() {
                                let _ = store.create_with_id(
                                    &active_conversation_id,
                                    verb_arg,
                                    active_persona.as_ref().map(|p| p.name.as_str()),
                                );
                            }
                            let _ = store.claim(&active_conversation_id);
                        }
                        // Step 26.4 (#583): drop scratchpad state so a fresh task
                        // never inherits the previous conversation's variables.
                        {
                            use newt_core::ScratchpadStore;
                            scratchpad_store.clear();
                        }
                        // Step 26.5.4 (#582): drop the semantic index + re-arm
                        // indexing so the next task re-indexes (picks up edits).
                        {
                            use newt_core::SemanticIndex;
                            semantic_index.clear();
                            if let Some(warmup) = semantic_warmup.take() {
                                // Iteration #4: a stale-corpus embed must not
                                // keep burning cores into the fresh session.
                                warmup.handle.abort();
                                drop(warmup.job);
                            }
                        }
                        semantic_indexed = false;
                        // #1387: pins/exclusions and search cockpit state are
                        // session-task scoped — clear with the index on /new.
                        retrieval_steer.clear();
                        index_status = newt_core::IndexStatus::default();
                        last_search = None;
                        nav_session.clear();
                        // #1285: drop the where_is index too so /new re-derives it
                        // (picks up file adds/removes on the next turn).
                        where_is_index = None;
                        if let Some(warmup) = nav_warmup.take() {
                            warmup.abort();
                        }
                        nav_warmup = Some(spawn_nav_warmup(&rt, workspace, &cfg, &index_status));
                        // Step 26.6a (#585): the experiential ledger is INTENTIONALLY
                        // NOT cleared here — it is cross-task by design (a later task
                        // reuses earlier lessons). It is dropped only at session end.
                        // Step 26.6b (#586): the plan ledger IS cleared — it is
                        // task-specific (a new task gets a fresh plan).
                        {
                            use newt_core::StepLedger;
                            step_ledger.clear();
                        }
                        let msg = close_out_message(reason, &started, outgoing_durable);
                        print_newt(&msg, color, verbose);
                        surface.save_history();
                        println!();
                        // #1030: /end now finalizes-and-CONTINUES like /new and
                        // /restart (it no longer exits the program) — /exit ·
                        // /quit · vi :wq are the leave-the-session verbs.
                        continue;
                    }
                    // #1030: `/rename <title>` retitles the CURRENT conversation
                    // so it is easy to find later in `/resume` (titles are what
                    // `/resume` lists and searches). Renaming before the first
                    // turn pre-titles an (empty) record so it shows up titled.
                    if verb == "rename" {
                        match conversation_store.as_ref() {
                            Some(store) => {
                                let title = verb_arg.trim();
                                if title.is_empty() {
                                    print_newt("usage: /rename <new title>", color, verbose);
                                } else {
                                    // #1030: match on exists() so a transient store
                                    // error (SQLITE_BUSY / NFS IO under concurrent-
                                    // newt contention) can NEVER read as "absent" and
                                    // route into create_with_id, whose INSERT OR
                                    // REPLACE would destroy the live conversation and
                                    // CASCADE-drop its turns. The save path guards the
                                    // same hazard with `exists()?`.
                                    match store.exists(&active_conversation_id) {
                                        Ok(true) => {
                                            match store.rename(&active_conversation_id, title) {
                                                Ok(()) => print_newt(
                                                    &format!(
                                                        "Renamed this conversation to \"{title}\"."
                                                    ),
                                                    color,
                                                    verbose,
                                                ),
                                                Err(e) => print_newt(
                                                    &format!("could not rename: {e}"),
                                                    color,
                                                    verbose,
                                                ),
                                            }
                                        }
                                        Ok(false) => match store.create_with_id(
                                            &active_conversation_id,
                                            title,
                                            active_persona.as_ref().map(|p| p.name.as_str()),
                                        ) {
                                            Ok(()) => print_newt(
                                                &format!("Titled this conversation \"{title}\"."),
                                                color,
                                                verbose,
                                            ),
                                            Err(e) => print_newt(
                                                &format!("could not title: {e}"),
                                                color,
                                                verbose,
                                            ),
                                        },
                                        Err(e) => print_newt(
                                            &format!("could not rename (store error: {e})"),
                                            color,
                                            verbose,
                                        ),
                                    }
                                }
                            }
                            None => print_newt(EPHEMERAL_SESSION_NOTICE, color, verbose),
                        }
                        surface.save_history();
                        println!();
                        continue;
                    }
                    let slash_body = task.trim_start_matches('/');
                    if slash_body == "conversation" || slash_body.starts_with("conversation ") {
                        let conversation_id_before = active_conversation_id.clone();
                        match conversation_store.as_ref() {
                            Some(store) => {
                                let mut conversation_ctx = ConversationCommandContext {
                                    store,
                                    persona_store: &persona_store,
                                    workspace,
                                    memory: &mut memory,
                                    system: &mut system,
                                    active_persona: &mut active_persona,
                                    active_conversation_id: &mut active_conversation_id,
                                    compress_state: &mut compress_state,
                                    scratchpad: &scratchpad_store
                                        as &dyn newt_core::ScratchpadStore,
                                    step_ledger: &step_ledger as &dyn newt_core::StepLedger,
                                    active_prompt_context: &mut active_prompt_context,
                                    mode_states: &conversation_mode_states,
                                };
                                match handle_conversation_command(&task, &mut conversation_ctx) {
                                    Ok(msg) => print_newt(&msg, color, verbose),
                                    Err(e) => print_newt(&format!("error: {e}"), color, verbose),
                                }
                            }
                            None => print_newt(EPHEMERAL_SESSION_NOTICE, color, verbose),
                        }
                        if active_conversation_id != conversation_id_before {
                            let store = conversation_store.as_ref().ok_or_else(|| {
                                anyhow::anyhow!("durable conversation restore lost its store")
                            })?;
                            pending_clarification = match active_prompt_context.as_ref() {
                                Some(parent) => match rehydrate_pending_clarification(
                                    store,
                                    &active_conversation_id,
                                    parent,
                                ) {
                                    Ok(pending) => pending,
                                    Err(e) => {
                                        return Err(anyhow::anyhow!(
                                            "could not safely restore a pending clarification: {e}"
                                        ));
                                    }
                                },
                                None => None,
                            };
                            if let Some(pending) = pending_clarification.as_ref() {
                                print_newt(&restored_clarification_notice(pending), color, verbose);
                            }
                        }
                        surface.save_history();
                        println!();
                        continue;
                    }
                    if slash_body == "recall" || slash_body.starts_with("recall ") {
                        match conversation_store.as_ref() {
                            Some(store) => match handle_recall_command(&task, store) {
                                Ok(msg) => print_newt(&msg, color, verbose),
                                Err(e) => print_newt(&format!("error: {e}"), color, verbose),
                            },
                            None => print_newt(EPHEMERAL_SESSION_NOTICE, color, verbose),
                        }
                        surface.save_history();
                        println!();
                        continue;
                    }
                    // #1030: `/resume` — find and reopen a past conversation,
                    // listed by liveness and searchable. Bare = browse; <query> =
                    // FTS5 search (or an id/prefix to open directly); <n> = pick
                    // the n-th row from the last listing. Reopening is claim-guarded
                    // so a conversation a live newt already holds is refused.
                    if slash_body == "resume" || slash_body.starts_with("resume ") {
                        match conversation_store.as_ref() {
                            Some(store) => {
                                let target: Option<String> = match parse_resume_command(&task) {
                                    ResumeCommand::Browse => {
                                        match resume_browse_message(store, &active_conversation_id)
                                        {
                                            Ok((msg, ids)) => {
                                                last_resume_listing = ids;
                                                print_newt(&msg, color, verbose);
                                            }
                                            Err(e) => {
                                                print_newt(&format!("error: {e}"), color, verbose);
                                            }
                                        }
                                        None
                                    }
                                    ResumeCommand::Select(n) => {
                                        match n
                                            .checked_sub(1)
                                            .and_then(|i| last_resume_listing.get(i))
                                        {
                                            Some(id) => Some(id.clone()),
                                            None => {
                                                print_newt(
                                                    "no such row — run /resume to list, then /resume <n>",
                                                    color,
                                                    verbose,
                                                );
                                                None
                                            }
                                        }
                                    }
                                    ResumeCommand::Query(token) => match store.resolve_id(&token) {
                                        Ok(id) => Some(id),
                                        Err(_) => {
                                            match resume_search_message(
                                                store,
                                                &token,
                                                &active_conversation_id,
                                            ) {
                                                Ok((msg, ids)) => {
                                                    last_resume_listing = ids;
                                                    print_newt(&msg, color, verbose);
                                                }
                                                Err(e) => print_newt(
                                                    &format!("error: {e}"),
                                                    color,
                                                    verbose,
                                                ),
                                            }
                                            None
                                        }
                                    },
                                };
                                if let Some(id) = target {
                                    if id == active_conversation_id {
                                        // #1030: resuming the CURRENT conversation is a
                                        // no-op — short-circuit so we never release +
                                        // (fail to) re-acquire our own claim, nor reset
                                        // this session's turn count (which would suppress
                                        // its close-time note extraction).
                                        print_newt("already in this conversation.", color, verbose);
                                    } else {
                                        // Claim-guard: refuse to reopen a conversation a
                                        // DIFFERENT live newt holds (that would mix turns).
                                        match store.claim(&id) {
                                            Ok(newt_core::ClaimOutcome::HeldBy { host, pid }) => {
                                                print_newt(
                                                &format!(
                                                    "conversation {} is open in another newt \
                                                     (pid {pid} on {host}) — /resume a different one",
                                                    short_conversation_id(&id)
                                                ),
                                                color,
                                                verbose,
                                            );
                                            }
                                            Ok(newt_core::ClaimOutcome::Claimed) => {
                                                let outgoing = active_conversation_id.clone();
                                                if outgoing != id {
                                                    let _ = store.release(&outgoing);
                                                }
                                                let mut resume_ctx = ConversationCommandContext {
                                                    store,
                                                    persona_store: &persona_store,
                                                    workspace,
                                                    memory: &mut memory,
                                                    system: &mut system,
                                                    active_persona: &mut active_persona,
                                                    active_conversation_id:
                                                        &mut active_conversation_id,
                                                    compress_state: &mut compress_state,
                                                    scratchpad: &scratchpad_store
                                                        as &dyn newt_core::ScratchpadStore,
                                                    step_ledger: &step_ledger
                                                        as &dyn newt_core::StepLedger,
                                                    active_prompt_context:
                                                        &mut active_prompt_context,
                                                    mode_states: &conversation_mode_states,
                                                };
                                                match resume_session_conversation(
                                                    &mut resume_ctx,
                                                    &id,
                                                ) {
                                                    Ok(banner) => {
                                                        turns_this_conversation = 0;
                                                        pending_clarification = match active_prompt_context.as_ref() {
                                                            Some(parent) => match rehydrate_pending_clarification(
                                                                store,
                                                                &active_conversation_id,
                                                                parent,
                                                            ) {
                                                                Ok(pending) => pending,
                                                                Err(e) => {
                                                                    return Err(anyhow::anyhow!(
                                                                        "could not safely restore a pending clarification: {e}"
                                                                    ));
                                                                }
                                                            },
                                                            None => None,
                                                        };
                                                        print_newt(&banner, color, verbose);
                                                        if let Some(pending) =
                                                            pending_clarification.as_ref()
                                                        {
                                                            print_newt(
                                                                &restored_clarification_notice(
                                                                    pending,
                                                                ),
                                                                color,
                                                                verbose,
                                                            );
                                                        }
                                                    }
                                                    Err(e) => {
                                                        // Restore failed — undo the swap: hand
                                                        // `id` back and re-claim the outgoing.
                                                        let _ = store.release(&id);
                                                        if outgoing != id {
                                                            let _ = store.claim(&outgoing);
                                                        }
                                                        print_newt(
                                                            &format!("could not resume: {e}"),
                                                            color,
                                                            verbose,
                                                        );
                                                    }
                                                }
                                            }
                                            Err(e) => print_newt(
                                                &format!("could not claim conversation: {e}"),
                                                color,
                                                verbose,
                                            ),
                                        }
                                    }
                                }
                            }
                            None => print_newt(EPHEMERAL_SESSION_NOTICE, color, verbose),
                        }
                        surface.save_history();
                        println!();
                        continue;
                    }
                    // #1030: /roadmap — author + view a Roadmap→Phase→Plan→Task
                    // tree; /tree renders the active roadmap (alias of /roadmap show).
                    if slash_body == "roadmap"
                        || slash_body.starts_with("roadmap ")
                        || slash_body == "plan"
                        || slash_body.starts_with("plan ")
                        || slash_body == "tree"
                    {
                        let cmd = if slash_body == "tree" {
                            "/roadmap show".to_string()
                        } else if slash_body == "plan" {
                            "/roadmap".to_string()
                        } else if let Some(rest) = slash_body.strip_prefix("plan ") {
                            format!("/roadmap {rest}")
                        } else {
                            task.clone()
                        };
                        match conversation_store.as_ref() {
                            Some(store) => {
                                match handle_roadmap_command(
                                    &cmd,
                                    store,
                                    &mut active_roadmap_id,
                                    &active_conversation_id,
                                    workspace,
                                ) {
                                    Ok(outcome) => {
                                        print_newt(&outcome.message, color, verbose);
                                        // #1030 resume-to-cursor: /roadmap next on a bound Plan
                                        // node switches to that node's conversation (same
                                        // claim-guarded restore as /resume).
                                        if let Some(target) = outcome.switch_to {
                                            if target != active_conversation_id {
                                                match store.claim(&target) {
                                                    Ok(newt_core::ClaimOutcome::HeldBy {
                                                        host,
                                                        pid,
                                                    }) => print_newt(
                                                        &format!(
                                                            "that node's conversation is open in \
                                                             another newt (pid {pid} on {host}) — \
                                                             not switching",
                                                        ),
                                                        color,
                                                        verbose,
                                                    ),
                                                    Ok(newt_core::ClaimOutcome::Claimed) => {
                                                        let outgoing =
                                                            active_conversation_id.clone();
                                                        let _ = store.release(&outgoing);
                                                        let mut ctx = ConversationCommandContext {
                                                            store,
                                                            persona_store: &persona_store,
                                                            workspace,
                                                            memory: &mut memory,
                                                            system: &mut system,
                                                            active_persona: &mut active_persona,
                                                            active_conversation_id:
                                                                &mut active_conversation_id,
                                                            compress_state: &mut compress_state,
                                                            scratchpad: &scratchpad_store
                                                                as &dyn newt_core::ScratchpadStore,
                                                            step_ledger: &step_ledger
                                                                as &dyn newt_core::StepLedger,
                                                            active_prompt_context:
                                                                &mut active_prompt_context,
                                                            mode_states: &conversation_mode_states,
                                                        };
                                                        match resume_session_conversation(
                                                            &mut ctx, &target,
                                                        ) {
                                                            Ok(banner) => {
                                                                turns_this_conversation = 0;
                                                                pending_clarification = match active_prompt_context.as_ref() {
                                                                    Some(parent) => match rehydrate_pending_clarification(
                                                                        store,
                                                                        &active_conversation_id,
                                                                        parent,
                                                                    ) {
                                                                        Ok(pending) => pending,
                                                                        Err(e) => {
                                                                            return Err(anyhow::anyhow!(
                                                                                "could not safely restore a pending clarification: {e}"
                                                                            ));
                                                                        }
                                                                    },
                                                                    None => None,
                                                                };
                                                                print_newt(&banner, color, verbose);
                                                                if let Some(pending) =
                                                                    pending_clarification.as_ref()
                                                                {
                                                                    print_newt(
                                                                        &restored_clarification_notice(pending),
                                                                        color,
                                                                        verbose,
                                                                    );
                                                                }
                                                            }
                                                            Err(e) => {
                                                                let _ = store.release(&target);
                                                                let _ = store.claim(&outgoing);
                                                                print_newt(
                                                                    &format!(
                                                                        "could not resume: {e}"
                                                                    ),
                                                                    color,
                                                                    verbose,
                                                                );
                                                            }
                                                        }
                                                    }
                                                    Err(e) => print_newt(
                                                        &format!(
                                                            "could not claim conversation: {e}"
                                                        ),
                                                        color,
                                                        verbose,
                                                    ),
                                                }
                                            }
                                        }
                                    }
                                    Err(e) => print_newt(&format!("error: {e}"), color, verbose),
                                }
                            }
                            None => print_newt(EPHEMERAL_SESSION_NOTICE, color, verbose),
                        }
                        surface.save_history();
                        println!();
                        continue;
                    }
                    if slash_body == "persona" || slash_body.starts_with("persona ") {
                        // `handle_persona_command` rotates `active_conversation_id`
                        // itself for the cases that start a new conversation
                        // (clear / set without --keep-context), so the per-session
                        // plan path follows (issue #220).
                        let persona_name_before = active_persona.as_ref().map(|p| p.name.clone());
                        let conversation_id_before = active_conversation_id.clone();
                        let mut reset_ctx = ConversationResetContext {
                            memory: &mut memory,
                            system: &mut system,
                            conversation_id: &mut active_conversation_id,
                            mode_states: &conversation_mode_states,
                        };
                        match handle_persona_command(
                            &task,
                            workspace,
                            &persona_store,
                            &mut active_persona,
                            &mut reset_ctx,
                        ) {
                            Ok(msg) => print_newt(&msg, color, verbose),
                            Err(e) => print_newt(&format!("error: {e}"), color, verbose),
                        }
                        if active_conversation_id != conversation_id_before {
                            active_prompt_context = None;
                            pending_clarification = None;
                            ephemeral_artifact_store =
                                session_artifact_store(ephemeral_session, &active_conversation_id)?;
                        }
                        // FR-4 (#1041): only warn when this command actually
                        // activated a (possibly new) persona — not on every
                        // `/persona show`/`list` re-check of an unchanged one.
                        if active_persona.as_ref().map(|p| &p.name) != persona_name_before.as_ref()
                        {
                            warn_on_missing_bound_skills(
                                active_persona.as_ref(),
                                &cfg.skill_search_dirs(),
                                color,
                                verbose,
                            );
                            // Persona backend auto-route: repoint the session's
                            // wire target to the new persona's `backend:` (if any),
                            // exactly as `/backends <name>` would; a persona with no
                            // backend (or a cleared one) reverts to the baseline.
                            let url_changed = apply_persona_backend(
                                active_persona.as_ref(),
                                &base_provider,
                                &base_model,
                                &cfg,
                                &mut choice,
                                &mut inf_url,
                                &mut inf_model,
                                &mut inf_kind,
                                &mut inf_key,
                                &mut inf_context_window,
                                color,
                                verbose,
                            );
                            if url_changed && verbose {
                                dgx_rx = dgx_probe::DgxTelemetry::try_connect(&inf_url)
                                    .map(|d| d.into_sampler(2));
                            }
                        }
                        surface.save_history();
                        println!();
                        continue;
                    }
                    if slash_body == "loadout" || slash_body.starts_with("loadout ") {
                        // The audit companion to `/config`: show the active loadout's
                        // declared axes vs what actually resolved this session. Needs
                        // live session state (resolved model/endpoint, active profile,
                        // persona), so it lives here rather than in `dispatch_slash`.
                        let arg = slash_body.strip_prefix("loadout").unwrap_or("").trim();
                        if arg.is_empty() || arg == "show" {
                            let loadout_name =
                                std::env::var("NEWT_LOADOUT").ok().filter(|s| !s.is_empty());
                            let loadout = loadout_name.as_deref().and_then(|n| cfg.loadouts.get(n));
                            // Recompute the profile pick (pure) for its provenance.
                            let profile_env = std::env::var("NEWT_PROFILE").ok();
                            let bundle_env = std::env::var("NEWT_BUNDLE").ok();
                            let pick = cfg
                                .pick_active_profile(
                                    profile_env.as_deref(),
                                    bundle_env.as_deref(),
                                    &inf_model,
                                )
                                .ok()
                                .flatten();
                            let view = LoadoutView {
                                name: loadout_name.as_deref(),
                                loadout,
                                inf_url: &inf_url,
                                inf_model: &inf_model,
                                profile_pick: pick.as_ref(),
                                persona: active_persona.as_ref().map(|p| p.name.as_str()),
                            };
                            print_newt(&view.render(), color, verbose);
                        } else {
                            print_newt(
                                &format!("unknown /loadout subcommand '{arg}' — try /loadout show"),
                                color,
                                verbose,
                            );
                        }
                        surface.save_history();
                        println!();
                        continue;
                    }
                    // #1665: bare `/psyche` IS the panel now (`psyche edit` stays
                    // as a muscle-memory alias). The panel needs a rich-tui TTY;
                    // otherwise bare `/psyche` falls through to dispatch_slash,
                    // which renders the text status view — so piped, headless,
                    // and lean sessions keep a working `/psyche` with zero noise.
                    // Token-wise match (review v2 on #1665): `/psyche  edit`
                    // with doubled spaces is still the alias, while
                    // `/psyche edit extra` is NOT (it falls through to the
                    // dispatch usage error rather than silently dropping the
                    // extra argument).
                    let psyche_tokens: Vec<&str> = slash_body.split_whitespace().collect();
                    let wants_psyche_panel = psyche_tokens.as_slice() == ["psyche", "edit"]
                        || (psyche_tokens.as_slice() == ["psyche"]
                            && cfg!(feature = "rich-tui")
                            && std::io::IsTerminal::is_terminal(&std::io::stdout()));
                    if wants_psyche_panel {
                        // The harness config panel (#14): a transient overlay for
                        // the psyche operator dials. It applies dials through the
                        // same globals the slash commands do (picked up next turn,
                        // no re-resolve); persona select + save come back for us to
                        // act on, since the session owns that state. Rich-tui only —
                        // the lean build has no ratatui surface, so it points at the
                        // text /psyche + per-dial commands instead.
                        #[cfg(feature = "rich-tui")]
                        {
                            use config_panel::{PanelOutcome, PersonaAction, PersonaChoice};
                            // review-3 §3: hand the panel each persona's declarations
                            // so it can PROJECT the selected persona's effective
                            // posture, plus the config/family tenacity base.
                            let personas: Vec<PersonaChoice> = persona_store
                                .list()
                                .map(|v| {
                                    v.into_iter()
                                        .filter_map(|s| persona_store.load(&s.name).ok())
                                        .map(|p| PersonaChoice {
                                            name: p.name.clone(),
                                            cognition: p.profile.cognition,
                                            tenacity: p.profile.tenacity,
                                            backend: p.profile.backend.clone(),
                                            crew: p.profile.crew,
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            // review-3 §3: the projection fallback for a persona that
                            // declares no backend is the operator BASELINE — what
                            // apply_persona_backend reverts to — NOT the outgoing
                            // persona's current backend.
                            let backend = base_provider.clone();
                            let current_persona = active_persona.as_ref().map(|p| p.name.clone());
                            let base_tenacity = newt_core::tenacity::base_tenacity();
                            // review-3 §1: the ONLY filesystem I/O, injected so a failed
                            // write keeps the panel open and mutates nothing. Report the
                            // store's NORMALIZED (on-disk) name so the confirmation
                            // matches the written file.
                            let persist =
                                |name: &str, content: &str, overwrite: bool| match persona_store
                                    .save(name, content, overwrite)
                                {
                                    Ok(path) => config_panel::SaveResult::Saved {
                                        name: path
                                            .file_stem()
                                            .map(|s| s.to_string_lossy().into_owned())
                                            .unwrap_or_else(|| name.to_string()),
                                    },
                                    Err(PersonaSaveError::Exists) => {
                                        config_panel::SaveResult::Exists {
                                            name: name.to_string(),
                                        }
                                    }
                                    Err(PersonaSaveError::InvalidName(m)) => {
                                        config_panel::SaveResult::InvalidName(m)
                                    }
                                    Err(PersonaSaveError::Io(e)) => {
                                        config_panel::SaveResult::Failed(e)
                                    }
                                };
                            // #1666: the model spinner's option list — fetched
                            // HERE (the panel stays network-free) via the same
                            // seam /models uses, tagged with cached conformance
                            // symbols. An unreachable backend → None → the row
                            // renders but won't dial.
                            let panel_choice = resolve_backend_choice(&cfg);
                            let served_models = fetch_models_for(
                                &panel_choice.url,
                                panel_choice.kind,
                                panel_choice.api_key.as_deref(),
                            )
                            .ok()
                            .map(|names| {
                                let cache = probe::load_cache();
                                names
                                    .into_iter()
                                    .map(|name| {
                                        let tag = cache
                                            .get(&probe::cap_key(
                                                newt_core::Serving::Multiplexer,
                                                "",
                                                &name,
                                            ))
                                            .map(|e| e.conformance.symbol().to_string())
                                            .unwrap_or_default();
                                        config_panel::ModelChoice { name, tag }
                                    })
                                    .collect::<Vec<_>>()
                            });
                            let outcome = run_psyche_panel(
                                config_panel::PanelSeed {
                                    personas,
                                    current_persona,
                                    backend,
                                    base_tenacity,
                                    models: served_models,
                                    current_model: panel_choice.model.clone(),
                                },
                                persist,
                                color,
                                verbose,
                            );
                            // Commit (review-3 §1/§2): the file (if any) was persisted
                            // inside the panel; dials were applied inside run() on an
                            // explicit apply. Here we report the save, apply the persona
                            // action, reroute the backend, route the model pick through
                            // the /model path, then report the committed posture from
                            // FRESH runtime state (never the working copy).
                            let (persona_action, saved_name, chosen_model, applied) = match outcome
                            {
                                PanelOutcome::Cancelled => (None, None, None, false),
                                PanelOutcome::Saved { name } => (None, Some(name), None, false),
                                PanelOutcome::Applied { persona, model } => {
                                    (Some(persona), None, model, true)
                                }
                                PanelOutcome::SavedAndApplied {
                                    name,
                                    persona,
                                    model,
                                } => (Some(persona), Some(name), model, true),
                            };
                            if let Some(name) = &saved_name {
                                print_newt(&format!("saved persona '{name}'"), color, verbose);
                            }
                            // #1665: a cancelled / no-op visit prints nothing —
                            // bare /psyche opens the panel, so browse-and-leave
                            // must be as quiet as never having opened it. (A
                            // lone :w still reports its save above.)
                            if applied {
                                if let Some(action) = persona_action {
                                    let persona_command = match action {
                                        PersonaAction::Keep => None,
                                        PersonaAction::Clear => Some("persona clear".to_string()),
                                        PersonaAction::Switch(name) => {
                                            Some(format!("persona set {name} --keep-context"))
                                        }
                                    };
                                    if let Some(cmd) = persona_command {
                                        let mut reset_ctx = ConversationResetContext {
                                            memory: &mut memory,
                                            system: &mut system,
                                            conversation_id: &mut active_conversation_id,
                                            mode_states: &conversation_mode_states,
                                        };
                                        let msg = match handle_persona_command(
                                            &cmd,
                                            workspace,
                                            &persona_store,
                                            &mut active_persona,
                                            &mut reset_ctx,
                                        ) {
                                            Ok(msg) => msg,
                                            Err(e) => format!("error: {e}"),
                                        };
                                        print_newt(&msg, color, verbose);
                                        let _ = apply_persona_backend(
                                            active_persona.as_ref(),
                                            &base_provider,
                                            &base_model,
                                            &cfg,
                                            &mut choice,
                                            &mut inf_url,
                                            &mut inf_model,
                                            &mut inf_kind,
                                            &mut inf_key,
                                            &mut inf_context_window,
                                            color,
                                            verbose,
                                        );
                                    }
                                }
                                // #1666: the model pick goes through the /model
                                // path — same #1122 served-validation gate, same
                                // #545 persistence rules, same Ollama warmup.
                                // AFTER the persona reroute deliberately: a visit
                                // that switched persona (new backend) AND picked
                                // a model validates the pick against the NEW
                                // backend — served → applies; not served → the
                                // gate refuses with its usual message.
                                if let Some(model) = &chosen_model {
                                    commands::model::apply_model_choice(model, color, verbose);
                                    // The panel path `continue`s BEFORE the loop's
                                    // post-command refresh, so do here what the
                                    // /model slash path gets there: repoint the
                                    // session at the (possibly) new model and
                                    // capture the operator baseline (review P1#2)
                                    // a later `/persona clear` reverts to. The
                                    // URL cannot change on a model pick, so the
                                    // DGX re-probe is deliberately skipped.
                                    cfg = newt_core::Config::resolve().unwrap_or_default();
                                    let _ = refresh_backend(
                                        &cfg,
                                        &mut choice,
                                        &mut inf_url,
                                        &mut inf_model,
                                        &mut inf_kind,
                                        &mut inf_key,
                                        &mut inf_context_window,
                                        color,
                                        verbose,
                                    );
                                    base_model = std::env::var("NEWT_DGX_MODEL").ok();
                                }
                                // Recompute + report from FRESH runtime state (§2).
                                // #1139: one resolved snapshot is the single source
                                // for this apply line — the same render `/psyche` and
                                // `solve` read, not a re-derivation of each dial here.
                                let snap = newt_core::RuntimeSettingsSnapshot::resolve(
                                    &cfg,
                                    active_persona.as_ref().map(|p| p.name.as_str()),
                                    active_persona
                                        .as_ref()
                                        .and_then(|p| p.profile.backend.as_deref()),
                                );
                                print_newt(&snap.summary(), color, verbose);
                            }
                        }
                        #[cfg(not(feature = "rich-tui"))]
                        print_newt(
                            "the psyche panel needs the rich TUI build with an interactive \
                             terminal — use /psyche status for the text view, or /psyche \
                             cognition / /psyche tenacity <level> to change the dials.",
                            color,
                            verbose,
                        );
                        surface.save_history();
                        println!();
                        continue;
                    }
                    let cont = dispatch_slash(
                        &task,
                        workspace,
                        color,
                        verbose,
                        markdown_enabled(&cfg, color, markdown_override),
                    )?;
                    surface.save_history();
                    // Skip config reload and terminal reinit when exiting — unnecessary
                    // work that can hang if the terminal is in a degraded state.
                    if !cont {
                        clean_exit = true;
                        break;
                    }
                    // Re-read config after a slash command (config.toml may have changed).
                    // This is the ONE intentional refresh — re-resolve `cfg` so the
                    // session picks up edits, then derive everything from it.
                    // Permissions can only NARROW within a session; a widening
                    // request is clamped (restart to widen — see SessionCapability).
                    cfg = newt_core::Config::resolve().unwrap_or_default();
                    // Ephemeral is a session-wide decision (17.7): a config
                    // refresh never re-grows a store handle mid-session.
                    if !ephemeral_session {
                        conversation_store = Some(conversation_store_for(workspace, &cfg)?);
                    }
                    let url_changed = refresh_backend(
                        &cfg,
                        &mut choice,
                        &mut inf_url,
                        &mut inf_model,
                        &mut inf_kind,
                        &mut inf_key,
                        &mut inf_context_window,
                        color,
                        verbose,
                    );
                    // Re-probe DCGM ONLY when the backend URL actually changed
                    // (and only in verbose mode, where the snapshot is shown).
                    // `try_connect` is a blocking ~3s network call (issue #412);
                    // a `/vi`/`/emacs` toggle never changes the URL. Dropping the
                    // old receiver stops the previous background sampler (#414).
                    if url_changed {
                        dgx_rx = if verbose {
                            dgx_probe::DgxTelemetry::try_connect(&inf_url)
                                .map(|d| d.into_sampler(2))
                        } else {
                            None
                        };
                    }
                    // Review P1#2: an operator backend command (/backends, /model,
                    // /backend) just wrote its explicit choice to the env — capture
                    // it as the baseline a later `/persona clear` reverts to. A
                    // persona's own routing runs in a separate branch that never
                    // reaches here, so it can never pollute the operator baseline.
                    if is_operator_backend_command(slash_body) {
                        base_provider = std::env::var("NEWT_PROVIDER").ok();
                        base_model = std::env::var("NEWT_DGX_MODEL").ok();
                    }
                    if cap.reapply(resolve_tui(&cfg), workspace) {
                        print_newt(
                            "permissions can only narrow within a session — restart newt to widen",
                            color,
                            verbose,
                        );
                    }
                    // A `/vi` / `/emacs` switch set NEWT_EDIT_MODE; rebuild the
                    // surface from fresh config so the next read uses the new
                    // mode, then keep is_vi in sync for the next prompt.
                    surface.reload()?;
                    is_vi = resolve_edit_mode() == newt_core::EditMode::Vi;
                } else if model_input_origin.is_operator()
                    && matches!(task.as_str(), "exit" | "quit")
                {
                    clean_exit = true;
                    break;
                } else {
                    // Durable ingress is the FIRST operation in the final
                    // model-input branch. It precedes hardware probes,
                    // retrieval, inference, and every tool-capable path. Raw
                    // bytes are the accepted surface line; model bytes are the
                    // exact normalized `task` sent below.
                    match begin_model_prompt(
                        PromptIngress {
                            durable: conversation_store.as_ref(),
                            ephemeral: &ephemeral_prompt_store,
                        },
                        &active_conversation_id,
                        &conversation_title_from_task(&task),
                        active_persona.as_ref().map(|p| p.name.as_str()),
                        line.as_bytes(),
                        task.as_bytes(),
                        &model_input_origin,
                    ) {
                        Ok(context) => {
                            // A3/W6: record the durable turn a web-injected prompt
                            // became — the additive, auditable "entered via web"
                            // proof (the receipt itself stays origin=operator, so
                            // no CHECK migration). Best-effort: a link failure must
                            // never abort a turn that was already durably minted.
                            if let (ModelInputOrigin::WebInjected { inbox_id }, Some(store)) =
                                (&model_input_origin, conversation_store.as_ref())
                            {
                                let _ = store.link_inbox_delivery(
                                    inbox_id,
                                    &context.submitted_prompt().id().to_string(),
                                );
                            }
                            active_prompt_context = Some(context);
                        }
                        Err(e) => {
                            active_prompt_context = None;
                            print_newt(
                                &format!("prompt not sent: prompt receipt creation failed ({e})"),
                                color,
                                verbose,
                            );
                            println!();
                            continue;
                        }
                    }

                    // Prompt comprehension is deliberately after receipt
                    // creation (so every manifest has a durable origin) and
                    // before hardware discovery, retrieval, inference, or any
                    // tool-capable path. A direct answer is resolved against
                    // the pending manifest, not reclassified in isolation.
                    let is_clarification_answer = matches!(
                        &model_input_origin,
                        ModelInputOrigin::OperatorContinuation { .. }
                    );
                    // #1260: disposition inference honors the operator's
                    // `[intake]` lexicon overrides (built-in defaults when
                    // unset) — the keyword lists and `?`-fallback are config,
                    // not code.
                    let intake_lexicon = cfg
                        .intake
                        .as_ref()
                        .map(newt_core::IntakeConfig::to_lexicon)
                        .unwrap_or_default();
                    let mut prompt_intake = if is_clarification_answer {
                        pending_clarification
                            .as_ref()
                            .map(|pending| pending.intake.resolve_with_operator_answer(&task))
                            .unwrap_or_else(|| {
                                newt_core::agentic::PromptIntake::analyze_with(
                                    &task,
                                    &intake_lexicon,
                                )
                            })
                    } else {
                        newt_core::agentic::PromptIntake::analyze_with(&task, &intake_lexicon)
                    };
                    // A model-selected Auto style is a one-shot instruction
                    // for the next action-shaped turn. Protected intake does
                    // not consume it; it remains pending until an Act turn or
                    // an explicit conversation/mode boundary clears it.
                    let plan_mode_active = conversation_mode_states.plan.is_active();
                    let auto_selected = (active_operating_mode == OperatingMode::Auto
                        && !plan_mode_active
                        && prompt_intake.disposition()
                            == newt_core::agentic::PromptDisposition::Act)
                        .then(|| {
                            conversation_mode_states
                                .auto
                                .take_for(&active_conversation_id)
                        })
                        .flatten();
                    let turn_operating_mode = effective_operating_mode(
                        active_operating_mode,
                        &prompt_intake,
                        plan_mode_active,
                        auto_selected,
                    );
                    apply_operating_mode_to_intake(turn_operating_mode, &mut prompt_intake);

                    // The manifest artifact deliberately contains only bounded
                    // counts and digests. It is written before an Ask handoff
                    // and before Act inference so a later compacted or resumed
                    // session can audit why the harness chose its disposition.
                    {
                        let durable_artifact_store_owner =
                            conversation_store.as_ref().map(|store| {
                                newt_core::agentic::StoreArtifactStore::new(
                                    store,
                                    active_conversation_id.clone(),
                                )
                            });
                        let artifact_source: Option<&dyn newt_core::agentic::ArtifactSource> =
                            durable_artifact_store_owner
                                .as_ref()
                                .map(|store| store as &dyn newt_core::agentic::ArtifactSource)
                                .or_else(|| {
                                    ephemeral_artifact_store.as_ref().map(|store| {
                                        store as &dyn newt_core::agentic::ArtifactSource
                                    })
                                });
                        let artifact_sink: Option<&dyn newt_core::agentic::PromptArtifactSink> =
                            durable_artifact_store_owner
                                .as_ref()
                                .map(|store| store as &dyn newt_core::agentic::PromptArtifactSink)
                                .or_else(|| {
                                    ephemeral_artifact_store.as_ref().map(|store| {
                                        store as &dyn newt_core::agentic::PromptArtifactSink
                                    })
                                });
                        if let (Some(turn), Some(sink)) =
                            (active_prompt_context.as_ref(), artifact_sink)
                        {
                            let context = newt_core::agentic::ArtifactReadContext::from_turn(
                                turn,
                                artifact_source,
                            );
                            if let Err(e) = newt_core::agentic::record_prompt_comprehension_manifest(
                                sink,
                                context,
                                &prompt_intake,
                            ) {
                                print_newt(
                                    &format!(
                                        "warning: could not record prompt-comprehension manifest: {e}"
                                    ),
                                    color,
                                    verbose,
                                );
                            }
                        }
                    }

                    if prompt_intake.disposition() == newt_core::agentic::PromptDisposition::Ask {
                        let Some(parent) = active_prompt_context.clone() else {
                            print_newt(
                                "prompt comprehension could not preserve the accepted receipt; no clarification was queued",
                                color,
                                verbose,
                            );
                            println!();
                            continue;
                        };
                        let clarification = prompt_intake.clarification_batch();
                        pending_clarification = Some(PendingClarification {
                            parent: Box::new(parent),
                            intake: prompt_intake,
                        });
                        print_newt(&clarification, color, verbose);
                        println!();
                        continue;
                    }

                    // The successor receipt and its resolved manifest are now
                    // durable; it is safe to forget the session-only pending
                    // handoff. A still-unresolved answer takes the Ask branch
                    // above and replaces this state with a new parent.
                    if is_clarification_answer {
                        pending_clarification = None;
                    }

                    // Pre-turn hardware snapshot: read the latest value the
                    // background sampler published (instant, never blocks). None
                    // unless verbose + a reachable DCGM (issue #414).
                    let hw_before = dgx_rx.as_ref().map(|rx| rx.borrow().clone());
                    if verbose {
                        if let Some(ref snap) = hw_before {
                            if snap.has_data() {
                                print_newt(&format!("hw: {}", snap.summary()), color, verbose);
                            }
                        }
                    }

                    print_thinking(color);
                    let t0 = std::time::Instant::now();

                    // The active route may have changed since the previous turn
                    // (`/model`, `/backend`, a persona switch — all of which
                    // update `choice` + `inf_model` before `continue`ing). Re-derive
                    // the canonical capability identity here, at the head of the
                    // inference path, so every empirical lookup, observation, and
                    // rebudget below keys the CURRENT serving principal — never the
                    // previous model's evidence, and never poisoning a sibling
                    // instance that happens to share a model name.
                    cap_id = session_cap_id(choice.serving, &choice.name, &inf_model);

                    // Per-model tuning: explicit config overrides global defaults.
                    let model_tune = cfg.find_model_tuning(&inf_model);
                    let configured_max_tool_rounds = model_tune
                        .and_then(|t| t.max_tool_rounds)
                        .unwrap_or_else(|| max_tool_rounds(&cfg));
                    let eff_max_tool_rounds = effective_tool_round_limit(
                        configured_max_tool_rounds,
                        max_tool_rounds_override,
                    );
                    let eff_workflow_grace_rounds = model_tune
                        .and_then(|t| t.workflow_grace_rounds)
                        .unwrap_or_else(|| workflow_grace_rounds(&cfg));
                    // #1162: the operator's live nudge dial (/nudge off|on).
                    let nudges_off =
                        std::env::var("NEWT_NUDGE").is_ok_and(|v| v.eq_ignore_ascii_case("off"));
                    let eff_narration_nudge_cap = model_tune
                        .and_then(|t| t.narration_nudge_cap)
                        .unwrap_or_else(|| narration_nudge_cap(&cfg));
                    let eff_mid_loop_trim = model_tune
                        .and_then(|t| t.mid_loop_trim_threshold)
                        .unwrap_or_else(|| mid_loop_trim_threshold(&cfg))
                        .min(eff_max_tool_rounds.saturating_sub(3));
                    // Token-based trim trigger (issue #223): per-model override, else
                    // the global `[tui].mid_loop_trim_tokens`. None OR zero disables
                    // (the zero-is-noop contract, F3).
                    let eff_mid_loop_trim_tokens = effective_mid_loop_trim_tokens(
                        model_tune.and_then(|t| t.mid_loop_trim_tokens),
                        cfg.tui.as_ref().and_then(|t| t.mid_loop_trim_tokens),
                    );
                    let eff_compaction_trigger_policy =
                        compaction_trigger_policy(&cfg, compaction_trigger_policy_override);
                    let eff_input_ceiling_pct = newt_core::config::normalize_input_ceiling_pct(
                        cfg.context
                            .as_ref()
                            .map(|c| c.input_ceiling_pct)
                            .unwrap_or(80),
                    );

                    // Lazy context-window discovery: /api/show is attempted at
                    // most ONCE per model per session — even when the fetch
                    // fails or the endpoint reports no context length, the
                    // `ctx_window_probed` negative cache prevents the
                    // every-turn refetch (Phase 20; `ensure_context_window`
                    // alone only early-outs on success). Also reads the
                    // empirically-confirmed max input (max_ok_input) used as
                    // the pre-send budget gate (issue #223) and the learned
                    // estimate-calibration ratio (Phase 20 §2.3).
                    let (
                        eff_context_window,
                        eff_safe_context,
                        eff_max_ok_input,
                        eff_estimate_ratio,
                        eff_recovered_hard_window,
                    ) = {
                        let entry = cap_cache.entry(cap_id.clone()).or_default();
                        // #1199: the server-declared window from session-start
                        // adopt (`inf_context_window`) is authoritative and
                        // cache-independent. Only fall back to the cache-side
                        // probe (`ensure_context_window`) when adopt got NONE —
                        // e.g. an authed gateway adopt couldn't reach — so a
                        // stale cached None can never starve a discovered
                        // window. The cache still holds the LEARNED facts
                        // (max_ok_input, estimate_ratio).
                        let updated = inf_context_window.is_none()
                            && ctx_window_probed.insert(cap_id.clone())
                            && probe::ensure_context_window(
                                entry,
                                &inf_url,
                                &inf_model,
                                !real_context_discovery(&cfg, &inf_model),
                                inf_kind,
                            );
                        let cached_sc = entry.safe_context;
                        let cached_window = entry.context_window;
                        let cached_hard_window = entry.hard_context_window;
                        let moi = entry.max_ok_input;
                        let ratio = entry.estimate_ratio;
                        if updated {
                            probe::save_cache(&cap_cache);
                        }
                        // Keep the full window separate from the derived input
                        // cap. Chat Completions needs the former to reserve its
                        // active maximum output; Ollama still uses the latter
                        // as its conservative KV-allocation fallback.
                        let requested_full_window = selected_model_context_window(
                            inf_context_window.or(cached_window),
                            model_tune.and_then(|t| t.context_window),
                            community_tunings
                                .find(&inf_model)
                                .and_then(|profile| profile.context_window),
                        );
                        let recovered_hard_window = cap_context_window_by_recovery(
                            recovered_context_windows.get(&cap_id).copied(),
                            cached_hard_window,
                        );
                        let full_window = cap_context_window_by_recovery(
                            requested_full_window,
                            recovered_hard_window,
                        );
                        let sc = if inf_kind == newt_core::BackendKind::Openai {
                            full_window
                                .map(|window| {
                                    newt_core::config::input_percentage_ceiling(
                                        window,
                                        eff_input_ceiling_pct,
                                    )
                                })
                                .or(cached_sc)
                        } else {
                            recovered_hard_window
                                .map(|window| {
                                    newt_core::config::input_percentage_ceiling(
                                        window,
                                        eff_input_ceiling_pct,
                                    )
                                })
                                .or_else(|| inf_context_window.map(|w| w * 80 / 100))
                                .or(cached_sc)
                                .or_else(|| model_tune.and_then(|t| t.context_window))
                        };
                        (full_window, sc, moi, ratio, recovered_hard_window)
                    };

                    // Apply the `/context size <N>` session override: it caps
                    // both the safe-context budget and the max-ok-input guard to
                    // the user's chosen ceiling. A raise past the probed value is
                    // honored too — the user is explicitly opting into a larger
                    // send window for experimentation.
                    let (eff_safe_context, eff_max_ok_input) = match context_size_override {
                        Some(n) => (Some(n), Some(n)),
                        None => (eff_safe_context, eff_max_ok_input),
                    };

                    // Memory providers keep their history but follow the
                    // currently selected model's budget. Rebinding every turn
                    // covers `/model`, `/backend`, and persona-driven routes,
                    // including switches that retain conversation context.
                    let active_memory_budget = probe::resolve_memory_budget(
                        mem_cfg.context_tokens,
                        eff_context_window,
                        cap_cache.get(&cap_id),
                    );
                    memory.set_context_tokens(active_memory_budget);
                    // A SESSION-INHERITING summarizer must follow the switch too —
                    // otherwise the history provider says "backend B, smaller
                    // window" while the embedded summarizer still targets backend A
                    // (the split-brain #1647 left open). Rebuild it from the CURRENT
                    // route, but only when that route actually changed, and only
                    // when it is not pinned. `set_summarizer` builds at most once
                    // and only if a Summarizing provider is present.
                    if summarizer_follows_route {
                        let route = (inf_url.clone(), inf_model.clone(), inf_kind);
                        if route != last_summarizer_route {
                            memory.set_summarizer(|| {
                                build_session_summarizer(
                                    &sum_cfg,
                                    &cfg,
                                    &inf_url,
                                    &inf_model,
                                    inf_kind,
                                    &inf_key,
                                    Some(active_memory_budget),
                                    color,
                                )
                            });
                            last_summarizer_route = route;
                        }
                    }

                    // Context-window resolution: explicit num_ctx first. For
                    // OpenAI, hand core the full window so its input percentage
                    // and output reserve apply exactly once. For Ollama, retain
                    // the safe-context fallback that caps KV allocation.
                    let requested_num_ctx = model_tune
                        .and_then(|t| t.num_ctx)
                        .or_else(|| num_ctx(&cfg))
                        .or_else(|| {
                            context_window_for_core(inf_kind, eff_context_window, eff_safe_context)
                        });
                    let eff_num_ctx = cap_context_window_by_recovery(
                        requested_num_ctx,
                        eff_recovered_hard_window,
                    );

                    // Build message list from memory manager. A fresh runtime
                    // block is prepended to the (frozen) system prompt EACH turn
                    // so the model can actually see its own name, the harness,
                    // the backend, and the current time — env-vars the agent
                    // would otherwise hallucinate (issue: model confabulated an
                    // identity for commit attribution). build_messages only uses
                    // the system string to fill message[0], so per-turn variation
                    // is safe.
                    // Step 26.3/26.4: resolve the per-turn feature set once (used
                    // for the <state> injection here and the ChatCtx fields below).
                    let turn_disposition = prompt_intake.disposition();
                    let turn_features = context_features(
                        &cfg,
                        context_manager(&cfg, context_manager_override),
                        &context_features_override,
                        inf_kind,
                    );
                    let tool_offload_on = turn_features.tool_offload;
                    let scratchpad_on = turn_features.scratchpad;
                    let semantic_on = turn_features.semantic;
                    let session_controls = session_control_prompt(
                        active_operating_mode,
                        turn_operating_mode,
                        active_posture.as_ref(),
                    );
                    let mut turn_system = format!(
                        "{}\n\n{}\n\n{system}\n\n{session_controls}",
                        workspace_state_block(workspace),
                        runtime_context_block(&inf_model, &inf_url, inf_kind, &session_identity)
                    );
                    if is_clarification_answer {
                        turn_system = format!(
                            "<clarification_context>\n\
                             This accepted operator line is a clarification continuation. The protected prompt card names its objective root; use prompt_read {{\"address\":\"root\"}} to re-read the original objective before acting whenever the answer alone is insufficient.\n\
                             </clarification_context>\n\n{turn_system}"
                        );
                    }
                    // Step 26.4 (#583): inject the <state> block at the HEAD of the
                    // turn — it rides the ephemeral message[0] (regenerated each
                    // turn from turn_system) and is NEVER persisted to the log.
                    if scratchpad_on {
                        if let Some(block) =
                            newt_core::agentic::scratchpad_state_block(&scratchpad_store)
                        {
                            turn_system = format!("{block}\n\n{turn_system}");
                        }
                    }
                    // Step 27.4: nudge a weak local model to actually USE the
                    // cross-round working-memory tools when they're on, so it
                    // keeps a checklist/state instead of re-deriving everything
                    // each round. Ephemeral (rides turn_system), never persisted.
                    if turn_features.scheduled || scratchpad_on {
                        let mut hints: Vec<&str> = Vec::new();
                        if turn_features.scheduled {
                            hints.push(
                                "For multi-step, ambiguous, resumed, or context-compacted work, \
                                 prefer calling update_plan first with a short 2-6 step ordered \
                                 plan (each step's status pending/in_progress/completed) before \
                                 more investigation. Re-send it with the finished step marked \
                                 completed as you go. If plan_get says no active plan, create one \
                                 with update_plan instead of polling plan_get again.",
                            );
                        }
                        if scratchpad_on {
                            hints.push(
                                "Record durable facts (paths, decisions) with state_set so they \
                                 survive context compaction; read them back with state_get.",
                            );
                        }
                        turn_system = format!("{}\n\n{turn_system}", hints.join(" "));
                    }
                    // Step 26.5.4 (#582): semantic RAG — index the repo's code once
                    // (lazily, on the first active turn), then inject a
                    // <code_evidence> block at the turn head (also ephemeral, never
                    // persisted). An absent embedding model degrades to a no-op.
                    // Step 26.5: build the embedder once when semantic is on — it
                    // serves the turn-head indexing/injection (26.5.4) AND the
                    // code_search tool's ChatCtx searcher (26.5.5), so it must
                    // outlive the ChatCtx below.
                    let mut semantic_cfg = cfg
                        .context
                        .as_ref()
                        .map(|c| c.semantic.clone())
                        .unwrap_or_default();
                    // #1279: with no explicit embedding_model_path, adopt the
                    // pulled default model (`newt models pull-embed`) if present,
                    // so on-host semantic retrieval works with zero config. The
                    // fs presence check lives here; the precedence is pure.
                    semantic_cfg.embedding_model_path = effective_embedding_model_path(
                        semantic_cfg.embedding_model_path.take(),
                        newt_inference::palette::embed_model_dir_if_present(),
                    );
                    // #720: the embedder is a `Box<dyn Embedder>` so it can be
                    // EITHER the HTTP `EmbeddingsClient` OR the in-process candle
                    // embedder (when `embeddings_api = "embedded"`) — the latter
                    // computes embeddings locally so retrieval never touches the
                    // DGX chat model's VRAM. The selection is a pure helper.
                    let semantic_embedder: Option<std::sync::Arc<dyn newt_core::Embedder>> =
                        if semantic_on {
                            if semantic_embedder_unavailable_reason(&semantic_cfg).is_some() {
                                None
                            } else {
                                Some(std::sync::Arc::from(build_semantic_embedder(
                                    &semantic_cfg,
                                    &inf_url,
                                    inf_kind,
                                    inf_key.as_deref(),
                                )))
                            }
                        } else {
                            None
                        };
                    // Iteration #4: surface a finished background embed once.
                    if let Some(n) = poll_semantic_indexing(&rt, &mut semantic_warmup) {
                        if n == 0 {
                            print_harness_notice(&semantic_zero_index_hint(&semantic_cfg), color);
                        } else {
                            print_newt(
                                &format!("semantic: indexed {n} code chunks (background)"),
                                color,
                                verbose,
                            );
                        }
                    }
                    if let Some(embedder) = semantic_embedder.as_ref() {
                        if !semantic_indexed {
                            // Attempt indexing ONCE per session (reset on /new),
                            // whether or not it yields chunks — so a missing
                            // embedding model doesn't re-walk + re-embed every turn.
                            semantic_indexed = true;
                            // Use the harness-owned source registry rather than a
                            // second rs/py list: prompt steering, exact inventory,
                            // semantic retrieval, and structural navigation now
                            // agree on what "code" means. Gather caps still bound
                            // the embedding work. #1387 keeps the manifest so
                            // completeness / index_id are honest.
                            let source_extensions = resolved_source_extensions(workspace, &cfg);
                            let (files, manifest) = newt_core::gather_with_manifest(
                                workspace,
                                &source_extensions,
                                newt_core::GatherCaps::default(),
                            );
                            let (git_head, dirty) = lightweight_git_meta(workspace);
                            index_status.generation = index_status.generation.saturating_add(1);
                            index_status.manifest = Some(manifest);
                            index_status.git_head = git_head;
                            index_status.dirty = dirty;
                            if !files.is_empty() {
                                print_newt(
                                    &format!(
                                        "embedding {} files for semantic retrieval in the \
                                         background — retrieval rides the lexical floor \
                                         until it finishes",
                                        files.len()
                                    ),
                                    color,
                                    verbose,
                                );
                                semantic_warmup = Some(spawn_semantic_indexing(
                                    &rt,
                                    files,
                                    std::sync::Arc::clone(embedder),
                                    std::sync::Arc::clone(&semantic_index),
                                    semantic_cfg.on_embed_failure,
                                ));
                            }
                        }
                        if let Some(result) = tokio::task::block_in_place(|| {
                            rt.block_on(newt_core::retrieve_ranked(
                                &task,
                                embedder.as_ref(),
                                semantic_index.as_ref(),
                                semantic_cfg.top_k,
                                Some(&retrieval_steer),
                                Some(&index_status),
                            ))
                        }) {
                            if let Some(block) = newt_core::render_code_evidence(&result) {
                                nav_session.turn_counter =
                                    nav_session.turn_counter.saturating_add(1);
                                let pins: Vec<_> =
                                    retrieval_steer.pinned.iter().map(|h| h.loc_key()).collect();
                                let ctx_hash = newt_core::hash_context(block.as_bytes());
                                nav_session.ledger.record_semantic(
                                    nav_session.turn_counter,
                                    &task,
                                    &result,
                                    &pins,
                                    &retrieval_steer.excluded_paths,
                                    &ctx_hash,
                                );
                                nav_session.last_semantic = Some(result);
                                turn_system = format!("{block}\n\n{turn_system}");
                            }
                        }
                    }
                    // Step 26.6a (#585): inject the <experience> block (relevant
                    // past lessons for this task) at the turn head — ephemeral
                    // message[0], never persisted (like <state> / <code_evidence>).
                    let experiential_on = turn_features.experiential;
                    if experiential_on {
                        if let Some(block) = newt_core::experience_block(
                            &experience_store,
                            &task,
                            newt_core::EXPERIENCE_TOP_K,
                        ) {
                            turn_system = format!("{block}\n\n{turn_system}");
                        }
                    }
                    // Step 26.6b (#586): inject the compiled <plan> checklist at the
                    // turn head — ephemeral message[0], never persisted (like the
                    // other feature blocks).
                    let scheduled_on = turn_features.scheduled;
                    if scheduled_on {
                        if let Some(block) = newt_core::plan_block(&step_ledger) {
                            turn_system = format!("{block}\n\n{turn_system}");
                        }
                    }
                    let messages = memory.build_messages(&turn_system, &task);
                    // The save_note sink borrows the manager for this call
                    // only; `/remember` and `save_note` share its NoteStore
                    // (one write path, one scan, one cap). Step 19.3, #248.
                    let mut note_sink = ManagerNoteSink {
                        memory: &mut memory,
                    };
                    // Cross-session recall source (Step 17.5, #246): the
                    // model's `recall` tool searches this workspace's PAST
                    // conversations through the same store `/recall` reads —
                    // minus the conversation we're in (that's what context
                    // is for). `None` in an ephemeral session (17.7): no
                    // store handle means no reads either, so ambient
                    // conversations can never leak into an ephemeral run.
                    let recall_source = conversation_store.as_ref().map(|store| {
                        newt_core::StoreRecallSource::new(store, &active_conversation_id)
                    });
                    // One conversation-fenced prompt resolver serves both the
                    // always-on `prompt_read` tool and, when progressive
                    // disclosure is enabled, the compatibility
                    // `memory_fetch prompt:<uuid>` route.
                    let durable_prompt_source_owner = conversation_store.as_ref().map(|store| {
                        newt_core::agentic::StorePromptSource::new(store, &active_conversation_id)
                    });
                    let ephemeral_prompt_source_owner = conversation_store
                        .is_none()
                        .then(|| ephemeral_prompt_store.source(active_conversation_id.clone()));
                    let prompt_source: Option<&dyn newt_core::agentic::PromptSource> = match (
                        durable_prompt_source_owner.as_ref(),
                        ephemeral_prompt_source_owner.as_ref(),
                    ) {
                        (Some(source), _) => Some(source),
                        (None, Some(source)) => Some(source),
                        (None, None) => None,
                    };
                    // One conversation-fenced artifact adapter serves both the
                    // model-facing artifact_read tool and all lifecycle writers
                    // for this turn. Persistent mode writes through SQLite;
                    // ephemeral mode uses the session-local hash-chained ledger.
                    let durable_artifact_store_owner = conversation_store.as_ref().map(|store| {
                        newt_core::agentic::StoreArtifactStore::new(
                            store,
                            active_conversation_id.clone(),
                        )
                    });
                    let artifact_source: Option<&dyn newt_core::agentic::ArtifactSource> = match (
                        durable_artifact_store_owner.as_ref(),
                        ephemeral_artifact_store.as_ref(),
                    ) {
                        (Some(store), _) => Some(store),
                        (None, Some(store)) => Some(store),
                        (None, None) => None,
                    };
                    let artifact_sink: Option<&dyn newt_core::agentic::PromptArtifactSink> = match (
                        durable_artifact_store_owner.as_ref(),
                        ephemeral_artifact_store.as_ref(),
                    ) {
                        (Some(store), _) => Some(store),
                        (None, Some(store)) => Some(store),
                        (None, None) => None,
                    };
                    // Progressive-disclosure memory (Workstream A MVP, #319):
                    // wired ONLY under `[memory] disclosure = "index"`. Default
                    // (`frozen`) leaves `memory_source: None` so the loop is
                    // bit-for-bit unchanged — the `memory_fetch` tool is never
                    // advertised. The source reads `note:` bodies from an
                    // independent read-only NoteStore over the same NOTES file
                    // the MemoryManager froze (the `note_sink` holds the only
                    // &mut to the manager), and `turn:` bodies from the session
                    // ConversationStore (workspace-fenced). Both surfaces
                    // already exist — no new persistence.
                    let memory_disclosure_index = cfg
                        .memory
                        .as_ref()
                        .map(|m| m.disclosure == newt_core::MemoryDisclosure::Index)
                        .unwrap_or(false);
                    let mem_fetch_notes = if memory_disclosure_index {
                        use newt_core::MemoryProvider as _;
                        let mut ns = newt_core::NoteStore::default_path();
                        let _ = rt.block_on(ns.initialize(&newt_core::SessionContext {
                            workspace: workspace.to_string(),
                            session_id: active_conversation_id.clone(),
                        }));
                        Some(ns)
                    } else {
                        None
                    };
                    let memory_source =
                        match (mem_fetch_notes.as_ref(), conversation_store.as_ref()) {
                            (Some(notes), Some(store)) => {
                                // Step 26.3 (#584): attach the spill store so the
                                // model can re-read offloaded payloads via `spill:`.
                                let source = newt_core::StoreMemorySource::new(notes, store)
                                    .with_spill_store(&spill_store)
                                    .with_compaction_store(&compaction_store);
                                Some(match prompt_source {
                                    Some(prompt) => source.with_prompt_source(prompt),
                                    None => source,
                                })
                            }
                            _ => None,
                        };
                    // Compression summarizer (Step 18.4, #247): rebuilt per
                    // turn so a mid-session `/backend` or model switch takes
                    // effect immediately.
                    // The same effective context cap the main loop sends — the
                    // summary request must not be silently truncated at Ollama's
                    // default window (F5).
                    let loop_summarizer = build_session_summarizer(
                        &sum_cfg,
                        &cfg,
                        &inf_url,
                        &inf_model,
                        inf_kind,
                        &inf_key,
                        eff_num_ctx,
                        color,
                    );
                    // Per-turn tool-event recorder (Step 17.6, #246): the
                    // loop pushes one event per tool call; the save site
                    // persists them into the turn's `events` column.
                    let mut turn_tool_events: Vec<newt_core::ToolEvent> = Vec::new();
                    // Per-turn phantom-reach recorder (#717): sibling to
                    // `turn_tool_events`; the loop pushes one record per phantom
                    // tool/capability reach; the save site persists them into the
                    // turn's `phantom_reaches` column.
                    let mut turn_phantom_reaches: Vec<newt_core::PhantomReach> = Vec::new();
                    let mut turn_end_reason: Option<newt_core::TurnEndReason> = None;
                    // #307: the EFFECTIVE caveats for this turn — the session
                    // base intersected with the active posture's preset clamp
                    // (a FLOOR). This single `meet` is what the gate base, the
                    // ChatCtx dispatch, and (via the preset clamp + exec_floor)
                    // the --disable-ocap bypass all enforce, so authority can
                    // never exceed the preset. With no posture it is the base
                    // unchanged. Computed once so all three consult one value.
                    let mut turn_caveats = meet_persona_caveats(
                        effective_caveats(cap.caveats(), active_posture.as_ref()),
                        active_persona.as_ref(),
                    );
                    // The effective turn mode includes both `/mode` and any
                    // model-entered legacy plan phase, so one clamp keeps the
                    // prompt card, catalog, and authority boundary aligned.
                    turn_caveats = operating_mode_caveats(turn_operating_mode, turn_caveats);
                    // FR-1 part 2 (#997): the active persona's tool allow-list
                    // (its `tools:` front-matter). Threaded into `ChatCtx` so the
                    // loop advertises ONLY these tools and the executor refuses
                    // the rest — the name-scoped complement to `turn_caveats`
                    // (the axis-scoped clamp above, part 1 / #1002). `None` (no
                    // persona, or a persona with no `tools:` list) leaves the
                    // full catalog in play.
                    let persona_tools = active_persona
                        .as_ref()
                        .and_then(|p| p.profile.tools.as_deref());
                    // Psyche: the turn's cognition → `reasoning.effort` (via
                    // ChatCtx.cognition). Effective precedence: a live `/cognition`
                    // override wins; else the active persona's declared cognition
                    // (installed as PERSONA_COGNITION on activation); else `None`.
                    let cognition = newt_core::cognition::effective_cognition();
                    // Cognition always rides the Responses wire and may also
                    // project to Chat Completions when the endpoint explicitly
                    // advertises that extension. Otherwise say so once — never
                    // silently accept and ignore a live dial.
                    if cognition.is_some() && !cognition_scope_noted {
                        let responses = std::env::var("NEWT_OPENAI_API")
                            .is_ok_and(|v| v.eq_ignore_ascii_case("responses"));
                        let capable_chat = choice.kind == newt_core::BackendKind::Openai
                            && choice.chat_completions_capability.cognition == Some(true);
                        if !responses && !capable_chat {
                            print_newt(
                                "note: the active backend does not advertise a cognition generation policy — cognition is ignored.",
                                color,
                                verbose,
                            );
                            cognition_scope_noted = true;
                        }
                    }
                    // The active posture's optional clamp is threaded to the
                    // gate (re-clamps any session grant). A skill/framing-only
                    // compatibility binding is genuinely `None` here.
                    let preset_clamp = active_posture
                        .as_ref()
                        .and_then(ActivePosture::permission_clamp)
                        .cloned();
                    // #774 (P0): the exec FLOOR threaded to the bypass is the
                    // operator's `[tui.permissions]` exec clamp — a NON-OPTIONAL
                    // floor enforced even with no active `/posture`.
                    // `turn_caveats.exec` is the base clamp already met with the
                    // posture preset (meet-only), so `/posture` only tightens
                    // it when a permission floor is actually configured.
                    let exec_floor = exec_floor_from(&turn_caveats.exec, preset_clamp.is_some());
                    let turn_cancel = std::sync::atomic::AtomicBool::new(false);
                    let turn_exit = std::sync::atomic::AtomicBool::new(false);
                    let turn_hard = std::sync::atomic::AtomicBool::new(false);
                    // Build the gate whenever the session has a usable TTY — NOT
                    // only when authorization prompting is on. `ask_question`
                    // (request_user_input) needs a present operator; permission
                    // prompting is a separate policy carried as
                    // `authorization_prompts_enabled`. A truly headless session
                    // (non-interactive) still gets `None` and the honest headless
                    // response.
                    let mut permission_gate = interactive.then(|| PromptPermissionGate {
                        state: &mut permission_state,
                        base: turn_caveats.clone(),
                        key_path: key_path.clone(),
                        conversation_id: active_conversation_id.clone(),
                        log_path: permission_log_path.clone(),
                        denials_path: permission_denials_path.clone(),
                        config_path: permission_config_path.clone(),
                        preset_clamp: preset_clamp.clone(),
                        danger: production_danger_table(),
                        color,
                        verbose,
                        authorization_prompts_enabled: prompt_permissions_enabled,
                        web_decision_timeout: std::time::Duration::from_secs(4 * 60),
                        cancel: Some(&turn_cancel),
                        exit: Some(&turn_exit),
                        ask_human: prompt_permission_choice
                            as fn(
                                &newt_core::tty::PromptWindow,
                                &newt_core::Question<PromptChoice>,
                            ) -> PromptChoice,
                    });
                    // Per-round observation hook (Phase 20,
                    // docs/design/model-self-tuning.md §2.2): evidence is
                    // applied to the capability cache and saved AT THE MOMENT
                    // OF OBSERVATION, so an accepted prompt survives a turn
                    // that later bails, errors, or hits the round cap — the
                    // motivating failure discarded a backend-accepted
                    // 8,734-token prompt because the only write-back lived in
                    // the Ok-arm epilogue below. `turn_saw_accepted` is a Cell
                    // so the epilogue can read it without contending with the
                    // closure's captures. Keep hard-400 discovery separate
                    // from the ordinary session probe: only the former may
                    // tighten an explicit per-turn window in the gauge.
                    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
                    let turn_saw_accepted = std::cell::Cell::new(false);
                    let recovered_context_window = std::cell::Cell::new(None);
                    let mut on_obs = |obs: newt_core::RoundObservation| {
                        if matches!(obs, newt_core::RoundObservation::Accepted { .. }) {
                            turn_saw_accepted.set(true);
                        }
                        // Inner block: the `entry` borrow must end before
                        // `save_cache` takes its shared borrow of the map.
                        let (dirty, persisted_hard_window) = {
                            let entry = cap_cache.entry(cap_id.clone()).or_default();
                            let dirty = probe::apply_observation_with_input_ceiling_pct(
                                entry,
                                &obs,
                                &today,
                                eff_input_ceiling_pct,
                            );
                            (dirty, entry.hard_context_window)
                        };
                        if matches!(obs, newt_core::RoundObservation::ContextWindow400 { .. }) {
                            let hard_window = persisted_hard_window
                                .expect("a numbered context-window observation persists its cap");
                            recovered_context_windows.insert(cap_id.clone(), hard_window);
                            inf_context_window = Some(hard_window);
                            recovered_context_window.set(Some(hard_window));
                        };
                        if dirty {
                            probe::save_cache(&cap_cache);
                        }
                    };
                    // Profile technique: retry (R2 action arm) — when the profile
                    // enables `retry`, lend the loop a per-turn write ledger so the
                    // file-write tools record newt's OWN writes; the post-turn gate
                    // then reverts exactly those files (and only those — a file newt
                    // did not write is never touched).
                    let retry_ledger =
                        active_profile
                            .as_ref()
                            .filter(|p| p.enables("retry"))
                            .map(|_| {
                                std::cell::RefCell::new(newt_core::verify_gate::WriteLedger::new())
                            });
                    let interruptible = io::stdin().is_terminal() && io::stdout().is_terminal();
                    // (tool_offload_on / scratchpad_on resolved at the turn head.)
                    // Prompt artifacts observe repository identity for every
                    // inference turn, independent of roadmap binding. This is
                    // a read-only before/after fact, not an authorship claim.
                    let artifact_head_before_turn =
                        git_head_snapshot(session_git_tool.as_ref(), &turn_caveats);
                    // #1062 auto-capture: snapshot HEAD before the turn so the
                    // after-turn hook can tell whether the model committed. Only
                    // when a roadmap is active (a possibly-bound turn) — otherwise
                    // skip the git read entirely.
                    let head_before_turn = active_roadmap_id
                        .as_ref()
                        .and_then(|_| git_head_short(workspace));
                    // #1235: publish the universal tool spill-view height for
                    // this turn (process-wide knob, output_budget precedent).
                    // #1640: on the LEAN surface the committed excerpt is the
                    // whole record (no viewport to recover hidden lines), so
                    // `committed_spill_lines` forces it unbounded there; only the
                    // RICH surface collapses + offers the interactive viewport.
                    let configured_spill_lines =
                        effective_spill_lines(spill_lines(&cfg), spill_lines_override);
                    // Review fix (#1663): the forced value applies to the
                    // COMMITTED record only. The LIVE in-progress viewport keeps
                    // the configured height on every surface — reusing the
                    // forced 0 for the live gate below silently killed the lean
                    // live viewport (#1235), which this PR must not change.
                    let committed_view =
                        committed_spill_lines(surface_is_rich, configured_spill_lines);
                    newt_core::set_spill_lines(committed_view);
                    #[cfg(feature = "live-spill")]
                    let live_spill = (configured_spill_lines > 0 && live_spill_capable())
                        .then_some(())
                        .and_then(|()| {
                            // #1410: `stdout` now returns the `Arc` itself —
                            // registration with the line arbiter needs
                            // `Arc<dyn Ephemeral>`, so the constructor owns the
                            // wrapping rather than leaving it to each caller.
                            crate::live_spill::LiveSpillRenderer::stdout(
                                configured_spill_lines,
                                color,
                            )
                        });
                    // #1640 Layer 1 + review fix (#1663): publish the
                    // committed-result mode AFTER the live renderer exists,
                    // because the collapse's whole justification is "the
                    // viewport + /spill vocabulary can recover the detail" —
                    // so the default engages ONLY when this turn actually has
                    // the completed-spill viewport (rich + live-spill built +
                    // renderer constructed). Rich-but-piped, or live-spill
                    // compiled out → no viewport → spilled results keep the
                    // excerpt. The /spill summary override still forces it.
                    #[cfg(all(feature = "rich-tui", feature = "live-spill"))]
                    let summary_recoverable = surface_is_rich && live_spill.is_some();
                    #[cfg(not(all(feature = "rich-tui", feature = "live-spill")))]
                    let summary_recoverable = false;
                    newt_core::set_spill_summary(crate::effective_spill_summary(
                        summary_recoverable,
                        spill_summary_override,
                    ));
                    #[cfg(feature = "live-spill")]
                    let spill_input = live_spill
                        .as_deref()
                        .map(|spill| spill as &dyn crate::SpillInput);
                    #[cfg(not(feature = "live-spill"))]
                    let spill_input: Option<&dyn crate::SpillInput> = None;
                    // #1303: the mouse tier is `mouse_capable()` (both TTYs +
                    // TERM + platform/feature) AND the `[tui] mouse_viewport`
                    // opt-in — a strict superset of the keyboard tier, read here
                    // mirroring `spill_lines(&cfg)`. Always false on the lean /
                    // non-interactive / opted-out path (Clause A / E).
                    #[cfg(feature = "live-spill")]
                    let mouse_tier = mouse_capable(mouse_viewport(&cfg));
                    #[cfg(not(feature = "live-spill"))]
                    let mouse_tier = false;
                    // #1285: build the model-free where_is index once per session
                    // (the gather is a capped, cheap structural walk — no model,
                    // no network). The typed-verdict lookup then rides this turn.
                    finish_nav_warmup(&rt, &mut nav_warmup, &mut where_is_index, &mut nav_session);
                    // Iteration #3: while the warm-up is still building, do NOT
                    // rebuild inline either — this turn rides the regex floor.
                    if nav_warmup.is_none()
                        && (where_is_index.is_none() || nav_session.usage.is_none())
                    {
                        ensure_nav_indexes(
                            workspace,
                            &cfg,
                            &mut where_is_index,
                            &mut nav_session,
                            &index_status,
                        );
                    }
                    // The core sees this collaborator only in human-selected
                    // Auto. It is bound to this conversation and any model
                    // selection takes effect on a later outer turn.
                    let turn_auto_mode_control =
                        conversation_mode_states.auto.bind(&active_conversation_id);
                    let operating_mode_control = (active_operating_mode == OperatingMode::Auto)
                        .then_some(
                            &turn_auto_mode_control
                                as &dyn newt_core::agentic::OperatingModeControl,
                        );
                    // Session disclosure filter: register the live provider secret
                    // so a tool result or summary echoing it is value-redacted
                    // before it reaches the model. Outlives the ChatCtx below.
                    let session_disclosure =
                        newt_core::ocap::session_disclosure_filter(inf_key.as_deref());
                    // Also install it on this thread so the memory / observation /
                    // compaction / spill paths (via `redact_secrets`) value-filter
                    // against the same secret. The turn runs on this thread
                    // (`block_in_place` + current-thread `block_on`), so the TLS is
                    // visible throughout.
                    let _disclosure_guard =
                        newt_core::ocap::scoped_session_disclosure(session_disclosure.clone());
                    let response = with_live_spill_watch(
                        interruptible,
                        &turn_cancel,
                        &turn_hard,
                        mouse_tier,
                        spill_input,
                        || {
                            tokio::task::block_in_place(|| {
                                rt.block_on(chat_complete_with_prompt_and_artifacts(
                                    ChatCtx {
                                        url: &inf_url,
                                        model: &inf_model,
                                        kind: inf_kind,
                                        api_key: inf_key.as_deref(),
                                        messages: &messages,
                                        task: &task,
                                        workspace,
                                        color,
                                        // Step 25.4 (#568): `[tui].markdown` ∧
                                        // `/markdown` override ∧ color.
                                        markdown: markdown_enabled(&cfg, color, markdown_override),
                                        // Step 26.3 (#584): offload oversized tool
                                        // results to the session spill store.
                                        tool_offload: tool_offload_on,
                                        spill_store: Some(
                                            &spill_store as &dyn newt_core::SpillStore,
                                        ),
                                        disclosure: Some(&session_disclosure),
                                        compaction_store: Some(
                                            &compaction_store as &dyn newt_core::SpillStore,
                                        ),
                                        // Step 26.4 (#583): scratchpad state.
                                        scratchpad: scratchpad_on,
                                        scratchpad_store: Some(
                                            &scratchpad_store as &dyn newt_core::ScratchpadStore,
                                        ),
                                        // Step 26.5.5 (#582): the code_search tool's
                                        // searcher — Some only when semantic is on.
                                        code_search: semantic_embedder.as_deref().map(|e| {
                                            newt_core::CodeSearch {
                                                embedder: e,
                                                index: semantic_index.as_ref(),
                                                top_k: semantic_cfg.top_k,
                                                steer: Some(&retrieval_steer),
                                                status: Some(&index_status),
                                            }
                                        }),
                                        // #1285: the exact typed-verdict symbol
                                        // lookup — Some once the model-free index
                                        // is built (first turn), degrading honestly.
                                        where_is: where_is_index.as_ref(),
                                        nav: Some(newt_core::NavToolCtx {
                                            workspace,
                                            where_is: where_is_index.as_ref(),
                                            usage: nav_session.usage.as_ref(),
                                            graph: nav_session.graph.as_ref(),
                                            project: nav_session.project.as_ref(),
                                            files: Some(nav_session.files.as_slice()),
                                            status: Some(&index_status),
                                        }),
                                        // #TEC Pass 1: resolve the tool-exposure
                                        // controller policy from `[tool_exposure]`.
                                        // Default `full` = identity (unchanged
                                        // advertised catalog); `auto`/`minimal`
                                        // size the schema set to the live budget.
                                        exposure: newt_core::ExposureSettings::from(
                                            cfg.tool_exposure(),
                                        ),
                                        // Step 26.6a (#585): the experiential store
                                        // for record/recall — Some only when on.
                                        experience_store: experiential_on.then_some(
                                            &experience_store as &dyn newt_core::ExperienceStore,
                                        ),
                                        // Step 26.6b (#586): the plan ledger for
                                        // update_plan — Some only when on.
                                        step_ledger: scheduled_on
                                            .then_some(&step_ledger as &dyn newt_core::StepLedger),
                                        // #307: the clamped effective caveats (base ∩
                                        // preset). Identical to `cap.caveats()` when no
                                        // posture is active.
                                        caveats: &turn_caveats,
                                        persona_tools,
                                        cognition,
                                        chat_completions_capability: choice
                                            .chat_completions_capability,
                                        reasoning_replay_scope: choice.reasoning_replay_scope,
                                        max_tool_rounds: eff_max_tool_rounds,
                                        narration_nudge_cap: eff_narration_nudge_cap,
                                        // #1162: the /nudge dial — env set by the
                                        // /nudge command; off = no action-pressure.
                                        action_nudges: !nudges_off,
                                        prompt_disposition: turn_disposition,
                                        prompt_intake: Some(&prompt_intake),
                                        workflow_grace_rounds: eff_workflow_grace_rounds,
                                        tool_output_lines: tool_output_lines(&cfg),
                                        debug: debug_mode(&cfg),
                                        trace: trace_mode(&cfg),
                                        num_ctx: eff_num_ctx,
                                        connect_timeout_secs: connect_timeout_secs(&cfg),
                                        inference_timeout_secs: inference_timeout_secs(&cfg),
                                        mid_loop_trim_threshold: eff_mid_loop_trim,
                                        compaction_trigger_policy: eff_compaction_trigger_policy,
                                        mid_loop_trim_tokens: eff_mid_loop_trim_tokens,
                                        max_ok_input: eff_max_ok_input,
                                        build_check_cmd: build_check_cmd(&cfg),
                                        safe_context: eff_safe_context,
                                        // The TUI recovers hard context-window 400s by
                                        // parsing the endpoint's real limit and persisting
                                        // it to model-capabilities.json (the probe cache
                                        // stays TUI-side). See issue #223.
                                        recover_cw_400: Some(recover_context_window_400),
                                        note_sink: Some(&mut note_sink),
                                        note_nudge: Some(&mut note_nudge),
                                        // Recall over past conversations (Step 17.5).
                                        recall_source: recall_source
                                            .as_ref()
                                            .map(|source| source as &dyn newt_core::RecallSource),
                                        // Progressive-disclosure memory_fetch (#319):
                                        // present only under disclosure = "index"; None
                                        // (the default) keeps the loop bit-for-bit.
                                        memory_source: memory_source
                                            .as_ref()
                                            .map(|s| s as &dyn newt_core::MemorySource),
                                        // Summarize-don't-discard (Step 18.4, #247).
                                        summarizer: Some(&*loop_summarizer),
                                        compress_state: Some(&mut compress_state),
                                        tool_events: Some(&mut turn_tool_events),
                                        phantom_reaches: Some(&mut turn_phantom_reaches),
                                        end_reason: Some(&mut turn_end_reason),
                                        // W0 (#1511): the solve contract is a
                                        // headless-lane artifact — the TUI
                                        // session records nothing here.
                                        solve_obs: None,
                                        // #263: present only when prompting is on —
                                        // the loop blocks on the prompt like a long
                                        // tool call; None keeps denials verbatim.
                                        permission_gate: permission_gate
                                            .as_mut()
                                            .map(|g| g as &mut dyn newt_core::PermissionGate),
                                        // Phase 20: per-round capability evidence +
                                        // the learned estimate calibration.
                                        on_round_usage: Some(&mut on_obs),
                                        estimate_ratio: eff_estimate_ratio,
                                        estimation: cfg
                                            .context
                                            .as_ref()
                                            .map(|c| c.estimation)
                                            .unwrap_or_default(),
                                        summary_input_cap_floor_chars: cfg
                                            .context
                                            .as_ref()
                                            .map(|c| c.summary_input_cap_floor_chars)
                                            .unwrap_or(8_192),
                                        input_ceiling_pct: eff_input_ceiling_pct,
                                        low_budget_pct: cfg
                                            .context
                                            .as_ref()
                                            .map(|c| c.low_budget_pct)
                                            .unwrap_or(15)
                                            .clamp(1, 50),
                                        // #307: the active preset's exec floor — the
                                        // ceiling the --disable-ocap bypass cannot
                                        // cross. None when no posture is active.
                                        exec_floor: exec_floor.as_ref(),
                                        // retry technique: the per-turn write ledger (Some
                                        // only under a `retry` profile). The write tools
                                        // record into it; the post-turn gate reverts from it.
                                        write_ledger: retry_ledger.as_ref(),
                                        // Esc-to-interrupt flag, tripped by the watcher.
                                        cancel: Some(&turn_cancel),
                                        #[cfg(feature = "live-spill")]
                                        live_tool_output: live_spill.as_ref().map(|spill| {
                                            spill.clone()
                                                as std::sync::Arc<dyn newt_core::LiveToolOutput>
                                        }),
                                        #[cfg(not(feature = "live-spill"))]
                                        live_tool_output: None,
                                        // #1640: Rich TUI completed spill renderer for interactive
                                        // completed tool output viewport. Only on Rich TUI + live-spill.
                                        #[cfg(all(feature = "rich-tui", feature = "live-spill"))]
                                        completed_spill_renderer: live_spill.as_ref().map(|spill| {
                                            spill.clone()
                                                as std::sync::Arc<dyn newt_core::agentic::CompletedSpillRenderer>
                                        }),
                                        #[cfg(not(all(feature = "rich-tui", feature = "live-spill")))]
                                        completed_spill_renderer: None,
                                        // PR4 (#461): the embedded git tool, now
                                        // always advertised (carries `init` for a
                                        // not-yet-a-repo workspace).
                                        git_tool: session_git_tool
                                            .as_ref()
                                            .map(|g| g as &dyn newt_core::agentic::GitTool),
                                        // #479 part 2: the crew/team runner, injected by
                                        // the binary (newt-cli) — advertises + dispatches
                                        // the `/team` tools when present.
                                        crew_runner,
                                        operating_mode_control,
                                        plan_mode_control: Some(
                                            &conversation_mode_states.plan
                                                as &dyn newt_core::agentic::PlanModeControl,
                                        ),
                                    },
                                    active_prompt_context.as_ref(),
                                    prompt_source,
                                    artifact_source,
                                    artifact_sink,
                                    &mut mcp,
                                ))
                            })
                        },
                    );

                    let elapsed = t0.elapsed();
                    erase_line();
                    // Snapshot again regardless of the response shape. A tool
                    // may have committed before cancellation or a later model
                    // error; preserve that transition as unattributed evidence.
                    let artifact_head_after_turn =
                        git_head_snapshot(session_git_tool.as_ref(), &turn_caveats);
                    if let (Some(sink), Some(turn)) =
                        (artifact_sink, active_prompt_context.as_ref())
                    {
                        let context = newt_core::agentic::ArtifactReadContext::from_turn(
                            turn,
                            artifact_source,
                        );
                        if let Err(e) = newt_core::agentic::record_observed_head_transition(
                            sink,
                            context,
                            artifact_head_before_turn
                                .as_ref()
                                .and_then(|snapshot| snapshot.head.as_deref()),
                            artifact_head_after_turn
                                .as_ref()
                                .and_then(|snapshot| snapshot.head.as_deref()),
                            artifact_head_after_turn
                                .as_ref()
                                .and_then(|snapshot| snapshot.branch.as_deref()),
                        ) {
                            print_newt(
                                &format!("warning: could not record observed git transition: {e}"),
                                color,
                                verbose,
                            );
                        }
                    }
                    if turn_exit.load(std::sync::atomic::Ordering::Relaxed) {
                        clean_exit = true;
                        break;
                    } else if turn_cancel.load(std::sync::atomic::Ordering::Relaxed) {
                        let note = if turn_hard.load(std::sync::atomic::Ordering::Relaxed) {
                            "⊘ stopped — back to you"
                        } else {
                            "⊘ interrupted — back to you"
                        };
                        print_newt(note, color, verbose);
                        println!();
                    } else {
                        match response {
                            Ok((reply, was_streamed, usage, hallucinations)) => {
                                if !was_streamed {
                                    // Step 25.4 (#568): the non-stream fallback also
                                    // renders Markdown when it is active.
                                    if markdown_enabled(&cfg, color, markdown_override) {
                                        let cols = crossterm::terminal::size()
                                            .map(|(c, _)| c as usize)
                                            .unwrap_or(80)
                                            .max(20);
                                        print!("▸  ");
                                        print!(
                                            "{}",
                                            newt_core::agentic::render_markdown(
                                                &reply,
                                                newt_core::agentic::RenderOpts {
                                                    color: true,
                                                    cols
                                                },
                                            )
                                        );
                                        println!();
                                    } else {
                                        print_newt(&reply, color, verbose);
                                    }
                                }
                                // Profile techniques, post-turn (R2). `retry` supersedes
                                // `verify_gate`: it runs the same gate but *acts* —
                                // reverting each fabricating file to its pre-turn state
                                // (↩), then re-prompting the model to ground the rewrite
                                // up to `max_retries` (↻) before an honest give-up (✗) —
                                // where bare `verify_gate` only warns (⚠).
                                if let Some(ledger) = retry_ledger.as_ref() {
                                    let mode = active_profile
                                        .as_ref()
                                        .map(|p| p.verify_gate_knobs().surface_match)
                                        .unwrap_or_default();
                                    let action = tokio::task::block_in_place(|| {
                                        rt.block_on(retry_revert(workspace, mode, ledger))
                                    });
                                    if let Some(action) = action {
                                        if let (Some(sink), Some(turn)) =
                                            (artifact_sink, active_prompt_context.as_ref())
                                        {
                                            let context =
                                                newt_core::agentic::ArtifactReadContext::from_turn(
                                                    turn,
                                                    artifact_source,
                                                );
                                            for path in &action.reverted {
                                                if let Err(e) =
                                                    newt_core::agentic::record_retry_revert_file(
                                                        sink,
                                                        context,
                                                        &path.to_string_lossy(),
                                                    )
                                                {
                                                    print_newt(
                                                        &format!(
                                                            "warning: could not record retry revert artifact: {e}"
                                                        ),
                                                        color,
                                                        verbose,
                                                    );
                                                }
                                            }
                                        }
                                        let extra = match retry_step(retry_budget) {
                                        RetryStep::Reprompt => {
                                            retry_budget -= 1;
                                            // Queue the grounded corrective turn as the
                                            // next loop iteration's derived input. It
                                            // inherits the operator root captured before
                                            // this turn; it never masquerades as a new
                                            // operator prompt.
                                            if let Some(parent) = active_prompt_context.clone() {
                                                pending_retry = Some(PendingRetry {
                                                    text: action.corrective,
                                                    parent: Box::new(parent),
                                                });
                                                format!(
                                                    "\n↻ retry: re-prompting the model to ground the rewrite ({retry_budget} re-prompt(s) remaining)"
                                                )
                                            } else {
                                                "\n✗ retry: corrective input was not queued because the turn has no prompt receipt"
                                                    .to_string()
                                            }
                                        }
                                        RetryStep::GiveUp => format!(
                                            "\n✗ retry: gave up after {retry_max} re-prompt(s) — file(s) left reverted"
                                        ),
                                    };
                                        let line = format!("↩ {}{extra}", action.banner);
                                        if color {
                                            let _ = execute!(
                                                io::stdout(),
                                                SetForegroundColor(CtColor::Yellow),
                                                Print(format!("{line}\n")),
                                                ResetColor,
                                            );
                                        } else {
                                            println!("{line}");
                                        }
                                    }
                                } else if let Some(p) =
                                    active_profile.as_ref().filter(|p| p.enables("verify_gate"))
                                {
                                    if let Some(warn) = verify_gate_summary(
                                        workspace,
                                        p.verify_gate_knobs().surface_match,
                                    ) {
                                        if color {
                                            let _ = execute!(
                                                io::stdout(),
                                                SetForegroundColor(CtColor::Yellow),
                                                Print(format!("⚠ {warn}\n")),
                                                ResetColor,
                                            );
                                        } else {
                                            println!("⚠ {warn}");
                                        }
                                    }
                                }
                                // Single TurnMetrics used for both memory sync and display.
                                let pricing = cfg.pricing.clone().unwrap_or_default();
                                let metrics = newt_core::TurnMetrics {
                                    elapsed_ms: elapsed.as_millis() as u64,
                                    usage,
                                    cost_usd: pricing.estimate_cost(&inf_model, usage.as_ref()),
                                    model_id: inf_model.clone(),
                                    endpoint: inf_url.clone(),
                                    hallucinations,
                                    end_reason: turn_end_reason,
                                };
                                // Iteration #2: keep a RoundCap-interrupted
                                // objective linkable for the next bare nudge.
                                interrupted_objective = (turn_end_reason
                                    == Some(newt_core::TurnEndReason::RoundCap))
                                .then(|| active_prompt_context.clone())
                                .flatten();
                                let memory_task =
                                    active_operator_task(active_prompt_context.as_ref(), &task);
                                tokio::task::block_in_place(|| {
                                    rt.block_on(memory.sync_all_with_active_task(
                                        &task,
                                        &reply,
                                        &metrics,
                                        memory_task,
                                    ));
                                });
                                // 19.4: this conversation now has extractable
                                // content — count it for the close-time gate.
                                turns_this_conversation += 1;
                                // #713: snapshot the live scratchpad <state> so
                                // resume can re-hydrate it (working memory, not
                                // chained). `entries()` is a trait method.
                                let scratchpad_snapshot = {
                                    use newt_core::ScratchpadStore;
                                    scratchpad_store.entries()
                                };
                                // #715: snapshot the live plan ledger so resume
                                // can re-hydrate the <plan> (working memory, not
                                // chained). `snapshot()` is a trait method.
                                let plan_snapshot = {
                                    use newt_core::StepLedger;
                                    step_ledger.snapshot()
                                };
                                // `take_compaction_record` is destructive: bind
                                // it once, retain one digest input, then pass the
                                // owned record to conversation storage. Append
                                // the checkpoint only after that save succeeds.
                                let compaction_record = memory.take_compaction_record();
                                let compaction_artifact_summary = compaction_record.clone();
                                let conversation_save = save_turn_if_persistent(
                                    conversation_store.as_ref(),
                                    &active_conversation_id,
                                    active_persona.as_ref(),
                                    &task,
                                    &reply,
                                    // 17.6: the turn's recorded tool events plus the
                                    // backend-reported token actuals (None when the
                                    // backend reported nothing — stored as NULL,
                                    // never an estimate).
                                    &turn_tool_events,
                                    // #717: the turn's recorded phantom reaches,
                                    // persisted into the turn's `phantom_reaches`.
                                    &turn_phantom_reaches,
                                    usage,
                                    // 18.5: a compaction summary minted by the
                                    // memory provider during sync_all persists as
                                    // its own turn record so restore can rehydrate
                                    // the prev-summary chain.
                                    compaction_record,
                                    // #713: the live scratchpad <state> snapshot,
                                    // persisted onto the conversation row so resume
                                    // re-hydrates it (working memory, not chained).
                                    &scratchpad_snapshot,
                                    // #715: the live plan-ledger snapshot, persisted
                                    // onto the conversation row so resume re-hydrates
                                    // it (working memory, not chained).
                                    &plan_snapshot,
                                );
                                match conversation_save {
                                    Ok(save_state) => {
                                        if let TurnSaveState::DurableWithAncillaryWarning(error) =
                                            save_state
                                        {
                                            print_newt(
                                                &format!("warning: conversation ancillary save failed: {error}"),
                                                color,
                                                verbose,
                                            );
                                        }
                                        // The transcript remains the source for
                                        // reply text. Append its digest-only
                                        // outcome only after the transcript save
                                        // succeeds, never before durable state.
                                        if let (Some(sink), Some(turn)) =
                                            (artifact_sink, active_prompt_context.as_ref())
                                        {
                                            let context =
                                                newt_core::agentic::ArtifactReadContext::from_turn(
                                                    turn,
                                                    artifact_source,
                                                );
                                            if let Err(e) = newt_core::agentic::record_turn_outcome(
                                                sink,
                                                context,
                                                &reply,
                                                metrics.usage,
                                                metrics.end_reason,
                                                metrics.elapsed_ms,
                                            ) {
                                                print_newt(
                                                    &format!(
                                                        "warning: could not record turn outcome artifact: {e}"
                                                    ),
                                                    color,
                                                    verbose,
                                                );
                                            }
                                        }
                                        if let (Some(summary), Some(sink), Some(turn)) = (
                                            compaction_artifact_summary.as_deref(),
                                            artifact_sink,
                                            active_prompt_context.as_ref(),
                                        ) {
                                            let context =
                                                newt_core::agentic::ArtifactReadContext::from_turn(
                                                    turn,
                                                    artifact_source,
                                                );
                                            if let Err(e) = newt_core::agentic::record_memory_compaction_checkpoint(
                                                sink, context, summary,
                                            ) {
                                                print_newt(
                                                    &format!(
                                                        "warning: could not record compaction checkpoint artifact: {e}"
                                                    ),
                                                    color,
                                                    verbose,
                                                );
                                            }
                                        }
                                    }
                                    Err(e) => print_newt(
                                        &format!("warning: conversation save failed: {e}"),
                                        color,
                                        verbose,
                                    ),
                                }
                                // #1062 auto-capture: if this bound conversation's
                                // turn produced a commit, attribute it to the bound
                                // Plan's next Task so /roadmap eval|drive close it
                                // from git truth — no manual /roadmap task … commit.
                                if let Some(store) = conversation_store.as_ref() {
                                    if let Some(note) = autocapture_commit_after_turn(
                                        store,
                                        &active_roadmap_id,
                                        &active_conversation_id,
                                        workspace,
                                        head_before_turn.as_deref(),
                                    ) {
                                        print_newt(&note, color, verbose);
                                    }
                                }
                                print_metrics(&metrics, color);
                                // Append to usage log and enforce rotation policy.
                                if let Some(log) = newt_core::Config::user_config_path()
                                    .map(|p| p.with_file_name("usage.jsonl"))
                                {
                                    let policy = cfg.logs.as_ref().cloned().unwrap_or_default();
                                    metrics.append_to_log_with_policy(&log, &policy);
                                }
                                // Turn-level tuning accounting (Phase 20,
                                // docs/design/model-self-tuning.md §3): success
                                // is gated on the turn having produced at least
                                // one quality-gated Accepted observation. The old
                                // `reply.is_empty()` keying was wrong twice over:
                                // every loop failure path returns non-empty
                                // placeholder text, so failed turns ratcheted
                                // confidence via record_success, and the overflow
                                // branch was dead code — overflow is now recorded
                                // at detection by the observation hook, with the
                                // truthful per-round number.
                                if let Some(input_tokens) = usage.map(|u| u.input_tokens) {
                                    if turn_saw_accepted.get() {
                                        let entry = cap_cache.entry(cap_id.clone()).or_default();
                                        let dirty = entry.record_success(input_tokens, &today);
                                        if dirty {
                                            probe::save_cache(&cap_cache);
                                        }
                                    }
                                    // Step 24.6 (#559): refresh the context-budget
                                    // gauge for the next header. Read observation state
                                    // again here: a numbered hard 400 may have replaced
                                    // the full window and cached caps during this turn.
                                    // Then use core's send-budget resolver so the visible
                                    // number includes the same cognition output reserve as
                                    // preflight, compaction, and context_remaining.
                                    let observed = cap_cache.get(&cap_id);
                                    let (gauge_max_ok, gauge_safe) = match context_size_override {
                                        Some(n) => (Some(n), Some(n)),
                                        None => (
                                            observed
                                                .and_then(|entry| entry.max_ok_input)
                                                .or(eff_max_ok_input),
                                            observed
                                                .and_then(|entry| entry.safe_context)
                                                .or(eff_safe_context),
                                        ),
                                    };
                                    let gauge_budget = context_gauge_budget(
                                        inf_kind,
                                        choice.api,
                                        eff_num_ctx,
                                        recovered_context_window.get(),
                                        eff_input_ceiling_pct,
                                        cognition,
                                        choice.chat_completions_capability,
                                        choice.reasoning_replay_scope,
                                        gauge_max_ok,
                                        gauge_safe,
                                    );
                                    if let Some(budget) = gauge_budget {
                                        token_gauge = Some((input_tokens, budget));
                                    }
                                }
                            }
                            Err(e) => print_newt(&format!("error: {e}"), color, verbose),
                        }
                    }
                }
                println!();
            }
            ReadOutcome::Interrupted | ReadOutcome::Eof => {
                clean_exit = true;
                break;
            }
            ReadOutcome::EndAndQuit => {
                // vi `:wq` — its turn already ran; end the conversation on the
                // way out so the next launch starts fresh.
                clean_exit = true;
                end_conversation_on_exit = true;
                break;
            }
            ReadOutcome::Fatal(msg) => {
                // Raw mode already disabled by the surface; clean_exit stays
                // false so the broken terminal skips the close-time round-trip.
                eprintln!("{msg}");
                break;
            }
        }
    }

    // 19.4: close-time extraction on a clean exit only — the EMFILE/panic
    // crash breaks above leave `clean_exit` false (a degraded terminal does
    // not need one more network round-trip). Failure never blocks exit.
    if clean_exit {
        let close_complete = build_session_summarizer(
            &sum_cfg,
            &cfg,
            &inf_url,
            &inf_model,
            inf_kind,
            &inf_key,
            Some(mem_budget),
            color,
        );
        if let Some(notice) = tokio::task::block_in_place(|| {
            rt.block_on(run_close_extraction(
                extract_on_close,
                ephemeral_session,
                turns_this_conversation,
                &mut memory,
                &close_complete,
            ))
        }) {
            print_newt(&notice, color, verbose);
        }
    }

    // vi `:wq` close-out: mark the active conversation ended so `latest_open`
    // skips it next launch (it stays in `/recall`). Runs after extraction so
    // the summary still reads completed turns. Mark any durable conversation,
    // including a prompt-only failed/cancelled turn, as ended.
    if end_conversation_on_exit {
        if let Some(store) = conversation_store.as_ref() {
            if store.exists(&active_conversation_id).unwrap_or(false) {
                let _ = store.end_conversation(&active_conversation_id, "wq");
            }
        }
    }

    // #1030: release this process's live-owner claim on the way out so the next
    // launch, another newt, or a later /resume can take the conversation. A
    // crash that skips this leaves a stale claim the next claim reclaims.
    if let Some(store) = conversation_store.as_ref() {
        let _ = store.release(&active_conversation_id);
    }

    surface.save_history();
    Ok(())
}

/// #1387: build where_is + usage + graph + project model once per session.
///
/// Uses the same language-pack registry as `find` `category=source` so "code"
/// means one thing everywhere.
fn ensure_nav_indexes(
    workspace: &str,
    cfg: &newt_core::Config,
    where_is_index: &mut Option<newt_core::WhereIsIndex>,
    nav_session: &mut newt_core::NavigatorSession,
    index_status: &newt_core::IndexStatus,
) {
    use newt_core::{gather_with_manifest, GatherCaps};
    let api_cfg = cfg
        .context
        .as_ref()
        .map(|context| context.api_surface.clone())
        .unwrap_or_default();
    let packs = resolved_language_packs(workspace, &api_cfg);
    let exts = newt_core::api_surface::source_extensions_for(&packs, None).unwrap_or_default();
    let (files, manifest) = gather_with_manifest(workspace, &exts, GatherCaps::default());
    let cuts_open = !manifest.cuts.is_empty();
    let id = index_status.index_id();
    if where_is_index.is_none() {
        *where_is_index = Some(newt_core::build_where_is_index(&files, &packs, &manifest));
    }
    if nav_session.usage.is_none() {
        nav_session.usage = Some(newt_core::UsageIndex::build(&files, cuts_open, &id));
    }
    if nav_session.graph.is_none() {
        nav_session.graph = Some(newt_core::GraphIndex::build(&files, cuts_open, &id));
    }
    if nav_session.project.is_none() {
        let root = std::path::Path::new(workspace);
        nav_session.project = newt_core::project_model::scan_project(
            root,
            &newt_core::project_model::builtin_project_packs(),
        );
    }
    nav_session.files = files;
    nav_session.ledger.set_index(id);
}

struct SemanticIndexWarmup {
    handle: tokio::task::JoinHandle<usize>,
    job: BackgroundJob,
}

/// Iteration #4 of bug/steering-regressions: embedding the gathered corpus
/// through the in-process CPU embedder ran SYNCHRONOUSLY inside the first
/// agentic turn (`block_on(index_files…)`) — observed live as 40–80 minutes at
/// ~6.5 cores with the session apparently wedged between a tool result and the
/// next dispatch. Same defect class as the navigator warm-up join (iteration
/// #3), one layer down. Indexing now runs as a background task; retrieval
/// rides the lexical floor until the index is ready. Never trade turn
/// liveness for index completeness.
fn spawn_semantic_indexing(
    rt: &tokio::runtime::Handle,
    files: Vec<(String, String)>,
    embedder: std::sync::Arc<dyn newt_core::Embedder>,
    index: std::sync::Arc<newt_core::SessionSemanticIndex>,
    on_failure: newt_core::OnEmbedFailure,
) -> SemanticIndexWarmup {
    let job = BackgroundJob::start("embedding repository for semantic retrieval");
    let completion = job.completion_guard();
    // Iteration #8: the embedded candle engine's forwards are SYNCHRONOUS
    // compute. Run on a plain async task they poll-block the runtime workers
    // themselves — observed live as total executor starvation (frozen pane,
    // zero network, every rt-worker pegged) MINUTES after iteration #4 moved
    // this off the turn. spawn_blocking confines the drive to one parked
    // blocking-pool thread; candle's internal parallelism is unaffected and
    // the async runtime stays responsive.
    let inner = rt.clone();
    let handle = rt.spawn_blocking(move || {
        let _completion = completion;
        inner.block_on(newt_core::index_files(
            &files,
            embedder.as_ref(),
            index.as_ref(),
            on_failure,
        ))
    });
    SemanticIndexWarmup { handle, job }
}

/// Adopt a FINISHED semantic-indexing warm-up (never blocks): returns the
/// chunk count once, for the completion notice. A still-running build is left
/// running; a panicked/aborted build is consumed silently (the lexical floor
/// already covers the gap).
fn poll_semantic_indexing(
    rt: &tokio::runtime::Handle,
    warmup: &mut Option<SemanticIndexWarmup>,
) -> Option<usize> {
    let pending = warmup.take()?;
    if !pending.handle.is_finished() {
        *warmup = Some(pending);
        return None;
    }
    tokio::task::block_in_place(|| rt.block_on(pending.handle)).ok()
}

type NavWarmupOutput = (Option<newt_core::WhereIsIndex>, newt_core::NavigatorSession);

struct NavWarmup {
    handle: tokio::task::JoinHandle<NavWarmupOutput>,
    job: BackgroundJob,
}

impl NavWarmup {
    fn abort(self) {
        self.handle.abort();
    }
}

fn spawn_nav_warmup(
    rt: &tokio::runtime::Handle,
    workspace: &str,
    cfg: &newt_core::Config,
    index_status: &newt_core::IndexStatus,
) -> NavWarmup {
    let workspace = workspace.to_string();
    let cfg = cfg.clone();
    let index_status = index_status.clone();
    let job = BackgroundJob::start("indexing repository");
    let completion = job.completion_guard();
    let handle = rt.spawn_blocking(move || {
        let _completion = completion;
        let mut where_is = None;
        let mut nav = newt_core::NavigatorSession::default();
        ensure_nav_indexes(&workspace, &cfg, &mut where_is, &mut nav, &index_status);
        (where_is, nav)
    });
    NavWarmup { handle, job }
}

/// Adopt the background navigator warm-up ONLY if it has already finished.
///
/// bug/steering-regressions iteration #3: this used to `block_on` the join,
/// so the turn stalled for as long as the index build ran — observed live
/// twice (2026-07-27): 40+ minutes at ~6 cores on a corpus-heavy workspace,
/// the session apparently wedged (no output, no inference, no tool calls).
/// The navigator's own contract already degrades honestly without an index
/// (regex floor, `complete=false`), so a still-running warm-up now simply
/// keeps running: this turn uses the floor and a later turn adopts the
/// finished index. Never trade turn liveness for index completeness.
fn finish_nav_warmup(
    rt: &tokio::runtime::Handle,
    warmup: &mut Option<NavWarmup>,
    where_is: &mut Option<newt_core::WhereIsIndex>,
    nav: &mut newt_core::NavigatorSession,
) {
    let Some(pending) = warmup.take() else {
        return;
    };
    if !pending.handle.is_finished() {
        *warmup = Some(pending);
        return;
    }
    if let Ok((warmed_where_is, warmed_nav)) =
        tokio::task::block_in_place(|| rt.block_on(pending.handle))
    {
        *where_is = warmed_where_is;
        *nav = warmed_nav;
    }
}

fn resolved_language_packs(
    workspace: &str,
    api_cfg: &newt_core::config::ApiSurfaceConfig,
) -> Vec<newt_core::config::LanguagePack> {
    newt_core::api_surface::resolve_language_packs(std::path::Path::new(workspace), api_cfg)
}

fn resolved_source_extensions(workspace: &str, cfg: &newt_core::Config) -> Vec<String> {
    let api_cfg = cfg
        .context
        .as_ref()
        .map(|context| context.api_surface.clone())
        .unwrap_or_default();
    let packs = resolved_language_packs(workspace, &api_cfg);
    newt_core::api_surface::source_extensions_for(&packs, None).unwrap_or_default()
}

fn handle_nav_command(
    cmd: crate::navigator_cmds::NavCommand,
    workspace: &str,
    nav_session: &mut newt_core::NavigatorSession,
    where_is: Option<&newt_core::WhereIsIndex>,
    index_status: &newt_core::IndexStatus,
) -> String {
    use crate::navigator_cmds::{NavCommand, RetrievalView};
    use newt_core::{
        compare_ledgers, compare_semantic_lexical, export_ledger_json, export_ledger_markdown,
        find_callees, find_callers, find_hierarchy, find_implementations, find_references,
        find_tests, format_ledger_diff, format_ledger_human, format_ledger_model, goto_definition,
        hash_context, impact_analysis, inspect_type, project_map_nav, text_search,
        GotoDefinitionArgs,
    };
    let id = index_status.index_id();
    let record =
        |nav_session: &mut newt_core::NavigatorSession, query: &str, nav: newt_core::NavResult| {
            nav_session.turn_counter = nav_session.turn_counter.saturating_add(1);
            let ctx = hash_context(nav.render().as_bytes());
            nav_session
                .ledger
                .record_nav(nav_session.turn_counter, query, &nav, &ctx);
            let rendered = nav.render();
            nav_session.last_nav = Some(nav);
            rendered
        };
    match cmd {
        NavCommand::Help(msg) => msg.to_string(),
        NavCommand::Def(sym) => {
            let Some(idx) = where_is else {
                return "where_is index not ready".into();
            };
            let nav = goto_definition(
                idx,
                GotoDefinitionArgs {
                    symbol: &sym,
                    kind: None,
                    index_id: &id,
                    files: Some(nav_session.files.as_slice()),
                },
            );
            record(nav_session, &sym, nav)
        }
        NavCommand::Text(q) => {
            let nav = text_search(&q, std::path::Path::new(workspace), &id);
            nav_session.last_lexical = Some(nav.clone());
            record(nav_session, &q, nav)
        }
        NavCommand::Uses(sym) => {
            let Some(idx) = nav_session.usage.as_ref() else {
                return "usage index not ready".into();
            };
            let nav = find_references(idx, &sym);
            record(nav_session, &sym, nav)
        }
        NavCommand::Tests(sym) => {
            let Some(idx) = nav_session.usage.as_ref() else {
                return "usage index not ready".into();
            };
            let nav = find_tests(idx, &sym);
            record(nav_session, &sym, nav)
        }
        NavCommand::Map { expand } => {
            if nav_session.project.is_none() {
                return "no project model detected for this workspace".into();
            }
            let seed = newt_core::project_map::load_seed(std::path::Path::new(workspace));
            let (out, nav) = {
                let model = nav_session.project.as_ref().expect("checked above");
                let mut out = newt_core::project_map::render_project_map(model, &seed)
                    .unwrap_or_else(|| "(empty project map)\n".into());
                if let Some(unit) = expand.as_ref() {
                    if let Some(u) = model
                        .units
                        .iter()
                        .find(|u| u.name == *unit || u.dir == *unit)
                    {
                        out.push_str(&format!(
                            "\nexpanded `{unit}`:\n  dir: {}\n  roots: {:?}\n  deps: {:?}\n  langs: {:?}\n",
                            u.dir, u.source_roots, u.deps, u.languages
                        ));
                    } else {
                        out.push_str(&format!("\n(no unit named `{unit}`)\n"));
                    }
                }
                let nav = project_map_nav(model, expand.as_deref(), &id);
                (out, nav)
            };
            if let Some(unit) = expand.clone() {
                nav_session.map_expand = Some(unit);
            }
            let _ = record(nav_session, expand.as_deref().unwrap_or("map"), nav);
            out
        }
        NavCommand::Callers(sym) => {
            let Some(idx) = nav_session.graph.as_ref() else {
                return "graph index not ready".into();
            };
            record(nav_session, &sym, find_callers(idx, &sym))
        }
        NavCommand::Callees(sym) => {
            let Some(idx) = nav_session.graph.as_ref() else {
                return "graph index not ready".into();
            };
            record(nav_session, &sym, find_callees(idx, &sym))
        }
        NavCommand::Implementations(sym) => {
            let Some(idx) = nav_session.graph.as_ref() else {
                return "graph index not ready".into();
            };
            record(nav_session, &sym, find_implementations(idx, &sym))
        }
        NavCommand::Hierarchy(sym) => {
            let Some(idx) = nav_session.graph.as_ref() else {
                return "graph index not ready".into();
            };
            record(nav_session, &sym, find_hierarchy(idx, &sym))
        }
        NavCommand::Type(sym) => {
            let nav = inspect_type(&sym, &nav_session.files, where_is, &id);
            record(nav_session, &sym, nav)
        }
        NavCommand::Impact(unit) => {
            let Some(model) = nav_session.project.as_ref() else {
                return "no project model — cannot compute impact".into();
            };
            let report = impact_analysis(
                &unit,
                model,
                &nav_session.files,
                std::path::Path::new(workspace),
            );
            let nav = report.to_nav(&id);
            let text = report.render();
            let _ = record(nav_session, &unit, nav);
            text
        }
        NavCommand::Retrieval { turn, view } => {
            let t = match turn {
                Some(n) => nav_session.ledger.get_turn(n),
                None => nav_session.ledger.turns.last(),
            };
            match t {
                None => "no retrieval ledger entries yet".into(),
                Some(tr) => match view {
                    RetrievalView::Human => format_ledger_human(tr),
                    RetrievalView::Model => format_ledger_model(tr),
                    RetrievalView::Diff => {
                        let prior = match turn {
                            Some(n) => nav_session.ledger.prior_turn(n),
                            None => {
                                let len = nav_session.ledger.turns.len();
                                if len >= 2 {
                                    Some(&nav_session.ledger.turns[len - 2])
                                } else {
                                    None
                                }
                            }
                        };
                        match prior {
                            Some(a) => format_ledger_diff(a, tr),
                            None => format_ledger_human(tr),
                        }
                    }
                },
            }
        }
        NavCommand::CompareSemanticLexical => compare_semantic_lexical(
            nav_session.last_semantic.as_ref(),
            nav_session.last_lexical.as_ref(),
        ),
        NavCommand::CompareTurns(a, b) => compare_ledgers(&nav_session.ledger, a, b),
        NavCommand::CompareIndex => format!(
            "session-index previous={:?} current={:?}\n",
            nav_session.ledger.previous_index_id, nav_session.ledger.current_index_id
        ),
        NavCommand::ExportJson => export_ledger_json(&nav_session.ledger),
        NavCommand::ExportMarkdown => export_ledger_markdown(&nav_session.ledger),
    }
}

#[cfg(test)]
mod prompt_ingress_tests {
    use super::*;

    /// Grounds the background task in a real source workspace: startup may run
    /// concurrently, but the first consumer joins a complete structural index
    /// rather than observing a partially built belief.
    #[tokio::test(flavor = "multi_thread")]
    async fn repository_navigator_warms_in_background_and_joins_complete() {
        let workspace = tempfile::TempDir::new().unwrap();
        std::fs::write(
            workspace.path().join("main.rs"),
            "pub fn warm_marker() {}\n",
        )
        .unwrap();
        let workspace = workspace.path().to_string_lossy().into_owned();
        let rt = tokio::runtime::Handle::current();
        let mut warmup = Some(spawn_nav_warmup(
            &rt,
            &workspace,
            &newt_core::Config::default(),
            &newt_core::IndexStatus::default(),
        ));
        let job = warmup.as_ref().unwrap().job.clone();
        let mut where_is = None;
        let mut nav = newt_core::NavigatorSession::default();

        // Iteration #3 contract: adoption happens only once the build is done —
        // wait for readiness (bounded), then adopt. A still-running warm-up is
        // covered by `unfinished_warmup_is_left_running_and_the_turn_degrades`.
        for _ in 0..200 {
            if warmup.as_ref().is_some_and(|w| w.handle.is_finished()) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        finish_nav_warmup(&rt, &mut warmup, &mut where_is, &mut nav);

        assert!(warmup.is_none(), "a finished warm-up must be adopted");
        assert!(
            !job.is_running(),
            "joining the warm-up must clear its generic liveness indicator"
        );
        assert!(where_is.is_some());
        assert!(
            nav.files.iter().any(|(path, _)| path == "main.rs"),
            "the joined navigator must contain the real source file"
        );
        assert!(nav.usage.is_some() && nav.graph.is_some());
    }

    /// bug/steering-regressions iteration #4 (live wedge #3, 2026-07-27): the
    /// first agentic turn block_on-joined `index_files` over the gathered
    /// corpus through the in-process CPU embedder — 40–80 minutes at ~6.5
    /// cores, the session frozen between a tool result and the next dispatch.
    /// Spawning must return promptly, leave the build running, and adopt only
    /// once finished.
    #[tokio::test(flavor = "multi_thread")]
    async fn semantic_indexing_never_blocks_the_turn() {
        struct GatedEmbedder(std::sync::Arc<tokio::sync::Notify>);
        #[async_trait::async_trait]
        impl newt_core::Embedder for GatedEmbedder {
            async fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
                // Hold the "model forward" open until the test releases it.
                self.0.notified().await;
                Ok(vec![1.0, 0.0])
            }
        }
        let rt = tokio::runtime::Handle::current();
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        let embedder: std::sync::Arc<dyn newt_core::Embedder> =
            std::sync::Arc::new(GatedEmbedder(std::sync::Arc::clone(&release)));
        let index = std::sync::Arc::new(newt_core::SessionSemanticIndex::default());
        let files = vec![("main.rs".to_string(), "pub fn f() {}".to_string())];

        let started = std::time::Instant::now();
        let mut warmup = Some(spawn_semantic_indexing(
            &rt,
            files,
            embedder,
            std::sync::Arc::clone(&index),
            newt_core::OnEmbedFailure::default(),
        ));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "spawning the embed must never block the turn"
        );
        assert!(
            poll_semantic_indexing(&rt, &mut warmup).is_none(),
            "an unfinished embed is never joined"
        );
        assert!(warmup.is_some(), "the running embed is left running");
        {
            use newt_core::SemanticIndex as _;
            assert_eq!(index.chunks_indexed(), 0, "nothing adopted early");
        }

        // Release the gated forward; the build finishes and a later poll
        // adopts it.
        release.notify_waiters();
        for _ in 0..200 {
            if warmup.as_ref().is_some_and(|w| w.handle.is_finished()) {
                break;
            }
            release.notify_waiters();
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        let adopted = poll_semantic_indexing(&rt, &mut warmup);
        assert!(warmup.is_none(), "a finished embed is consumed");
        assert!(
            adopted.is_some_and(|n| n >= 1),
            "the finished embed reports its chunk count, got {adopted:?}"
        );
    }

    /// bug/steering-regressions iteration #3 (live wedges 2026-07-27): the
    /// turn must NEVER block on a still-running index warm-up. Two live
    /// sessions sat 40+ minutes at ~6 cores — no output, no inference —
    /// because the consumer `block_on`-joined an unbounded build. A
    /// still-running warm-up stays running; adoption happens on a later turn.
    #[tokio::test(flavor = "multi_thread")]
    async fn unfinished_warmup_is_left_running_and_the_turn_degrades() {
        let rt = tokio::runtime::Handle::current();
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let job = BackgroundJob::start("indexing repository");
        let completion = job.completion_guard();
        let handle = rt.spawn_blocking(move || {
            let _completion = completion;
            // Hold the "build" open until the test releases it.
            let _ = gate.recv();
            (None, newt_core::NavigatorSession::default())
        });
        let mut warmup = Some(NavWarmup { handle, job });
        let mut where_is = None;
        let mut nav = newt_core::NavigatorSession::default();

        let started = std::time::Instant::now();
        finish_nav_warmup(&rt, &mut warmup, &mut where_is, &mut nav);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "finish must return promptly, never join an unfinished build"
        );
        assert!(
            warmup.is_some(),
            "a still-running warm-up must be left running for a later turn"
        );
        assert!(
            where_is.is_none(),
            "nothing adopted from an unfinished build"
        );

        // Release the build; a later turn adopts it.
        release.send(()).unwrap();
        for _ in 0..200 {
            if warmup.as_ref().is_some_and(|w| w.handle.is_finished()) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        finish_nav_warmup(&rt, &mut warmup, &mut where_is, &mut nav);
        assert!(warmup.is_none(), "the finished build is adopted next turn");
    }

    fn prompt_store() -> (tempfile::TempDir, newt_core::ConversationStore, String) {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let store =
            newt_core::ConversationStore::new(tmp.path().join("state"), &workspace, 100).unwrap();
        (tmp, store, newt_core::new_conversation_id())
    }

    #[test]
    fn session_artifact_store_exists_only_for_ephemeral_mode() {
        let conversation_id = newt_core::new_conversation_id();

        assert!(session_artifact_store(false, &conversation_id)
            .unwrap()
            .is_none());
        let ephemeral = session_artifact_store(true, &conversation_id)
            .unwrap()
            .expect("ephemeral ledger");
        assert_eq!(ephemeral.conversation_id(), conversation_id);
    }

    #[test]
    fn session_artifact_store_rebinds_when_conversation_rotates() {
        let first_id = newt_core::new_conversation_id();
        let second_id = newt_core::new_conversation_id();
        let mut ledger = session_artifact_store(true, &first_id)
            .unwrap()
            .expect("first ledger");
        assert_eq!(ledger.conversation_id(), first_id);

        ledger = session_artifact_store(true, &second_id)
            .unwrap()
            .expect("replacement ledger");
        assert_eq!(ledger.conversation_id(), second_id);
        assert_ne!(ledger.conversation_id(), first_id);
    }

    #[test]
    fn git_head_snapshot_requires_effective_workspace_read_authority() {
        let tmp = tempfile::TempDir::new().unwrap();
        let tool = newt_git::LocalGitTool {
            root: tmp.path().to_path_buf(),
            author: newt_git::Author {
                name: "test".into(),
                email: "test@example.com".into(),
            },
            coauthor: None,
        };
        let mut denied = newt_core::Caveats::top();
        denied.fs_read = newt_core::Scope::none();
        assert!(git_head_snapshot(Some(&tool), &denied).is_none());
        assert!(git_head_snapshot(None, &newt_core::Caveats::top()).is_none());
    }

    #[test]
    fn persistent_ingress_keeps_exact_raw_and_model_bytes_in_prompt_only_conversation() {
        let (_tmp, store, conversation_id) = prompt_store();
        let ephemeral_prompts = newt_core::agentic::SessionPromptStore::default();
        let raw = b"  inspect src \\\nthen test  ";
        let model = b"inspect src \nthen test";

        let context = begin_model_prompt(
            PromptIngress {
                durable: Some(&store),
                ephemeral: &ephemeral_prompts,
            },
            &conversation_id,
            "inspect src",
            Some("coder"),
            raw,
            model,
            &ModelInputOrigin::Operator,
        )
        .unwrap();

        let submitted = context.submitted_prompt().receipt();
        assert_eq!(submitted.raw_text(), raw);
        assert_eq!(submitted.model_text(), model);
        assert_eq!(submitted.origin(), newt_core::PromptOrigin::Operator);
        assert_eq!(submitted.root_prompt_id(), submitted.id());
        submitted.verify_integrity().unwrap();

        let record = store.load(&conversation_id).unwrap();
        assert!(record.turns.is_empty(), "ingress precedes turn completion");
        assert_eq!(record.title, "inspect src");
        assert_eq!(record.persona.as_deref(), Some("coder"));
    }

    /// A3/W6 end-to-end at the seam level: a prompt enqueued via the attach
    /// API is dequeued and minted by the running session (D2), its durable
    /// receipt stays `origin=operator` (so no prompt_receipts CHECK migration),
    /// the inbox row is linked to that receipt, and — crucially — a web-injected
    /// line is NOT operator input to the TUI (its `/exit`/`!rm` can never reach
    /// the host-shell/slash/history gates).
    #[test]
    fn web_injected_prompt_mints_operator_receipt_and_is_inert_tui_input() {
        // Containment: the load-bearing safety property.
        let origin = ModelInputOrigin::WebInjected {
            inbox_id: "ib-1".into(),
        };
        assert!(
            !origin.is_operator(),
            "a web-injected line must be inert model text, never TUI operator input"
        );

        let (_tmp, store, conversation_id) = prompt_store();
        store
            .create_with_id(&conversation_id, "attached session", None)
            .unwrap();
        // The attach surface enqueues; it never mints the turn.
        store
            .inject_prompt(&conversation_id, "fix the flaky test", Some("req-1"))
            .unwrap();
        let injected = store
            .take_injected_prompt(&conversation_id)
            .unwrap()
            .expect("one queued prompt");
        assert_eq!(injected.body, "fix the flaky test");

        // The RUNNING session mints the turn (D2), durable origin = operator.
        let ephemeral_prompts = newt_core::agentic::SessionPromptStore::default();
        let context = begin_model_prompt(
            PromptIngress {
                durable: Some(&store),
                ephemeral: &ephemeral_prompts,
            },
            &conversation_id,
            "attached session",
            None,
            injected.body.as_bytes(),
            injected.body.as_bytes(),
            &ModelInputOrigin::WebInjected {
                inbox_id: injected.id.clone(),
            },
        )
        .unwrap();
        let submitted = context.submitted_prompt().receipt();
        assert_eq!(
            submitted.origin(),
            newt_core::PromptOrigin::Operator,
            "durable origin stays operator — no prompt_receipts CHECK migration"
        );
        submitted.verify_integrity().unwrap();

        // Additive provenance + exactly-once drain.
        store
            .link_inbox_delivery(&injected.id, &submitted.id().to_string())
            .unwrap();
        assert_eq!(
            store.take_injected_prompt(&conversation_id).unwrap(),
            None,
            "the inbox drained exactly once"
        );
    }

    #[test]
    fn persistent_ingress_error_is_returned_before_a_prompt_can_run() {
        let (_tmp, store, _conversation_id) = prompt_store();
        let ephemeral_prompts = newt_core::agentic::SessionPromptStore::default();
        let err = begin_model_prompt(
            PromptIngress {
                durable: Some(&store),
                ephemeral: &ephemeral_prompts,
            },
            "../outside",
            "unsafe id",
            None,
            b"do work",
            b"do work",
            &ModelInputOrigin::Operator,
        )
        .unwrap_err();

        assert!(err.to_string().contains("invalid"), "got: {err}");
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn ephemeral_ingress_has_equivalent_context_without_a_store() {
        let conversation_id = newt_core::new_conversation_id();
        let ephemeral_prompts = newt_core::agentic::SessionPromptStore::default();
        let context = begin_model_prompt(
            PromptIngress {
                durable: None,
                ephemeral: &ephemeral_prompts,
            },
            &conversation_id,
            "unused",
            None,
            b" raw ",
            b"raw",
            &ModelInputOrigin::Operator,
        )
        .unwrap();

        assert_eq!(context.submitted_prompt().receipt().raw_text(), b" raw ");
        assert_eq!(context.active_operator_prompt().model_text(), b"raw");
        assert_eq!(
            context.submitted_prompt().receipt().origin(),
            newt_core::PromptOrigin::Operator
        );
    }

    #[test]
    fn ephemeral_ingress_chains_attempts_and_fences_a_new_conversation() {
        use newt_core::agentic::PromptSource as _;

        let prompts = newt_core::agentic::SessionPromptStore::default();
        let first_conversation = newt_core::new_conversation_id();
        let first = begin_model_prompt(
            PromptIngress {
                durable: None,
                ephemeral: &prompts,
            },
            &first_conversation,
            "unused",
            None,
            b"first raw",
            b"FIRST",
            &ModelInputOrigin::Operator,
        )
        .unwrap();
        let second = begin_model_prompt(
            PromptIngress {
                durable: None,
                ephemeral: &prompts,
            },
            &first_conversation,
            "unused",
            None,
            b"second raw",
            b"SECOND",
            &ModelInputOrigin::Operator,
        )
        .unwrap();
        assert_eq!(
            second.submitted().receipt().previous_prompt_id(),
            Some(first.submitted().id())
        );

        let retry_one = begin_model_prompt(
            PromptIngress {
                durable: None,
                ephemeral: &prompts,
            },
            &first_conversation,
            "unused",
            None,
            b"retry one raw",
            b"RETRY ONE",
            &ModelInputOrigin::HarnessRetry {
                parent: Box::new(second.clone()),
            },
        )
        .unwrap();
        let retry_two = begin_model_prompt(
            PromptIngress {
                durable: None,
                ephemeral: &prompts,
            },
            &first_conversation,
            "unused",
            None,
            b"retry two raw",
            b"RETRY TWO",
            &ModelInputOrigin::HarnessRetry {
                parent: Box::new(retry_one.clone()),
            },
        )
        .unwrap();
        assert_eq!(
            retry_two.submitted().receipt().previous_prompt_id(),
            Some(retry_one.submitted().id())
        );
        assert_eq!(
            retry_two.submitted().receipt().parent_prompt_id(),
            Some(retry_one.submitted().id())
        );
        assert_eq!(
            prompts
                .source(&first_conversation)
                .fetch_prompt(retry_one.submitted().id())
                .unwrap()
                .unwrap()
                .model_text(),
            b"RETRY ONE"
        );

        // `/new` changes this binding. Even though the process-local store
        // retains the old receipts until exit, the new view cannot address
        // them and therefore cannot leak the previous task.
        let new_conversation = newt_core::new_conversation_id();
        let fresh = begin_model_prompt(
            PromptIngress {
                durable: None,
                ephemeral: &prompts,
            },
            &new_conversation,
            "unused",
            None,
            b"fresh raw",
            b"FRESH",
            &ModelInputOrigin::Operator,
        )
        .unwrap();
        let fresh_source = prompts.source(&new_conversation);
        assert!(fresh_source
            .fetch_prompt(retry_one.submitted().id())
            .unwrap()
            .is_none());
        assert!(fresh_source
            .fetch_prompt(fresh.submitted().id())
            .unwrap()
            .is_some());
    }

    #[test]
    fn harness_retry_is_derived_and_keeps_operator_root_active() {
        let (_tmp, store, conversation_id) = prompt_store();
        let ephemeral_prompts = newt_core::agentic::SessionPromptStore::default();
        let operator = begin_model_prompt(
            PromptIngress {
                durable: Some(&store),
                ephemeral: &ephemeral_prompts,
            },
            &conversation_id,
            "fix the parser",
            None,
            b"fix the parser",
            b"fix the parser",
            &ModelInputOrigin::Operator,
        )
        .unwrap();
        let derived = ModelInputOrigin::HarnessRetry {
            parent: Box::new(operator.clone()),
        };
        let retry = begin_model_prompt(
            PromptIngress {
                durable: Some(&store),
                ephemeral: &ephemeral_prompts,
            },
            &conversation_id,
            "fix the parser",
            None,
            b"/exit",
            b"/exit",
            &derived,
        )
        .unwrap();

        assert!(
            !derived.is_operator(),
            "retry must bypass human interceptors"
        );
        assert_eq!(
            retry.submitted_prompt().receipt().origin(),
            newt_core::PromptOrigin::HarnessRetry
        );
        assert_ne!(
            retry.submitted_prompt().id(),
            operator.submitted_prompt().id()
        );
        assert_eq!(
            retry.submitted_prompt().receipt().parent_prompt_id(),
            Some(operator.submitted_prompt().id())
        );
        assert_eq!(
            retry.active_operator_prompt().id(),
            operator.active_operator_prompt().id()
        );
        assert_eq!(
            retry.active_operator_prompt().model_text(),
            b"fix the parser"
        );

        let retry_again = begin_model_prompt(
            PromptIngress {
                durable: Some(&store),
                ephemeral: &ephemeral_prompts,
            },
            &conversation_id,
            "fix the parser",
            None,
            b"ground it again",
            b"ground it again",
            &ModelInputOrigin::HarnessRetry {
                parent: Box::new(retry.clone()),
            },
        )
        .unwrap();
        assert_eq!(
            retry_again.submitted_prompt().receipt().parent_prompt_id(),
            Some(retry.submitted_prompt().id()),
            "attempt ancestry must not be flattened to the operator root"
        );
        assert_eq!(
            retry_again.active_operator_prompt().id(),
            operator.active_operator_prompt().id()
        );
        assert_eq!(
            active_operator_task(Some(&retry_again), "ground it again"),
            "fix the parser",
            "derived memory must never relabel retry prose as the active task"
        );
    }

    #[test]
    fn clarification_answer_is_an_operator_continuation_of_the_pending_objective() {
        let (_tmp, store, conversation_id) = prompt_store();
        let ephemeral_prompts = newt_core::agentic::SessionPromptStore::default();
        let original = begin_model_prompt(
            PromptIngress {
                durable: Some(&store),
                ephemeral: &ephemeral_prompts,
            },
            &conversation_id,
            "implement the selected storage backend",
            None,
            b"implement either SQLite or Postgres",
            b"implement either SQLite or Postgres",
            &ModelInputOrigin::Operator,
        )
        .unwrap();
        let answer_origin = ModelInputOrigin::OperatorContinuation {
            parent: Box::new(original.clone()),
        };
        let answer = begin_model_prompt(
            PromptIngress {
                durable: Some(&store),
                ephemeral: &ephemeral_prompts,
            },
            &conversation_id,
            "implement the selected storage backend",
            None,
            b"1: SQLite",
            b"1: SQLite",
            &answer_origin,
        )
        .unwrap();

        assert!(answer_origin.is_operator());
        assert_eq!(
            answer.submitted_prompt().receipt().parent_prompt_id(),
            Some(original.submitted_prompt().id())
        );
        assert_eq!(
            answer.submitted_prompt().receipt().root_prompt_id(),
            original.submitted_prompt().receipt().root_prompt_id(),
            "a clarification answer must retain the original objective root"
        );
        // bug/steering-regressions (#1443): a clarification/decision CONTINUATION
        // refines the parent objective — it must NOT become the active operator
        // authority. Otherwise the protected prompt card carries the ceremony
        // reply ("1: SQLite") for the whole turn and mid-turn compaction can
        // evict the real task.
        assert_eq!(
            answer.active_operator_prompt().id(),
            original.submitted_prompt().id(),
            "a clarification answer must keep the original objective as active authority"
        );
        assert_ne!(
            answer.active_operator_prompt().id(),
            answer.submitted_prompt().id(),
            "the ceremony reply itself is not the active operator prompt"
        );
    }

    #[test]
    fn durable_pending_clarification_rehydrates_its_lineage_after_resume() {
        let (_tmp, store, conversation_id) = prompt_store();
        let ephemeral_prompts = newt_core::agentic::SessionPromptStore::default();
        let original = begin_model_prompt(
            PromptIngress {
                durable: Some(&store),
                ephemeral: &ephemeral_prompts,
            },
            &conversation_id,
            "select storage",
            None,
            b"Implement either SQLite or Postgres.",
            b"Implement either SQLite or Postgres.",
            &ModelInputOrigin::Operator,
        )
        .unwrap();
        let unclear_answer = begin_model_prompt(
            PromptIngress {
                durable: Some(&store),
                ephemeral: &ephemeral_prompts,
            },
            &conversation_id,
            "select storage",
            None,
            b"continue",
            b"continue",
            &ModelInputOrigin::OperatorContinuation {
                parent: Box::new(original.clone()),
            },
        )
        .unwrap();

        let restored = store
            .turn_prompt_context(&conversation_id, unclear_answer.submitted_prompt().id())
            .unwrap()
            .expect("durable prompt context");
        let pending = rehydrate_pending_clarification(&store, &conversation_id, &restored)
            .unwrap()
            .expect("unclear answer must remain an outstanding clarification");
        assert_eq!(
            pending.parent.submitted_prompt().id(),
            unclear_answer.submitted_prompt().id(),
            "the next answer must descend from the durable latest clarification receipt"
        );
        assert_eq!(
            pending.intake.disposition(),
            newt_core::agentic::PromptDisposition::Ask
        );

        let resolved = begin_model_prompt(
            PromptIngress {
                durable: Some(&store),
                ephemeral: &ephemeral_prompts,
            },
            &conversation_id,
            "select storage",
            None,
            b"1: SQLite",
            b"1: SQLite",
            &ModelInputOrigin::OperatorContinuation {
                parent: pending.parent.clone(),
            },
        )
        .unwrap();
        assert_eq!(
            resolved.submitted_prompt().receipt().root_prompt_id(),
            original.submitted_prompt().receipt().root_prompt_id(),
            "the resumed answer remains in the original objective"
        );
        let resolved_context = store
            .turn_prompt_context(&conversation_id, resolved.submitted_prompt().id())
            .unwrap()
            .expect("durable resolved context");
        assert!(
            rehydrate_pending_clarification(&store, &conversation_id, &resolved_context)
                .unwrap()
                .is_none(),
            "a fully explicit answer must clear the recovered pending state"
        );
    }
}
