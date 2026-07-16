//! Always-on, read-only recovery of prompt-rooted work artifacts.
//!
//! Artifact storage is an append-only provenance ledger, not a second chat
//! transcript. This module exposes only a bounded projection of that ledger:
//! immutable metadata plus bounded internal artifact bodies. Raw tool streams
//! are intentionally absent from [`ArtifactReadRecord`].
//!
//! The security boundary lives in [`ArtifactSource`]. Implementations are
//! constructed for one conversation inside one already-opened workspace store;
//! no model-supplied conversation or workspace key is accepted by this API.

use super::display::{print_tool_call, print_tool_output};
use crate::artifact::{ArtifactId, NewPromptArtifact, PromptArtifact};
use crate::{ConversationStore, PromptId, TurnPromptContext};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Mutex;

const DEFAULT_ARTIFACT_PAGE_ITEMS: usize = 20;
const MAX_ARTIFACT_PAGE_ITEMS: usize = 50;
const INDEX_BODY_PREVIEW_CHARS: usize = 512;
const DEFAULT_ARTIFACT_BODY_CHARS: usize = 8_000;
const MAX_ARTIFACT_BODY_CHARS: usize = 32_000;
const MAX_ARTIFACT_METADATA_CHARS: usize = 4_000;
const MAX_ARTIFACT_FIELD_CHARS: usize = 2_048;
const MAX_ARTIFACT_INDEX_PROJECTION_BYTES: usize = 100_000;
const SESSION_ARTIFACT_GENESIS_PREFIX: &[u8] = b"newt-prompt-artifact-chain-genesis:v1";

/// Read-only model-facing projection of one immutable artifact.
///
/// This type deliberately has no tool-output or event-stream field. `body` is
/// reserved for bounded harness-authored state such as a plan revision or
/// compaction checkpoint. File and commit artifacts should use locators and
/// digests rather than copying file contents or command output into `body`.
#[derive(Debug, Clone, PartialEq)]
pub struct ArtifactReadRecord {
    pub id: ArtifactId,
    pub prompt_id: PromptId,
    pub root_prompt_id: PromptId,
    pub writer_fingerprint: String,
    pub seq: u64,
    pub prev_hash: String,
    pub kind: String,
    pub relation: String,
    pub locator: Option<String>,
    pub body: Option<String>,
    pub metadata: Value,
    pub ts_claim: i64,
    pub artifact_hash: String,
}

/// One source-bounded artifact index page.
#[derive(Debug, Clone, PartialEq)]
pub struct ArtifactPage {
    pub records: Vec<ArtifactReadRecord>,
    pub total: usize,
}

/// Conversation- and workspace-fenced access to immutable artifacts.
///
/// Implementations MUST bind the conversation and workspace when the source is
/// constructed. A missing record therefore means "not present in this active
/// fence", even if the same id exists in another conversation or workspace.
/// Implementations MUST NOT return raw tool streams in an artifact body or its
/// metadata. The executor defensively re-applies all output bounds.
pub trait ArtifactSource: Send + Sync {
    /// Page artifacts whose immediate origin is `prompt_id`.
    fn list_for_prompt(
        &self,
        prompt_id: PromptId,
        offset: usize,
        limit: usize,
    ) -> anyhow::Result<ArtifactPage>;

    /// Page all artifacts descended from the objective `root_prompt_id`.
    fn list_for_root(
        &self,
        root_prompt_id: PromptId,
        offset: usize,
        limit: usize,
    ) -> anyhow::Result<ArtifactPage>;

    /// Fetch one artifact by immutable address inside the active fence.
    fn fetch_artifact(&self, id: ArtifactId) -> anyhow::Result<Option<ArtifactReadRecord>>;
}

/// Append-only writer for derived work rooted in a durable prompt receipt.
///
/// The caller supplies both the submitted receipt that caused the work and
/// the already-validated objective root. Implementations must reject a write
/// that would cross their conversation/workspace fence or silently change the
/// supplied root.
pub trait PromptArtifactSink: Send + Sync {
    fn append_artifact(
        &self,
        originating_prompt_id: PromptId,
        objective_root_id: PromptId,
        artifact: NewPromptArtifact,
    ) -> anyhow::Result<ArtifactReadRecord>;
}

/// Persistent read/write adapter fenced to one conversation in one
/// workspace-bound [`ConversationStore`].
pub struct StoreArtifactStore<'a> {
    store: &'a ConversationStore,
    conversation_id: String,
}

impl<'a> StoreArtifactStore<'a> {
    pub fn new(store: &'a ConversationStore, conversation_id: impl Into<String>) -> Self {
        Self {
            store,
            conversation_id: conversation_id.into(),
        }
    }

    pub fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    fn page_from(
        &self,
        page: anyhow::Result<(Vec<PromptArtifact>, usize)>,
    ) -> anyhow::Result<ArtifactPage> {
        let (records, total) = page?;
        let records = records
            .into_iter()
            .map(ArtifactReadRecord::try_from)
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(ArtifactPage { records, total })
    }
}

