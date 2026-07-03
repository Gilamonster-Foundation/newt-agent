//! Shared conversation types and free functions.
//!
//! Until step 17.1b this module also housed the original JSON-file
//! `ConversationStore` (one pretty-printed record per conversation under
//! `<root>/conversations/<workspace-uuid>/<id>.json`). That write path is
//! gone: the SQLite store at [`crate::store::ConversationStore`] is the only
//! backend, and it performs a one-time import of any legacy JSON tree on
//! open (renaming it to `conversations.imported/` as a backup). What remains
//! here is the storage-agnostic surface both the store and the TUI share:
//! the record/summary/turn types, conversation-id minting, and the
//! per-session plan paths (issue #220).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

static CLOCK_TIEBREAKER: AtomicU64 = AtomicU64::new(0);

/// One tool invocation recorded during a turn (Step 17.6, issue #246).
///
/// Serialized (as an array element) into the store's `turns.events` JSON
/// column. The field names `tool` and `args_digest` are **load-bearing**:
/// the FTS5 recall index derives its `tool_names` / `tool_args_digest`
/// columns by extracting exactly `$.tool` and `$.args_digest` from each
/// element (see `store.rs` — `events_extract_sql`, the 17.3 seam) — rename
/// either and tool recall goes dark. Unknown extra fields are ignored on
/// load, so the shape can grow additively without a migration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolEvent {
    /// Tool name exactly as the model invoked it (`read_file`,
    /// `run_command`, an MCP `server__tool`, or a hallucinated name —
    /// recorded as called, ok = false tells the story).
    pub tool: String,
    /// Privacy-preserving argument digest: the argument *key names*
    /// (searchable) plus a truncated BLAKE3 of the canonical args JSON
    /// (correlatable). **Never raw argument values** — args carry file
    /// contents and can carry secrets. Built by [`ToolEvent::from_call`].
    pub args_digest: String,
    /// Whether the tool result read as success. Best-effort: the loop's
    /// tool results are plain strings, so this mirrors the `error:` /
    /// `capability denied:` / `unknown tool` prefixes `execute_tool` emits.
    pub ok: bool,
    /// Wall-clock duration of the tool call in milliseconds. A display
    /// claim only (§6): never an ordering key — order within the turn is
    /// the events array position.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

impl ToolEvent {
    /// Record one tool call. `args` are digested, never stored raw: the
    /// digest is the object's key names (space-joined — useful FTS terms
    /// that cannot leak values) plus `b3:` + the first 16 hex chars of
    /// BLAKE3 over the canonical JSON, so two turns calling the same tool
    /// with identical args correlate without exposing what the args were.
    pub fn from_call(
        tool: impl Into<String>,
        args: &serde_json::Value,
        ok: bool,
        duration_ms: Option<u64>,
    ) -> Self {
        // serde_json's default map is ordered (BTreeMap), so `to_string`
        // is canonical for a given parsed value and the digest is stable.
        let hash = blake3::hash(args.to_string().as_bytes()).to_hex();
        let short = &hash.as_str()[..16];
        let keys = args
            .as_object()
            .map(|m| m.keys().cloned().collect::<Vec<_>>().join(" "))
            .unwrap_or_default();
        let args_digest = if keys.is_empty() {
            format!("b3:{short}")
        } else {
            format!("{keys} b3:{short}")
        };
        Self {
            tool: tool.into(),
            args_digest,
            ok,
            duration_ms,
        }
    }
}

/// #717: a phantom tool/capability reach — a tool/notion the model grabbed for
/// that is NOT a real tool (an alias or hallucination), or a real tool that
/// returned empty-by-design (`state_get` for an unset key, `recall` with no
/// matches). Distinct from [`ToolEvent`] (which stays for tool-recall): this is
/// the alias-seam telemetry — *which* foreign notions weak local models reach
/// for, so the seam is driven by data, not guesses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhantomReach {
    /// The literal tool/notion name the model emitted.
    pub name_as_called: String,
    /// How newt resolved the reach.
    pub resolution: PhantomResolution,
    /// Context features in effect when the reach happened (so we can see e.g.
    /// `scheduled` was on but the model still reached for a phantom `plan`).
    /// Empty when unknown.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_context_features: Vec<String>,
}

