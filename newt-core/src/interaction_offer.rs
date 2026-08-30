//! **The offer/answer transport** (B0b-2, #1846).
//!
//! Replaces `permission_requests`. A published offer is an
//! [`InteractionDefinition`] plus the [`InteractionInstance`] that binds
//! it, stored so a second process — the web attachment — can render it and
//! answer it, and so the answer can be bound back to the offer that was
//! actually published.
//!
//! # One row, one CAS
//!
//! The offer and its terminal state share a row. The race has two kinds of
//! winner — a surface ANSWERS (producing a [`Response`]) or the local
//! operator CANCELS (producing none) — and a single `outcome IS NULL`
//! compare-and-swap serializes both, exactly as the old
//! `resolved = 0 AND verdict IS NULL` CAS did.
//!
//! That is why this does not route through A3's generic
//! [`ResolutionStore`](newt_interaction::ResolutionStore): its contract
//! takes a `ResolutionRecord` carrying a required `ResponseId`, and a local
//! cancellation has no response to name. Forcing one would mean either a
//! sentinel response id or offering Back/Exit as options of the form —
//! and the second would move A0's frozen goldens. `ResolutionStore` stays
//! A3's generic facility with its own contract tests; the permission
//! transport needs a decision point that can also say "nobody answered".
//!
//! # What the instance binding now buys
//!
//! B0b-1 could only mint an instance at answer time, so the DEFINITION
//! binding was authoritative while the INSTANCE binding was merely
//! self-consistent. The instance is persisted here, so an answer is now
//! validated against the offer that was actually published — the
//! cross-process bound B0b-1 explicitly did not carry.

use rusqlite::{OptionalExtension, TransactionBehavior};

use newt_interaction::{Audience, InteractionDefinition, InteractionInstance, Lifecycle, Response};

use crate::store::{AnswerOutcome, ConversationStore};
use crate::PermissionAction;

/// The danger tier a gate stamped on an offer.
///
/// A plain word, never JSON. The column this replaces was written as a
/// JSON string (`"\"high\""`) and read as a JSON array, so its reader
/// could never return `false` — #1836.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfferDanger {
    /// Ordinary.
    Low,
    /// High danger: durable grants are refused and a human is asked.
    High,
}

impl OfferDanger {
    /// The stored form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::High => "high",
        }
    }

    /// Read a stored tier. An unreadable tier is HIGH: the failure
    /// direction that asks a human rather than the one that skips them.
    #[must_use]
    pub fn from_stored(text: &str) -> Self {
        if text.eq_ignore_ascii_case("low") {
            Self::Low
        } else {
            Self::High
        }
    }
}

/// A published offer that is still answerable.
#[derive(Debug, Clone)]
pub struct PendingOffer {
    /// The offer's canonical `InstanceId`, as published.
    pub instance_id: String,
    /// The definition, serialized.
    pub definition_json: String,
    /// The instance that binds it, serialized.
    pub instance_json: String,
    /// The gate-stamped tier.
    pub danger: OfferDanger,
}

/// Parse a stored instance and check it against the identity it is filed
/// under.
///
/// **Why this is not merely a parse.** `instance_id` is a `ContentId` over
/// the instance, so the row key IS the checksum for these bytes. Until now
/// it was computed once at publish time and never read back — the
/// `provenance-audit` skill's third question, *is the evidence actually
/// read*, answered no. The bytes come out of a row a SECOND process can
/// write, so re-deriving here is input validation at a trust boundary.
///
/// It is a free function with a named caller rather than an inline check
/// inside the answer path, because an unreachable correct answer is how
/// this epic got its worst bug: `RawGuard` was private, so the next surface
/// reached for crossterm and inherited the defect three times (C2b, #1891).
/// Anything needing the stored instance goes through here.
///
/// `None` deliberately conflates "does not parse" with "is not what it
/// claims to be": neither may authorize anything, and a caller able to tell
/// them apart would be tempted to treat one as recoverable.
#[must_use]
fn verified_instance(instance_id: &str, instance_json: &str) -> Option<InteractionInstance> {
    let instance: InteractionInstance = serde_json::from_str(instance_json).ok()?;
    (instance.instance_id().ok()?.to_string() == instance_id).then_some(instance)
}

