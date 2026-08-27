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
    /// A REQUIRED surface feature could not be met — either this build
    /// does not recognize it, or the surface lacks it. Both are the same
    /// fact to an operator: the document cannot be shown faithfully.
    #[error(
        "cannot present this interaction: it requires `{feature}`{}",
        if *known { ", which this surface does not provide" }
        else { ", which this build does not recognize" }
    )]
    UnsupportedFeature {
        /// The feature name, verbatim as the document wrote it.
        feature: String,
        /// Whether this build recognizes the name at all.
        known: bool,
    },
    /// The bytes decode, but they are not the canonical encoding of what
    /// they decode to — reordered map keys, a non-minimal integer, an
    /// indefinite-length string. Accepting them would mint an id different
    /// from the one their author published.
    #[error(
        "non-canonical encoding of {schema}: {input_len} bytes in, \
         {decoded_len} out when re-encoded"
    )]
    NonCanonical {
        /// The tag the record declared.
        schema: String,
        /// Length of the canonical re-encoding.
        decoded_len: usize,
        /// Length of the input.
        input_len: usize,
    },
    /// The bytes are not a readable record. Corruption is a different fact
    /// from a version we do not know, and is reported differently.
    #[error("malformed record: {reason}")]
    Malformed {
        /// What could not be read.
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
