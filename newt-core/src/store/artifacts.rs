//! Prompt-artifact persistence, fenced reads, and tamper-evident chain verification.
//!
//! Artifact reads share a verified SQLite snapshot; appends verify and extend the
//! chain in one write transaction. Prompt receipt authority lives in the sibling prompts module.

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

use super::{
    load_prompt_in_conversation_on_conn, prompt_chain_on_conn, validate_objective_root_on_conn,
    validate_record_id, ConversationStore,
};
use crate::artifact::{
    ArtifactId, ArtifactKind, ArtifactRelation, NewPromptArtifact, PromptArtifact,
};
use crate::prompt::PromptId;

/// Domain-separation prefix for the per-conversation prompt-artifact chain.
const ARTIFACT_GENESIS_PREFIX: &[u8] = b"newt-prompt-artifact-chain-genesis:v1";

/// Hard upper bound for one paged artifact read. Internal verification still
/// covers the complete chain before a page is returned.
const MAX_ARTIFACT_PAGE_SIZE: usize = 256;

impl ConversationStore {
    /// Atomically append one bounded derived-work artifact to a prompt.
    ///
    /// The objective root is derived from the immutable prompt receipt, never
    /// accepted from the caller. The complete existing artifact chain is
    /// verified under the same `BEGIN IMMEDIATE` transaction that allocates
    /// the next sequence and inserts the row, so concurrent processes cannot
    /// fork the chain and pre-existing tampering fails closed.
    pub fn append_prompt_artifact(
        &self,
        conversation_id: &str,
        prompt_id: PromptId,
        content: NewPromptArtifact,
    ) -> anyhow::Result<PromptArtifact> {
        validate_record_id(conversation_id)?;
        // Validate bounded content before taking SQLite's write lock.
        content.validate()?;
        let now = (self.claim_clock)();
        let conn = self.lock_conn();
        let tx = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)?;

        let owner: Option<String> = tx
            .query_row(
                "SELECT workspace_key FROM conversations WHERE id = ?1",
                [conversation_id],
                |row| row.get(0),
            )
            .optional()?;
        match owner.as_deref() {
            Some(owner) if owner == self.workspace_id => {}
            Some(owner) => anyhow::bail!(
                "conversation id `{conversation_id}` belongs to another workspace \
                 (key {owner}); refusing to append an artifact across the workspace fence"
            ),
            None => anyhow::bail!("unknown conversation `{conversation_id}`"),
        }

        // Artifact provenance is meaningful only while the complete prompt
        // chronology remains valid. In particular, fail closed if mutable
        // receipt_order metadata was tampered even though this one receipt's
        // content hash still verifies.
        let prompt_chain = prompt_chain_on_conn(&tx, conversation_id, &self.workspace_id)?;
        let prompt = prompt_chain
            .iter()
            .find(|prompt| prompt.id() == prompt_id)
            .ok_or_else(|| {
                anyhow::anyhow!(
                "prompt {prompt_id} is not in conversation `{conversation_id}` in this workspace"
            )
            })?;
        validate_objective_root_on_conn(
            &tx,
            conversation_id,
            prompt.root_prompt_id(),
            &self.workspace_id,
        )?;

