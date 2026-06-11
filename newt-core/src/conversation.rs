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

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

static CLOCK_TIEBREAKER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationTurn {
    pub user: String,
    pub assistant: String,
}

impl ConversationTurn {
    pub fn new(user: impl Into<String>, assistant: impl Into<String>) -> Self {
        Self {
            user: user.into(),
            assistant: assistant.into(),
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
/// `.newt/sessions/<conversation-id>`. Workspace-relative so the file tools'
/// workspace fence permits writing it and it travels with the repo. See #220.
pub fn session_plan_dir(conversation_id: &str) -> PathBuf {
    Path::new(".newt").join("sessions").join(conversation_id)
}

/// Workspace-relative per-session plan document path:
/// `.newt/sessions/<conversation-id>/plan.md`. This is the path the system
/// prompt tells the model to use; it replaces the old fixed `.newt/plan.md`
/// that collided when several newt instances ran in one repo. See issue #220.
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
