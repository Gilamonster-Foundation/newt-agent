//! Always-on exact recovery of the active operator prompt.
//!
//! The chat transcript is a lossy presentation: compaction may summarize or
//! reorder its historical user-role messages. This module instead reads the
//! caller-owned [`crate::ActivePrompt`] and, when available, a conversation-
//! scoped durable source. `prompt_read` is deliberately independent of the
//! optional general-memory disclosure surface.

#[cfg(test)]
use super::display::{print_tool_call, print_tool_output};
use super::prompt_intake::{PromptIntake, PROMPT_COMPREHENSION_MODEL_CARD_PREFIX};
use crate::{
    ConversationStore, NewPrompt, PromptId, PromptOrigin, PromptReceipt, TurnPromptContext,
};
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::Mutex;

/// Marker for the one compression-immune system card carried by every primary
/// model request. Public within the crate so wire tests can assert cardinality.
pub(crate) const ACTIVE_PROMPT_PREFIX: &str = "[NEWT ACTIVE PROMPT v1]";

/// Standing harness policy carried by every protected active-prompt card.
///
/// This is deliberately independent of prompt classification and persona
/// text: Markdown is the display protocol for every answer, and source-first
/// repository investigation is the default evidence policy for every agent
/// surface. Prompt intake may add refinements, but it does not decide whether
/// this policy exists.
const RESPONSE_REPOSITORY_POLICY: &str = "\
[NEWT RESPONSE AND REPOSITORY POLICY v1]\n\
response_format: gfm_markdown\n\
response_structure: adaptive\n\
markdown_instruction: valid GFM; tables for repeated or comparable fields; headings/lists otherwise; no whole-answer fence\n\
repository_evidence: source_first\n\
source_definition: resolved_language_packs\n\
repository_instruction: inspect source first via find category=source; narrow by language; docs/manifests/lockfiles/generated only if requested or necessary; never replace code evidence with metadata";

#[cfg(test)]
pub(crate) fn response_repository_policy_tokens() -> usize {
    crate::tokens::TokenEstimation::default()
        .tokens_for_chars(RESPONSE_REPOSITORY_POLICY.chars().count())
}

const DEFAULT_PROMPT_READ_CHARS: usize = 32_000;
const MAX_PROMPT_READ_CHARS: usize = 100_000;
// Keep ephemeral lineage validation aligned with the durable store's bound.
// This is intentionally finite so adversarial retry ancestry cannot cause
// unbounded traversal or stack growth.
const MAX_SESSION_PROMPT_LINEAGE_DEPTH: usize = 256;

/// Read one immutable prompt receipt by id. Implementations must fence reads to
/// the active conversation, not merely the active workspace.
pub trait PromptSource: Send + Sync {
    fn fetch_prompt(&self, id: PromptId) -> anyhow::Result<Option<PromptReceipt>>;
}

/// Conversation-scoped durable prompt reader used by the TUI.
pub struct StorePromptSource<'a> {
    store: &'a ConversationStore,
    conversation_id: &'a str,
}

impl<'a> StorePromptSource<'a> {
    pub fn new(store: &'a ConversationStore, conversation_id: &'a str) -> Self {
        Self {
            store,
            conversation_id,
        }
    }
}

impl PromptSource for StorePromptSource<'_> {
    fn fetch_prompt(&self, id: PromptId) -> anyhow::Result<Option<PromptReceipt>> {
        self.store
            .load_prompt_in_conversation(self.conversation_id, id)
    }
}

/// Session-local prompt minting and recovery for an explicitly ephemeral run.
///
/// Ephemeral means "no persistence", not "no provenance". This authority
/// serializes every accepted prompt in memory, assigns the same chronological
/// and semantic links as [`ConversationStore::begin_prompt`], and retains the
/// exact receipts until the process exits. A caller can only read through a
/// conversation-bound [`SessionPromptSource`], so changing conversation (for
/// example via `/new`) cannot expose receipts from the previous task.
#[derive(Default)]
pub struct SessionPromptStore {
    state: Mutex<SessionPromptState>,
}

#[derive(Default)]
struct SessionPromptState {
    receipts: HashMap<PromptId, PromptReceipt>,
    latest_by_conversation: HashMap<String, PromptId>,
    tick: i64,
}

impl SessionPromptStore {
    /// Accept one prompt into the in-memory receipt chain before inference.
    ///
    /// This is intentionally the minting authority rather than a passive cache:
    /// two operator submissions receive a real `previous_prompt_id`, and a
    /// retry-of-a-retry can resolve its immediate attempt as both `parent` and
    /// `previous`. No SQLite store is opened or touched.
    pub fn begin_prompt(
        &self,
        conversation_id: &str,
        prompt: NewPrompt,
    ) -> anyhow::Result<TurnPromptContext> {
        if conversation_id.is_empty() {
            anyhow::bail!("ephemeral prompt conversation id cannot be empty");
        }
        std::str::from_utf8(prompt.model_text()).map_err(|error| {
            anyhow::anyhow!(
                "prompt model text is not valid UTF-8 and cannot be sent to inference: {error}"
            )
        })?;

        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("ephemeral prompt store lock was poisoned"))?;
        let parent = match prompt.parent_prompt_id() {
            Some(parent_id) => Some(
                prompt_in_conversation(&state.receipts, conversation_id, parent_id).ok_or_else(
                    || {
                        anyhow::anyhow!(
                            "prompt parent {parent_id} is not in ephemeral conversation \
                             `{conversation_id}`"
                        )
                    },
                )?,
            ),
            None => None,
        };
        if prompt.origin() == PromptOrigin::HarnessRetry && parent.is_none() {
            anyhow::bail!("a harness retry must name an operator-prompt parent");
        }

