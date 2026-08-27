//! **Exactly-once resolution: the contract, not the storage** (A3, #1837).
//!
//! The abstract shape lives here because it is a set of function
//! signatures over types this crate already owns, which costs nothing
//! against the dependency guard. The only implementation that needs a
//! database lives in `newt-core`, which is the layer that has one.
//!
//! **This contract does not mandate a SQL shape.** The store in
//! `newt-core` resolves through a rowcount CAS inside an `Immediate`
//! transaction, but that is a property of *that* implementation — the
//! serialization it needs comes from SQLite's own locking across
//! independent connections, not from anything stated here. An in-memory
//! implementation may legitimately collapse the whole thing to one
//! compare-and-swap. What every implementation must preserve is the
//! observable contract below.
//!
//! **Idempotency keys are a promise, and a violated promise is an
//! error.** A key says "this is the same submission I already sent". If
//! two DIFFERENT responses arrive under one key, honouring the first and
//! silently discarding the second would let a retry substitute a
//! different answer — an authorization-relevant substitution disguised as
//! a network retry. So that case is
//! [`ResolutionError::IdempotencyConflict`], not a quiet first-wins. A
//! genuine retry — the same response bytes, hence the same
//! [`ResponseId`] — collapses to [`Resolution::Replayed`].

use thiserror::Error;

use crate::ids::{IdempotencyKey, InstanceId, ResponseId};

/// What a caller asks a store to persist: this response, resolving this
/// offer, under this key.
///
/// Ids only. The store is a resolver, not a second copy of the record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolutionRecord {
    /// The offer being resolved.
    pub instance: InstanceId,
    /// The response resolving it — already validated by
    /// [`validate_response`](crate::binding::validate_response).
    pub response: ResponseId,
    /// The caller's replay key for this submission.
    pub idempotency_key: IdempotencyKey,
}

/// The outcome of an attempt to resolve an offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// This response resolved the offer. Exactly one attempt per offer
    /// ever sees this.
    Won,
    /// The offer was already resolved by a different response. The loser
    /// is told WHO won, so every racer observes the same terminal fact
    /// rather than a bare failure.
    Lost {
        /// The response that resolved the offer.
        winner: ResponseId,
    },
    /// The same submission arrived again. Not a second resolution.
    Replayed {
        /// The response that resolved the offer — the same one.
        winner: ResponseId,
    },
}

/// The facts of an idempotency-key collision.
///
/// Boxed into [`ResolutionError`] rather than inlined: two content ids
/// and a key would make every `Result` in this module carry the conflict
/// case's width on the success path too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdempotencyConflict {
    /// The reused key.
    pub key: String,
    /// The response that used it first.
    pub existing: ResponseId,
    /// The different response presented under it.
    pub presented: ResponseId,
}

/// Why a resolution attempt could not be carried out.
///
/// Generic over the implementation's own error so the contract does not
/// drag a storage error type into this crate.
#[derive(Debug, Error)]
pub enum ResolutionError<E> {
    /// Two different submissions were presented under one idempotency
    /// key. Refused rather than resolved: see the module docs.
    #[error(
        "idempotency key `{}` was already used by response `{}`, \
         but `{}` is a different submission",
        .0.key, .0.existing, .0.presented
    )]
    IdempotencyConflict(Box<IdempotencyConflict>),
    /// The implementation failed.
    #[error("resolution store failed: {0}")]
    Store(E),
}

/// Persisting the exactly-once decision.
///
/// One offer resolves at most once. Every implementation must satisfy:
///
/// - the FIRST valid response for an offer returns [`Resolution::Won`];
/// - any later, different response returns [`Resolution::Lost`] naming
///   the winner — never a second `Won`, and never a silent success;
/// - the same response under the same key returns
///   [`Resolution::Replayed`], leaving the winner unchanged;
/// - a different response under an already-used key is
///   [`ResolutionError::IdempotencyConflict`];
/// - concurrent attempts from independent connections settle on ONE
///   winner, and every loser observes that same winner.
pub trait ResolutionStore {
    /// The implementation's own failure type.
    type Error;

    /// Attempt to resolve an offer, exactly once.
    ///
    /// # Errors
    ///
    /// [`ResolutionError::IdempotencyConflict`] when a key is reused for
    /// a different submission, or [`ResolutionError::Store`] when the
    /// implementation itself fails.
    fn resolve(
        &self,
        record: &ResolutionRecord,
    ) -> Result<Resolution, ResolutionError<Self::Error>>;

    /// Who resolved this offer, if anyone has.
    ///
    /// # Errors
    ///
    /// The implementation's own failure.
    fn winner(&self, instance: &InstanceId) -> Result<Option<ResponseId>, Self::Error>;
}

/// The key as a string, for a conflict message.
#[must_use]
pub fn key_display(key: &IdempotencyKey) -> String {
    key.as_str().to_string()
}