impl PromptArtifactSink for StoreArtifactStore<'_> {
    fn append_artifact(
        &self,
        originating_prompt_id: PromptId,
        objective_root_id: PromptId,
        artifact: NewPromptArtifact,
    ) -> anyhow::Result<ArtifactReadRecord> {
        // Verify the harness-owned root before taking the write path. Prompt
        // receipts are immutable, so the store can safely derive the same root
        // again inside its atomic append transaction.
        let prompt = self
            .store
            .load_prompt_in_conversation(&self.conversation_id, originating_prompt_id)?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "prompt {originating_prompt_id} is not in active conversation `{}`",
                    self.conversation_id
                )
            })?;
        prompt.verify_integrity()?;
        if prompt.root_prompt_id() != objective_root_id {
            anyhow::bail!(
                "prompt {originating_prompt_id} is rooted at {}, not {objective_root_id}",
                prompt.root_prompt_id()
            );
        }
        let record = self.store.append_prompt_artifact(
            &self.conversation_id,
            originating_prompt_id,
            artifact,
        )?;
        if record.prompt_id() != originating_prompt_id
            || record.root_prompt_id() != objective_root_id
        {
            anyhow::bail!(
                "artifact store changed provenance for {} after append",
                record.id()
            );
        }
        ArtifactReadRecord::try_from(record)
    }
}

impl ArtifactSource for StoreArtifactStore<'_> {
    fn list_for_prompt(
        &self,
        prompt_id: PromptId,
        offset: usize,
        limit: usize,
    ) -> anyhow::Result<ArtifactPage> {
        self.page_from(self.store.page_prompt_artifacts_for_prompt(
            &self.conversation_id,
            prompt_id,
            offset,
            limit,
        ))
    }

    fn list_for_root(
        &self,
        root_prompt_id: PromptId,
        offset: usize,
        limit: usize,
    ) -> anyhow::Result<ArtifactPage> {
        self.page_from(self.store.page_prompt_artifacts_for_root(
            &self.conversation_id,
            root_prompt_id,
            offset,
            limit,
        ))
    }

    fn fetch_artifact(&self, id: ArtifactId) -> anyhow::Result<Option<ArtifactReadRecord>> {
        self.store
            .load_prompt_artifact(&self.conversation_id, id)?
            .map(ArtifactReadRecord::try_from)
            .transpose()
    }
}

/// In-memory append/read parity for explicitly ephemeral conversations.
///
/// One instance is bound to exactly one conversation. It mints the same
/// immutable [`PromptArtifact`] records as the SQLite adapter and verifies the
/// complete hash chain before every read. Ephemeral means "not persisted",
/// not "unrooted" or "unfenced".
pub struct SessionArtifactStore {
    conversation_id: String,
    state: Mutex<SessionArtifactState>,
}

#[derive(Default)]
struct SessionArtifactState {
    artifacts: Vec<PromptArtifact>,
    roots_by_prompt: HashMap<PromptId, PromptId>,
    tick: i64,
}

impl SessionArtifactStore {
    /// Create an empty ledger fenced to `conversation_id`.
    pub fn new(conversation_id: impl Into<String>) -> anyhow::Result<Self> {
        let conversation_id = conversation_id.into();
        if conversation_id.is_empty() {
            anyhow::bail!("ephemeral artifact conversation id cannot be empty");
        }
        Ok(Self {
            conversation_id,
            state: Mutex::new(SessionArtifactState::default()),
        })
    }

    /// Conversation fence carried by this session-local ledger.
    pub fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    fn verify_chain(&self, state: &SessionArtifactState) -> anyhow::Result<()> {
        let mut expected_prev = artifact_genesis_hash(&self.conversation_id);
        for (index, artifact) in state.artifacts.iter().enumerate() {
            artifact.verify_integrity()?;
            if artifact.conversation_id() != self.conversation_id {
                anyhow::bail!(
                    "ephemeral artifact {} crossed its conversation fence",
                    artifact.id()
                );
            }
            let expected_seq = i64::try_from(index)
                .ok()
                .and_then(|index| index.checked_add(1))
                .ok_or_else(|| anyhow::anyhow!("ephemeral artifact sequence overflow"))?;
            if artifact.seq() != expected_seq {
                anyhow::bail!(
                    "ephemeral artifact {} has sequence {}, expected {expected_seq}",
                    artifact.id(),
                    artifact.seq()
                );
            }
            if artifact.prev_hash() != expected_prev {
                anyhow::bail!(
                    "ephemeral artifact {} chain predecessor mismatch",
                    artifact.id()
                );
            }
            if state.roots_by_prompt.get(&artifact.prompt_id()) != Some(&artifact.root_prompt_id())
            {
                anyhow::bail!("ephemeral artifact {} root index mismatch", artifact.id());
            }
            expected_prev = artifact.artifact_hash().to_string();
        }
        Ok(())
    }

    fn page(
        &self,
        offset: usize,
        limit: usize,
        keep: impl Fn(&PromptArtifact) -> bool,
    ) -> anyhow::Result<ArtifactPage> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("ephemeral artifact store lock was poisoned"))?;
        self.verify_chain(&state)?;
        let selected: Vec<_> = state
            .artifacts
            .iter()
            .filter(|artifact| keep(artifact))
            .collect();
        let total = selected.len();
        let records = selected
            .into_iter()
            .skip(offset)
            .take(limit)
            .cloned()
            .map(ArtifactReadRecord::try_from)
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(ArtifactPage { records, total })
    }
}

