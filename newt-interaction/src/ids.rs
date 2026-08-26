//! Identifiers. Three content-derived ids, and the locators that travel
//! beside them.
//!
//! The content-derived ids ([`DefinitionId`], [`InstanceId`], [`ResponseId`])
//! are newtypes over `ContentId` with **no string constructor** — the only
//! ways in are minting from a record and [`parse`](DefinitionId::parse),
//! which refuses any spelling that is not the id's own canonical rendering.
//! Two strings must never name one record.

use content_addressable::ContentId;
use serde::{Deserialize, Serialize};

use crate::error::ProtocolError;

/// Generate the shared body of a content-derived id newtype.
macro_rules! content_id_newtype {
    ($(#[$meta:meta])* $name:ident, $kind:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(ContentId);

        impl $name {
            /// Wrap an id already minted from the record it names.
            ///
            /// Crate-private on purpose: a public constructor would let any
            /// `ContentId` be DECLARED a definition id, which is the
            /// assigned-identity failure the whole crate exists to avoid.
            /// Records mint their own typed ids; the sanctioned ways in from
            /// outside are [`parse`](Self::parse), which refuses a
            /// non-canonical spelling, and serde at a wire boundary — after
            /// which A3 revalidates against the record anyway.
            #[must_use]
            pub(crate) fn from_content_id(id: ContentId) -> Self {
                Self(id)
            }

            /// The underlying content id.
            #[must_use]
            pub fn content_id(&self) -> &ContentId {
                &self.0
            }

            /// Parse an id from its canonical rendering.
            ///
            /// # Errors
            ///
            /// [`ProtocolError::NonCanonicalId`] when `text` parses but is not
            /// byte-identical to the id's own rendering — an alternate
            /// spelling would let two strings name one record.
            pub fn parse(text: &str) -> Result<Self, ProtocolError> {
                let id: ContentId = text.parse().map_err(|_| ProtocolError::InvalidId {
                    kind: $kind,
                    reason: format!("`{text}` is not a content id"),
                })?;
                if id.to_string() != text {
                    return Err(ProtocolError::NonCanonicalId {
                        presented: text.to_string(),
                    });
                }
                Ok(Self(id))
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

content_id_newtype!(
    /// The identity of an [`InteractionDefinition`](crate::InteractionDefinition)
    /// — and therefore its **exact form digest**. A response binds this, so a
    /// definition that changed by one byte cannot be answered with an offer
    /// minted against the old one.
    DefinitionId,
    "definition id"
);
content_id_newtype!(
    /// The identity of an [`InteractionInstance`](crate::InteractionInstance).
    /// Distinct from the instance's [`Nonce`], which routes.
    InstanceId,
    "instance id"
);
content_id_newtype!(
    /// The identity of a [`Response`](crate::Response).
    ResponseId,
    "response id"
);

/// A stable, author-assigned name for one control inside a definition.
///
/// Deliberately not content-derived: a control id must survive being written
/// by a human in a `+++` envelope, and the definition it lives in commits to
/// it — so the definition's `ContentId` is what protects its integrity.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ControlId(String);

impl ControlId {
    /// Build a control id: non-empty, ASCII alphanumeric plus `-` and `_`.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::InvalidId`] when empty or outside the charset.
    pub fn new(name: impl Into<String>) -> Result<Self, ProtocolError> {
        let name = name.into();
        if name.is_empty() {
            return Err(ProtocolError::InvalidId {
                kind: "control id",
                reason: "must not be empty".to_string(),
            });
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(ProtocolError::InvalidId {
                kind: "control id",
                reason: format!("`{name}` has characters outside [A-Za-z0-9_-]"),
            });
        }
        Ok(Self(name))
    }

    /// The name as written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A host-minted, fresh, unguessable handle for one live instance.
///
/// **The one thing here that is not content-derived, deliberately.** It makes
/// an instance unenumerable; it does not make its holder authorized. The
/// controller revalidates every response against the definition regardless of
/// who presented which nonce.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Nonce(String);

impl Nonce {
    /// Adopt a nonce minted by the host.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::InvalidId`] when empty.
    pub fn new(value: impl Into<String>) -> Result<Self, ProtocolError> {
        let value = value.into();
        if value.is_empty() {
            return Err(ProtocolError::InvalidId {
                kind: "nonce",
                reason: "must not be empty".to_string(),
            });
        }
        Ok(Self(value))
    }

    /// The handle as minted.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A definition's revision counter.
///
/// `Revision(0)` is a VALID revision, so a "consumed through" marker must be
/// `Option<Revision>` and never a `0` sentinel. That lesson is already paid
/// for in `newt_core::agentic::steering::Rev` (`steering.rs:110-115`); this
/// crate cannot depend on `newt-core` (dependency direction is binding), so
/// it carries the lesson rather than the type. #1828 records moving `Rev`
/// down here as a named follow-up.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default,
)]
#[serde(transparent)]
pub struct Revision(u64);

impl Revision {
    /// The first revision of a definition.
    pub const FIRST: Self = Self(0);

    /// Adopt a revision number.
    #[must_use]
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    /// The next revision.
    #[must_use]
    pub fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    /// The raw counter.
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }
}

/// A caller-supplied key that makes a response replay-safe: the same key on
/// the same instance is the same submission, not a second one.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct IdempotencyKey(String);

impl IdempotencyKey {
    /// Adopt an idempotency key.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::InvalidId`] when empty.
    pub fn new(value: impl Into<String>) -> Result<Self, ProtocolError> {
        let value = value.into();
        if value.is_empty() {
            return Err(ProtocolError::InvalidId {
                kind: "idempotency key",
                reason: "must not be empty".to_string(),
            });
        }
        Ok(Self(value))
    }

    /// The key as supplied.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