        let prompt_id = PromptId::new();
        let root_prompt_id = match parent.as_ref() {
            Some(parent) => {
                validate_session_objective_root(
                    &state.receipts,
                    conversation_id,
                    parent.root_prompt_id(),
                )?;
                parent.root_prompt_id()
            }
            None => prompt_id,
        };
        let active_operator = match (prompt.origin(), parent.as_ref()) {
            // A fresh operator submission is its own active authority.
            (PromptOrigin::Operator, None) => None,
            // bug/steering-regressions: an operator CONTINUATION (a decision
            // or clarification reply bound to a parent ask) REFINES the parent
            // objective — it must not usurp it as the active operator prompt.
            // Otherwise the protected active-prompt card carries the ceremony
            // ("1: proceed") for the whole agentic turn and mid-turn
            // compaction evicts the real task (live gpt-4.1 + Qwen3-Coder
            // drives, 2026-07-26/27: post-compaction the goal was gone and
            // both models wandered).
            (PromptOrigin::Operator, Some(parent)) | (PromptOrigin::HarnessRetry, Some(parent)) => {
                let (active, parent_depth) =
                    resolve_session_active_operator(&state.receipts, conversation_id, parent)?;
                if parent_depth >= MAX_SESSION_PROMPT_LINEAGE_DEPTH {
                    anyhow::bail!(
                        "prompt lineage would exceed the maximum prompt lineage depth of \
                         {MAX_SESSION_PROMPT_LINEAGE_DEPTH} receipts"
                    );
                }
                Some(active)
            }
            (PromptOrigin::HarnessRetry, None) => {
                anyhow::bail!("harness retry requires a parent prompt")
            }
        };
        let active_operator_id = active_operator
            .as_ref()
            .map_or(prompt_id, PromptReceipt::id);
        let previous_prompt_id = state.latest_by_conversation.get(conversation_id).copied();
        state.tick = state
            .tick
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("ephemeral prompt sequence exhausted"))?;
        let tick = state.tick;
        let receipt = PromptReceipt::new(
            prompt_id,
            conversation_id.to_string(),
            "ephemeral-session".to_string(),
            tick,
            previous_prompt_id,
            parent.as_ref().map(PromptReceipt::id),
            root_prompt_id,
            active_operator_id,
            prompt.origin,
            prompt.raw_text,
            prompt.model_text,
            tick,
        );
        receipt.verify_integrity()?;
        state.receipts.insert(prompt_id, receipt.clone());
        state
            .latest_by_conversation
            .insert(conversation_id.to_string(), prompt_id);

        Ok(TurnPromptContext::new(
            receipt.clone(),
            active_operator.unwrap_or(receipt),
        ))
    }

    /// Create a read-only view fenced to exactly one ephemeral conversation.
    pub fn source<'a>(&'a self, conversation_id: impl Into<String>) -> SessionPromptSource<'a> {
        SessionPromptSource {
            store: self,
            conversation_id: conversation_id.into(),
        }
    }
}

/// Conversation-fenced reader over a [`SessionPromptStore`].
pub struct SessionPromptSource<'a> {
    store: &'a SessionPromptStore,
    conversation_id: String,
}

impl PromptSource for SessionPromptSource<'_> {
    fn fetch_prompt(&self, id: PromptId) -> anyhow::Result<Option<PromptReceipt>> {
        let state = self
            .store
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("ephemeral prompt store lock was poisoned"))?;
        let receipt = prompt_in_conversation(&state.receipts, &self.conversation_id, id);
        if let Some(receipt) = receipt.as_ref() {
            receipt.verify_integrity()?;
        }
        Ok(receipt)
    }
}

fn prompt_in_conversation(
    receipts: &HashMap<PromptId, PromptReceipt>,
    conversation_id: &str,
    id: PromptId,
) -> Option<PromptReceipt> {
    receipts
        .get(&id)
        .filter(|receipt| receipt.conversation_id() == conversation_id)
        .cloned()
}

fn validate_session_objective_root(
    receipts: &HashMap<PromptId, PromptReceipt>,
    conversation_id: &str,
    root_id: PromptId,
) -> anyhow::Result<PromptReceipt> {
    let root = prompt_in_conversation(receipts, conversation_id, root_id).ok_or_else(|| {
        anyhow::anyhow!(
            "prompt root {root_id} is missing from ephemeral conversation `{conversation_id}`"
        )
    })?;
    root.verify_integrity()?;
    if root.origin() != PromptOrigin::Operator
        || root.root_prompt_id() != root.id()
        || root.active_operator_id() != Some(root.id())
    {
        anyhow::bail!("prompt root {root_id} is not a self-rooted operator prompt");
    }
    Ok(root)
}

fn resolve_session_active_operator(
    receipts: &HashMap<PromptId, PromptReceipt>,
    conversation_id: &str,
    submitted: &PromptReceipt,
) -> anyhow::Result<(PromptReceipt, usize)> {
    validate_session_objective_root(receipts, conversation_id, submitted.root_prompt_id())?;
    let objective_root = submitted.root_prompt_id();
    let mut current = submitted.clone();
    let mut visited = HashSet::new();
    let mut retry_authorities: Vec<(PromptId, Option<PromptId>)> = Vec::new();

    for depth in 1..=MAX_SESSION_PROMPT_LINEAGE_DEPTH {
        current.verify_integrity()?;
        if !visited.insert(current.id()) {
            anyhow::bail!("prompt parent cycle detected at {}", current.id());
        }
        if current.conversation_id() != conversation_id
            || current.root_prompt_id() != objective_root
        {
            anyhow::bail!(
                "prompt {} crosses its conversation or objective-root boundary",
                current.id()
            );
        }
        // A continuation hop: an operator DECISION/CLARIFICATION reply (or a
        // harness retry) names its lineage's authority rather than itself
        // (bug/steering-regressions). Both walk to their parent; only an
        // operator prompt that IS its own authority terminates the walk.
        // Parent-bearing is the primary signal so v1 rows (no persisted
        // pointer) recover the same authority by walking explicit parents.
        let is_operator_continuation = current.origin() == PromptOrigin::Operator
            && current.parent_prompt_id().is_some()
            && current.active_operator_id() != Some(current.id());
        if current.origin() == PromptOrigin::HarnessRetry || is_operator_continuation {
            let parent_id = current.parent_prompt_id().ok_or_else(|| {
                anyhow::anyhow!(
                    "prompt {} continues a lineage but has no parent",
                    current.id()
                )
            })?;
            retry_authorities.push((current.id(), current.active_operator_id()));
            current =
                prompt_in_conversation(receipts, conversation_id, parent_id).ok_or_else(|| {
                    anyhow::anyhow!(
                        "prompt {} references missing parent {parent_id}",
                        current.id()
                    )
                })?;
        } else {
            for (retry_id, stored_authority) in retry_authorities {
                if stored_authority != Some(current.id()) {
                    anyhow::bail!(
                        "prompt {retry_id} active operator disagrees with parent \
                         authority {}",
                        current.id()
                    );
                }
            }
            return Ok((current, depth));
        }
    }

    anyhow::bail!(
        "prompt lineage from {} exceeds the maximum depth of \
         {MAX_SESSION_PROMPT_LINEAGE_DEPTH} receipts",
        submitted.id()
    )
}

/// Per-turn prompt recovery view. `fallback_model_text` keeps headless and
/// ephemeral callers useful even when there is no durable receipt.
#[derive(Clone, Copy)]
pub struct PromptReadContext<'a> {
    turn: Option<&'a TurnPromptContext>,
    fallback_model_text: &'a str,
    source: Option<&'a dyn PromptSource>,
}

impl<'a> PromptReadContext<'a> {
    pub fn new(
        turn: Option<&'a TurnPromptContext>,
        fallback_model_text: &'a str,
        source: Option<&'a dyn PromptSource>,
    ) -> Self {
        Self {
            turn,
            fallback_model_text,
            source,
        }
    }