impl PromptArtifactSink for SessionArtifactStore {
    fn append_artifact(
        &self,
        originating_prompt_id: PromptId,
        objective_root_id: PromptId,
        artifact: NewPromptArtifact,
    ) -> anyhow::Result<ArtifactReadRecord> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("ephemeral artifact store lock was poisoned"))?;
        self.verify_chain(&state)?;
        if let Some(recorded_root) = state.roots_by_prompt.get(&originating_prompt_id) {
            if *recorded_root != objective_root_id {
                anyhow::bail!(
                    "prompt {originating_prompt_id} is already rooted at {recorded_root}, not {objective_root_id}"
                );
            }
        }
        let seq = state
            .tick
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("ephemeral artifact sequence exhausted"))?;
        let prev_hash = state.artifacts.last().map_or_else(
            || artifact_genesis_hash(&self.conversation_id),
            |artifact| artifact.artifact_hash().to_string(),
        );
        let record = PromptArtifact::mint(
            ArtifactId::new(),
            self.conversation_id.clone(),
            "ephemeral-session".to_string(),
            seq,
            prev_hash,
            originating_prompt_id,
            objective_root_id,
            artifact,
            seq,
        )?;
        record.verify_integrity()?;
        state
            .roots_by_prompt
            .insert(originating_prompt_id, objective_root_id);
        state.tick = seq;
        state.artifacts.push(record.clone());
        ArtifactReadRecord::try_from(record)
    }
}

impl ArtifactSource for SessionArtifactStore {
    fn list_for_prompt(
        &self,
        prompt_id: PromptId,
        offset: usize,
        limit: usize,
    ) -> anyhow::Result<ArtifactPage> {
        self.page(offset, limit, |artifact| artifact.prompt_id() == prompt_id)
    }

    fn list_for_root(
        &self,
        root_prompt_id: PromptId,
        offset: usize,
        limit: usize,
    ) -> anyhow::Result<ArtifactPage> {
        self.page(offset, limit, |artifact| {
            artifact.root_prompt_id() == root_prompt_id
        })
    }

    fn fetch_artifact(&self, id: ArtifactId) -> anyhow::Result<Option<ArtifactReadRecord>> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("ephemeral artifact store lock was poisoned"))?;
        self.verify_chain(&state)?;
        state
            .artifacts
            .iter()
            .find(|artifact| artifact.id() == id)
            .cloned()
            .map(ArtifactReadRecord::try_from)
            .transpose()
    }
}

impl TryFrom<PromptArtifact> for ArtifactReadRecord {
    type Error = anyhow::Error;

    fn try_from(artifact: PromptArtifact) -> Result<Self, Self::Error> {
        artifact.verify_integrity()?;
        let kind = enum_name(artifact.kind())?;
        let relation = enum_name(artifact.relation())?;
        Ok(Self {
            id: artifact.id(),
            prompt_id: artifact.prompt_id(),
            root_prompt_id: artifact.root_prompt_id(),
            writer_fingerprint: artifact.writer_fingerprint().to_string(),
            seq: u64::try_from(artifact.seq())
                .map_err(|_| anyhow::anyhow!("prompt artifact sequence cannot be negative"))?,
            prev_hash: artifact.prev_hash().to_string(),
            kind,
            relation,
            locator: artifact.locator().map(str::to_string),
            body: artifact.body().map(str::to_string),
            metadata: artifact.metadata().clone(),
            ts_claim: artifact.ts_claim(),
            artifact_hash: artifact.artifact_hash().to_string(),
        })
    }
}

/// Per-turn authority for artifact retrieval.
#[derive(Clone, Copy)]
pub struct ArtifactReadContext<'a> {
    originating_prompt_id: Option<PromptId>,
    active_prompt_id: Option<PromptId>,
    root_prompt_id: Option<PromptId>,
    source: Option<&'a dyn ArtifactSource>,
}

impl<'a> ArtifactReadContext<'a> {
    /// Build a context from explicit, harness-owned prompt identities.
    pub fn new(
        originating_prompt_id: Option<PromptId>,
        active_prompt_id: Option<PromptId>,
        root_prompt_id: Option<PromptId>,
        source: Option<&'a dyn ArtifactSource>,
    ) -> Self {
        Self {
            originating_prompt_id,
            active_prompt_id,
            root_prompt_id,
            source,
        }
    }

    /// Bind a source to the active operator prompt and its objective root.
    pub fn from_turn(turn: &'a TurnPromptContext, source: Option<&'a dyn ArtifactSource>) -> Self {
        let active = turn.active_operator_prompt();
        Self::new(
            Some(turn.submitted_prompt().id()),
            Some(active.id()),
            Some(active.root_prompt_id()),
            source,
        )
    }

    /// Receipt that caused work in this turn, including a submitted harness
    /// retry. Artifact writers must use this id rather than silently
    /// reparenting retry work to the active operator receipt.
    pub fn originating_prompt_id(self) -> Option<PromptId> {
        self.originating_prompt_id
    }

    /// Prompt whose immediate derived-work index `current` reads.
    pub fn active_prompt_id(self) -> Option<PromptId> {
        self.active_prompt_id
    }

    /// Objective root whose full derived-work index `root` reads.
    pub fn root_prompt_id(self) -> Option<PromptId> {
        self.root_prompt_id
    }

    /// The already-fenced backing source, if this session has one.
    pub fn source(self) -> Option<&'a dyn ArtifactSource> {
        self.source
    }
}