        let chain = artifact_chain_on_conn(&tx, conversation_id, &self.workspace_id)?;
        let (seq, prev_hash) = match chain.last() {
            Some(previous) => (
                previous
                    .seq()
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("prompt artifact sequence overflow"))?,
                previous.artifact_hash().to_string(),
            ),
            None => (1, artifact_genesis_hash(conversation_id)),
        };
        let artifact = PromptArtifact::mint(
            ArtifactId::new(),
            conversation_id.to_string(),
            self.writer_fingerprint.clone(),
            seq,
            prev_hash,
            prompt.id(),
            prompt.root_prompt_id(),
            content,
            now,
        )?;
        insert_prompt_artifact(&tx, &artifact)?;
        tx.commit()?;
        Ok(artifact)
    }

    /// Fetch one artifact through both its conversation and workspace fence.
    /// The complete conversation chain is verified before the record is
    /// returned, so deletion/reordering of a predecessor is detected too.
    pub fn load_prompt_artifact(
        &self,
        conversation_id: &str,
        artifact_id: ArtifactId,
    ) -> anyhow::Result<Option<PromptArtifact>> {
        validate_record_id(conversation_id)?;
        let conn = self.lock_conn();
        let tx = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Deferred)?;
        let found = artifact_chain_on_conn(&tx, conversation_id, &self.workspace_id)?
            .into_iter()
            .find(|artifact| artifact.id() == artifact_id);
        tx.commit()?;
        Ok(found)
    }

    /// Read a bounded page of the verified artifact chain in causal order.
    pub fn list_prompt_artifacts(
        &self,
        conversation_id: &str,
        offset: usize,
        limit: usize,
    ) -> anyhow::Result<Vec<PromptArtifact>> {
        self.page_prompt_artifacts(conversation_id, offset, limit)
            .map(|(artifacts, _)| artifacts)
    }

    /// Return a verified causal page and its snapshot-consistent total.
    pub fn page_prompt_artifacts(
        &self,
        conversation_id: &str,
        offset: usize,
        limit: usize,
    ) -> anyhow::Result<(Vec<PromptArtifact>, usize)> {
        validate_record_id(conversation_id)?;
        validate_artifact_page_size(limit)?;
        let conn = self.lock_conn();
        let tx = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Deferred)?;
        let chain = artifact_chain_on_conn(&tx, conversation_id, &self.workspace_id)?;
        let total = chain.len();
        let page = chain.into_iter().skip(offset).take(limit).collect();
        tx.commit()?;
        Ok((page, total))
    }

    /// Count the verified artifact chain through the workspace fence.
    pub fn count_prompt_artifacts(&self, conversation_id: &str) -> anyhow::Result<usize> {
        self.page_prompt_artifacts(conversation_id, 0, 0)
            .map(|(_, total)| total)
    }

    /// Read a bounded causal page containing only artifacts attached directly
    /// to `prompt_id`. A prompt outside this conversation/workspace resolves
    /// to an empty page rather than becoming an address-existence oracle.
    pub fn list_prompt_artifacts_for_prompt(
        &self,
        conversation_id: &str,
        prompt_id: PromptId,
        offset: usize,
        limit: usize,
    ) -> anyhow::Result<Vec<PromptArtifact>> {
        self.page_prompt_artifacts_for_prompt(conversation_id, prompt_id, offset, limit)
            .map(|(artifacts, _)| artifacts)
    }

    /// Return a direct-prompt page and its total from one read transaction.
    pub fn page_prompt_artifacts_for_prompt(
        &self,
        conversation_id: &str,
        prompt_id: PromptId,
        offset: usize,
        limit: usize,
    ) -> anyhow::Result<(Vec<PromptArtifact>, usize)> {
        validate_record_id(conversation_id)?;
        validate_artifact_page_size(limit)?;
        let conn = self.lock_conn();
        let tx = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Deferred)?;
        if load_prompt_in_conversation_on_conn(&tx, conversation_id, prompt_id, &self.workspace_id)?
            .is_none()
        {
            tx.commit()?;
            return Ok((Vec::new(), 0));
        }
        let matching: Vec<_> = artifact_chain_on_conn(&tx, conversation_id, &self.workspace_id)?
            .into_iter()
            .filter(|artifact| artifact.prompt_id() == prompt_id)
            .collect();
        let total = matching.len();
        let page = matching.into_iter().skip(offset).take(limit).collect();
        tx.commit()?;
        Ok((page, total))
    }

    /// Count verified artifacts attached directly to one prompt.
    pub fn count_prompt_artifacts_for_prompt(
        &self,
        conversation_id: &str,
        prompt_id: PromptId,
    ) -> anyhow::Result<usize> {
        self.page_prompt_artifacts_for_prompt(conversation_id, prompt_id, 0, 0)
            .map(|(_, total)| total)
    }

    /// Read a bounded causal page for an entire objective root, including
    /// artifacts attached to later clarifications and harness retries.
    pub fn list_prompt_artifacts_for_root(
        &self,
        conversation_id: &str,
        root_prompt_id: PromptId,
        offset: usize,
        limit: usize,
    ) -> anyhow::Result<Vec<PromptArtifact>> {
        self.page_prompt_artifacts_for_root(conversation_id, root_prompt_id, offset, limit)
            .map(|(artifacts, _)| artifacts)
    }

    /// Return an objective-root page and its total from one read transaction.
    pub fn page_prompt_artifacts_for_root(
        &self,
        conversation_id: &str,
        root_prompt_id: PromptId,
        offset: usize,
        limit: usize,
    ) -> anyhow::Result<(Vec<PromptArtifact>, usize)> {
        validate_record_id(conversation_id)?;
        validate_artifact_page_size(limit)?;
        let conn = self.lock_conn();
        let tx = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Deferred)?;
        let Some(root) = load_prompt_in_conversation_on_conn(
            &tx,
            conversation_id,
            root_prompt_id,
            &self.workspace_id,
        )?
        else {
            tx.commit()?;
            return Ok((Vec::new(), 0));
        };
        if root.id() != root.root_prompt_id() {
            anyhow::bail!("prompt {root_prompt_id} is not an objective root");
        }
        validate_objective_root_on_conn(&tx, conversation_id, root_prompt_id, &self.workspace_id)?;
        let matching: Vec<_> = artifact_chain_on_conn(&tx, conversation_id, &self.workspace_id)?
            .into_iter()
            .filter(|artifact| artifact.root_prompt_id() == root_prompt_id)
            .collect();
        let total = matching.len();
        let page = matching.into_iter().skip(offset).take(limit).collect();
        tx.commit()?;
        Ok((page, total))
    }

    /// Count verified artifacts in one objective-root lineage.
    pub fn count_prompt_artifacts_for_root(
        &self,
        conversation_id: &str,
        root_prompt_id: PromptId,
    ) -> anyhow::Result<usize> {
        self.page_prompt_artifacts_for_root(conversation_id, root_prompt_id, 0, 0)
            .map(|(_, total)| total)
    }

    /// Verify every artifact hash, causal sequence, and predecessor link in a
    /// conversation. Unknown and cross-workspace conversations expose an empty
    /// valid chain, matching the fenced read APIs.
    pub fn verify_prompt_artifact_chain(&self, conversation_id: &str) -> anyhow::Result<()> {
        validate_record_id(conversation_id)?;
        let conn = self.lock_conn();
        let tx = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Deferred)?;
        artifact_chain_on_conn(&tx, conversation_id, &self.workspace_id)?;
        tx.commit()?;
        Ok(())
    }
}