/// How a [`PhantomReach`] resolved (#717).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "detail")]
pub enum PhantomResolution {
    /// A foreign name rewritten to a real tool (the canonical name).
    Rewrite(String),
    /// A foreign name corrected with guidance (the message naming the right tool).
    Correct(String),
    /// An unknown name with no alias — a true phantom tool.
    Unknown,
    /// A real tool that misfired / returned empty-by-design (the reason).
    RealToolMiss(String),
    /// #479 (G4): a real tool reached while its surface is presence-gated OFF —
    /// `crew`/`compose_roster` with the crew/team surface absent (the default,
    /// since the runner is only built when the operator sets `NEWT_TEAM`). The
    /// name stays real (in `ALL_TOOL_NAMES`, so `is_hallucination` is unchanged
    /// and the ON path dispatches normally), so `classify_phantom_reach` never
    /// flags it — yet a gated-off reach is exactly the delegation signal we want
    /// to mine for the common OFF default. The string names the gated surface.
    GatedOff(String),
    /// A reach narrated in prose that never became a tool call (reserved; NOT
    /// emitted in v1 — the narration scanner is a deferred follow-up).
    NarratedNoCall,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationTurn {
    pub user: String,
    pub assistant: String,
    /// Tool events recorded during the turn (17.6). Empty for pre-17.6
    /// rows and for turns that called no tools.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<ToolEvent>,
    /// Phantom tool/capability reaches recorded during the turn (#717). Empty
    /// for pre-#717 turns or turns with no phantom reach. Additive: `#[serde(default)]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phantom_reaches: Vec<PhantomReach>,
    /// Backend-reported prompt tokens for the turn (largest single prompt
    /// across the turn's rounds — see `chat_complete`'s Step 18.1 usage
    /// semantics). `None` when the backend reported nothing: absence is
    /// stored as absence, never an estimate (18.5 rehydrates from this).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_in: Option<u32>,
    /// Backend-reported completion tokens (summed across rounds). `None`
    /// when the backend reported nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_out: Option<u32>,
}

impl ConversationTurn {
    pub fn new(user: impl Into<String>, assistant: impl Into<String>) -> Self {
        Self {
            user: user.into(),
            assistant: assistant.into(),
            events: Vec::new(),
            phantom_reaches: Vec::new(),
            tokens_in: None,
            tokens_out: None,
        }
    }
}

/// A full conversation as loaded from the store. Also the on-disk shape of
/// the retired JSON backend's records — kept serde-compatible so the 17.1b
/// one-time import can parse legacy files with the exact semantics they were
/// written under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationRecord {
    pub id: String,
    pub title: String,
    pub workspace: String,
    pub workspace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona: Option<String>,
    #[serde(default)]
    pub turns: Vec<ConversationTurn>,
    /// The conversation's scratchpad `<state>` snapshot (#713, Step 26.4): the
    /// structured working memory the model keeps via `state_set` / `state_get`.
    /// Persisted at the conversation/record level so an interrupt + auto-resume
    /// restores the `<state>` it already restores the transcript for — without
    /// it, the first `state_get("current_task")` after a resume reads an empty
    /// live store and the model blind-probes for a task that is simply gone.
    /// Additive: `#[serde(default)]` keeps legacy records loadable, and an empty
    /// map is skipped on serialize so it never bloats the on-disk file. **Working
    /// memory, not provenance** — kept OUT of the §6 content chain (it is NOT a
    /// turn field and never enters `canonical_encoding_v1`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub scratchpad: BTreeMap<String, String>,
    /// The conversation's plan-ledger snapshot (#715, Step 26.6b): the ordered
    /// `<plan>` steps with each one's done/active status. Persisted at the
    /// conversation/record level — exactly like `scratchpad` — so an interrupt +
    /// auto-resume re-hydrates the in-memory ledger instead of rebuilding it
    /// empty (which loses the `<plan>` block and `plan_get` after a resume).
    /// Additive: `#[serde(default)]` keeps legacy records loadable, and an empty
    /// snapshot is skipped on serialize so it never bloats the on-disk file.
    /// **Working memory, not provenance** — kept OUT of the §6 content chain (it
    /// is NOT a turn field and never enters `canonical_encoding_v1`).
    #[serde(default, skip_serializing_if = "crate::PlanSnapshot::is_empty")]
    pub plan: crate::PlanSnapshot,
    pub created_at_unix_nanos: u128,
    pub updated_at_unix_nanos: u128,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationSummary {
    pub id: String,
    pub title: String,
    pub persona: Option<String>,
    pub turn_count: usize,
    pub updated_at_unix_nanos: u128,
}

/// Mint a fresh conversation id: `{unix_nanos}-{uuid_v4}`.
///
/// Exposed so the TUI can pre-generate an id at session start — the same id
/// keys both the durable conversation record and the per-session plan dir
/// (issue #220) — and then hand it to
/// [`crate::store::ConversationStore::create_with_id`]. Two concurrent newt
/// processes mint distinct ids (distinct nanos + distinct UUIDs), so their
/// plan files never collide.
pub fn new_conversation_id() -> String {
    format!("{}-{}", unix_nanos(), uuid::Uuid::new_v4())
}

/// Workspace-relative directory holding a conversation's per-session plan:
/// `.scratch/sessions/<conversation-id>`. Under `.scratch/` (the ephemeral root,
/// #844) so it stays out of `.newt/` tracked config; workspace-relative so the
/// file tools' workspace fence permits writing it. See #220, #844.
pub fn session_plan_dir(conversation_id: &str) -> PathBuf {
    Path::new(".scratch").join("sessions").join(conversation_id)
}

/// Workspace-relative per-session plan document path:
/// `.scratch/sessions/<conversation-id>/plan.md`. This is the path the system
/// prompt tells the model to use; it replaces the old fixed `.newt/plan.md`
/// that collided when several newt instances ran in one repo. See #220, #844.
pub fn session_plan_path(conversation_id: &str) -> PathBuf {
    session_plan_dir(conversation_id).join("plan.md")
}

fn unix_nanos() -> u128 {
    let base = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    base + CLOCK_TIEBREAKER.fetch_add(1, Ordering::Relaxed) as u128
}