/// Build the always-advertised model-facing schema for artifact recovery.
pub fn artifact_read_tool_definition() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "artifact_read",
            "description": "Inspect immutable work artifacts derived from prompts. With no address, returns a bounded index for the current active operator prompt. Use `root` for the current objective lineage, `root:<uuid>` for an earlier objective root returned by prompt_read, `prompt:<uuid>` for one receipt's immediate artifacts, or `artifact:<uuid>` for one artifact with a paged body. `root:prompt:<uuid>` is also accepted when copying a prompt address verbatim. Reads are fenced to this conversation and workspace. Results contain metadata and bounded harness-authored bodies only; raw tool streams are never stored here.",
            "parameters": {
                "type": "object",
                "properties": {
                    "address": {
                        "type": "string",
                        "description": "Optional selector: `current`/`active` (default), `root`, an earlier objective as `root:<uuid>` or `root:prompt:<uuid>`, an exact `prompt:<uuid>` receipt address, or an exact `artifact:<uuid>` address"
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Artifact-index offset for current/root listings (default 0)"
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_ARTIFACT_PAGE_ITEMS,
                        "description": "Maximum index entries for current/root listings (default 20, max 50)"
                    },
                    "body_offset": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Unicode-character offset into an explicit artifact body (default 0)"
                    },
                    "body_limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_ARTIFACT_BODY_CHARS,
                        "description": "Maximum Unicode characters from an explicit artifact body (default 8000, max 32000)"
                    }
                },
                "required": []
            }
        }
    })
}

/// Execute one bounded artifact read.
///
/// The source has already applied its conversation/workspace fence. This
/// function applies independent output caps so a buggy or adversarial source
/// cannot turn an index read into an unbounded context injection.
pub(crate) fn execute_artifact_read(
    args: &Value,
    context: ArtifactReadContext<'_>,
    color: bool,
    tool_output_lines: usize,
) -> String {
    let address = match optional_string(args, "address", "current") {
        Ok("") => "current",
        Ok(address) => address,
        Err(error) => return tool_error(&error),
    };
    print_tool_call("artifact_read", address, color);

    let Some(source) = context.source() else {
        return tool_error("no artifact source in this session");
    };
    let output = match address {
        "current" | "active" => {
            execute_index(args, context.active_prompt_id(), false, "current", source)
        }
        "root" => execute_index(args, context.root_prompt_id(), true, "root", source),
        explicit if explicit.starts_with("root:") => {
            let root_address = explicit
                .strip_prefix("root:")
                .expect("the match guard verified the prefix");
            match PromptId::from_str(root_address) {
                Ok(root_prompt_id) => {
                    execute_index(args, Some(root_prompt_id), true, "root", source)
                }
                Err(_) => Err(format!(
                    "invalid artifact root selector `{explicit}`; expected root:<uuid> or root:prompt:<uuid>"
                )),
            }
        }
        explicit => match ArtifactId::from_str(explicit) {
            Ok(id) => execute_explicit(args, id, source),
            Err(_) => match PromptId::from_str(explicit) {
                Ok(prompt_id) if explicit.starts_with("prompt:") => {
                    execute_index(args, Some(prompt_id), false, "prompt", source)
                }
                _ => Err(format!(
                    "unknown artifact selector `{explicit}`; expected current, active, root, root:<uuid>, root:prompt:<uuid>, prompt:<uuid>, or artifact:<uuid>"
                )),
            },
        },
    };

    match output {
        Ok((model_output, display)) => {
            print_tool_output(&display, tool_output_lines, color);
            model_output
        }
        Err(error) => {
            let output = tool_error(&error);
            print_tool_output(&output, tool_output_lines, color);
            output
        }
    }
}

fn execute_index(
    args: &Value,
    prompt_id: Option<PromptId>,
    root: bool,
    selector: &str,
    source: &dyn ArtifactSource,
) -> Result<(String, String), String> {
    let prompt_id = prompt_id.ok_or_else(|| {
        format!("artifact_read selector `{selector}` has no active prompt receipt")
    })?;
    let offset = parse_nonnegative_usize(args, "offset", 0)?;
    let limit = parse_nonnegative_usize(args, "limit", DEFAULT_ARTIFACT_PAGE_ITEMS)?;
    if limit == 0 {
        return Err("artifact_read `limit` must be at least 1".to_string());
    }
    let limit = limit.min(MAX_ARTIFACT_PAGE_ITEMS);
    let page = if root {
        source.list_for_root(prompt_id, offset, limit)
    } else {
        source.list_for_prompt(prompt_id, offset, limit)
    }
    .map_err(|error| format!("artifact source failed: {error}"))?;

    // Never trust a source to honor the requested page cap. Total is metadata,
    // not authority; make it at least consistent with the returned slice.
    let available = page.records.len().min(limit);
    let mut projected_bytes = 0usize;
    let mut records = Vec::with_capacity(available);
    for record in page.records.iter().take(limit) {
        let belongs = if root {
            record.root_prompt_id == prompt_id
        } else {
            record.prompt_id == prompt_id
        };
        if !belongs {
            return Err(format!(
                "artifact source returned {} outside the `{selector}` prompt fence",
                record.id
            ));
        }
        let projected = index_projection(record);
        let encoded_bytes = serde_json::to_vec(&projected)
            .expect("serializing one artifact projection cannot fail")
            .len();
        if projected_bytes.saturating_add(encoded_bytes) > MAX_ARTIFACT_INDEX_PROJECTION_BYTES {
            break;
        }
        projected_bytes = projected_bytes.saturating_add(encoded_bytes);
        records.push(projected);
    }
    let returned = records.len();
    let output_truncated_by_byte_cap = returned < available;
    let minimum_total = offset.saturating_add(returned);
    let total = page.total.max(minimum_total);
    let next_offset = offset.saturating_add(returned);
    let complete = next_offset >= total || returned == 0;
    let output = serde_json::to_string_pretty(&json!({
        "selector": selector,
        "prompt_address": prompt_id,
        "offset": offset,
        "limit": limit,
        "returned": returned,
        "total": total,
        "complete": complete,
        "next_offset": (!complete).then_some(next_offset),
        "output_truncated_by_byte_cap": output_truncated_by_byte_cap,
        "artifacts": records,
    }))
    .expect("serializing an artifact index cannot fail");
    let display = format!(
        "artifact {selector}: returned {returned} of {total} records at offset {offset}{}",
        if complete {
            " (complete)".to_string()
        } else {
            format!(" (next offset {next_offset})")
        }
    );
    Ok((output, display))
}

