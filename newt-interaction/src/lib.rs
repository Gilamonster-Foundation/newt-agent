//! **Newt Markup interaction protocol — the renderer-neutral model (A2, #1828).**
//!
//! ADR: `docs/decisions/newt_markup_interaction_architecture.md`. This crate
//! is the model half of that MVC variation: an immutable, semantic
//! [`InteractionDefinition`], a host-minted, out-of-band
//! [`InteractionInstance`], and a typed [`Response`]. It defines the records
//! and the STATES; the controller that drives transitions is A3's, and no
//! renderer lives here or ever will.
//!
//! **Dependency direction is binding.** This is the inward layer: it takes no
//! Ratatui, crossterm, Axum, HTMX, ammonia, browser, mobile, filesystem, or
//! application dependency, and no `newt-*` crate. `newt-core` depends on this
//! crate; never the reverse. `tests/guard.rs` arms that as a test rather than
//! a comment — one half over the resolved dependency closure, one over this
//! crate's own source — and each half carries an anti-vacuous twin, because a
//! guard that cannot fail is decoration.
//!
//! **Identity is derived, never assigned.** Every record here is a canonical
//! structured value, so its identity is a `ContentId` minted through
//! `ContentAddressable` + `canonical::to_canonical_dagcbor` — the codec makes
//! determinism a property of the encoder, not of caller discipline. Nothing
//! in this crate hand-rolls a digest or a canonical encoding. The single
//! deliberate exception is the instance NONCE, which is fresh and
//! unguessable rather than content-derived: it is a locator that travels
//! beside the address, and possession of it authorizes nothing. **IDs route;
//! they are not credentials.**
//!
//! This file is a manifest: composition only, no logic.

pub mod definition;
pub mod downgrade;
pub mod error;
pub mod ids;
pub mod instance;
pub mod response;
pub mod tag;

pub use definition::{
    ChoiceOption, Control, ControlKind, FeatureDemand, InteractionDefinition, InteractionKind,
    Requirement, SemanticRole, SurfaceFeature, DEFINITION_SCHEMA_V1,
};
pub use downgrade::{
    decode_definition, decode_instance, decode_response, plan_presentation, Decoded, Degradation,
    Presentation, RawRecord, UnknownReason,
};
pub use error::ProtocolError;
pub use ids::{
    ControlId, DefinitionId, IdempotencyKey, InstanceId, Nonce, OptionId, ResponseId, Revision,
};
pub use instance::{
    Audience, InteractionInstance, LifecycleState, Provenance, ResponderPolicy, Scope,
    INSTANCE_SCHEMA_V1,
};
pub use response::{
    AssertionKind, ControlValue, ResponderProvenance, Response, SecretRef, Submission,
    RESPONSE_SCHEMA_V1,
};
pub use tag::{DefinitionTag, InstanceTag, ResponseTag};
