//! **agent-frame** — the derivation kernel.
//!
//! Not "context management with a hash on it". This is a content-addressed
//! *derivation* system whose principal output happens to be a model's working
//! frame. Nothing in the core mentions context windows; it is about **what a
//! derived thing may assert, what it owes its source, and how far it may travel
//! from that source before it stops being accountable.**
//!
//! It is the third question in a stack: `agent-bridle` asks what am I allowed
//! to do, `agent-mesh` asks who am I talking to, **agent-frame asks what do I
//! know and how do I know it**, and `agent-store` holds durable ordering.
//!
//! # A unit's address is self-validating
//!
//! The single property this crate exists to provide: **given a unit's id and
//! the source bytes, a consumer who did not run the build can establish which
//! material the unit represents and how it was derived.**
//!
//! That is a claim about the *hash*, not about a comment. A [`Unit`]'s id is
//! the id of its [`Derivation`] — `{ op, source, span, elided, root }` — so two
//! elisions over different sources, or over different ranges of one source,
//! cannot collide. [`verify::verify_unit`] recomputes all three claims from the
//! bytes and compares.
//!
//! An earlier revision failed this. It hashed `{ op, depth, root_kind, life,
//! addressed }`, which names no material: every elision with the same root kind
//! minted the *same* id, and `addressed: true` was a declaration rather than an
//! address. See `docs/decisions/agent-frame-v0-monorepo.md`.
//!
//! # The admission boundary is closed
//!
//! [`Unit`] does not implement [`serde::Deserialize`]. Foreign bytes decode to
//! [`RawUnit`] and cross into a `Unit` only through the fallible
//! [`TryFrom`] impl, which enforces the v0 contract. Deriving `Deserialize`
//! made the constructor one path of two, and the decoder was the unchecked one.
//!
//! [`Packet`] is closed the same way and for the same reason. It is a
//! [`MerkleNode`], which carries a parent *set*; v0 mints a **chain**, whose
//! Lean referent has no multi-parent constructor. Decoding therefore lands in
//! [`RawPacket`] and admission refuses two or more parents — the state that
//! satisfied `is_genesis() == false` and `prior() == None` at once.
//!
//! [`MerkleNode`]: content_addressable::MerkleNode
//!
//! # The machine-checked referent
//!
//! Every table in [`op`] and every rule in [`unit`] mirrors
//! `formal/ContextOps/Basic.lean`, which CI builds.
//!
//! ```text
//! Op          concise | elide | generate
//! depthAfter  generate => d+1 ;  concise, elide => d
//! Unit.wf     depth <= 1
//! checkOf     .elide = .rederive
//! ```
//!
//! # v0 mints elision only
//!
//! [`Op::Concise`] and [`Op::Generate`] exist in the type; [`Unit::seal`] and
//! the admission boundary both refuse them. The referent is `checkOf .elide =
//! .rederive`: elision asserts nothing and is verified by re-derivation, so
//! **every v0 assertion is a deterministic recompute** — and
//! [`verify::verify_unit`] is that recompute, not a word for one.
//!
//! # What this crate is not
//!
//! No daemon, no socket, no async, no wire format, no storage. A layer is a
//! crate. `tests/no_service_edge.rs` asserts the absence of any HTTP or
//! inference dependency edge, so the crate stays daemon-able for free without
//! being a daemon today.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// Derivations: the addressed fact a unit's identity is computed over.
pub mod derivation;
/// Causal events: observations, interventions, and checked derivation ancestry.
pub mod event;
/// The operation algebra: what was done, what it asserts, how it is checked.
pub mod op;
/// Packets: the chain, and identity over the whole body.
pub mod packet;
/// Roots: which event caused a derivation.
pub mod root;
/// Units: what a derived thing is, and what it owes its source.
pub mod unit;
/// The one verification decision, and its resolver seam.
pub mod verify;

pub use derivation::{Derivation, Span};
pub use event::{Event, EventBody, EventError, EventKind, EventOrigin, RawEvent, ReplyVerdict};
pub use op::{Check, Op};
pub use packet::{Packet, PacketAdmitError, PacketBody, PacketId, RawPacket, UnitId};
pub use root::{RootEvent, RootKind};
pub use unit::{AdmitError, Life, RawUnit, SealError, Unit};
pub use verify::{verify_unit, verify_unit_with, Mismatch, SourceResolver, Verified, VerifyError};
