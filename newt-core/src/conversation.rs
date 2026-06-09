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

#[derive(Debug, Clone)]
pub struct ConversationStore {
    root: PathBuf,
    workspace: PathBuf,
    workspace_id: String,
    max_per_workspace: usize,
}

impl ConversationStore {
    pub fn new(
        root: impl AsRef<Path>,
        workspace: impl AsRef<Path>,
        max_per_workspace: usize,
    ) -> anyhow::Result<Self> {
        let workspace = std::fs::canonicalize(workspace.as_ref())?;
        let workspace_id = Self::workspace_id_for_path(&workspace)?;
        Ok(Self {
            root: root.as_ref().to_path_buf(),
            workspace,
            workspace_id,
            max_per_workspace,
        })
    }

    pub fn workspace_id_for_path(path: impl AsRef<Path>) -> anyhow::Result<String> {
        let canonical = std::fs::canonicalize(path.as_ref())?;
        let normalized = canonical.to_string_lossy().replace('\\', "/");
        Ok(uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, normalized.as_bytes()).to_string())
    }

    pub fn create(&self, title: &str, persona: Option<&str>) -> anyhow::Result<String> {
        let id = new_conversation_id();
        self.create_with_id(&id, title, persona)?;
        Ok(id)
    }

    /// Create a conversation record using a caller-supplied `id`.
    ///
    /// The TUI pre-generates a conversation id at session start (so the
    /// per-session plan path is stable from turn 1, see issue #220) and then
    /// has the record adopt that same id when the first turn is saved. Splitting
    /// this out of [`create`](Self::create) lets the id and the record share a
    /// value instead of `create` minting its own.
    pub fn create_with_id(
        &self,
        id: &str,
        title: &str,
        persona: Option<&str>,
    ) -> anyhow::Result<()> {
        std::fs::create_dir_all(self.workspace_dir())?;
        let now = unix_nanos();
        let record = ConversationRecord {
            id: id.to_string(),
            title: title.trim().to_string(),
            workspace: self.workspace.to_string_lossy().into_owned(),
            workspace_id: self.workspace_id.clone(),
            persona: persona.map(str::to_string),
            turns: Vec::new(),
            created_at_unix_nanos: now,
            updated_at_unix_nanos: now,
        };
        self.save_record(&record)?;
        self.prune_to_cap()?;
        Ok(())
    }

    /// `true` if a record for `id` already exists for this workspace. Used by
    /// the save path to decide between [`create_with_id`](Self::create_with_id)
    /// (first turn) and [`append_turn`](Self::append_turn).
    pub fn exists(&self, id: &str) -> bool {
        self.record_path(id).is_file()
    }

    pub fn append_turn(&self, id: &str, user: &str, assistant: &str) -> anyhow::Result<()> {
        let mut record = self.load(id)?;
        record.turns.push(ConversationTurn::new(user, assistant));
        record.updated_at_unix_nanos = unix_nanos();
        // No prune here: appending never changes the record count, and
        // pruning re-reads + deserializes every record in the workspace —
        // O(records) on every turn. Pruning happens on `create` only.
        self.save_record(&record)
    }

    pub fn load(&self, id: &str) -> anyhow::Result<ConversationRecord> {
        let resolved_id = self.resolve_id(id)?;
        let text = std::fs::read_to_string(self.record_path(&resolved_id))?;
        let record: ConversationRecord = serde_json::from_str(&text)?;
        if record.workspace_id != self.workspace_id {
            anyhow::bail!("conversation `{resolved_id}` does not belong to this workspace");
        }
        Ok(record)
    }

    pub fn list(&self) -> anyhow::Result<Vec<ConversationSummary>> {
        let mut records = self.load_records()?;
        records.sort_by(oldest_records_first);
        Ok(records
            .into_iter()
            .map(|r| ConversationSummary {
                id: r.id,
                title: r.title,
                persona: r.persona,
                turn_count: r.turns.len(),
                updated_at_unix_nanos: r.updated_at_unix_nanos,
            })
            .collect())
    }

    pub fn rename(&self, id: &str, title: &str) -> anyhow::Result<()> {
        let mut record = self.load(id)?;
        record.title = title.trim().to_string();
        record.updated_at_unix_nanos = unix_nanos();
        self.save_record(&record)
    }

    pub fn delete(&self, id: &str) -> anyhow::Result<()> {
        let resolved_id = self.resolve_id(id)?;
        let path = self.record_path(&resolved_id);
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        // Best-effort: drop the conversation's per-session plan dir too, so plan
        // files don't outlive their conversation (issue #220). Ignore errors —
        // the dir may not exist (no plan was ever written) and a stray plan must
        // never block deletion of the record.
        let plan_dir = self.workspace.join(session_plan_dir(&resolved_id));
        let _ = std::fs::remove_dir_all(plan_dir);
        Ok(())
    }

    pub fn resolve_id(&self, id_or_prefix: &str) -> anyhow::Result<String> {
        validate_record_id(id_or_prefix)?;
        if self.record_path(id_or_prefix).exists() {
            return Ok(id_or_prefix.to_string());
        }

        let matches: Vec<_> = self
            .load_records()?
            .into_iter()
            .filter(|record| record.id.starts_with(id_or_prefix))
            .map(|record| record.id)
            .collect();

        match matches.as_slice() {
            [id] => Ok(id.clone()),
            [] => anyhow::bail!("conversation `{id_or_prefix}` not found"),
            many => anyhow::bail!(
                "ambiguous conversation id prefix `{}`; matches: {}",
                id_or_prefix,
                many.join(", ")
            ),
        }
    }

    fn save_record(&self, record: &ConversationRecord) -> anyhow::Result<()> {
        validate_record_id(&record.id)?;
        std::fs::create_dir_all(self.workspace_dir())?;
        let text = serde_json::to_string_pretty(record)?;
        // Write-then-rename so a crash mid-write can never leave a
        // half-written record where a good one used to be. The temp file
        // lives in the same directory so the rename never crosses a
        // filesystem boundary (std::fs::rename replaces the destination
        // on both Unix and Windows). Stray `.tmp` files from a crash are
        // ignored by `load_records` (it only reads `.json`).
        let path = self.record_path(&record.id);
        let tmp = self.workspace_dir().join(format!("{}.json.tmp", record.id));
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    fn prune_to_cap(&self) -> anyhow::Result<()> {
        if self.max_per_workspace == 0 {
            return Ok(());
        }
        let mut records = self.load_records()?;
        if records.len() <= self.max_per_workspace {
            return Ok(());
        }
        records.sort_by(oldest_records_first);
        let prune_count = records.len() - self.max_per_workspace;
        for record in records.into_iter().take(prune_count) {
            self.delete(&record.id)?;
        }
        Ok(())
    }

    fn load_records(&self) -> anyhow::Result<Vec<ConversationRecord>> {
        let dir = self.workspace_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut records = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            // One unreadable or corrupt record must not poison the whole
            // workspace — propagating here would break `list`, `prune`,
            // and prefix resolution for every conversation over a single
            // bad file. Skip it loudly instead.
            let parsed = std::fs::read_to_string(&path)
                .map_err(anyhow::Error::from)
                .and_then(|text| Ok(serde_json::from_str::<ConversationRecord>(&text)?));
            let record = match parsed {
                Ok(record) => record,
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "skipping unreadable conversation record"
                    );
                    continue;
                }
            };
            if record.workspace_id == self.workspace_id {
                records.push(record);
            }
        }
        Ok(records)
    }

    fn record_path(&self, id: &str) -> PathBuf {
        self.workspace_dir().join(format!("{id}.json"))
    }

    fn workspace_dir(&self) -> PathBuf {
        self.root.join("conversations").join(&self.workspace_id)
    }
}

fn oldest_records_first(a: &ConversationRecord, b: &ConversationRecord) -> std::cmp::Ordering {
    a.updated_at_unix_nanos
        .cmp(&b.updated_at_unix_nanos)
        .then_with(|| a.created_at_unix_nanos.cmp(&b.created_at_unix_nanos))
        .then_with(|| a.id.cmp(&b.id))
}

fn validate_record_id(id: &str) -> anyhow::Result<()> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        anyhow::bail!("invalid conversation id `{id}`");
    }
    Ok(())
}

/// Mint a fresh conversation id: `{unix_nanos}-{uuid_v4}`.
///
/// Exposed so the TUI can pre-generate an id at session start — the same id
/// keys both the durable conversation record and the per-session plan dir
/// (issue #220) — and then hand it to
/// [`ConversationStore::create_with_id`]. Two concurrent newt processes mint
/// distinct ids (distinct nanos + distinct UUIDs), so their plan files never
/// collide.
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