impl PendingOffer {
    /// The definition this offer publishes.
    ///
    /// # Errors
    ///
    /// A deserialization failure.
    pub fn definition(&self) -> serde_json::Result<InteractionDefinition> {
        serde_json::from_str(&self.definition_json)
    }

    /// The instance that binds it, checked against the identity it is
    /// filed under.
    ///
    /// `None` when the bytes do not parse, or when they do not hash to
    /// `instance_id` — see [`verified_instance`].
    #[must_use]
    pub fn instance(&self) -> Option<InteractionInstance> {
        verified_instance(&self.instance_id, &self.instance_json)
    }
}

impl ConversationStore {
    /// Publish an offer for a conversation, returning its instance id.
    ///
    /// # Errors
    ///
    /// An unknown conversation, one owned by another workspace, or a
    /// storage failure.
    pub fn publish_interaction_offer(
        &self,
        conversation_id: &str,
        definition: &InteractionDefinition,
        danger: OfferDanger,
        audience: Audience,
    ) -> anyhow::Result<String> {
        // The STORE stamps the fence and the tick. A caller that minted the
        // instance itself could stamp a fence the row is not filed under,
        // and then every answer to that offer would be refused for a reason
        // no operator could see. Minting here makes that unrepresentable.
        let (instance, _lifecycle) = crate::interaction_gate::mint_offer(
            definition,
            self.workspace_fence(),
            conversation_id,
            audience,
            self.claim_tick(),
        )
        .map_err(|e| anyhow::anyhow!("cannot mint an offer for this definition: {e}"))?;
        let instance = &instance;
        let instance_id = instance
            .instance_id()
            .map_err(|e| anyhow::anyhow!("offer has no identity: {e}"))?
            .to_string();
        let definition_json = serde_json::to_string(definition)?;
        let instance_json = serde_json::to_string(instance)?;
        let now = self.claim_tick();

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
            None => anyhow::bail!(
                "cannot publish an interaction offer for unknown conversation `{conversation_id}`"
            ),
            Some(key) if key != self.workspace_fence() => {
                anyhow::bail!("conversation `{conversation_id}` belongs to another workspace")
            }
            _ => {}
        }
        tx.execute(
            "INSERT OR REPLACE INTO interaction_offers
               (instance_id, conversation_id, workspace_key, definition_json,
                instance_json, danger_tier, published_tick, outcome, response_json, resolved_tick)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, NULL, NULL)",
            rusqlite::params![
                instance_id,
                conversation_id,
                self.workspace_fence(),
                definition_json,
                instance_json,
                danger.as_str(),
                now,
            ],
        )?;
        tx.commit()?;
        Ok(instance_id)
    }

    /// The oldest still-answerable offer for a conversation.
    ///
    /// Workspace-fenced, and TTL'd on the same clock the offer's own
    /// `ttl_ticks` was minted from.
    ///
    /// # Errors
    ///
    /// A storage failure.
    pub fn pending_interaction_offer(
        &self,
        conversation_id: &str,
    ) -> anyhow::Result<Option<PendingOffer>> {
        let conn = self.lock_conn();
        let cutoff = self
            .claim_tick()
            .saturating_sub(Self::PERMISSION_REQUEST_TTL_NANOS);
        conn.query_row(
            "SELECT instance_id, definition_json, instance_json, danger_tier
               FROM interaction_offers
              WHERE conversation_id = ?1 AND workspace_key = ?2 AND outcome IS NULL
                AND published_tick > ?3
              ORDER BY published_tick ASC LIMIT 1",
            rusqlite::params![conversation_id, self.workspace_fence(), cutoff],
            |row| {
                Ok(PendingOffer {
                    instance_id: row.get(0)?,
                    definition_json: row.get(1)?,
                    instance_json: row.get(2)?,
                    danger: OfferDanger::from_stored(&row.get::<_, String>(3)?),
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    /// Answer an offer with one action, authorizing it first.
    ///
    /// The authorization is `validate_response`'s, against the definition
    /// AND the instance as published — so an answer is now bound to the
    /// offer that was actually made, not to one minted at answer time.
    ///
    /// # Errors
    ///
    /// A storage failure. An unauthorized or losing answer is an
    /// [`AnswerOutcome`], not an error.
    pub fn answer_interaction_offer(
        &self,
        conversation_id: &str,
        instance_id: &str,
        action: PermissionAction,
        audience: Audience,
    ) -> anyhow::Result<AnswerOutcome> {
        let now = self.claim_tick();
        let conn = self.lock_conn();
        let tx = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)?;
        let row: Option<(String, String, Option<String>)> = tx
            .query_row(
                "SELECT definition_json, instance_json, outcome FROM interaction_offers
                  WHERE instance_id = ?1 AND conversation_id = ?2 AND workspace_key = ?3",
                rusqlite::params![instance_id, conversation_id, self.workspace_fence()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((definition_json, instance_json, outcome)) = row else {
            tx.commit()?;
            return Ok(AnswerOutcome::Unknown);
        };
        if outcome.is_some() {
            tx.commit()?;
            return Ok(AnswerOutcome::AlreadyResolved);
        }

        let Ok(definition) = serde_json::from_str::<InteractionDefinition>(&definition_json) else {
            tx.commit()?;
            return Ok(AnswerOutcome::InvalidAction);
        };
        // Stored bytes that do not hash to their own row key name no offer
        // this identity ever published. `Unknown`, not `InvalidAction`: the
        // action is not what is wrong, and this fails closed exactly as
        // expiry does below.
        let Some(instance) = verified_instance(instance_id, &instance_json) else {
            tx.commit()?;
            return Ok(AnswerOutcome::Unknown);
        };
        // Expiry authorizes nothing, and synthesizes nothing.
        if Lifecycle::has_elapsed(&instance, now) {
            tx.commit()?;
            return Ok(AnswerOutcome::Unknown);
        }
        let Ok(response) = crate::interaction_gate::authorized_response(
            &definition,
            &instance,
            self.workspace_fence(),
            action,
            audience,
        ) else {
            tx.commit()?;
            return Ok(AnswerOutcome::InvalidAction);
        };
        let response_json = serde_json::to_string(&response)?;

        // The CAS. Rowcount 1 means this writer resolved the offer.
        let changed = tx.execute(
            "UPDATE interaction_offers
                SET outcome = 'answered', response_json = ?4, resolved_tick = ?5
              WHERE instance_id = ?1 AND conversation_id = ?2 AND workspace_key = ?3
                AND outcome IS NULL",
            rusqlite::params![
                instance_id,
                conversation_id,
                self.workspace_fence(),
                response_json,
                now
            ],
        )?;
        tx.commit()?;
        Ok(if changed == 1 {
            AnswerOutcome::Answered
        } else {
            AnswerOutcome::AlreadyResolved
        })
    }

    /// Claim an offer locally without answering it (the TTY aborted, or
    /// the deadline fired).
    ///
    /// Returns `true` when THIS call won. `false` means a surface already
    /// answered, and its verdict stands.
    ///
    /// # Errors
    ///
    /// A storage failure.
    pub fn cancel_interaction_offer(
        &self,
        conversation_id: &str,
        instance_id: &str,
    ) -> anyhow::Result<bool> {
        let now = self.claim_tick();
        let conn = self.lock_conn();
        let changed = conn.execute(
            "UPDATE interaction_offers
                SET outcome = 'cancelled', resolved_tick = ?4
              WHERE instance_id = ?1 AND conversation_id = ?2 AND workspace_key = ?3
                AND outcome IS NULL",
            rusqlite::params![instance_id, conversation_id, self.workspace_fence(), now],
        )?;
        Ok(changed == 1)
    }

    /// The action a surface answered with, if one did.
    ///
    /// # Errors
    ///
    /// A storage failure.
    pub fn take_interaction_decision(
        &self,
        conversation_id: &str,
        instance_id: &str,
    ) -> anyhow::Result<Option<PermissionAction>> {
        Ok(self
            .interaction_response(conversation_id, instance_id)?
            .and_then(|response| {
                let [submission] = response.values.as_slice() else {
                    return None;
                };
                let newt_interaction::ControlValue::Choice { option } = &submission.value else {
                    return None;
                };
                crate::interaction_adapter::action_for_option(option.as_str())
            }))
    }

    /// **Who answered** — the audit fact `answered_by` used to carry.
    ///
    /// Recoverable because the winning Response body is persisted:
    /// `responder_provenance.audience` names the surface. `None` means
    /// nobody answered — cancelled, expired, or still open — which the
    /// offer's own row and TTL distinguish.
    ///
    /// # Errors
    ///
    /// A storage failure.
    pub fn interaction_answered_by(
        &self,
        conversation_id: &str,
        instance_id: &str,
    ) -> anyhow::Result<Option<Audience>> {
        Ok(self
            .interaction_response(conversation_id, instance_id)?
            .map(|response| response.responder_provenance.audience))
    }

    /// The winning response body, if the offer was answered.
    fn interaction_response(
        &self,
        conversation_id: &str,
        instance_id: &str,
    ) -> anyhow::Result<Option<Response>> {
        let conn = self.lock_conn();
        let stored: Option<Option<String>> = conn
            .query_row(
                "SELECT response_json FROM interaction_offers
                  WHERE instance_id = ?1 AND conversation_id = ?2 AND workspace_key = ?3
                    AND outcome = 'answered'",
                rusqlite::params![instance_id, conversation_id, self.workspace_fence()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(Some(json)) = stored else {
            return Ok(None);
        };
        Ok(serde_json::from_str(&json).ok())
    }
}

#[cfg(test)]
mod b0b2 {
    use super::*;
    use crate::store::ConversationStore;
    use std::sync::{Arc, Barrier};

    fn definition() -> InteractionDefinition {
        let question = crate::Question::<PermissionAction> {
            markdown: "\u{2298} run_command wants to run `bash`".to_string(),
            actions: vec![
                crate::Action::new(PermissionAction::AllowOnce, "a", "allow once"),
                crate::Action::new(PermissionAction::Deny, "d", "deny (default)"),
            ],
            note: None,
        };
        crate::interaction_adapter::question_to_definition(&question).unwrap()
    }

    fn store_and_conv(root: &std::path::Path, ws: &std::path::Path) -> (ConversationStore, String) {
        let store = ConversationStore::new(root, ws, 100).unwrap();
        let conv = store.create("s", None).unwrap();
        (store, conv)
    }

    /// The whole point of a transport: a DIFFERENT process can read the
    /// offer. B0b-1 could not carry the instance binding across processes
    /// because nothing persisted it.
    #[test]
    fn an_offer_survives_a_process_restart() {
        let root = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let (store, conv) = store_and_conv(root.path(), ws.path());
        let published = store
            .publish_interaction_offer(&conv, &definition(), OfferDanger::High, Audience::Web)
            .unwrap();
        drop(store);

        // A fresh connection — the shape newt-web gets on every request.
        let reopened = ConversationStore::new(root.path(), ws.path(), 100).unwrap();
        let pending = reopened
            .pending_interaction_offer(&conv)
            .unwrap()
            .expect("the offer outlived the process that published it");
        assert_eq!(pending.instance_id, published);
        assert_eq!(pending.danger, OfferDanger::High);
        // ...and it round-trips to the same definition, so the rendering a
        // second process produces is the one the gate published.
        assert_eq!(pending.definition().unwrap(), definition());
        assert_eq!(
            pending
                .instance()
                .unwrap()
                .instance_id()
                .unwrap()
                .to_string(),
            published
        );
        // The answer binds to THAT instance, which is the bound B0b-1
        // could not carry.
        assert_eq!(
            reopened
                .answer_interaction_offer(
                    &conv,
                    &published,
                    PermissionAction::AllowOnce,
                    Audience::Web
                )
                .unwrap(),
            AnswerOutcome::Answered
        );
    }

    #[test]
    fn the_offer_table_is_workspace_fenced() {
        let root = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let (store, conv) = store_and_conv(root.path(), ws.path());
        let id = store
            .publish_interaction_offer(&conv, &definition(), OfferDanger::Low, Audience::Web)
            .unwrap();
        assert!(store.pending_interaction_offer(&conv).unwrap().is_some());

        // Same database, different workspace: the offer is invisible AND
        // unanswerable.
        let foreign = ConversationStore::new(root.path(), other.path(), 100).unwrap();
        assert!(foreign.pending_interaction_offer(&conv).unwrap().is_none());
        assert_eq!(
            foreign
                .answer_interaction_offer(&conv, &id, PermissionAction::AllowOnce, Audience::Web)
                .unwrap(),
            AnswerOutcome::Unknown
        );
        assert!(!foreign.cancel_interaction_offer(&conv, &id).unwrap());
        // ...and the owner can still answer it, so the fence refused the
        // foreigner rather than breaking the row.
        assert_eq!(
            store
                .answer_interaction_offer(&conv, &id, PermissionAction::AllowOnce, Audience::Web)
                .unwrap(),
            AnswerOutcome::Answered
        );
    }

    /// **The `answered_by` decision, pinned.** The winning Response body is
    /// persisted, so the surface that answered stays recoverable — the
    /// audit fact `permission_requests.answered_by` used to carry.
    #[test]
    fn who_answered_is_recoverable_after_resolution() {
        let root = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let (store, conv) = store_and_conv(root.path(), ws.path());

        let id = store
            .publish_interaction_offer(&conv, &definition(), OfferDanger::Low, Audience::Web)
            .unwrap();
        // Nobody has answered yet.
        assert_eq!(store.interaction_answered_by(&conv, &id).unwrap(), None);
        store
            .answer_interaction_offer(&conv, &id, PermissionAction::Deny, Audience::Web)
            .unwrap();
        assert_eq!(
            store.interaction_answered_by(&conv, &id).unwrap(),
            Some(Audience::Web),
            "who answered is no longer recoverable"
        );

        // A CANCELLED offer has no answerer, and says so rather than
        // guessing — the honest analogue of the old `answered_by = 'tty'`
        // and `'expired'` rows, which recorded a claim, not an answer.
        let cancelled = store
            .publish_interaction_offer(&conv, &definition(), OfferDanger::Low, Audience::Web)
            .unwrap();
        assert!(store.cancel_interaction_offer(&conv, &cancelled).unwrap());
        assert_eq!(
            store.interaction_answered_by(&conv, &cancelled).unwrap(),
            None
        );
        assert_eq!(
            store.take_interaction_decision(&conv, &cancelled).unwrap(),
            None,
            "a cancelled offer must not yield a decision"
        );
    }

    /// **The real race**: separate connections, one per thread, released
    /// together. Two threads sharing one store would exercise the
    /// in-process `Arc<Mutex<Connection>>` and prove nothing about SQLite.
    #[test]
    fn separate_connections_racing_one_offer_resolve_once() {
        let root = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let (seed, conv) = store_and_conv(root.path(), ws.path());
        let id = seed
            .publish_interaction_offer(&conv, &definition(), OfferDanger::Low, Audience::Web)
            .unwrap();
        drop(seed);

        let barrier = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();
        for action in [PermissionAction::AllowOnce, PermissionAction::Deny] {
            let root_path = root.path().to_path_buf();
            let ws_path = ws.path().to_path_buf();
            let (conv, id) = (conv.clone(), id.clone());
            let barrier = Arc::clone(&barrier);
            workers.push(std::thread::spawn(move || {
                // A FRESH store: its own connection, as a web request gets.
                let store = ConversationStore::new(root_path, ws_path, 100).unwrap();
                barrier.wait();
                (
                    action,
                    store.answer_interaction_offer(&conv, &id, action, Audience::Web),
                )
            }));
        }
        barrier.wait();
        let outcomes: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();

        let winners: Vec<PermissionAction> = outcomes
            .iter()
            .filter(|(_, r)| matches!(r, Ok(AnswerOutcome::Answered)))
            .map(|(a, _)| *a)
            .collect();
        assert_eq!(
            winners.len(),
            1,
            "exactly one connection must win; got {outcomes:#?}"
        );
        assert!(
            outcomes
                .iter()
                .any(|(_, r)| matches!(r, Ok(AnswerOutcome::AlreadyResolved))),
            "the loser must be told it lost: {outcomes:#?}"
        );

        // Everyone afterwards reads the winner's answer, and the audit
        // fact survives the race.
        let reopened = ConversationStore::new(root.path(), ws.path(), 100).unwrap();
        assert_eq!(
            reopened.take_interaction_decision(&conv, &id).unwrap(),
            Some(winners[0])
        );
        assert_eq!(
            reopened.interaction_answered_by(&conv, &id).unwrap(),
            Some(Audience::Web)
        );
    }

    /// **The trust boundary** (C4, #1894). The answer path reads
    /// `instance_json` back out of a row a SECOND process can write, and
    /// until now nothing re-derived the identity committing to those bytes.
    ///
    /// `authorized_response` re-checks the instance→definition binding via
    /// `publish`, which leaves every OTHER field of the instance unchecked.
    /// `responder_policy.audiences` is one of those, and it is the
    /// eligibility gate at `binding.rs:426` — so widening it in the stored
    /// bytes escalates an offer the publisher scoped to one surface, under a
    /// row key still claiming to name the original.
    #[test]
    fn a_tampered_instance_cannot_answer_under_the_published_identity() {
        let root = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let (store, conv) = store_and_conv(root.path(), ws.path());
        let id = store
            .publish_interaction_offer(&conv, &definition(), OfferDanger::Low, Audience::Web)
            .unwrap();

        // Control: as published, the offer is Web-only, so the terminal is
        // refused. This is the property the tamper tries to buy.
        assert_eq!(
            store
                .answer_interaction_offer(
                    &conv,
                    &id,
                    PermissionAction::AllowOnce,
                    Audience::Terminal
                )
                .unwrap(),
            AnswerOutcome::InvalidAction,
            "a Web-only offer must refuse a Terminal answer, or this test proves nothing"
        );

        // Tamper: widen the audience set in the STORED bytes, leaving the
        // row key — the content id of the ORIGINAL instance — untouched.
        let mut instance: InteractionInstance = {
            let conn = store.lock_conn();
            let json: String = conn
                .query_row(
                    "SELECT instance_json FROM interaction_offers WHERE instance_id = ?1",
                    [&id],
                    |r| r.get(0),
                )
                .unwrap();
            serde_json::from_str(&json).unwrap()
        };
        instance.responder_policy.audiences.push(Audience::Terminal);
        assert_ne!(
            instance.instance_id().unwrap().to_string(),
            id,
            "the tamper did not change the identity, so it is not a tamper"
        );
        {
            let conn = store.lock_conn();
            conn.execute(
                "UPDATE interaction_offers SET instance_json = ?2 WHERE instance_id = ?1",
                rusqlite::params![&id, serde_json::to_string(&instance).unwrap()],
            )
            .unwrap();
        }

        assert_eq!(
            store
                .answer_interaction_offer(
                    &conv,
                    &id,
                    PermissionAction::AllowOnce,
                    Audience::Terminal
                )
                .unwrap(),
            AnswerOutcome::Unknown,
            "stored bytes that do not hash to their own row key must not authorize an answer"
        );
    }

    /// A stored tier is a plain word, and an unreadable one fails toward
    /// asking a human.
    #[test]
    fn an_unreadable_danger_tier_reads_as_high() {
        assert_eq!(OfferDanger::from_stored("low"), OfferDanger::Low);
        assert_eq!(OfferDanger::from_stored("LOW"), OfferDanger::Low);
        assert_eq!(OfferDanger::from_stored("high"), OfferDanger::High);
        // The shapes the column this replaces could hold — a JSON string,
        // a JSON array, and nothing at all — all fail closed.
        for unreadable in ["\"high\"", "[\"low\"]", "", "garbage"] {
            assert_eq!(
                OfferDanger::from_stored(unreadable),
                OfferDanger::High,
                "`{unreadable}` did not fail toward asking a human"
            );
        }
    }
}
