//! One-shot retirement of the v1 JSON store and UUIDv5 workspace keys.
//!
//! Import must run before workspace re-keying so records imported during this
//! open receive the v2 key. The parent [`store`](super) module documents the
//! full contract under “One-time JSON import” and “Workspace identity v2”.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

use crate::conversation::ConversationRecord;

use super::turn_chain::{genesis_hash, insert_turn_row, next_tick, TurnRow};
use super::{clamp_claim, validate_record_id, ConversationStore};

/// The retired JSON backend's tree under the store root: one
/// `<workspace-uuid>/<id>.json` per conversation. Imported once on open.
const LEGACY_JSON_DIR: &str = "conversations";

/// Where the legacy tree is moved after a successful import (kept as a
/// backup, never deleted by newt).
const LEGACY_BACKUP_DIR: &str = "conversations.imported";

/// SQL expression deriving a space-joined list of `$.{key}` string values
/// from the `events` JSON array carried by `source` (e.g. `new.events`,
/// `old.events`, or the bare column in the content view).
/// One-time import of the retired JSON backend's tree (see the module docs).
///
/// Runs on every open and is a fast no-op when `<root>/conversations/` does
/// not exist (i.e. always, after the first successful import renames it to
/// the backup dir). Records are imported oldest-first by the legacy MRU
/// ordering (`updated_at`, then `created_at`, then id — the JSON backend's
/// own sort), so ticks assigned in import order reproduce the conversation
/// ordering users saw before the migration.
pub(super) fn import_legacy_json(
    conn: &Connection,
    root: &Path,
    writer_fingerprint: &str,
) -> anyhow::Result<()> {
    let legacy_root = root.join(LEGACY_JSON_DIR);
    if !legacy_root.is_dir() {
        return Ok(());
    }
    let mut records = collect_legacy_records(&legacy_root)?;
    records.sort_by(|a, b| {
        a.updated_at_unix_nanos
            .cmp(&b.updated_at_unix_nanos)
            .then_with(|| a.created_at_unix_nanos.cmp(&b.created_at_unix_nanos))
            .then_with(|| a.id.cmp(&b.id))
    });
    let mut imported = 0usize;
    for record in &records {
        if import_one_record(conn, record, writer_fingerprint)? {
            imported += 1;
        }
    }
    let backup = retire_legacy_dir(root, &legacy_root)?;
    tracing::info!(
        imported,
        found = records.len(),
        backup = %backup.display(),
        "one-time import of legacy JSON conversations complete; \
         the original tree is kept as a backup"
    );
    Ok(())
}

/// 17.2 one-shot row migration (see module docs — Workspace identity v2):
/// re-key every conversation that carries THIS workspace's retired UUIDv5
/// key to the v2 key, in one UPDATE inside an immediate transaction.
///
/// Idempotent by construction — once no rows carry the old key the UPDATE
/// matches nothing. Scoped by construction — a UUIDv5 key is derived from
/// one canonical path, so the WHERE clause can only ever select rows that
/// belonged to this workspace; every other workspace's rows are left for
/// their own open to migrate. Re-keying is metadata, not activity: no tick
/// is allocated, and the §6 chain is untouched (`workspace_key` is not part
/// of the turn encoding or the genesis hash).
pub(super) fn migrate_workspace_key(
    conn: &Connection,
    workspace: &Path,
    v2_key: &str,
) -> anyhow::Result<()> {
    // The deprecated v1 derivation is retained exactly for this lookup.
    #[allow(deprecated)]
    let old_key = ConversationStore::workspace_id_for_path(workspace)?;
    let tx = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let migrated = tx.execute(
        "UPDATE conversations SET workspace_key = ?2 WHERE workspace_key = ?1",
        rusqlite::params![old_key, v2_key],
    )?;
    tx.commit()?;
    if migrated > 0 {
        tracing::info!(
            migrated,
            workspace = %workspace.display(),
            "re-keyed conversations from the retired UUIDv5 workspace key to v2"
        );
    }
    Ok(())
}

/// Walk `<legacy_root>/<workspace-uuid>/<id>.json` and parse every readable
/// record — all workspaces, not just the opening store's. Corrupt or
/// unreadable records are skipped with a warning (the legacy store's own
/// semantics); whatever is skipped survives untouched in the backup dir.
fn collect_legacy_records(legacy_root: &Path) -> anyhow::Result<Vec<ConversationRecord>> {
    let mut records = Vec::new();
    for ws_entry in std::fs::read_dir(legacy_root)? {
        let ws_dir = ws_entry?.path();
        if !ws_dir.is_dir() {
            // Stray file at the workspace level — not a record; the backup
            // rename preserves it.
            continue;
        }
        let dir_key = ws_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        for entry in std::fs::read_dir(&ws_dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                // The legacy store only ever read `.json` (crash-leftover
                // `.tmp` files were invisible to it).
                continue;
            }
            let parsed = std::fs::read_to_string(&path)
                .map_err(anyhow::Error::from)
                .and_then(|text| Ok(serde_json::from_str::<ConversationRecord>(&text)?));
            let record = match parsed {
                Ok(record) => record,
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "skipping unreadable legacy conversation record \
                         (the file is kept in the import backup)"
                    );
                    continue;
                }
            };
            // The legacy store only served records whose body workspace_id
            // matched their dir name; a mismatched record was invisible to
            // every workspace. Importing it would resurrect data no store
            // could see — skip it (it stays in the backup).
            if record.workspace_id != dir_key {
                tracing::warn!(
                    path = %path.display(),
                    body_workspace = %record.workspace_id,
                    dir_workspace = %dir_key,
                    "skipping legacy record whose workspace id does not match its dir"
                );
                continue;
            }
            if let Err(e) = validate_record_id(&record.id) {
                tracing::warn!(path = %path.display(), error = %e, "skipping legacy record");
                continue;
            }
            records.push(record);
        }
    }
    Ok(records)
}

