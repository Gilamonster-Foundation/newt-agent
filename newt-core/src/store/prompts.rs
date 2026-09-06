//! Immutable prompt receipts, fenced resolution, and prompt lineage validation.

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

use super::{genesis_hash, next_tick, validate_record_id, ConversationStore};
use crate::prompt::{NewPrompt, PromptId, PromptOrigin, PromptReceipt, TurnPromptContext};

/// Maximum number of receipts examined while resolving the active operator
/// through a harness-retry parent chain. This includes both the submitted
/// receipt and the terminal operator receipt. A finite bound makes corrupted
/// or adversarial lineage fail closed without unbounded CPU/memory use; 256
/// still permits far more consecutive automatic retries than a useful turn
/// should ever require.
pub(super) const MAX_PROMPT_LINEAGE_DEPTH: usize = 256;

impl ConversationStore {
    /// Atomically accept one prompt before any model or tool work begins.
    ///
    /// The transaction lazy-creates `conversation_id` when this is its first
    /// prompt, allocates the prompt's Lamport sequence, resolves explicit
    /// ancestry, writes the immutable receipt, and advances conversation
    /// activity. It deliberately does **not** touch the conversation's turn
    /// `writer_fingerprint` / `tip_hash` pair: prompts have their own hashes
    /// and must not masquerade as completed turns in the existing chain.
    ///
    /// `previous_prompt_id` is assigned from serialized receipt chronology.
    /// That is independent from semantic parentage: a plain operator prompt
    /// has no parent and roots at itself. Explicit continuations/retries must
    /// name a same-conversation parent and inherit its validated root.
    pub fn begin_prompt(
        &self,
        conversation_id: &str,
        title: &str,
        persona: Option<&str>,
        prompt: NewPrompt,
    ) -> anyhow::Result<TurnPromptContext> {
        validate_record_id(conversation_id)?;
        std::str::from_utf8(prompt.model_text()).map_err(|error| {
            anyhow::anyhow!(
                "prompt model text is not valid UTF-8 and cannot be sent to inference: {error}"
            )
        })?;
        let now = (self.claim_clock)();
        let conn = self.lock_conn();
        let tx = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)?;