fn execute_explicit(
    args: &Value,
    id: ArtifactId,
    source: &dyn ArtifactSource,
) -> Result<(String, String), String> {
    let record = source
        .fetch_artifact(id)
        .map_err(|error| format!("artifact source failed: {error}"))?
        .ok_or_else(|| format!("no artifact {id} in this active conversation and workspace"))?;
    if record.id != id {
        return Err(format!(
            "artifact source returned {} for requested {id}",
            record.id
        ));
    }
    let body_offset = parse_nonnegative_usize(args, "body_offset", 0)?;
    let body_limit = parse_nonnegative_usize(args, "body_limit", DEFAULT_ARTIFACT_BODY_CHARS)?;
    if body_limit == 0 {
        return Err("artifact_read `body_limit` must be at least 1".to_string());
    }
    let body_limit = body_limit.min(MAX_ARTIFACT_BODY_CHARS);
    let body = record.body.as_deref().unwrap_or("");
    let total_body_chars = body.chars().count();
    if body_offset > total_body_chars {
        return Err(format!(
            "artifact_read body_offset {body_offset} is past {id}'s {total_body_chars} Unicode characters"
        ));
    }
    let body_page: String = body.chars().skip(body_offset).take(body_limit).collect();
    let returned_body_chars = body_page.chars().count();
    let next_body_offset = body_offset.saturating_add(returned_body_chars);
    let body_complete = next_body_offset >= total_body_chars;
    let output = serde_json::to_string_pretty(&json!({
        "artifact": explicit_projection(&record, body_page),
        "total_body_chars": total_body_chars,
        "body_offset": body_offset,
        "returned_body_chars": returned_body_chars,
        "body_complete": body_complete,
        "next_body_offset": (!body_complete).then_some(next_body_offset),
    }))
    .expect("serializing an artifact read cannot fail");
    let display = format!(
        "{id}: returned {returned_body_chars} of {total_body_chars} body characters at offset {body_offset}{}",
        if body_complete {
            " (complete)".to_string()
        } else {
            format!(" (next body_offset {next_body_offset})")
        }
    );
    Ok((output, display))
}

fn index_projection(record: &ArtifactReadRecord) -> Value {
    let body = record.body.as_deref().unwrap_or("");
    let total_body_chars = body.chars().count();
    let body_preview: String = body.chars().take(INDEX_BODY_PREVIEW_CHARS).collect();
    json!({
        "address": record.id,
        "prompt_address": record.prompt_id,
        "root_address": record.root_prompt_id,
        "writer_fingerprint": bounded_field(&record.writer_fingerprint),
        "seq": record.seq,
        "prev_hash": bounded_field(&record.prev_hash),
        "kind": bounded_field(&record.kind),
        "relation": bounded_field(&record.relation),
        "locator": record.locator.as_deref().map(bounded_field),
        "body_preview": body_preview,
        "body_total_chars": total_body_chars,
        "body_preview_complete": total_body_chars <= INDEX_BODY_PREVIEW_CHARS,
        "metadata": bounded_metadata(&record.metadata),
        "ts_claim": record.ts_claim,
        "artifact_hash": bounded_field(&record.artifact_hash),
    })
}

fn explicit_projection(record: &ArtifactReadRecord, body: String) -> Value {
    json!({
        "address": record.id,
        "prompt_address": record.prompt_id,
        "root_address": record.root_prompt_id,
        "writer_fingerprint": bounded_field(&record.writer_fingerprint),
        "seq": record.seq,
        "prev_hash": bounded_field(&record.prev_hash),
        "kind": bounded_field(&record.kind),
        "relation": bounded_field(&record.relation),
        // An explicit artifact address is the lossless recovery path. Stored
        // locators are already capped at append time, so return the complete
        // value here; only the bounded index preview may abbreviate it.
        "locator": record.locator.clone(),
        "body": body,
        "metadata": bounded_metadata(&record.metadata),
        "ts_claim": record.ts_claim,
        "artifact_hash": bounded_field(&record.artifact_hash),
    })
}

fn bounded_metadata(metadata: &Value) -> Value {
    let encoded = serde_json::to_string(metadata).unwrap_or_else(|_| "null".to_string());
    let total_chars = encoded.chars().count();
    if total_chars <= MAX_ARTIFACT_METADATA_CHARS {
        return metadata.clone();
    }
    json!({
        "omitted": true,
        "reason": "metadata exceeds artifact_read output bound",
        "total_chars": total_chars,
        "blake3": blake3::hash(encoded.as_bytes()).to_hex().to_string(),
    })
}

fn enum_name(value: impl serde::Serialize) -> anyhow::Result<String> {
    serde_json::to_value(value)?
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("artifact enum did not serialize as a string"))
}

fn artifact_genesis_hash(conversation_id: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(SESSION_ARTIFACT_GENESIS_PREFIX);
    hasher.update(&(conversation_id.len() as u64).to_le_bytes());
    hasher.update(conversation_id.as_bytes());
    hasher.finalize().to_hex().to_string()
}

fn bounded_field(value: &str) -> String {
    let total = value.chars().count();
    if total <= MAX_ARTIFACT_FIELD_CHARS {
        return value.to_string();
    }
    let mut bounded: String = value.chars().take(MAX_ARTIFACT_FIELD_CHARS).collect();
    bounded.push_str(&format!("… [truncated; {total} characters total]"));
    bounded
}

