//! **The SQLite half of A3's exactly-once resolution** (#1837).
//!
//! `newt-interaction` owns the rules and the contract; this module is the
//! one thing `newt-core` supplies — a database-backed implementation of
//! [`ResolutionStore`]. There is deliberately no dispatch registry, no
//! handler table, and no policy here: handlers are caller-injected, and
//! all authority-minting stays above the store (epic law 13), where
//! `newt-tui/src/permissions.rs` already keeps it.
//!
//! **Where the exactly-once actually comes from.** Not from the in-process
//! `Arc<Mutex<Connection>>` — that serializes calls on ONE store instance
//! and would prove nothing about the race that exists. `newt-web` opens a
//! fresh [`ConversationStore`] per HTTP request, so the TTY-owning process
//! and each web request hold INDEPENDENT connections, serialized only by
//! SQLite's own locking. The guarantee therefore lives in the schema:
//! `instance_id` is the PRIMARY KEY, so a second writer's INSERT conflicts
//! rather than duplicating, and the losing writer reads back the winner
//! inside the same `Immediate` transaction. That is what
//! `resolution::separate_connections_racing_one_instance_resolve_once`
//! exercises, with a store per thread.

use anyhow::Context;
use rusqlite::{OptionalExtension, TransactionBehavior};

use newt_interaction::resolution::{
    IdempotencyConflict, Resolution, ResolutionError, ResolutionRecord, ResolutionStore,
};
use newt_interaction::{InstanceId, ResponseId};

use crate::store::ConversationStore;

/// Wrap a storage failure in the contract's error type.
fn store_err<E: Into<anyhow::Error>>(error: E) -> ResolutionError<anyhow::Error> {
    ResolutionError::Store(error.into())
}

impl ResolutionStore for ConversationStore {
    type Error = anyhow::Error;

    fn resolve(
        &self,
        record: &ResolutionRecord,
    ) -> Result<Resolution, ResolutionError<Self::Error>> {
        let instance = record.instance.to_string();
        let response = record.response.to_string();
        let key = record.idempotency_key.as_str().to_string();

        let conn = self.lock_conn();
        let tx = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)
            .map_err(store_err)?;

        // The CAS. `ON CONFLICT DO NOTHING` makes the rowcount the whole
        // decision: 1 means this writer created the resolution, 0 means
        // somebody else already had.
        let changed = tx
            .execute(
                "INSERT INTO interaction_resolutions
                     (instance_id, workspace_key, response_id, idempotency_key, resolved_tick)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(instance_id) DO NOTHING",
                rusqlite::params![
                    instance,
                    self.workspace_fence(),
                    response,
                    key,
                    self.claim_tick()
                ],
            )
            .map_err(store_err)?;
        if changed == 1 {
            tx.commit().map_err(store_err)?;
            return Ok(Resolution::Won);
        }

        // Lost the CAS, or this is a replay. Read the winner in the SAME
        // transaction, so the answer cannot be a row a third writer
        // replaced in between.
        let existing: Option<(String, String)> = tx
            .query_row(
                "SELECT response_id, idempotency_key FROM interaction_resolutions
                  WHERE instance_id = ?1 AND workspace_key = ?2",
                rusqlite::params![instance, self.workspace_fence()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(store_err)?;
        tx.commit().map_err(store_err)?;

        let Some((winner, used_key)) = existing else {
            // The INSERT conflicted but no row is visible under this
            // fence. An InstanceId embeds its own scope, so this cannot
            // happen for a well-formed instance — report it rather than
            // inventing an outcome.
            return Err(store_err(anyhow::anyhow!(
                "instance `{instance}` is resolved outside this workspace fence"
            )));
        };
        let winner = ResponseId::parse(&winner)
            .with_context(|| format!("stored winner `{winner}` is not a canonical response id"))
            .map_err(store_err)?;

        if winner == record.response {
            return Ok(Resolution::Replayed { winner });
        }
        if used_key == key {
            return Err(ResolutionError::IdempotencyConflict(Box::new(
                IdempotencyConflict {
                    key: used_key,
                    existing: winner,
                    presented: record.response,
                },
            )));
        }
        Ok(Resolution::Lost { winner })
    }

    fn winner(&self, instance: &InstanceId) -> Result<Option<ResponseId>, Self::Error> {
        let conn = self.lock_conn();
        let stored: Option<String> = conn
            .query_row(
                "SELECT response_id FROM interaction_resolutions
                  WHERE instance_id = ?1 AND workspace_key = ?2",
                rusqlite::params![instance.to_string(), self.workspace_fence()],
                |row| row.get(0),
            )
            .optional()?;
        stored
            .map(|winner| {
                ResponseId::parse(&winner).with_context(|| {
                    format!("stored winner `{winner}` is not a canonical response id")
                })
            })
            .transpose()
    }
}