    pub(crate) fn active_text(self) -> &'a str {
        self.turn
            .and_then(|turn| turn.active_operator_prompt().model_text_utf8().ok())
            .unwrap_or(self.fallback_model_text)
    }

    pub(crate) fn active_receipt(self) -> Option<&'a PromptReceipt> {
        self.turn
            .map(|turn| turn.active_operator_prompt().receipt())
    }

    pub(crate) fn submitted_receipt(self) -> Option<&'a PromptReceipt> {
        self.turn.map(|turn| turn.submitted_prompt().receipt())
    }

    fn resolve_id(self, id: PromptId) -> anyhow::Result<Option<PromptReceipt>> {
        if let Some(receipt) = self
            .submitted_receipt()
            .filter(|receipt| receipt.id() == id)
        {
            return Ok(Some(receipt.clone()));
        }
        if let Some(receipt) = self.active_receipt().filter(|receipt| receipt.id() == id) {
            return Ok(Some(receipt.clone()));
        }
        match self.source {
            Some(source) => source.fetch_prompt(id),
            None => Ok(None),
        }
    }
}

/// Build the model-facing tool schema. It is registered with `Gate::Always`.
pub fn prompt_read_tool_definition() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "prompt_read",
            "description": "Re-read exact prompt text from immutable receipts. Use this after compaction, resume, or whenever the task is unclear instead of guessing. With no address it reads the current active operator prompt. Every result names the selected receipt's origin and previous/parent/root links plus the current submitted receipt metadata. Selectors are `current`/`active`, `submitted`/`request`, `root`, `parent`, `previous`, or an explicit `prompt:<uuid>` handle shown by Newt. Reads are fenced to this active conversation.",
            "parameters": {
                "type": "object",
                "properties": {
                    "address": {
                        "type": "string",
                        "description": "Optional selector: `current`/`active` (default), `submitted`/`request`, `root`, `parent`, `previous`, or an exact `prompt:<uuid>` address from this conversation"
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Zero-based Unicode-character offset for a long prompt (default 0)"
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum Unicode characters to return (default 32000, max 100000)"
                    }
                },
                "required": []
            }
        }
    })
}

/// Insert exactly one protected active-prompt pair after the leading system
/// messages: a metadata-only system card followed by the exact operator text
/// at its original user priority. The adjacency lets compression protect the
/// pair without promoting untrusted operator content into system instructions.
///
/// Optional harness-validated intake adds its content-free comprehension
/// projection inside that same card. Raw operator, atomic-ask, decision, and
/// clarification text remain outside the system card.
pub(crate) fn ensure_active_prompt_card(
    messages: &mut Vec<serde_json::Value>,
    context: PromptReadContext<'_>,
    intake: Option<&PromptIntake>,
) {
    let base_card = active_prompt_card(context);
    let card = match intake {
        Some(intake) => active_prompt_card_with_intake(base_card.clone(), intake),
        None => base_card.clone(),
    };
    insert_active_prompt_card(messages, context, &base_card, card);
}

fn insert_active_prompt_card(
    messages: &mut Vec<serde_json::Value>,
    context: PromptReadContext<'_>,
    base_card: &str,
    card: String,
) {
    let mut cleaned = Vec::with_capacity(messages.len());
    let mut index = 0;
    while index < messages.len() {
        // Remove only this harness instance's exact base/current card or a
        // marker-validated comprehension extension of that exact base. A
        // configured system prompt is untrusted presentation data: matching the
        // public active-prompt prefix alone could mistake it for our card and
        // erase the live user message that follows it.
        let is_owned_card = messages[index]["role"] == "system"
            && messages[index]["content"].as_str().is_some_and(|content| {
                content == base_card
                    || content == card.as_str()
                    || is_augmented_active_prompt_card(content, base_card)
            });
        let is_owned_pair = index + 1 < messages.len()
            && is_owned_card
            && messages[index + 1]["role"] == "user"
            && messages[index + 1]["content"].as_str() == Some(context.active_text());
        if is_owned_pair {
            index += 2;
            continue;
        }
        cleaned.push(messages[index].clone());
        index += 1;
    }
    *messages = cleaned;

    // Keep the memory provider's current-task message in its original tail
    // position. The duplicate near the system prompt is the compression-proof
    // recovery copy; moving the sole copy to the head would make a historical
    // assistant reply the apparent end of a multi-turn conversation.
    let insert_at = messages
        .iter()
        .take_while(|message| message["role"] == "system")
        .count();
    messages.splice(
        insert_at..insert_at,
        [
            serde_json::json!({"role": "system", "content": card}),
            serde_json::json!({"role": "user", "content": context.active_text()}),
        ],
    );
}

fn active_prompt_card_with_intake(base_card: String, intake: &PromptIntake) -> String {
    append_prompt_comprehension_model_card(base_card, &intake.model_card())
}

fn append_prompt_comprehension_model_card(mut active_card: String, model_card: &str) -> String {
    let model_card = model_card.trim();
    if !model_card.is_empty() {
        active_card.push('\n');
        if !has_prompt_comprehension_marker(model_card) {
            active_card.push_str(PROMPT_COMPREHENSION_MODEL_CARD_PREFIX);
            active_card.push('\n');
        }
        active_card.push_str(model_card);
    }
    active_card
}

fn is_augmented_active_prompt_card(content: &str, base_card: &str) -> bool {
    content
        .strip_prefix(base_card)
        .and_then(|suffix| suffix.strip_prefix('\n'))
        .is_some_and(has_prompt_comprehension_marker)
}

fn has_prompt_comprehension_marker(content: &str) -> bool {
    content.lines().next() == Some(PROMPT_COMPREHENSION_MODEL_CARD_PREFIX)
}

pub(crate) fn active_prompt_card(context: PromptReadContext<'_>) -> String {
    let (address, root, digest) = context.active_receipt().map_or_else(
        || {
            (
                "<ephemeral-unrecorded>".to_string(),
                "<ephemeral-unrecorded>".to_string(),
                "<unavailable>".to_string(),
            )
        },
        |receipt| {
            (
                receipt.id().to_string(),
                receipt.root_prompt_id().to_string(),
                receipt.model_digest().to_string(),
            )
        },
    );
    let submitted = context.submitted_receipt();
    let submitted_address = display_optional_prompt_id(submitted.map(PromptReceipt::id));
    let submitted_origin = submitted
        .map(|receipt| prompt_origin_name(receipt.origin()).to_string())
        .unwrap_or_else(|| "null".to_string());
    let submitted_previous =
        display_optional_prompt_id(submitted.and_then(PromptReceipt::previous_prompt_id));
    let submitted_parent =
        display_optional_prompt_id(submitted.and_then(PromptReceipt::parent_prompt_id));
    let submitted_root = display_optional_prompt_id(submitted.map(PromptReceipt::root_prompt_id));
    let submitted_is_plain_operator = submitted.is_some_and(|receipt| {
        context.active_receipt().is_some_and(|active| {
            receipt.id() == active.id()
                && receipt.previous_prompt_id().is_none()
                && receipt.parent_prompt_id().is_none()
        })
    });
    let submitted_block = if submitted_is_plain_operator {
        format!("submitted_origin: {submitted_origin}")
    } else {
        format!(
            "submitted_address: {submitted_address}\n\
             submitted_origin: {submitted_origin}\n\
             submitted_previous_address: {submitted_previous}\n\
             submitted_parent_address: {submitted_parent}\n\
             submitted_root_address: {submitted_root}"
        )
    };
    format!(
        "{ACTIVE_PROMPT_PREFIX}\n\
         address: {address}\n\
         objective_root: {root}\n\
         model_digest: {digest}\n\
         {submitted_block}\n\
         recovery: prompt_read current\n\
         artifact_recovery: for prompt-rooted work, artifact_read {{\"address\":\"root\"}}\n\
         {RESPONSE_REPOSITORY_POLICY}"
    )
}

