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
        std::fs::create_dir_all(self.workspace_dir())?;
        let now = unix_nanos();
        let id = format!("{now}-{}", uuid::Uuid::new_v4());
        let record = ConversationRecord {
            id: id.clone(),
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
        Ok(id)
    }

    pub fn append_turn(&self, id: &str, user: &str, assistant: &str) -> anyhow::Result<()> {
        let mut record = self.load(id)?;
        record.turns.push(ConversationTurn::new(user, assistant));
        record.updated_at_unix_nanos = unix_nanos();
        self.save_record(&record)?;
        self.prune_to_cap()
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
        std::fs::write(self.record_path(&record.id), text)?;
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
            let text = std::fs::read_to_string(&path)?;
            let record: ConversationRecord = serde_json::from_str(&text)?;
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

fn unix_nanos() -> u128 {
    let base = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    base + CLOCK_TIEBREAKER.fetch_add(1, Ordering::Relaxed) as u128
}
