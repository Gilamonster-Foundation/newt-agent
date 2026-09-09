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
//! # The machine-checked referent
//!
//! Every table in [`op`] and every rule in [`unit`] mirrors
//! `formal/ContextOps/Basic.lean`, which CI builds. A change that breaks a
//! proven invariant fails the Lean build, not just the Rust tests. The
//! correspondence is the point: these are not two implementations that happen
//! to agree, they are one law and its executable form.
//!
//! ```text
//! Op          concise | elide | generate
//! depthAfter  generate => d+1 ;  concise, elide => d
//! Unit        { op, depth, root, life, addressed }
//! Unit.wf     depth <= 1
//! ```
//!
//! # v0 mints elision only
//!
//! [`Op::Concise`] and [`Op::Generate`] exist in the type; [`Unit::seal`]
//! refuses them. The referent is `checkOf .elide = .rederive`: elision asserts
//! nothing and is verified by re-derivation, so **every v0 assertion is a
//! deterministic recompute** — no model, no corpus, no grounding threshold, no
//! flake.
//!
//! The elision cut also dissolves the largest hole in the formal core rather
//! than deferring it. The predicate *"this segment is SOURCE, not prior packet
//! prose"* is definable nowhere on a `Unit`, which carries no content. Elision
//! addresses a byte range of the source, so the question is answered by
//! construction — and that predicate becomes the gate on ever enabling
//! generation.
//!
//! # What this crate is not
//!
//! No daemon, no socket, no async, no wire format, no storage. A layer is a
//! crate. `tests/no_service_edge.rs` asserts the absence of any HTTP or
//! inference dependency edge, so the crate stays daemon-able for free without
//! being a daemon today.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// The operation algebra: what was done, what it asserts, how it is checked.
pub mod op;
/// Packets: the chain, and identity over the whole body.
pub mod packet;
/// Units: what a derived thing is, and what it owes its source.
pub mod unit;

pub use op::{Check, Op};
pub use packet::{Packet, PacketId, UnitId};
pub use unit::{Life, Root, SealError, Unit};