        // The global conversation id is itself an authority boundary. Never
        // let a caller in workspace B attach a prompt to workspace A's row.
        let owner: Option<String> = tx
            .query_row(
                "SELECT workspace_key FROM conversations WHERE id = ?1",
                [conversation_id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(owner) = owner.as_deref() {
            if owner != self.workspace_id {
                anyhow::bail!(
                    "conversation id `{conversation_id}` belongs to another workspace \
                     (key {owner}); refusing to attach a prompt across the workspace fence"
                );
            }
        }

        let tick = next_tick(&tx, &self.writer_fingerprint)?;
        let created = owner.is_none();
        if created {
            // Unlike `create_with_id`, this is an INSERT, never REPLACE. A
            // prompt receipt and the conversation that owns it become visible
            // together at commit; no crash window can leave only one behind.
            tx.execute(
                "INSERT INTO conversations
                   (id, title, workspace_path, workspace_key, persona, end_reason,
                    writer_fingerprint, activity_tick, tip_hash,
                    started_at_claim, updated_at_claim)
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, ?7, ?8, ?9, ?9)",
                rusqlite::params![
                    conversation_id,
                    title.trim(),
                    self.workspace.to_string_lossy(),
                    self.workspace_id,
                    persona,
                    self.writer_fingerprint,
                    tick,
                    genesis_hash(conversation_id, &self.writer_fingerprint),
                    now,
                ],
            )?;
        } else {
            // A submitted prompt is real activity even if inference later
            // fails. Do not move the turn writer/tip pair here.
            tx.execute(
                "UPDATE conversations
                    SET activity_tick = ?2, updated_at_claim = ?3
                  WHERE id = ?1 AND workspace_key = ?4",
                rusqlite::params![conversation_id, tick, now, self.workspace_id],
            )?;
        }

        let previous_prompt_id = latest_prompt_on_conn(&tx, conversation_id, &self.workspace_id)?
            .map(|receipt| receipt.id());
        let prompt_id = PromptId::new();

        let parent = match prompt.parent_prompt_id {
            Some(parent_id) => Some(
                load_prompt_in_conversation_on_conn(
                    &tx,
                    conversation_id,
                    parent_id,
                    &self.workspace_id,
                )?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "prompt parent {parent_id} is not in conversation \
                             `{conversation_id}` in this workspace"
                    )
                })?,
            ),
            None => None,
        };
        if prompt.origin == PromptOrigin::HarnessRetry && parent.is_none() {
            anyhow::bail!("a harness retry must name an operator-prompt parent");
        }

        let root_prompt_id = match parent.as_ref() {
            Some(parent) => {
                validate_objective_root_on_conn(
                    &tx,
                    conversation_id,
                    parent.root_prompt_id(),
                    &self.workspace_id,
                )?;
                parent.root_prompt_id()
            }
            None => prompt_id,
        };
        let active_operator = match (prompt.origin, parent.as_ref()) {
            // A fresh operator submission is its own active authority.
            (PromptOrigin::Operator, None) => None,
            // bug/steering-regressions: an operator CONTINUATION (a decision
            // or clarification reply bound to a parent ask) REFINES the parent
            // objective — it must not usurp it as the active operator prompt.
            // Otherwise the protected active-prompt card carries the ceremony
            // ("1: proceed") for the whole agentic turn and mid-turn
            // compaction evicts the real task (live gpt-4.1 + Qwen3-Coder
            // drives, 2026-07-26/27).
            (PromptOrigin::Operator, Some(parent)) | (PromptOrigin::HarnessRetry, Some(parent)) => {
                let (active, parent_depth) = resolve_active_operator_on_conn(
                    &tx,
                    conversation_id,
                    parent,
                    &self.workspace_id,
                )?;
                if parent_depth >= MAX_PROMPT_LINEAGE_DEPTH {
                    anyhow::bail!(
                        "prompt lineage would exceed the maximum prompt lineage depth of \
                         {MAX_PROMPT_LINEAGE_DEPTH} receipts"
                    );
                }
                Some(active)
            }
            (PromptOrigin::HarnessRetry, None) => {
                anyhow::bail!("a harness retry must name an operator-prompt parent")
            }
        };
        let active_operator_id = active_operator
            .as_ref()
            .map_or(prompt_id, PromptReceipt::id);

        let receipt = PromptReceipt::new(
            prompt_id,
            conversation_id.to_string(),
            self.writer_fingerprint.clone(),
            tick,
            previous_prompt_id,
            parent.as_ref().map(PromptReceipt::id),
            root_prompt_id,
            active_operator_id,
            prompt.origin,
            prompt.raw_text,
            prompt.model_text,
            now,
        );
        insert_prompt_receipt(&tx, &receipt)?;
        let context = TurnPromptContext::new(
            receipt.clone(),
            active_operator.unwrap_or_else(|| receipt.clone()),
        );
        tx.commit()?;
        drop(conn);

        if created {
            // The receipt is already committed. Retention is housekeeping,
            // not part of prompt acceptance: reporting an error here would
            // tell the caller the prompt was not recorded even though it is
            // durable, inviting a duplicate retry. Keep the accepted receipt
            // authoritative and surface pruning failure as a diagnostic.
            if let Err(error) = self.prune_to_cap_excluding(conversation_id) {
                tracing::warn!(
                    %error,
                    conversation_id,
                    "prompt committed but conversation retention pruning failed"
                );
            }
        }
        Ok(context)
    }

    /// Read one prompt by its stable address, fenced to this store's
    /// workspace. An address owned by another workspace is indistinguishable
    /// from absence.
    pub fn load_prompt(&self, prompt_id: PromptId) -> anyhow::Result<Option<PromptReceipt>> {
        let conn = self.lock_conn();
        load_prompt_on_conn(&conn, prompt_id, &self.workspace_id)
    }

    /// Read one prompt only when both its address and owning conversation
    /// match. This is the narrow resolver used by an always-on prompt tool: a
    /// model cannot use a valid same-workspace handle to escape its active
    /// conversation.
    pub fn load_prompt_in_conversation(
        &self,
        conversation_id: &str,
        prompt_id: PromptId,
    ) -> anyhow::Result<Option<PromptReceipt>> {
        validate_record_id(conversation_id)?;
        let conn = self.lock_conn();
        load_prompt_in_conversation_on_conn(&conn, conversation_id, prompt_id, &self.workspace_id)
    }

    /// The most recently received prompt in a conversation, or `None` for an
    /// unknown/empty conversation. Receipt order is SQLite-serialized append
    /// order, not a wall-clock claim.
    pub fn latest_prompt(&self, conversation_id: &str) -> anyhow::Result<Option<PromptReceipt>> {
        validate_record_id(conversation_id)?;
        let conn = self.lock_conn();
        latest_prompt_on_conn(&conn, conversation_id, &self.workspace_id)
    }

    /// Follow the automatic chronological predecessor link for one prompt.
    pub fn previous_prompt(&self, prompt_id: PromptId) -> anyhow::Result<Option<PromptReceipt>> {
        let conn = self.lock_conn();
        let tx = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Deferred)?;
        let Some(receipt) = load_prompt_on_conn(&tx, prompt_id, &self.workspace_id)? else {
            tx.commit()?;
            return Ok(None);
        };
        let Some(previous) = receipt.previous_prompt_id() else {
            tx.commit()?;
            return Ok(None);
        };
        let previous = load_prompt_in_conversation_on_conn(
            &tx,
            receipt.conversation_id(),
            previous,
            &self.workspace_id,
        )?;
        tx.commit()?;
        Ok(previous)
    }

    /// All prompt receipts in durable receipt order for one conversation.
    pub fn prompt_chain(&self, conversation_id: &str) -> anyhow::Result<Vec<PromptReceipt>> {
        validate_record_id(conversation_id)?;
        let conn = self.lock_conn();
        prompt_chain_on_conn(&conn, conversation_id, &self.workspace_id)
    }

    /// Rebuild the submitted-vs-active authority context for a prompt. An
    /// operator receipt is active itself; a harness retry resolves to the
    /// nearest validated operator authority inherited from its parent. The
    /// objective root remains a separate lineage pointer.
    pub fn turn_prompt_context(
        &self,
        conversation_id: &str,
        submitted_prompt_id: PromptId,
    ) -> anyhow::Result<Option<TurnPromptContext>> {
        validate_record_id(conversation_id)?;
        let conn = self.lock_conn();
        let tx = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Deferred)?;
        let Some(submitted) = load_prompt_in_conversation_on_conn(
            &tx,
            conversation_id,
            submitted_prompt_id,
            &self.workspace_id,
        )?
        else {
            tx.commit()?;
            return Ok(None);
        };
        let (active, _) =
            resolve_active_operator_on_conn(&tx, conversation_id, &submitted, &self.workspace_id)?;
        tx.commit()?;
        Ok(Some(TurnPromptContext::new(submitted, active)))
    }
}