/// The two projections of a `prompt_read` result.
///
/// `model` contains the exact bounded prompt page. `display` is safe to show in
/// the operator transcript: it carries only receipt and pagination metadata,
/// never the recovered prompt text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PromptReadOutput {
    pub(crate) model: String,
    pub(crate) display: String,
}

impl PromptReadOutput {
    fn shared(output: String) -> Self {
        Self {
            display: output.clone(),
            model: output,
        }
    }
}

fn prompt_read_selector(args: &serde_json::Value) -> Result<&str, String> {
    let selector = match args.get("address") {
        None | Some(serde_json::Value::Null) => "current",
        Some(serde_json::Value::String(value)) if value.trim().is_empty() => "current",
        Some(serde_json::Value::String(value)) => value.trim(),
        Some(_) => return Err(tool_error("prompt_read `address` must be a string")),
    };
    Ok(selector)
}

/// Execute `prompt_read` without writing to the terminal.
///
/// The central tool presenter uses this seam so it can spill the operator-safe
/// `display` projection while returning the complete `model` projection to the
/// inference backend.
pub(crate) fn execute_prompt_read_silent(
    args: &serde_json::Value,
    context: PromptReadContext<'_>,
) -> PromptReadOutput {
    let selector = match prompt_read_selector(args) {
        Ok(selector) => selector,
        Err(output) => return PromptReadOutput::shared(output),
    };

    let selected = match resolve_selector(selector, context) {
        Ok(selected) => selected,
        Err(error) => return PromptReadOutput::shared(tool_error(&error.to_string())),
    };
    let offset = match parse_nonnegative_usize(args, "offset", 0) {
        Ok(offset) => offset,
        Err(error) => return PromptReadOutput::shared(tool_error(&error)),
    };
    let limit = match parse_nonnegative_usize(args, "limit", DEFAULT_PROMPT_READ_CHARS) {
        Ok(0) => {
            return PromptReadOutput::shared(tool_error("prompt_read `limit` must be at least 1"))
        }
        Ok(limit) => limit.min(MAX_PROMPT_READ_CHARS),
        Err(error) => return PromptReadOutput::shared(tool_error(&error)),
    };

    let (address, origin, previous_address, parent_address, root_address, model_digest, model_text) =
        match selected {
            SelectedPrompt::Receipt(receipt) => {
                if let Err(error) = receipt.verify_integrity() {
                    return PromptReadOutput::shared(tool_error(&format!(
                        "prompt_read refused a corrupt receipt: {error}"
                    )));
                }
                let text = match receipt.model_text_utf8() {
                    Ok(text) => text,
                    Err(_) => {
                        return PromptReadOutput::shared(tool_error(&format!(
                            "prompt_read cannot render {} because its model text is not UTF-8",
                            receipt.id()
                        )))
                    }
                };
                (
                    Some(receipt.id()),
                    Some(receipt.origin()),
                    receipt.previous_prompt_id(),
                    receipt.parent_prompt_id(),
                    Some(receipt.root_prompt_id()),
                    Some(receipt.model_digest().to_string()),
                    text.to_string(),
                )
            }
            SelectedPrompt::Fallback(text) => {
                (None, None, None, None, None, None, text.to_string())
            }
        };
    let submitted = context.submitted_receipt();
    let submitted_receipt = serde_json::json!({
        "address": submitted.map(PromptReceipt::id),
        "origin": submitted.map(PromptReceipt::origin),
        "previous_address": submitted.and_then(PromptReceipt::previous_prompt_id),
        "parent_address": submitted.and_then(PromptReceipt::parent_prompt_id),
        "root_address": submitted.map(PromptReceipt::root_prompt_id),
    });
    let total_chars = model_text.chars().count();
    if offset > total_chars {
        return PromptReadOutput::shared(tool_error(&format!(
            "prompt_read offset {offset} is past the prompt's {total_chars} Unicode characters"
        )));
    }
    let page: String = model_text.chars().skip(offset).take(limit).collect();
    let returned_chars = page.chars().count();
    let next = offset + returned_chars;
    let complete = next >= total_chars;
    let display = format!(
        "{}: returned {returned_chars} of {total_chars} Unicode characters at offset {offset}{}",
        address.map_or_else(|| "ephemeral prompt".to_string(), |id| id.to_string()),
        if complete {
            " (complete)".to_string()
        } else {
            format!(" (next offset {next})")
        }
    );
    let output = serde_json::to_string_pretty(&serde_json::json!({
        "address": address,
        "origin": origin,
        "previous_address": previous_address,
        "parent_address": parent_address,
        "root_address": root_address,
        "model_digest": model_digest,
        "submitted_receipt": submitted_receipt,
        "total_bytes": model_text.len(),
        "total_chars": total_chars,
        "offset": offset,
        "returned_chars": returned_chars,
        "complete": complete,
        "next_offset": (!complete).then_some(next),
        "model_text": page,
    }))
    .expect("serializing a prompt_read result cannot fail");
    // The exact page is returned to the model, but never echoed to the
    // terminal as a single escaped JSON line. Besides defeating the normal
    // line cap, that could re-display credentials from an older prompt. The
    // operator-facing trace carries only address and pagination metadata.
    PromptReadOutput {
        model: output,
        display,
    }
}

/// Execute `prompt_read` through the historical direct-printing interface.
/// New dispatch code should prefer [`execute_prompt_read_silent`] and present
/// its two projections at the central tool boundary.
#[cfg(test)]
pub(crate) fn execute_prompt_read(
    args: &serde_json::Value,
    context: PromptReadContext<'_>,
    color: bool,
    tool_output_lines: usize,
) -> String {
    if let Ok(selector) = prompt_read_selector(args) {
        print_tool_call("prompt_read", selector, color);
    }
    let output = execute_prompt_read_silent(args, context);
    print_tool_output(&output.display, tool_output_lines, color);
    output.model
}

enum SelectedPrompt<'a> {
    Receipt(Box<PromptReceipt>),
    Fallback(&'a str),
}

