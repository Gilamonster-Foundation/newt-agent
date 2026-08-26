//! Protocol errors. Fail closed: none of these fall back to guessing.

use thiserror::Error;

/// What can go wrong constructing or reading a protocol record.
#[derive(Debug, Error)]
pub enum ProtocolError {
    /// A canonical encoding or content-id operation failed.
    #[error("content addressing failed: {0}")]
    Identity(#[from] content_addressable::ContentError),
    /// An id was presented in a form that is not its canonical rendering.
    /// Accepting an alternate spelling would let two strings name one record.
    #[error("`{presented}` is not the canonical form of this id")]
    NonCanonicalId {
        /// What the caller supplied.
        presented: String,
    },
    /// An identifier's shape is invalid (empty, or outside the charset).
    #[error("invalid {kind}: {reason}")]
    InvalidId {
        /// Which identifier kind was rejected.
        kind: &'static str,
        /// Why it was rejected.
        reason: String,
    },
    /// A record declared a schema tag this build does not know. Unknown
    /// REQUIRED behavior fails closed (ADR law 5).
    #[error("unknown schema tag `{tag}` for {expected}")]
    UnknownSchema {
        /// The tag the record carried.
        tag: String,
        /// The tag this build understands.
        expected: &'static str,
    },
}