/// SQLite representation of one immutable prompt artifact. Rich parsing and
/// hash verification happen outside rusqlite's row callback.
#[derive(Debug)]
struct ArtifactRow {
    id: String,
    conversation_id: String,
    writer_fingerprint: String,
    seq: i64,
    prev_hash: String,
    prompt_id: String,
    root_prompt_id: String,
    kind: String,
    relation: String,
    locator: Option<String>,
    body: Option<String>,
    metadata_json: String,
    ts_claim: i64,
    encoding_version: i64,
    artifact_hash: String,
}

impl ArtifactRow {
    fn into_artifact(self) -> anyhow::Result<PromptArtifact> {
        let artifact = PromptArtifact::from_stored_parts(
            self.id.parse()?,
            self.conversation_id,
            self.writer_fingerprint,
            self.seq,
            self.prev_hash,
            self.prompt_id.parse()?,
            self.root_prompt_id.parse()?,
            ArtifactKind::from_db_str(&self.kind)?,
            ArtifactRelation::from_db_str(&self.relation)?,
            self.locator,
            self.body,
            self.metadata_json,
            self.ts_claim,
            self.encoding_version,
            self.artifact_hash,
        )?;
        artifact.verify_integrity()?;
        Ok(artifact)
    }
}

fn artifact_row_from_sql(row: &rusqlite::Row<'_>) -> rusqlite::Result<ArtifactRow> {
    Ok(ArtifactRow {
        id: row.get(0)?,
        conversation_id: row.get(1)?,
        writer_fingerprint: row.get(2)?,
        seq: row.get(3)?,
        prev_hash: row.get(4)?,
        prompt_id: row.get(5)?,
        root_prompt_id: row.get(6)?,
        kind: row.get(7)?,
        relation: row.get(8)?,
        locator: row.get(9)?,
        body: row.get(10)?,
        metadata_json: row.get(11)?,
        ts_claim: row.get(12)?,
        encoding_version: row.get(13)?,
        artifact_hash: row.get(14)?,
    })
}

fn insert_prompt_artifact(conn: &Connection, artifact: &PromptArtifact) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO prompt_artifacts
           (id, conversation_id, writer_fingerprint, seq, prev_hash,
            prompt_id, root_prompt_id, kind, relation, locator, body, metadata,
            ts_claim, encoding_version, artifact_hash)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        rusqlite::params![
            artifact.id().to_string(),
            artifact.conversation_id(),
            artifact.writer_fingerprint(),
            artifact.seq(),
            artifact.prev_hash(),
            artifact.prompt_id().to_string(),
            artifact.root_prompt_id().to_string(),
            artifact.kind().as_db_str(),
            artifact.relation().as_db_str(),
            artifact.locator(),
            artifact.body(),
            serde_json::to_string(artifact.metadata())?,
            artifact.ts_claim(),
            artifact.encoding_version(),
            artifact.artifact_hash(),
        ],
    )?;
    conn.execute(
        "INSERT INTO prompt_artifact_tips (conversation_id, last_seq, tip_hash)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(conversation_id) DO UPDATE SET
             last_seq = excluded.last_seq,
             tip_hash = excluded.tip_hash",
        rusqlite::params![
            artifact.conversation_id(),
            artifact.seq(),
            artifact.artifact_hash(),
        ],
    )?;
    Ok(())
}