fn resolve_selector<'a>(
    selector: &str,
    context: PromptReadContext<'a>,
) -> anyhow::Result<SelectedPrompt<'a>> {
    let receipt = match selector {
        "current" | "active" => context.active_receipt(),
        "submitted" | "request" => context.submitted_receipt(),
        "root" => match context.submitted_receipt() {
            Some(submitted) => {
                return context
                    .resolve_id(submitted.root_prompt_id())
                    .and_then(|receipt| require_receipt(selector, receipt));
            }
            None => None,
        },
        "parent" => {
            let Some(submitted) = context.submitted_receipt() else {
                anyhow::bail!("prompt_read selector `parent` is unavailable without a receipt");
            };
            let Some(id) = submitted.parent_prompt_id() else {
                anyhow::bail!("prompt {} has no semantic parent", submitted.id());
            };
            return context
                .resolve_id(id)
                .and_then(|receipt| require_receipt(selector, receipt));
        }
        "previous" => {
            let Some(submitted) = context.submitted_receipt() else {
                anyhow::bail!("prompt_read selector `previous` is unavailable without a receipt");
            };
            let Some(id) = submitted.previous_prompt_id() else {
                anyhow::bail!("prompt {} has no chronological predecessor", submitted.id());
            };
            return context
                .resolve_id(id)
                .and_then(|receipt| require_receipt(selector, receipt));
        }
        explicit => {
            let id = PromptId::from_str(explicit).map_err(|_| {
                anyhow::anyhow!(
                    "unknown prompt selector `{explicit}`; expected current, active, submitted, request, root, parent, previous, or prompt:<uuid>"
                )
            })?;
            return context
                .resolve_id(id)
                .and_then(|receipt| require_receipt(explicit, receipt));
        }
    };
    match receipt {
        Some(receipt) => Ok(SelectedPrompt::Receipt(Box::new(receipt.clone()))),
        None if matches!(
            selector,
            "current" | "active" | "submitted" | "request" | "root"
        ) =>
        {
            Ok(SelectedPrompt::Fallback(context.fallback_model_text))
        }
        None => anyhow::bail!("no such prompt `{selector}` in this active conversation"),
    }
}

fn require_receipt<'a>(
    selector: &str,
    receipt: Option<PromptReceipt>,
) -> anyhow::Result<SelectedPrompt<'a>> {
    receipt
        .map(|receipt| SelectedPrompt::Receipt(Box::new(receipt)))
        .ok_or_else(|| anyhow::anyhow!("no such prompt `{selector}` in this active conversation"))
}

fn parse_nonnegative_usize(
    args: &serde_json::Value,
    field: &str,
    default: usize,
) -> Result<usize, String> {
    match args.get(field) {
        None | Some(serde_json::Value::Null) => Ok(default),
        Some(value) => value.as_u64().map_or_else(
            || {
                Err(format!(
                    "prompt_read `{field}` must be a non-negative integer"
                ))
            },
            |value| {
                usize::try_from(value)
                    .map_err(|_| format!("prompt_read `{field}` is too large for this platform"))
            },
        ),
    }
}

fn tool_error(message: &str) -> String {
    format!("prompt_read error: {message}")
}

fn display_optional_prompt_id(id: Option<PromptId>) -> String {
    id.map_or_else(|| "null".to_string(), |id| id.to_string())
}