/// SQLite representation of an immutable prompt receipt. Conversion is kept
/// separate from rusqlite's row callback so address parsing and cryptographic
/// verification can return rich `anyhow` errors.
#[derive(Debug)]
struct PromptRow {
    id: String,
    conversation_id: String,
    writer_fingerprint: String,
    seq: i64,
    previous_prompt_id: Option<String>,
    parent_prompt_id: Option<String>,
    root_prompt_id: String,
    active_operator_id: Option<String>,
    origin: String,
    raw_text: Vec<u8>,
    model_text: Vec<u8>,
    raw_digest: String,
    model_digest: String,
    receipt_hash: String,
    ts_claim: i64,
    encoding_version: i64,
}

impl PromptRow {
    fn into_receipt(self) -> anyhow::Result<PromptReceipt> {
        let receipt = PromptReceipt::from_stored_parts(
            self.id.parse()?,
            self.conversation_id,
            self.writer_fingerprint,
            self.seq,
            self.previous_prompt_id.map(|id| id.parse()).transpose()?,
            self.parent_prompt_id.map(|id| id.parse()).transpose()?,
            self.root_prompt_id.parse()?,
            self.active_operator_id.map(|id| id.parse()).transpose()?,
            PromptOrigin::from_db_str(&self.origin)?,
            self.raw_text,
            self.model_text,
            self.raw_digest,
            self.model_digest,
            self.receipt_hash,
            self.ts_claim,
            self.encoding_version,
        );
        receipt.verify_integrity()?;
        Ok(receipt)
    }
}