fn artifact_chain_on_conn(
    conn: &Connection,
    conversation_id: &str,
    workspace_id: &str,
) -> anyhow::Result<Vec<PromptArtifact>> {
    // Never return or extend a derived-work chain whose prompt authority is
    // itself corrupt. Prompt receipt_order is intentionally outside each
    // receipt content hash, so the full chronology validator is required here.
    let _ = prompt_chain_on_conn(conn, conversation_id, workspace_id)?;
    let mut stmt = conn.prepare(
        "SELECT a.id, a.conversation_id, a.writer_fingerprint, a.seq,
                a.prev_hash, a.prompt_id, a.root_prompt_id, a.kind, a.relation,
                a.locator, a.body, a.metadata, a.ts_claim, a.encoding_version,
                a.artifact_hash
           FROM prompt_artifacts a
           JOIN conversations c ON c.id = a.conversation_id
          WHERE a.conversation_id = ?1 AND c.workspace_key = ?2
          ORDER BY a.seq ASC",
    )?;
    let rows = stmt.query_map(
        rusqlite::params![conversation_id, workspace_id],
        artifact_row_from_sql,
    )?;
    let artifacts: Vec<PromptArtifact> = rows
        .map(|row| {
            row.map_err(anyhow::Error::from)
                .and_then(ArtifactRow::into_artifact)
        })
        .collect::<anyhow::Result<_>>()?;

    let mut expected_seq = 1_i64;
    let mut expected_prev = artifact_genesis_hash(conversation_id);
    for artifact in &artifacts {
        let prompt = load_prompt_in_conversation_on_conn(
            conn,
            conversation_id,
            artifact.prompt_id(),
            workspace_id,
        )?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "prompt artifact {} is orphaned from prompt {}",
                artifact.id(),
                artifact.prompt_id()
            )
        })?;
        if prompt.root_prompt_id() != artifact.root_prompt_id() {
            anyhow::bail!(
                "prompt artifact {} claims objective root {}, but prompt {} is rooted at {}",
                artifact.id(),
                artifact.root_prompt_id(),
                prompt.id(),
                prompt.root_prompt_id()
            );
        }
        validate_objective_root_on_conn(
            conn,
            conversation_id,
            artifact.root_prompt_id(),
            workspace_id,
        )?;
        if artifact.seq() != expected_seq {
            anyhow::bail!(
                "prompt artifact sequence mismatch in conversation `{conversation_id}` at {}: \
                 expected {expected_seq}, stored {}",
                artifact.id(),
                artifact.seq()
            );
        }
        if artifact.prev_hash() != expected_prev {
            anyhow::bail!(
                "prompt artifact predecessor mismatch in conversation `{conversation_id}` at {}",
                artifact.id()
            );
        }
        expected_seq = expected_seq
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("prompt artifact sequence overflow"))?;
        expected_prev = artifact.artifact_hash().to_string();
    }

    let stored_tip: Option<(i64, String)> = conn
        .query_row(
            "SELECT t.last_seq, t.tip_hash
               FROM prompt_artifact_tips t
               JOIN conversations c ON c.id = t.conversation_id
              WHERE t.conversation_id = ?1 AND c.workspace_key = ?2",
            rusqlite::params![conversation_id, workspace_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    match (artifacts.last(), stored_tip) {
        (None, None) => {}
        (Some(last), Some((last_seq, tip_hash)))
            if last.seq() == last_seq && last.artifact_hash() == tip_hash => {}
        (None, Some(_)) => anyhow::bail!(
            "prompt artifact chain tip exists without rows in conversation `{conversation_id}`"
        ),
        (Some(_), None) => anyhow::bail!(
            "prompt artifact rows exist without a chain tip in conversation `{conversation_id}`"
        ),
        (Some(last), Some((last_seq, tip_hash))) => anyhow::bail!(
            "prompt artifact chain tip mismatch in conversation `{conversation_id}`: \
             row tip is ({}, {}), stored tip is ({last_seq}, {tip_hash})",
            last.seq(),
            last.artifact_hash()
        ),
    }
    Ok(artifacts)
}

fn artifact_genesis_hash(conversation_id: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(ARTIFACT_GENESIS_PREFIX);
    hasher.update(&(conversation_id.len() as u64).to_le_bytes());
    hasher.update(conversation_id.as_bytes());
    hasher.finalize().to_hex().to_string()
}

fn validate_artifact_page_size(limit: usize) -> anyhow::Result<()> {
    if limit > MAX_ARTIFACT_PAGE_SIZE {
        anyhow::bail!("prompt artifact page limit exceeds {MAX_ARTIFACT_PAGE_SIZE}");
    }
    Ok(())
}