fn prompt_origin_name(origin: PromptOrigin) -> &'static str {
    match origin {
        PromptOrigin::Operator => "operator",
        PromptOrigin::HarnessRetry => "harness_retry",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NewPrompt;

    fn test_store() -> (tempfile::TempDir, tempfile::TempDir, ConversationStore) {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
        (root, workspace, store)
    }

    fn receipt_context<'a>(
        turn: &'a TurnPromptContext,
        source: &'a dyn PromptSource,
    ) -> PromptReadContext<'a> {
        PromptReadContext::new(Some(turn), "wrong fallback", Some(source))
    }

    #[test]
    fn schema_is_arg_optional_and_names_all_selectors() {
        let def = prompt_read_tool_definition();
        assert_eq!(def["function"]["name"], "prompt_read");
        assert_eq!(
            def["function"]["parameters"]["required"],
            serde_json::json!([])
        );
        let desc = def["function"]["description"].as_str().unwrap();
        for selector in [
            "current",
            "active",
            "submitted",
            "request",
            "root",
            "parent",
            "previous",
        ] {
            assert!(
                desc.contains(selector),
                "missing selector {selector}: {desc}"
            );
        }
        assert!(desc.contains("prompt:<uuid>"));
    }

    #[test]
    fn headless_current_returns_exact_fallback_text() {
        let exact = "first line\nUnicode 🦭\nfinal newline\n";
        let out = execute_prompt_read(
            &serde_json::json!({}),
            PromptReadContext::new(None, exact, None),
            false,
            20,
        );
        let json: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(json["model_text"], exact);
        assert_eq!(json["complete"], true);
        assert_eq!(json["address"], serde_json::Value::Null);
    }

    #[test]
    fn silent_prompt_read_separates_model_payload_from_safe_display() {
        let exact = "operator secret that must not be echoed";
        let presented = execute_prompt_read_silent(
            &serde_json::json!({}),
            PromptReadContext::new(None, exact, None),
        );

        let model: serde_json::Value = serde_json::from_str(&presented.model).unwrap();
        assert_eq!(model["model_text"], exact);
        assert_eq!(
            presented.display,
            "ephemeral prompt: returned 39 of 39 Unicode characters at offset 0 (complete)"
        );
        assert!(!presented.display.contains(exact));
    }

    #[test]
    fn ephemeral_source_preserves_operator_chronology_and_chained_retry_ancestry() {
        // No ConversationStore or filesystem fixture exists in this test: the
        // complete receipt chain is session-local memory.
        let store = SessionPromptStore::default();
        let first = store
            .begin_prompt("conv", NewPrompt::operator("raw one", "FIRST exact"))
            .unwrap();
        let second = store
            .begin_prompt("conv", NewPrompt::operator("raw two", "SECOND exact"))
            .unwrap();
        assert_eq!(
            second.submitted().receipt().previous_prompt_id(),
            Some(first.submitted().id())
        );

        let source = store.source("conv");
        let second_context = PromptReadContext::new(Some(&second), "wrong", Some(&source));
        let previous: serde_json::Value = serde_json::from_str(&execute_prompt_read(
            &serde_json::json!({"address":"previous"}),
            second_context,
            false,
            20,
        ))
        .unwrap();
        assert_eq!(previous["model_text"], "FIRST exact");

        let retry_one = store
            .begin_prompt(
                "conv",
                NewPrompt::harness_retry(
                    "retry one raw",
                    "RETRY ONE exact",
                    second.submitted().id(),
                ),
            )
            .unwrap();
        let retry_two = store
            .begin_prompt(
                "conv",
                NewPrompt::harness_retry(
                    "retry two raw",
                    "RETRY TWO exact",
                    retry_one.submitted().id(),
                ),
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
        assert_eq!(retry_two.active().id(), second.submitted().id());

        let retry_context = PromptReadContext::new(Some(&retry_two), "wrong", Some(&source));
        for selector in ["previous", "parent"] {
            let selected: serde_json::Value = serde_json::from_str(&execute_prompt_read(
                &serde_json::json!({"address": selector}),
                retry_context,
                false,
                20,
            ))
            .unwrap();
            assert_eq!(selected["model_text"], "RETRY ONE exact", "{selector}");
            assert_eq!(
                selected["address"],
                retry_one.submitted().id().to_string(),
                "{selector}"
            );
        }
        let current: serde_json::Value = serde_json::from_str(&execute_prompt_read(
            &serde_json::json!({"address":"current"}),
            retry_context,
            false,
            20,
        ))
        .unwrap();
        assert_eq!(current["model_text"], "SECOND exact");
    }

    #[test]
    fn ephemeral_source_and_ancestry_are_conversation_fenced() {
        let store = SessionPromptStore::default();
        let old = store
            .begin_prompt("old-conversation", NewPrompt::operator("old", "OLD SECRET"))
            .unwrap();
        let fresh = store
            .begin_prompt("fresh-conversation", NewPrompt::operator("new", "FRESH"))
            .unwrap();
        let fresh_source = store.source("fresh-conversation");

        assert!(fresh_source
            .fetch_prompt(old.submitted().id())
            .unwrap()
            .is_none());
        assert_eq!(
            fresh_source
                .fetch_prompt(fresh.submitted().id())
                .unwrap()
                .unwrap()
                .model_text(),
            b"FRESH"
        );

        let error = store
            .begin_prompt(
                "fresh-conversation",
                NewPrompt::harness_retry("retry", "retry", old.submitted().id()),
            )
            .unwrap_err();
        assert!(error.to_string().contains("not in ephemeral conversation"));
    }

    #[test]
    fn ephemeral_retry_lineage_is_iterative_and_bounded_like_durable_lineage() {
        let store = SessionPromptStore::default();
        let mut context = store
            .begin_prompt("conv", NewPrompt::operator("root", "ROOT"))
            .unwrap();
        for attempt in 1..MAX_SESSION_PROMPT_LINEAGE_DEPTH {
            context = store
                .begin_prompt(
                    "conv",
                    NewPrompt::harness_retry(
                        format!("retry {attempt}"),
                        format!("RETRY {attempt}"),
                        context.submitted().id(),
                    ),
                )
                .unwrap();
        }

        let error = store
            .begin_prompt(
                "conv",
                NewPrompt::harness_retry("one too many", "ONE TOO MANY", context.submitted().id()),
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("maximum prompt lineage depth of 256"),
            "{error}"
        );
    }

    #[test]
    fn durable_current_returns_verified_handle_root_digest_and_exact_text() {
        let (_root, _workspace, store) = test_store();
        let turn = store
            .begin_prompt(
                "conv-a",
                "title",
                None,
                NewPrompt::operator("raw", "exact model text\n"),
            )
            .unwrap();
        let source = StorePromptSource::new(&store, "conv-a");
        let active = turn.active_operator_prompt();
        let out = execute_prompt_read(
            &serde_json::json!({"address": "current"}),
            receipt_context(&turn, &source),
            false,
            20,
        );
        let json: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(json["address"], active.id().to_string());
        assert_eq!(json["origin"], "operator");
        assert_eq!(json["previous_address"], serde_json::Value::Null);
        assert_eq!(json["parent_address"], serde_json::Value::Null);
        assert_eq!(json["root_address"], active.root_prompt_id().to_string());
        assert_eq!(json["model_digest"], active.model_digest());
        assert_eq!(
            json["submitted_receipt"]["address"],
            active.id().to_string()
        );
        assert_eq!(json["submitted_receipt"]["origin"], "operator");
        assert_eq!(json["model_text"], "exact model text\n");
        assert_eq!(json["total_bytes"], 17);
    }

    #[test]
    fn explicit_address_cannot_escape_the_bound_conversation() {
        let (_root, _workspace, store) = test_store();
        let a = store
            .begin_prompt("conv-a", "A", None, NewPrompt::operator("a", "secret-a"))
            .unwrap();
        let b = store
            .begin_prompt("conv-b", "B", None, NewPrompt::operator("b", "secret-b"))
            .unwrap();
        let source = StorePromptSource::new(&store, "conv-a");
        let out = execute_prompt_read(
            &serde_json::json!({"address": b.active().id().to_string()}),
            receipt_context(&a, &source),
            false,
            20,
        );
        assert!(out.contains("no such prompt"), "{out}");
        assert!(!out.contains("secret-b"), "{out}");
    }

    #[test]
    fn long_prompt_paginates_without_splitting_unicode() {
        let exact = "🦭".repeat(DEFAULT_PROMPT_READ_CHARS + 3);
        let context = PromptReadContext::new(None, &exact, None);
        let first = execute_prompt_read(&serde_json::json!({}), context, false, 20);
        let first: serde_json::Value = serde_json::from_str(&first).unwrap();
        assert_eq!(
            first["model_text"].as_str().unwrap().chars().count(),
            DEFAULT_PROMPT_READ_CHARS
        );
        assert_eq!(first["complete"], false);
        assert_eq!(first["next_offset"], DEFAULT_PROMPT_READ_CHARS);

        let tail = execute_prompt_read(
            &serde_json::json!({"offset": DEFAULT_PROMPT_READ_CHARS, "limit": 10}),
            context,
            false,
            20,
        );
        let tail: serde_json::Value = serde_json::from_str(&tail).unwrap();
        assert_eq!(tail["model_text"].as_str().unwrap(), "🦭🦭🦭");
        assert_eq!(tail["complete"], true);
    }

    #[test]
    fn protected_pair_is_single_user_priority_prompt_and_uses_active_operator_not_retry() {
        let (_root, _workspace, store) = test_store();
        let operator = store
            .begin_prompt(
                "conv",
                "title",
                None,
                NewPrompt::operator("operator", "DO THE OPERATOR TASK\nexactly"),
            )
            .unwrap();
        let retry = store
            .begin_prompt(
                "conv",
                "title",
                None,
                NewPrompt::harness_retry("retry", "act now", operator.active().id()),
            )
            .unwrap();
        let source = StorePromptSource::new(&store, "conv");
        let context = receipt_context(&retry, &source);
        let mut messages = vec![
            serde_json::json!({"role":"system", "content":"base"}),
            serde_json::json!({"role":"user", "content":"old ask"}),
            serde_json::json!({"role":"user", "content":"act now"}),
        ];

        ensure_active_prompt_card(&mut messages, context, None);
        ensure_active_prompt_card(&mut messages, context, None);

        let cards: Vec<_> = messages
            .iter()
            .filter(|m| {
                m["content"]
                    .as_str()
                    .is_some_and(|s| s.starts_with(ACTIVE_PROMPT_PREFIX))
            })
            .collect();
        assert_eq!(cards.len(), 1);
        assert_eq!(messages[1]["role"], "system");
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[2]["content"], "DO THE OPERATOR TASK\nexactly");
        let card = cards[0]["content"].as_str().unwrap();
        assert!(!card.contains("DO THE OPERATOR TASK"), "{card}");
        assert!(!card.contains("act now"), "{card}");
        assert!(card.contains(&operator.active().id().to_string()), "{card}");
        assert!(card.contains(operator.active().model_digest()), "{card}");
        assert!(
            card.contains(&format!("submitted_address: {}", retry.submitted().id())),
            "{card}"
        );
        assert!(card.contains("submitted_origin: harness_retry"), "{card}");
        assert!(
            card.contains("artifact_read {\"address\":\"root\"}"),
            "{card}"
        );
        assert!(
            card.contains(&format!(
                "submitted_parent_address: {}",
                operator.submitted().id()
            )),
            "{card}"
        );
    }

    #[test]
    fn comprehension_projection_replaces_base_and_prior_projection_in_one_active_card() {
        let exact = "PRIVATE OPERATOR PROMPT that remains at user priority";
        let context = PromptReadContext::new(None, exact, None);
        let base_card = active_prompt_card(context);
        let research_card = append_prompt_comprehension_model_card(
            base_card.clone(),
            &format!(
                "{PROMPT_COMPREHENSION_MODEL_CARD_PREFIX}\n\
                 disposition: research\n\
                 atomic_ask_count: 2\n\
                 decision_count: 1"
            ),
        );
        let act_card = append_prompt_comprehension_model_card(
            base_card.clone(),
            &format!(
                "{PROMPT_COMPREHENSION_MODEL_CARD_PREFIX}\n\
                 disposition: act\n\
                 atomic_ask_count: 2\n\
                 decision_count: 1"
            ),
        );
        let mut messages = vec![
            serde_json::json!({"role":"system", "content":"base"}),
            serde_json::json!({"role":"user", "content":exact}),
        ];

        ensure_active_prompt_card(&mut messages, context, None);
        insert_active_prompt_card(&mut messages, context, &base_card, research_card);
        insert_active_prompt_card(&mut messages, context, &base_card, act_card);

        let cards: Vec<_> = messages
            .iter()
            .filter(|message| {
                message["role"] == "system"
                    && message["content"]
                        .as_str()
                        .is_some_and(|content| content.starts_with(ACTIVE_PROMPT_PREFIX))
            })
            .collect();
        assert_eq!(cards.len(), 1, "{messages:#?}");
        let card = cards[0]["content"].as_str().unwrap();
        assert!(
            card.contains(PROMPT_COMPREHENSION_MODEL_CARD_PREFIX),
            "{card}"
        );
        assert!(card.contains("disposition: act"), "{card}");
        assert!(!card.contains("disposition: research"), "{card}");
        assert!(!card.contains(exact), "{card}");
        assert_eq!(
            messages
                .iter()
                .filter(|message| {
                    message["role"] == "system"
                        && message["content"].as_str().is_some_and(|content| {
                            content.starts_with(PROMPT_COMPREHENSION_MODEL_CARD_PREFIX)
                        })
                })
                .count(),
            0,
            "the comprehension projection is nested, never a second system card"
        );
        assert_eq!(messages[1]["role"], "system");
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[2]["content"], exact);
        assert_eq!(
            messages
                .iter()
                .filter(|message| {
                    message["role"] == "user" && message["content"].as_str() == Some(exact)
                })
                .count(),
            2,
            "one protected recovery copy plus the tail presentation copy"
        );
    }

    #[test]
    fn active_prompt_card_always_steers_markdown_and_source_first_repository_evidence() {
        let card = active_prompt_card(PromptReadContext::new(
            None,
            "How does authentication work in this repository?",
            None,
        ));

        assert!(
            card.contains("response_format: gfm_markdown"),
            "all agent surfaces must target the Markdown renderer even without prompt intake: \
             {card}"
        );
        assert!(
            card.contains("response_structure: adaptive")
                && card.contains("tables for repeated or comparable fields"),
            "the harness should choose Markdown structure by result semantics, not incident \
             keywords: {card}"
        );
        assert!(
            card.contains("repository_evidence: source_first")
                && card.contains("source_definition: resolved_language_packs")
                && card.contains("find category=source"),
            "general repository investigation must begin with harness-defined code files: {card}"
        );
        assert!(
            card.contains("docs/manifests/lockfiles/generated")
                && card.contains("requested or necessary"),
            "metadata remains available as supporting evidence without replacing code: {card}"
        );
    }

    #[test]
    fn intake_helper_nests_content_free_projection_without_exposing_asks_or_clarification() {
        let exact = "Implement either PRIVATE_SQLITE_ALPHA or SECRET_POSTGRES_BETA; \
                     create PRIVATE_MIGRATION_GAMMA and open a PR.";
        let intake = PromptIntake::analyze(exact);
        let clarification = intake.clarification_batch();
        assert!(clarification.contains("PRIVATE_SQLITE_ALPHA"));
        assert!(clarification.contains("SECRET_POSTGRES_BETA"));
        assert_eq!(intake.atomic_asks().len(), 2);

        let context = PromptReadContext::new(None, exact, None);
        let mut messages = vec![
            serde_json::json!({"role":"system", "content":"base"}),
            serde_json::json!({"role":"user", "content":exact}),
        ];

        // Intake runs after receipt/card creation. Repeated insertion models a
        // retry or compression rebuild and must still replace, not accumulate.
        ensure_active_prompt_card(&mut messages, context, None);
        ensure_active_prompt_card(&mut messages, context, Some(&intake));
        ensure_active_prompt_card(&mut messages, context, Some(&intake));

        let cards = messages
            .iter()
            .filter(|message| {
                message["role"] == "system"
                    && message["content"]
                        .as_str()
                        .is_some_and(|content| content.starts_with(ACTIVE_PROMPT_PREFIX))
            })
            .collect::<Vec<_>>();
        assert_eq!(cards.len(), 1, "{messages:#?}");
        assert_eq!(
            messages
                .iter()
                .filter(|message| message["role"] == "system")
                .count(),
            2,
            "base system prompt plus one protected active-prompt card"
        );

        let card = cards[0]["content"].as_str().unwrap();
        assert!(card.starts_with(ACTIVE_PROMPT_PREFIX), "{card}");
        assert_eq!(
            card.matches(PROMPT_COMPREHENSION_MODEL_CARD_PREFIX).count(),
            1,
            "{card}"
        );
        assert!(card.contains("disposition: ask"), "{card}");
        assert!(card.contains("atomic_ask_count: 2"), "{card}");
        for private_text in [
            exact,
            "PRIVATE_SQLITE_ALPHA",
            "SECRET_POSTGRES_BETA",
            "PRIVATE_MIGRATION_GAMMA",
            clarification.as_str(),
        ] {
            assert!(!card.contains(private_text), "{card}");
        }
        for ask in intake.atomic_asks() {
            assert!(!card.contains(ask.text()), "{card}");
        }

        assert_eq!(messages[1]["role"], "system");
        assert_eq!(
            messages[2],
            serde_json::json!({"role":"user", "content":exact})
        );
        assert_eq!(
            messages.last(),
            Some(&serde_json::json!({"role":"user", "content":exact}))
        );
    }

    #[test]
    fn user_content_that_starts_with_harness_marker_is_never_dropped() {
        let exact = "operator task";
        let context = PromptReadContext::new(None, exact, None);
        let user_data = format!("{ACTIVE_PROMPT_PREFIX} but this is user data");
        let mut messages = vec![
            serde_json::json!({"role":"system", "content":"base"}),
            serde_json::json!({"role":"user", "content":user_data}),
        ];

        ensure_active_prompt_card(&mut messages, context, None);
        ensure_active_prompt_card(&mut messages, context, None);

        assert!(messages.iter().any(|message| {
            message["role"] == "user"
                && message["content"]
                    .as_str()
                    .is_some_and(|content| content.ends_with("but this is user data"))
        }));
        assert_eq!(
            messages
                .iter()
                .filter(|message| {
                    message["role"] == "system"
                        && message["content"]
                            .as_str()
                            .is_some_and(|content| content.starts_with(ACTIVE_PROMPT_PREFIX))
                })
                .count(),
            1
        );
        assert_eq!(
            messages[2],
            serde_json::json!({"role":"user", "content":exact})
        );
    }

    #[test]
    fn protected_copy_does_not_move_the_newest_operator_turn_out_of_the_tail() {
        let current = "modify the source and open the approved PRs";
        let context = PromptReadContext::new(None, current, None);
        let mut messages = vec![
            serde_json::json!({"role":"system", "content":"base"}),
            serde_json::json!({"role":"user", "content":"can you access MCP?"}),
            serde_json::json!({"role":"assistant", "content":"yes"}),
            serde_json::json!({"role":"user", "content":current}),
        ];

        ensure_active_prompt_card(&mut messages, context, None);
        ensure_active_prompt_card(&mut messages, context, None);

        assert_eq!(
            messages.last(),
            Some(&serde_json::json!({"role":"user", "content":current})),
            "the presentation copy remains the newest conversational turn"
        );
        assert_eq!(
            messages
                .iter()
                .filter(|message| {
                    message["role"] == "user" && message["content"].as_str() == Some(current)
                })
                .count(),
            2,
            "one protected recovery copy plus one tail presentation copy"
        );
    }

    #[test]
    fn prefix_colliding_system_prompt_cannot_own_or_erase_the_live_ask() {
        let current = "this live ask must remain at the tail";
        let context = PromptReadContext::new(None, current, None);
        let collision = format!("{ACTIVE_PROMPT_PREFIX}\noperator-configured text");
        let mut messages = vec![
            serde_json::json!({"role":"system", "content":collision}),
            serde_json::json!({"role":"user", "content":current}),
        ];

        ensure_active_prompt_card(&mut messages, context, None);
        ensure_active_prompt_card(&mut messages, context, None);

        assert!(messages.iter().any(|message| {
            message["role"] == "system" && message["content"].as_str() == Some(collision.as_str())
        }));
        assert_eq!(
            messages.last(),
            Some(&serde_json::json!({"role":"user", "content":current}))
        );
    }

    #[test]
    fn retry_context_resolves_every_selector_from_submitted_receipt() {
        let (_root, _workspace, store) = test_store();
        let root = store
            .begin_prompt("conv", "title", None, NewPrompt::operator("one", "ROOT"))
            .unwrap();
        let prior = store
            .begin_prompt("conv", "title", None, NewPrompt::operator("two", "PRIOR"))
            .unwrap();
        let retry = store
            .begin_prompt(
                "conv",
                "title",
                None,
                NewPrompt::harness_retry("retry", "RETRY", root.submitted().id()),
            )
            .unwrap();
        let source = StorePromptSource::new(&store, "conv");
        let context = receipt_context(&retry, &source);

        let read = |address: &str| {
            let output =
                execute_prompt_read(&serde_json::json!({"address": address}), context, false, 20);
            serde_json::from_str::<serde_json::Value>(&output).unwrap()
        };
        for selector in ["current", "active", "root", "parent"] {
            assert_eq!(read(selector)["model_text"], "ROOT", "{selector}");
        }
        for selector in ["submitted", "request"] {
            assert_eq!(read(selector)["model_text"], "RETRY", "{selector}");
        }
        assert_eq!(read("previous")["model_text"], "PRIOR");
        assert_eq!(
            read(&prior.submitted().id().to_string())["model_text"],
            "PRIOR"
        );
    }

    #[test]
    fn prompt_only_restart_then_continue_exposes_and_resolves_previous_without_replay() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let conversation_id = "prompt-only-restart";
        let original_text = "ORIGINAL multi-part prompt\nwith exact Unicode: 🦭\n";

        let original_id = {
            let store = ConversationStore::new(state.path(), workspace.path(), 100).unwrap();
            let accepted = store
                .begin_prompt(
                    conversation_id,
                    "unfinished prompt",
                    None,
                    NewPrompt::operator(original_text.as_bytes(), original_text.as_bytes()),
                )
                .unwrap();
            assert!(store.load(conversation_id).unwrap().turns.is_empty());
            accepted.submitted().id()
        };

        // Reopen the store to model a process restart. Rehydrating the receipt
        // does not turn it into presentation history or queued input.
        let reopened = ConversationStore::new(state.path(), workspace.path(), 100).unwrap();
        let restored = reopened
            .latest_prompt(conversation_id)
            .unwrap()
            .expect("prompt-only receipt survives restart");
        assert_eq!(restored.id(), original_id);
        assert!(reopened.load(conversation_id).unwrap().turns.is_empty());

        // The next operator input is current authority, and its automatic
        // chronological link makes the interrupted prompt discoverable without
        // replaying or guessing an opaque id.
        let current = reopened
            .begin_prompt(
                conversation_id,
                "ignored for existing conversation",
                None,
                NewPrompt::operator("continue", "continue"),
            )
            .unwrap();
        assert_eq!(
            current.submitted().receipt().previous_prompt_id(),
            Some(original_id)
        );
        assert!(
            reopened.load(conversation_id).unwrap().turns.is_empty(),
            "receipts never auto-execute or synthesize a completed turn"
        );

        let source = StorePromptSource::new(&reopened, conversation_id);
        let context = receipt_context(&current, &source);
        let mut messages = vec![
            serde_json::json!({"role":"system", "content":"base"}),
            serde_json::json!({"role":"user", "content":"continue"}),
        ];
        ensure_active_prompt_card(&mut messages, context, None);
        let card = messages[1]["content"].as_str().unwrap();
        assert!(
            card.contains(&format!("submitted_previous_address: {original_id}")),
            "{card}"
        );
        assert!(
            !card.contains(original_text),
            "card must remain metadata-only"
        );
        assert_eq!(
            messages[2],
            serde_json::json!({"role":"user", "content":"continue"}),
            "the protected prompt remains at user priority"
        );

        let current_json: serde_json::Value = serde_json::from_str(&execute_prompt_read(
            &serde_json::json!({"address":"current"}),
            context,
            false,
            20,
        ))
        .unwrap();
        assert_eq!(current_json["previous_address"], original_id.to_string());
        assert_eq!(
            current_json["submitted_receipt"]["previous_address"],
            original_id.to_string()
        );

        let previous_json: serde_json::Value = serde_json::from_str(&execute_prompt_read(
            &serde_json::json!({"address":"previous"}),
            context,
            false,
            20,
        ))
        .unwrap();
        assert_eq!(previous_json["address"], original_id.to_string());
        assert_eq!(previous_json["origin"], "operator");
        assert_eq!(previous_json["model_text"], original_text);
    }
}