/// Import one legacy record inside its own `BEGIN IMMEDIATE` transaction:
/// conversation row first (one tick, genesis tip), then each turn through
/// the normal tick + chain path, then the activity/tip update — exactly the
/// shape the live write path produces, so `verify_chain` holds post-import.
/// Returns `false` (and writes nothing) when the id already exists in the
/// database — in any workspace: that means an earlier pass imported it (or
/// the id collides), and the import never overwrites.
fn import_one_record(
    conn: &Connection,
    record: &ConversationRecord,
    writer_fingerprint: &str,
) -> anyhow::Result<bool> {
    let tx = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let already: Option<i64> = tx
        .query_row(
            "SELECT 1 FROM conversations WHERE id = ?1",
            [&record.id],
            |row| row.get(0),
        )
        .optional()?;
    if already.is_some() {
        tracing::debug!(id = %record.id, "legacy conversation already in the db; skipping");
        return Ok(false);
    }

    // §6: the legacy unix_nanos enter ONLY as display claims.
    let started_claim = clamp_claim(record.created_at_unix_nanos);
    let updated_claim = clamp_claim(record.updated_at_unix_nanos);
    let create_tick = next_tick(&tx, writer_fingerprint)?;
    tx.execute(
        "INSERT INTO conversations
           (id, title, workspace_path, workspace_key, persona, end_reason,
            writer_fingerprint, activity_tick, tip_hash,
            started_at_claim, updated_at_claim)
         VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, ?7, ?8, ?9, ?10)",
        rusqlite::params![
            record.id,
            record.title,
            record.workspace,
            record.workspace_id,
            record.persona,
            writer_fingerprint,
            create_tick,
            genesis_hash(&record.id, writer_fingerprint),
            started_claim,
            updated_claim,
        ],
    )?;

    let mut prev_hash = genesis_hash(&record.id, writer_fingerprint);
    let mut last_tick = create_tick;
    for turn in &record.turns {
        let seq = next_tick(&tx, writer_fingerprint)?;
        let row = TurnRow {
            conversation_id: record.id.clone(),
            writer_fingerprint: writer_fingerprint.to_string(),
            seq,
            prev_hash,
            user: turn.user.clone(),
            assistant: turn.assistant.clone(),
            events: "[]".to_string(),
            tokens_in: None,
            tokens_out: None,
            // #717 / #1786: the legacy JSON backend predates phantom reaches
            // and sources alike — empty, exactly as `events: "[]"`.
            phantom_reaches: "[]".to_string(),
            sources: "[]".to_string(),
            // The legacy format recorded no per-turn time; the record-level
            // updated_at is the only available claim (display only, §6).
            ts_claim: updated_claim,
            // PINNED at v1, deliberately NOT `TURN_ENCODING_VERSION_CURRENT`
            // (#1786 spec §9.1): legacy records carry no sources or reaches,
            // so v2 buys them nothing — and a v1-pinned import keeps a
            // post-import rollback able to verify the imported history. The
            // import retires its source tree and cannot re-run, so a
            // rolled-back binary otherwise faces a store it can neither
            // verify nor re-import.
            encoding_version: 1,
        };
        insert_turn_row(&tx, &row)?;
        prev_hash = row.content_hash()?;
        last_tick = seq;
    }
    if !record.turns.is_empty() {
        tx.execute(
            "UPDATE conversations SET activity_tick = ?2, tip_hash = ?3 WHERE id = ?1",
            rusqlite::params![record.id, last_tick, prev_hash],
        )?;
        // #1786 §5: the import writes the per-writer witness too — it is a
        // fresh producer writing rows in a transaction that already computed
        // every hash, so "the writer predates the table" is false here, and
        // absence would silently reopen the multi-writer hole for every
        // imported conversation the moment the operator's fingerprint later
        // changes (the exact writer-handoff motivation of #1794 residual 2).
        tx.execute(
            "INSERT INTO writer_tips (conversation_id, writer_fingerprint, tip_hash, tip_seq)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (conversation_id, writer_fingerprint)
             DO UPDATE SET tip_hash = excluded.tip_hash, tip_seq = excluded.tip_seq",
            rusqlite::params![record.id, writer_fingerprint, prev_hash, last_tick],
        )?;
    }
    tx.commit()?;
    Ok(true)
}

/// Move the legacy tree to the backup name (`conversations.imported/`,
/// suffixed if that already exists). A concurrent opener may win the rename;
/// finding the source already gone is success, not an error.
fn retire_legacy_dir(root: &Path, legacy_root: &Path) -> anyhow::Result<PathBuf> {
    for attempt in 0u32..100 {
        let candidate = if attempt == 0 {
            root.join(LEGACY_BACKUP_DIR)
        } else {
            root.join(format!("{LEGACY_BACKUP_DIR}.{attempt}"))
        };
        if candidate.exists() {
            continue;
        }
        return match std::fs::rename(legacy_root, &candidate) {
            Ok(()) => Ok(candidate),
            Err(_) if !legacy_root.exists() => Ok(candidate),
            Err(e) => Err(anyhow::Error::from(e).context(format!(
                "imported legacy conversations but could not move {} aside to {}",
                legacy_root.display(),
                candidate.display()
            ))),
        };
    }
    anyhow::bail!(
        "no free backup name for {} (conversations.imported* all taken)",
        legacy_root.display()
    )
}