fn optional_string<'a>(args: &'a Value, field: &str, default: &'a str) -> Result<&'a str, String> {
    match args.get(field) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::String(value)) => Ok(value.trim()),
        Some(_) => Err(format!("artifact_read `{field}` must be a string")),
    }
}

fn parse_nonnegative_usize(args: &Value, field: &str, default: usize) -> Result<usize, String> {
    match args.get(field) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value.as_u64().map_or_else(
            || {
                Err(format!(
                    "artifact_read `{field}` must be a non-negative integer"
                ))
            },
            |value| {
                usize::try_from(value)
                    .map_err(|_| format!("artifact_read `{field}` is too large for this platform"))
            },
        ),
    }
}

fn tool_error(message: &str) -> String {
    format!("error: artifact_read: {message}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use uuid::Uuid;

    #[derive(Default)]
    struct MockSource {
        records: Mutex<HashMap<ArtifactId, ArtifactReadRecord>>,
    }

    impl MockSource {
        fn insert(&self, record: ArtifactReadRecord) {
            self.records.lock().unwrap().insert(record.id, record);
        }
    }

    impl ArtifactSource for MockSource {
        fn list_for_prompt(
            &self,
            prompt_id: PromptId,
            offset: usize,
            limit: usize,
        ) -> anyhow::Result<ArtifactPage> {
            self.page(offset, limit, |record| record.prompt_id == prompt_id)
        }

        fn list_for_root(
            &self,
            root_prompt_id: PromptId,
            offset: usize,
            limit: usize,
        ) -> anyhow::Result<ArtifactPage> {
            self.page(offset, limit, |record| {
                record.root_prompt_id == root_prompt_id
            })
        }

        fn fetch_artifact(&self, id: ArtifactId) -> anyhow::Result<Option<ArtifactReadRecord>> {
            Ok(self.records.lock().unwrap().get(&id).cloned())
        }
    }

    impl MockSource {
        fn page(
            &self,
            offset: usize,
            limit: usize,
            keep: impl Fn(&ArtifactReadRecord) -> bool,
        ) -> anyhow::Result<ArtifactPage> {
            let records = self.records.lock().unwrap();
            let mut selected: Vec<_> = records
                .values()
                .filter(|record| keep(record))
                .cloned()
                .collect();
            selected.sort_by_key(|record| record.seq);
            let total = selected.len();
            Ok(ArtifactPage {
                records: selected.into_iter().skip(offset).take(limit).collect(),
                total,
            })
        }
    }

    fn artifact_id(n: u128) -> ArtifactId {
        ArtifactId::from_uuid(Uuid::from_u128(n))
    }

    fn prompt_id(n: u128) -> PromptId {
        PromptId::from_uuid(Uuid::from_u128(n))
    }

    fn record(id: u128, prompt: PromptId, root: PromptId, seq: u64) -> ArtifactReadRecord {
        ArtifactReadRecord {
            id: artifact_id(id),
            prompt_id: prompt,
            root_prompt_id: root,
            writer_fingerprint: "test-writer".to_string(),
            seq,
            prev_hash: format!("prev-{seq}"),
            kind: "plan_revision".to_string(),
            relation: "updates".to_string(),
            locator: Some("plan:active".to_string()),
            body: Some(format!("body-{seq}")),
            metadata: json!({"revision": seq}),
            ts_claim: seq as i64,
            artifact_hash: format!("hash-{seq}"),
        }
    }

    #[test]
    fn definition_is_always_on_shape_with_only_fenced_selectors() {
        let definition = artifact_read_tool_definition();
        assert_eq!(definition["function"]["name"], "artifact_read");
        let properties = &definition["function"]["parameters"]["properties"];
        assert!(properties.get("workspace").is_none());
        assert!(properties.get("conversation").is_none());
        assert_eq!(properties["limit"]["maximum"], MAX_ARTIFACT_PAGE_ITEMS);
        let address_help = properties["address"]["description"].as_str().unwrap();
        assert!(address_help.contains("root:<uuid>"), "{address_help}");
        assert!(
            address_help.contains("root:prompt:<uuid>"),
            "{address_help}"
        );
    }

    #[test]
    fn retry_context_keeps_submitted_origin_distinct_from_active_authority() {
        let operator = TurnPromptContext::ephemeral_operator("c", b"raw", b"operator");
        let retry = TurnPromptContext::ephemeral_harness_retry(
            "c",
            b"retry raw",
            b"retry model",
            &operator,
        )
        .unwrap();
        let context = ArtifactReadContext::from_turn(&retry, None);
        assert_eq!(
            context.originating_prompt_id(),
            Some(retry.submitted_prompt().id())
        );
        assert_eq!(
            context.active_prompt_id(),
            Some(operator.active_operator_prompt().id())
        );
        assert_eq!(
            context.root_prompt_id(),
            Some(operator.active_operator_prompt().root_prompt_id())
        );
    }

    #[test]
    fn session_store_preserves_submitted_origin_and_objective_root() {
        let root = prompt_id(1);
        let retry = prompt_id(2);
        let store = SessionArtifactStore::new("conversation-a").unwrap();
        let written = store
            .append_artifact(
                retry,
                root,
                NewPromptArtifact::new(
                    crate::artifact::ArtifactKind::PlanRevision,
                    crate::artifact::ArtifactRelation::Updates,
                )
                .with_body("locked plan"),
            )
            .unwrap();
        assert_eq!(written.prompt_id, retry);
        assert_eq!(written.root_prompt_id, root);
        assert_eq!(store.list_for_prompt(root, 0, 20).unwrap().total, 0);
        assert_eq!(store.list_for_prompt(retry, 0, 20).unwrap().total, 1);
        assert_eq!(store.list_for_root(root, 0, 20).unwrap().total, 1);
        assert_eq!(store.fetch_artifact(written.id).unwrap().unwrap(), written);
    }

    #[test]
    fn session_store_rejects_silent_reparenting() {
        let prompt = prompt_id(1);
        let store = SessionArtifactStore::new("conversation-a").unwrap();
        let artifact = || {
            NewPromptArtifact::new(
                crate::artifact::ArtifactKind::Decision,
                crate::artifact::ArtifactRelation::DerivedFrom,
            )
        };
        store
            .append_artifact(prompt, prompt_id(2), artifact())
            .unwrap();
        let error = store
            .append_artifact(prompt, prompt_id(3), artifact())
            .unwrap_err();
        assert!(error.to_string().contains("already rooted"));
    }

    #[test]
    fn persistent_adapter_returns_snapshot_total_and_rejects_wrong_root() {
        let root_dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let conversation_id = "artifact-adapter";
        let store = ConversationStore::new(root_dir.path(), workspace.path(), 100).unwrap();
        let turn = store
            .begin_prompt(
                conversation_id,
                "artifact adapter",
                None,
                crate::NewPrompt::operator(b"raw".to_vec(), b"model".to_vec()),
            )
            .unwrap();
        let prompt = turn.submitted_prompt().id();
        let adapter = StoreArtifactStore::new(&store, conversation_id);
        let content = || {
            NewPromptArtifact::new(
                crate::artifact::ArtifactKind::Decision,
                crate::artifact::ArtifactRelation::DerivedFrom,
            )
        };

        let error = adapter
            .append_artifact(prompt, prompt_id(999), content())
            .unwrap_err();
        assert!(error.to_string().contains("is rooted at"));
        assert_eq!(store.count_prompt_artifacts(conversation_id).unwrap(), 0);

        adapter.append_artifact(prompt, prompt, content()).unwrap();
        adapter.append_artifact(prompt, prompt, content()).unwrap();
        let page = adapter.list_for_prompt(prompt, 0, 1).unwrap();
        assert_eq!(page.records.len(), 1);
        assert_eq!(page.total, 2);
    }

    #[test]
    fn current_and_root_indexes_use_distinct_prompt_authority() {
        let root = prompt_id(1);
        let active = prompt_id(2);
        let sibling = prompt_id(3);
        let prior_root = prompt_id(5);
        let prior_child = prompt_id(6);
        let source = MockSource::default();
        source.insert(record(11, active, root, 1));
        source.insert(record(12, sibling, root, 2));
        source.insert(record(13, prompt_id(4), prompt_id(4), 3));
        source.insert(record(14, prior_child, prior_root, 4));
        let context =
            ArtifactReadContext::new(Some(active), Some(active), Some(root), Some(&source));

        let current: Value =
            serde_json::from_str(&execute_artifact_read(&json!({}), context, false, 20)).unwrap();
        assert_eq!(current["returned"], 1);
        assert_eq!(
            current["artifacts"][0]["address"],
            artifact_id(11).to_string()
        );

        let root_page: Value = serde_json::from_str(&execute_artifact_read(
            &json!({"address": "root"}),
            context,
            false,
            20,
        ))
        .unwrap();
        assert_eq!(root_page["returned"], 2);
        assert_eq!(root_page["total"], 2);

        // A model can follow `prompt_read previous` with this exact receipt
        // address to recover an approved plan from an earlier objective
        // without the harness guessing semantic parentage.
        let explicit_prompt: Value = serde_json::from_str(&execute_artifact_read(
            &json!({"address": sibling.to_string()}),
            context,
            false,
            20,
        ))
        .unwrap();
        assert_eq!(explicit_prompt["selector"], "prompt");
        assert_eq!(explicit_prompt["returned"], 1);
        assert_eq!(
            explicit_prompt["artifacts"][0]["address"],
            artifact_id(12).to_string()
        );

        // `root` means this turn's objective. An explicit root selector lets
        // the model follow a prior prompt_read result without asking the
        // harness to relabel that old objective as current. Accept both the
        // compact UUID form and a verbatim `prompt:<uuid>` address.
        for address in [
            format!("root:{}", prior_root.as_uuid()),
            format!("root:{prior_root}"),
        ] {
            let explicit_root: Value = serde_json::from_str(&execute_artifact_read(
                &json!({"address": address}),
                context,
                false,
                20,
            ))
            .unwrap();
            assert_eq!(explicit_root["selector"], "root");
            assert_eq!(explicit_root["prompt_address"], prior_root.to_string());
            assert_eq!(explicit_root["returned"], 1);
            assert_eq!(
                explicit_root["artifacts"][0]["address"],
                artifact_id(14).to_string()
            );
        }
    }

    #[test]
    fn malformed_explicit_root_reports_the_supported_root_addresses() {
        let prompt = prompt_id(1);
        let source = MockSource::default();
        let context =
            ArtifactReadContext::new(Some(prompt), Some(prompt), Some(prompt), Some(&source));
        let malformed =
            execute_artifact_read(&json!({"address": "root:not-a-uuid"}), context, false, 20);
        assert!(malformed.starts_with("error: artifact_read: invalid artifact root selector"));
        assert!(malformed.contains("root:<uuid>"), "{malformed}");
        assert!(malformed.contains("root:prompt:<uuid>"), "{malformed}");

        let unknown =
            execute_artifact_read(&json!({"address": "objective:unknown"}), context, false, 20);
        assert!(unknown.contains("root:<uuid>"), "{unknown}");
        assert!(unknown.contains("artifact:<uuid>"), "{unknown}");
    }

    #[test]
    fn index_paginates_and_caps_a_misbehaving_source() {
        struct GreedySource(Vec<ArtifactReadRecord>);
        impl ArtifactSource for GreedySource {
            fn list_for_prompt(
                &self,
                _: PromptId,
                _: usize,
                _: usize,
            ) -> anyhow::Result<ArtifactPage> {
                Ok(ArtifactPage {
                    records: self.0.clone(),
                    total: self.0.len(),
                })
            }
            fn list_for_root(
                &self,
                id: PromptId,
                offset: usize,
                limit: usize,
            ) -> anyhow::Result<ArtifactPage> {
                self.list_for_prompt(id, offset, limit)
            }
            fn fetch_artifact(&self, _: ArtifactId) -> anyhow::Result<Option<ArtifactReadRecord>> {
                Ok(None)
            }
        }

        let prompt = prompt_id(1);
        let source = GreedySource(
            (1..=80)
                .map(|seq| record(seq as u128, prompt, prompt, seq))
                .collect(),
        );
        let output: Value = serde_json::from_str(&execute_artifact_read(
            &json!({"limit": 1_000}),
            ArtifactReadContext::new(Some(prompt), Some(prompt), Some(prompt), Some(&source)),
            false,
            20,
        ))
        .unwrap();
        assert_eq!(output["limit"], MAX_ARTIFACT_PAGE_ITEMS);
        assert_eq!(output["returned"], MAX_ARTIFACT_PAGE_ITEMS);
        assert_eq!(output["next_offset"], MAX_ARTIFACT_PAGE_ITEMS);
    }

    #[test]
    fn explicit_read_pages_unicode_body_and_never_has_raw_stream_field() {
        let prompt = prompt_id(1);
        let mut artifact = record(7, prompt, prompt, 1);
        artifact.body = Some("aé🦀z".to_string());
        let source = MockSource::default();
        source.insert(artifact);
        let output: Value = serde_json::from_str(&execute_artifact_read(
            &json!({
                "address": artifact_id(7).to_string(),
                "body_offset": 1,
                "body_limit": 2
            }),
            ArtifactReadContext::new(Some(prompt), Some(prompt), Some(prompt), Some(&source)),
            false,
            20,
        ))
        .unwrap();
        assert_eq!(output["artifact"]["body"], "é🦀");
        assert_eq!(output["returned_body_chars"], 2);
        assert_eq!(output["next_body_offset"], 3);
        assert!(output["artifact"].get("tool_output").is_none());
        assert!(output["artifact"].get("events").is_none());
    }

    #[test]
    fn explicit_read_returns_complete_stored_locator_after_bounded_index_preview() {
        let prompt = prompt_id(1);
        let mut artifact = record(7, prompt, prompt, 1);
        let locator = format!("workspace/{}", "x".repeat(MAX_ARTIFACT_FIELD_CHARS + 256));
        artifact.locator = Some(locator.clone());
        let source = MockSource::default();
        source.insert(artifact);
        let context =
            ArtifactReadContext::new(Some(prompt), Some(prompt), Some(prompt), Some(&source));

        let index: Value =
            serde_json::from_str(&execute_artifact_read(&json!({}), context, false, 20)).unwrap();
        assert_ne!(index["artifacts"][0]["locator"], locator);
        assert!(index["artifacts"][0]["locator"]
            .as_str()
            .is_some_and(|value| value.contains("truncated")));

        let explicit: Value = serde_json::from_str(&execute_artifact_read(
            &json!({"address": artifact_id(7).to_string()}),
            context,
            false,
            20,
        ))
        .unwrap();
        assert_eq!(explicit["artifact"]["locator"], locator);
    }

    #[test]
    fn oversized_metadata_is_replaced_by_digest_not_leaked() {
        let prompt = prompt_id(1);
        let mut artifact = record(7, prompt, prompt, 1);
        let secret = "sensitive-tool-stream".repeat(MAX_ARTIFACT_METADATA_CHARS);
        artifact.metadata = json!({"raw": secret});
        let source = MockSource::default();
        source.insert(artifact);
        let output = execute_artifact_read(
            &json!({"address": artifact_id(7).to_string()}),
            ArtifactReadContext::new(Some(prompt), Some(prompt), Some(prompt), Some(&source)),
            false,
            20,
        );
        assert!(!output.contains("sensitive-tool-stream"));
        let output: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(output["artifact"]["metadata"]["omitted"], true);
        assert!(output["artifact"]["metadata"]["blake3"].is_string());
    }

    #[test]
    fn absent_source_and_cross_fence_missing_id_fail_closed() {
        let prompt = prompt_id(1);
        let absent = execute_artifact_read(
            &json!({}),
            ArtifactReadContext::new(Some(prompt), Some(prompt), Some(prompt), None),
            false,
            20,
        );
        assert_eq!(
            absent,
            "error: artifact_read: no artifact source in this session"
        );

        let missing = execute_artifact_read(
            &json!({"address": artifact_id(99).to_string()}),
            ArtifactReadContext::new(
                Some(prompt),
                Some(prompt),
                Some(prompt),
                Some(&MockSource::default()),
            ),
            false,
            20,
        );
        assert!(missing.contains("active conversation and workspace"));
    }
}