fn prompt_row_from_sql(row: &rusqlite::Row<'_>) -> rusqlite::Result<PromptRow> {
    Ok(PromptRow {
        id: row.get(0)?,
        conversation_id: row.get(1)?,
        writer_fingerprint: row.get(2)?,
        seq: row.get(3)?,
        previous_prompt_id: row.get(4)?,
        parent_prompt_id: row.get(5)?,
        root_prompt_id: row.get(6)?,
        active_operator_id: row.get(7)?,
        origin: row.get(8)?,
        raw_text: row.get(9)?,
        model_text: row.get(10)?,
        raw_digest: row.get(11)?,
        model_digest: row.get(12)?,
        receipt_hash: row.get(13)?,
        ts_claim: row.get(14)?,
        encoding_version: row.get(15)?,
    })
}

pub(super) fn insert_prompt_receipt(
    conn: &Connection,
    receipt: &PromptReceipt,
) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO prompt_receipts
           (id, conversation_id, writer_fingerprint, seq, previous_prompt_id,
            parent_prompt_id, root_prompt_id, active_operator_id, origin,
            raw_text, model_text, raw_digest, model_digest, receipt_hash,
            ts_claim, encoding_version)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        rusqlite::params![
            receipt.id().to_string(),
            receipt.conversation_id(),
            receipt.writer_fingerprint(),
            receipt.seq(),
            receipt.previous_prompt_id().map(|id| id.to_string()),
            receipt.parent_prompt_id().map(|id| id.to_string()),
            receipt.root_prompt_id().to_string(),
            receipt.active_operator_id().map(|id| id.to_string()),
            receipt.origin().as_db_str(),
            receipt.raw_text(),
            receipt.model_text(),
            receipt.raw_digest(),
            receipt.model_digest(),
            receipt.receipt_hash(),
            receipt.ts_claim(),
            receipt.encoding_version(),
        ],
    )?;
    Ok(())
}

fn load_prompt_on_conn(
    conn: &Connection,
    prompt_id: PromptId,
    workspace_id: &str,
) -> anyhow::Result<Option<PromptReceipt>> {
    let row = conn
        .query_row(
            "SELECT p.id, p.conversation_id, p.writer_fingerprint, p.seq,
                    p.previous_prompt_id, p.parent_prompt_id, p.root_prompt_id,
                    p.active_operator_id, p.origin, p.raw_text, p.model_text,
                    p.raw_digest, p.model_digest, p.receipt_hash, p.ts_claim,
                    p.encoding_version
               FROM prompt_receipts p
               JOIN conversations c ON c.id = p.conversation_id
              WHERE p.id = ?1 AND c.workspace_key = ?2",
            rusqlite::params![prompt_id.to_string(), workspace_id],
            prompt_row_from_sql,
        )
        .optional()?;
    row.map(PromptRow::into_receipt).transpose()
}

pub(super) fn load_prompt_in_conversation_on_conn(
    conn: &Connection,
    conversation_id: &str,
    prompt_id: PromptId,
    workspace_id: &str,
) -> anyhow::Result<Option<PromptReceipt>> {
    let row = conn
        .query_row(
            "SELECT p.id, p.conversation_id, p.writer_fingerprint, p.seq,
                    p.previous_prompt_id, p.parent_prompt_id, p.root_prompt_id,
                    p.active_operator_id, p.origin, p.raw_text, p.model_text,
                    p.raw_digest, p.model_digest, p.receipt_hash, p.ts_claim,
                    p.encoding_version
               FROM prompt_receipts p
               JOIN conversations c ON c.id = p.conversation_id
              WHERE p.id = ?1 AND p.conversation_id = ?2
                AND c.workspace_key = ?3",
            rusqlite::params![prompt_id.to_string(), conversation_id, workspace_id],
            prompt_row_from_sql,
        )
        .optional()?;
    row.map(PromptRow::into_receipt).transpose()
}

pub(super) fn validate_objective_root_on_conn(
    conn: &Connection,
    conversation_id: &str,
    root_prompt_id: PromptId,
    workspace_id: &str,
) -> anyhow::Result<PromptReceipt> {
    let root =
        load_prompt_in_conversation_on_conn(conn, conversation_id, root_prompt_id, workspace_id)?
            .ok_or_else(|| {
            anyhow::anyhow!(
                "prompt root {root_prompt_id} is missing from conversation `{conversation_id}`"
            )
        })?;
    if root.origin() != PromptOrigin::Operator
        || root.root_prompt_id() != root.id()
        || root
            .active_operator_id()
            .is_some_and(|active| active != root.id())
    {
        anyhow::bail!("prompt root {root_prompt_id} is not a self-rooted operator prompt");
    }
    Ok(root)
}

