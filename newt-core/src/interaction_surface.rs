//! **One semantic interaction, carried across a surface seam** (C1 of epic
//! #1803, #1862).
//!
//! A session asks a terminal-owning surface to present an interaction and
//! report what the operator did. This module is the *semantic* half of that
//! exchange: what is being asked, and how the asker expects it to behave.
//! The channel that carries it is the caller's business and deliberately not
//! described here.
//!
//! # Why this is not `SurfaceRequest`
//!
//! `newt_tui::session_worker::SurfaceRequest` already carries session→UI
//! traffic, and the epic lists promoting it to the durable protocol as an
//! explicit NON-GOAL. The code shows why: `SurfaceRequest::ReadLine` carries
//! a `SyncSender<anyhow::Result<ReadOutcome>>` and `TurnStarted` carries two
//! `Arc<AtomicBool>`. Those are correct for a thread-shaped adapter and
//! unshippable as a protocol — neither can cross a process, so a semantic
//! type embedding them could never reach C4's multi-view broker.
//!
//! So the two stay layered: `SurfaceRequest` remains the thread-shaped
//! envelope, and it CARRIES a [`SurfaceInteraction`], which is the part that
//! means something.
//!
//! # The constraint is unrepresentable, not merely linted
//!
//! [`SurfaceInteraction`] derives `Serialize`/`Deserialize`, and that is the
//! guard. `std::sync::mpsc::SyncSender` has no `Serialize` impl, and neither
//! does `AtomicBool` — so adding either to this type is a **compile error**
//! rather than something review has to notice. "It can cross a process" stops
//! being a claim and becomes a property the compiler checks.
//!
//! The two `compile_fail` doctests on [`SurfaceInteraction`] prove the
//! negative, and `c1::the_semantic_type_survives_a_round_trip` proves the
//! positive. The doctests live on the PUBLIC item on purpose: rustdoc
//! collects doctests only from public items, so the same blocks written
//! inside the `#[cfg(test)]` module ran zero times.
//!
//! # What the seam carries that a formatted string could not
//!
//! Today the session hands the UI thread an already-rendered prompt string
//! (`SurfaceRequest::ReadLine { prompt: String }`) — the semantic-loss point
//! the A0 inventory named (§7.5). A [`SurfaceInteraction`] carries the
//! [`InteractionDefinition`] instead, so the surface that renders it can
//! choose HOW: plain lines, a Ratatui modal, or an HTML form. Rendering it is
//! `newt_core::markup::plain::render`'s job (C0a/C0b) and there is exactly
//! one such renderer.

use newt_interaction::InteractionDefinition;
use serde::{Deserialize, Serialize};

/// Whether the asker parks on a reply.
///
/// **Lifecycle data, not a view choice.** Today this is implicit in which
/// `SurfaceRequest` variant a caller picks (`expects_reply()` reads it off
/// the variant), which means the *transport* encodes it. Naming it here moves
/// it into the model, where a broker with several attached views can honour
/// it without knowing what channel the request arrived on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Disposition {
    /// The asker is parked until this resolves. It cannot make progress.
    Blocking,
    /// The asker continues; a late answer is still welcome.
    NonBlocking,
}

/// Whether presenting this should interrupt what the operator is doing.
///
/// **This is where `modal` went.** The ADR says `modal` is a view decision,
/// not a model kind, and A2 kept it out of [`InteractionKind`] accordingly.
/// But the *reason* a view would choose a modal is semantic — the asker needs
/// the operator now — and that belongs in the model. So the model says
/// "attention requested" and the view decides whether that means a modal, a
/// highlighted row, or a bell.
///
/// [`InteractionKind`]: newt_interaction::InteractionKind
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Attention {
    /// Interrupt: the operator is being asked to act now.
    Requested,
    /// Present it where it fits; nothing is waiting on the operator's eyes.
    Passive,
}

/// One interaction, offered to whichever surface owns the terminal.
///
/// Carries no channel, no `Arc`, and no handle — see the module docs. The
/// reply travels back as a [`HumanQuestionOutcome`], the typed outcome this
/// tree already uses for "what did the human actually do", rather than a
/// second parallel outcome enum.
///
/// # The thread-shaped types cannot be expressed here
///
/// This is the guard for the epic's non-goal, and it is a COMPILE error
/// rather than a lint. `SyncSender` has no `Serialize` impl, so a reply
/// channel cannot be added to this type:
///
/// ```compile_fail
/// # use serde::Serialize;
/// #[derive(Serialize)]
/// struct WithAChannel {
///     definition: newt_interaction::InteractionDefinition,
///     reply: std::sync::mpsc::SyncSender<()>,
/// }
/// ```
///
/// Nor can `TurnStarted`'s cancellation flags, for the same reason —
/// `AtomicBool` has no `Serialize` impl either:
///
/// ```compile_fail
/// # use serde::Serialize;
/// #[derive(Serialize)]
/// struct WithAFlag {
///     cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
/// }
/// ```
///
/// Both of these live on `newt_tui::session_worker::SurfaceRequest` today and
/// are correct there. They are what a thread-shaped adapter is FOR; they are
/// simply not what a semantic type may contain.
///
/// [`HumanQuestionOutcome`]: crate::HumanQuestionOutcome
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SurfaceInteraction {
    /// What is being asked. The surface renders THIS, not a string the asker
    /// pre-rendered for it.
    pub definition: InteractionDefinition,
    /// Whether the asker is parked on the answer.
    pub disposition: Disposition,
    /// Whether answering should interrupt the operator.
    pub attention: Attention,
}