/// Resolve the nearest operator authority through explicit semantic parentage.
///
/// Version-2 receipts persist and hash the expected result, but the parent walk
/// remains the validator: a stored pointer that disagrees with its parent's
/// authority is rejected. Version-1 rows have no pointer and are recovered by
/// the same walk, preserving receipts written before the additive column. The
/// walk is iterative and capped at [`MAX_PROMPT_LINEAGE_DEPTH`] receipts so a
/// corrupt database cannot induce stack growth or unbounded traversal.
fn resolve_active_operator_on_conn(
    conn: &Connection,
    conversation_id: &str,
    submitted: &PromptReceipt,
    workspace_id: &str,
) -> anyhow::Result<(PromptReceipt, usize)> {
    validate_objective_root_on_conn(
        conn,
        conversation_id,
        submitted.root_prompt_id(),
        workspace_id,
    )?;
    let objective_root = submitted.root_prompt_id();
    let mut visited = std::collections::HashSet::new();
    let mut retry_authorities: Vec<(PromptId, Option<PromptId>)> = Vec::new();
    let mut current = submitted.clone();

    for depth in 1..=MAX_PROMPT_LINEAGE_DEPTH {
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
            current = load_prompt_in_conversation_on_conn(
                conn,
                conversation_id,
                parent_id,
                workspace_id,
            )?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "prompt {} references missing parent {parent_id}",
                    current.id()
                )
            })?;
        } else {
            for (retry_id, stored_authority) in retry_authorities {
                if stored_authority.is_some_and(|stored| stored != current.id()) {
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
         {MAX_PROMPT_LINEAGE_DEPTH} receipts",
        submitted.id()
    )
}

fn latest_prompt_on_conn(
    conn: &Connection,
    conversation_id: &str,
    workspace_id: &str,
) -> anyhow::Result<Option<PromptReceipt>> {
    // `receipt_order` is serialized database presentation metadata, not part
    // of an immutable receipt hash. Never trust its largest value by itself:
    // validate the complete hashed `previous_prompt_id` chain first, then take
    // its verified tip. This makes both reads and the next append fail closed
    // if mutable row order is corrupt instead of silently forking chronology.
    let mut chain = prompt_chain_on_conn(conn, conversation_id, workspace_id)?;
    Ok(chain.pop())
}

pub(super) fn prompt_chain_on_conn(
    conn: &Connection,
    conversation_id: &str,
    workspace_id: &str,
) -> anyhow::Result<Vec<PromptReceipt>> {
    let mut stmt = conn.prepare(
        "SELECT p.id, p.conversation_id, p.writer_fingerprint, p.seq,
                p.previous_prompt_id, p.parent_prompt_id, p.root_prompt_id,
                p.active_operator_id, p.origin, p.raw_text, p.model_text,
                p.raw_digest, p.model_digest, p.receipt_hash, p.ts_claim,
                p.encoding_version
           FROM prompt_receipts p
           JOIN conversations c ON c.id = p.conversation_id
          WHERE p.conversation_id = ?1 AND c.workspace_key = ?2
          ORDER BY p.receipt_order ASC",
    )?;
    let rows = stmt.query_map(
        rusqlite::params![conversation_id, workspace_id],
        prompt_row_from_sql,
    )?;
    let receipts: Vec<PromptReceipt> = rows
        .map(|row| {
            row.map_err(anyhow::Error::from)
                .and_then(PromptRow::into_receipt)
        })
        .collect::<anyhow::Result<_>>()?;
    let mut expected_previous = None;
    for receipt in &receipts {
        if receipt.previous_prompt_id() != expected_previous {
            anyhow::bail!(
                "prompt chronology mismatch in conversation `{conversation_id}` at {}: \
                 expected previous {:?}, stored {:?}",
                receipt.id(),
                expected_previous,
                receipt.previous_prompt_id()
            );
        }
        expected_previous = Some(receipt.id());
    }
    Ok(receipts)
}