impl SurfaceInteraction {
    /// The common case: the asker is parked and needs the operator now.
    #[must_use]
    pub fn blocking(definition: InteractionDefinition) -> Self {
        Self {
            definition,
            disposition: Disposition::Blocking,
            attention: Attention::Requested,
        }
    }

    /// Whether the asker is parked on this.
    #[must_use]
    pub fn is_blocking(&self) -> bool {
        self.disposition == Disposition::Blocking
    }

    /// Whether presenting this should interrupt the operator.
    #[must_use]
    pub fn wants_attention(&self) -> bool {
        self.attention == Attention::Requested
    }
}

#[cfg(test)]
mod c1 {
    use super::*;
    use newt_interaction::{Control, ControlId, ControlKind, InteractionKind, Requirement};

    fn definition() -> InteractionDefinition {
        InteractionDefinition::new(
            InteractionKind::Prompt,
            "? which file",
            vec![Control {
                id: ControlId::new("answer").expect("valid control id"),
                kind: ControlKind::Text,
                label: String::new(),
                requirement: Requirement::Required,
            }],
        )
    }

    /// **The semantic type can cross a process.** Which is the whole reason
    /// it exists separately from `SurfaceRequest`: at C4 a broker publishes
    /// one interaction to several attached views, and a type holding a
    /// `SyncSender` could never be published anywhere.
    #[test]
    fn the_semantic_type_survives_a_round_trip() {
        let interaction = SurfaceInteraction::blocking(definition());
        let json = serde_json::to_string(&interaction).expect("serializes");
        let back: SurfaceInteraction = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(back, interaction);
        assert!(back.is_blocking());
        assert!(back.wants_attention());
    }

    /// **The derive that IS the guard is present.**
    ///
    /// The negative — that a `SyncSender` or an `Arc<AtomicBool>` field fails
    /// to compile — is proved by the two `compile_fail` doctests on
    /// [`SurfaceInteraction`] itself.
    ///
    /// They live on the PUBLIC item deliberately: rustdoc collects doctests
    /// only from public items, so the same two blocks written inside this
    /// `#[cfg(test)]` module were never run at all
    /// (`cargo test -p newt-core --doc interaction_surface` reported
    /// `running 0 tests`). That is the shape constraint 9 names — a guard that
    /// would report success whether or not the thing it measures existed —
    /// and it is recorded here because the first cut of this file had it.
    ///
    /// This test covers the other half: deleting the derive would remove the
    /// guard while every behavioural test kept passing.
    #[test]
    fn the_thread_shaped_types_cannot_be_expressed_here() {
        // If the derive were removed, this would not compile — which is the
        // guard. Asserting it here means the guard has a named owner.
        fn assert_serializable<T: Serialize + for<'de> Deserialize<'de>>() {}
        assert_serializable::<SurfaceInteraction>();
        assert_serializable::<Disposition>();
        assert_serializable::<Attention>();
    }

    /// Disposition and attention are INDEPENDENT. A blocking ask that does
    /// not want the operator's eyes (a background confirmation the session
    /// parks on) and a non-blocking ask that does (a notice worth surfacing)
    /// are both expressible, or the pair would be one flag wearing two names.
    #[test]
    fn disposition_and_attention_are_independent() {
        let quiet_block = SurfaceInteraction {
            definition: definition(),
            disposition: Disposition::Blocking,
            attention: Attention::Passive,
        };
        assert!(quiet_block.is_blocking());
        assert!(!quiet_block.wants_attention());

        let loud_nonblock = SurfaceInteraction {
            definition: definition(),
            disposition: Disposition::NonBlocking,
            attention: Attention::Requested,
        };
        assert!(!loud_nonblock.is_blocking());
        assert!(loud_nonblock.wants_attention());
    }

    /// The wire names are kebab-case and pinned, so a rename is a visible
    /// change rather than a silent one when C4 puts this on a wire.
    #[test]
    fn the_wire_vocabulary_is_pinned() {
        assert_eq!(
            serde_json::to_string(&Disposition::NonBlocking).unwrap(),
            "\"non-blocking\""
        );
        assert_eq!(
            serde_json::to_string(&Disposition::Blocking).unwrap(),
            "\"blocking\""
        );
        assert_eq!(
            serde_json::to_string(&Attention::Requested).unwrap(),
            "\"requested\""
        );
        assert_eq!(
            serde_json::to_string(&Attention::Passive).unwrap(),
            "\"passive\""
        );
    }
}
