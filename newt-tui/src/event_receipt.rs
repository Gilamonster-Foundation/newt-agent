//! **Where a state mutator's journal event is named** (#2085 PR-E2, #2009 §4.4).
//!
//! `newt_core::event_journal` is the chain; this is the layer that decides
//! *which* event a mutator is, and refuses to write one the registry has not
//! declared a destination for. It is the non-setting twin of
//! [`settings_form::change_for`](crate::settings_form) — same discipline, same
//! reason: **a command is receipted because `slash_registry` says where its
//! receipt lands, not because a call site remembered to.**
//!
//! # Why each mutator gets a named constructor rather than a raw `record` call
//!
//! Three things about an event are decisions, not parameters, and a decision
//! spread across six call sites inside `run_chat` is a decision nobody can
//! test:
//!
//! - **the kind.** `/dock disable` is a [`EventKind::Kill`] and `/dock enable`
//!   is a [`EventKind::Grant`]; that mapping lives here, where a test can ask
//!   for it.
//! - **the vocabulary.** `subject` and `detail` are the event's own words — a
//!   conversation id, a note destination, a pipeline's `how` — **never the
//!   operator's**. The journal must not become a second transcript, which is
//!   the rule `settings_receipt` holds itself to and states in the same words.
//!   The constructors take a byte COUNT rather than the note, and an id rather
//!   than the title, so the raw line an operator typed cannot reach the journal
//!   even by accident.
//! - **the route.** `via` is the verb actually typed. `/compress` and
//!   `/compact` are one effect and **two events**; so are `/rename` and
//!   `/name`, and `/resume delete` and `/resume rm`. Normalising them would
//!   throw away the only half of the record a reader cannot reconstruct
//!   afterwards.
//!
//! Each constructor hands back the [`JournalEvent`] it recorded, so a test
//! asserts on the object that was written rather than on a second one minted
//! from the same arguments and hoped to match. Recording is best effort by
//! construction (`event_journal::record_event` swallows its own failures):
//! failing to observe a mutation must never undo the mutation.

use crate::slash_registry::{receipt_for, Receipt};
use newt_core::event_journal::{EventKind, JournalEvent};

/// Journal one mutator event, when the registry declares a destination for it.
///
/// **The production reader of the receipt column for non-setting mutators.**
/// `None` means the registry says this token has nowhere to write — the same
/// answer an unregistered token gets, and deliberately so: a receipt sent to a
/// destination nobody declared looks like coverage without being any.
///
/// It reads [`Receipt::Event`] and not `Receipt::Journal`, so a SETTING's row
/// cannot be journalled as an operation by a call site that reached for the
/// wrong recorder — the same refusal, in the other direction, as
/// `settings_form::change_for`.
fn record(
    kind: EventKind,
    token: &str,
    subject: &str,
    detail: &str,
    via: &str,
) -> Option<JournalEvent> {
    if !matches!(receipt_for(token), Receipt::Event) {
        return None;
    }
    let event = JournalEvent::new(kind, subject, detail, via);
    let _ = newt_core::event_journal::record_event(event.clone());
    Some(event)
}

/// `/remember <fact>` — a fact appended to the note store.
///
/// **`bytes`, not the fact.** The note's text is the operator's, and the whole
/// point of the vocabulary rule is that it does not get copied into a second
/// place; the size is the part a reader cannot reconstruct from "a note was
/// appended" alone. Taking a count rather than a `&str` is what makes the
/// leak unrepresentable instead of merely discouraged.
pub(crate) fn note_appended(bytes: usize) -> Option<JournalEvent> {
    record(
        EventKind::NoteAppend,
        "remember",
        "notes",
        &format!("{bytes} bytes"),
        "/remember",
    )
}

/// `/compress` · `/compact` — one manual compression that FIRED.
///
/// `verb` is the token actually typed, so the two spellings journal as two
/// events. `how` is [`ManualCompressOutcome::how`](newt_core::ManualCompressOutcome),
/// the pipeline's own description of what it did — already a `&'static str`
/// vocabulary, so the journal and the operator notice cannot drift.
pub(crate) fn compressed(
    conversation_id: &str,
    verb: &str,
    how: &str,
    tokens_before: usize,
    tokens_after: usize,
) -> Option<JournalEvent> {
    record(
        EventKind::Compression,
        "compress",
        conversation_id,
        &format!("{how}: {tokens_before} -> {tokens_after} tokens"),
        &format!("/{verb}"),
    )
}

/// `/undo-lock <n>` — an assumption the harness adjudicated on its own,
/// reopened for the operator (#1749).
///
/// The ordinal is the assumption's number in the `Assuming:` block, which is
/// how the operator names it and the only handle the reopened decision has.
pub(crate) fn decision_reopened(conversation_id: &str, ordinal: usize) -> Option<JournalEvent> {
    record(
        EventKind::Reopen,
        "undo-lock",
        conversation_id,
        &format!("assumption {ordinal}"),
        "/undo-lock",
    )
}

/// `/dock disable|off|enable|on` — the remote-docking kill-switch.
///
/// **Both directions, and they are different kinds.** `disable` forcibly
/// undocks every hub, which is the [`EventKind::Kill`] the security switch
/// exists for; `enable` hands approved hubs the ability to dock back, which is
/// a [`EventKind::Grant`]. Journalling only the kill would leave an audit that
/// says the door was shut and never that it was reopened — the one-sided
/// record is worse than none, because it reads as still shut.
///
/// `None` for `status` and for an unknown subcommand: neither writes anything.
pub(crate) fn dock_switched(sub: &str) -> Option<JournalEvent> {
    let kind = match sub {
        "disable" | "off" => EventKind::Kill,
        "enable" | "on" => EventKind::Grant,
        _ => return None,
    };
    record(
        kind,
        "dock",
        "remote-htmx-docking",
        if kind == EventKind::Kill {
            "kill-switch on"
        } else {
            "kill-switch off"
        },
        &format!("/dock {sub}"),
    )
}

/// `/resume restore|rename|delete|rm <id>` — a conversation op that performed.
///
/// `op` is the canonical operation and `input` is the line it was reached
/// through, so `/resume rm` and `/resume delete` journal the same `detail` and
/// two different routes. Only the `/resume` door gets here: the retired
/// `/conversation` door redirects its mutators without mutating
/// (`conversation_op_plan`), so there is nothing for it to record.
pub(crate) fn conversation_op(input: &str, op: &str, id: &str) -> Option<JournalEvent> {
    record(
        EventKind::ConversationOp,
        "resume",
        id,
        op,
        &route_of(input),
    )
}

/// `/rename <title>` · `/name <title>` — retitling the ACTIVE conversation.
///
/// A different mutator from [`conversation_op`], not a second door onto it:
/// that one renames a conversation an operator NAMES, this one renames the one
/// they are in, and `/rename` on a conversation with no durable row yet
/// *creates* the row titled. `created` distinguishes those two, because "titled
/// a new record" and "renamed an existing one" are not the same fact.
///
/// The title itself never travels — the id does.
pub(crate) fn conversation_titled(verb: &str, id: &str, created: bool) -> Option<JournalEvent> {
    record(
        EventKind::ConversationOp,
        "rename",
        id,
        if created { "title" } else { "rename" },
        &format!("/{verb}"),
    )
}

/// The route a conversation-op line was typed through: the door and the verb,
/// and nothing after them — the rest is an id and, for `rename`, the
/// operator's own words.
fn route_of(input: &str) -> String {
    let body = input.trim().trim_start_matches('/').trim();
    let mut words = body.split_whitespace();
    match (words.next(), words.next()) {
        (Some(door), Some(verb)) => format!("/{door} {verb}"),
        (Some(door), None) => format!("/{door}"),
        _ => "/".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Every test here holds this.** The constructors WRITE, and with no
    /// override `journal_path()` resolves to the developer's real
    /// `~/.newt/events.jsonl` — where a test's noise would not merely land in
    /// the file but advance its head ref and chain itself into a real audit
    /// trail. The guard blanks `JOURNAL_PATH_ENV`, so these run fs-free.
    fn guard() -> newt_core::test_guard::GlobalSettingsGuard {
        newt_core::test_guard::GlobalSettingsGuard::acquire()
    }

    #[test]
    fn a_note_append_records_its_size_and_never_the_note() {
        let _g = guard();
        let event = note_appended(42).expect("/remember is an Event row");
        assert_eq!(event.kind, EventKind::NoteAppend);
        assert_eq!(event.subject, "notes");
        assert_eq!(event.detail, "42 bytes");
        assert_eq!(event.via, "/remember");
    }

    /// The two spellings are ONE effect and TWO events — the route is the half
    /// a reader cannot reconstruct from the journal afterwards.
    #[test]
    fn compress_and_compact_are_two_routes_to_one_kind() {
        let _g = guard();
        let compress = compressed("conv-1", "compress", "prune + summary", 900, 400)
            .expect("/compress is an Event row");
        let compact = compressed("conv-1", "compact", "prune + summary", 900, 400)
            .expect("/compress is an Event row");
        assert_eq!(compress.kind, EventKind::Compression);
        assert_eq!(compact.kind, EventKind::Compression);
        assert_eq!(compress.subject, "conv-1");
        assert_eq!(compress.detail, "prune + summary: 900 -> 400 tokens");
        assert_eq!(compress.via, "/compress");
        assert_eq!(compact.via, "/compact");
        assert_ne!(compress.via, compact.via);
    }

    #[test]
    fn a_reopened_decision_records_the_assumption_it_reopened() {
        let _g = guard();
        let event = decision_reopened("conv-7", 2).expect("/undo-lock is an Event row");
        assert_eq!(event.kind, EventKind::Reopen);
        assert_eq!(event.subject, "conv-7");
        assert_eq!(event.detail, "assumption 2");
        assert_eq!(event.via, "/undo-lock");
    }

    /// **The kill and the ungating are different kinds.** An audit that
    /// recorded only the `disable` would read as "still shut" forever.
    #[test]
    fn the_dock_kill_switch_records_both_directions_as_different_kinds() {
        let _g = guard();
        let off = dock_switched("disable").expect("/dock is an Event row");
        let on = dock_switched("enable").expect("/dock is an Event row");
        assert_eq!(off.kind, EventKind::Kill);
        assert_eq!(off.detail, "kill-switch on");
        assert_eq!(off.via, "/dock disable");
        assert_eq!(on.kind, EventKind::Grant);
        assert_eq!(on.detail, "kill-switch off");
        assert_eq!(on.via, "/dock enable");
        assert_eq!(off.subject, on.subject);
    }

    /// The aliases are their own routes, not normalised onto the canonical
    /// spelling.
    #[test]
    fn the_dock_aliases_keep_their_own_routes() {
        let _g = guard();
        assert_eq!(dock_switched("off").expect("alias").via, "/dock off");
        assert_eq!(dock_switched("on").expect("alias").via, "/dock on");
    }

    /// A read writes nothing — and neither does a subcommand that does not
    /// exist, which would otherwise journal a mutation that never happened.
    #[test]
    fn a_dock_read_or_typo_records_nothing() {
        let _g = guard();
        assert!(dock_switched("status").is_none());
        assert!(dock_switched("").is_none());
        assert!(dock_switched("disabl").is_none());
    }

    #[test]
    fn a_conversation_op_records_the_id_the_op_and_the_typed_route() {
        let _g = guard();
        let deleted =
            conversation_op("/resume delete abc123", "delete", "abc123").expect("Event row");
        assert_eq!(deleted.kind, EventKind::ConversationOp);
        assert_eq!(deleted.subject, "abc123");
        assert_eq!(deleted.detail, "delete");
        assert_eq!(deleted.via, "/resume delete");
    }

    /// `rm` is `delete`'s alias in the parser and a DIFFERENT route here.
    #[test]
    fn the_delete_alias_is_its_own_route() {
        let _g = guard();
        let rm = conversation_op("/resume rm abc123", "delete", "abc123").expect("Event row");
        assert_eq!(rm.detail, "delete", "the op is canonical");
        assert_eq!(rm.via, "/resume rm", "the route is what was typed");
    }

    /// The route stops at the verb. `/resume rename <id> <title>` carries the
    /// operator's own title, and it must not ride into the journal on `via`.
    #[test]
    fn the_route_never_carries_the_operator_s_words() {
        let _g = guard();
        let renamed = conversation_op("/resume rename abc123 the secret plan", "rename", "abc123")
            .expect("Event row");
        assert_eq!(renamed.via, "/resume rename");
        assert!(
            !renamed.via.contains("secret") && !renamed.detail.contains("secret"),
            "the typed title reached the journal: {renamed:?}"
        );
    }

    #[test]
    fn retitling_the_active_conversation_distinguishes_titling_from_renaming() {
        let _g = guard();
        let renamed = conversation_titled("rename", "conv-9", false).expect("Event row");
        let titled = conversation_titled("name", "conv-9", true).expect("Event row");
        assert_eq!(renamed.kind, EventKind::ConversationOp);
        assert_eq!(renamed.detail, "rename");
        assert_eq!(renamed.via, "/rename");
        assert_eq!(titled.detail, "title");
        assert_eq!(titled.via, "/name");
    }

    // --- the gate ---

    /// **The registry decides, not the call site.** A row that declares no
    /// destination writes nothing, which is the whole reason the receipt column
    /// is production data rather than documentation.
    #[test]
    fn a_row_with_no_declared_destination_records_nothing() {
        let _g = guard();
        assert!(
            matches!(receipt_for("crew"), Receipt::Missing),
            "this test needs a row that is still Missing to be meaningful"
        );
        assert!(record(EventKind::Kill, "crew", "s", "d", "/crew").is_none());
        assert!(matches!(receipt_for("status"), Receipt::None_));
        assert!(record(EventKind::Kill, "status", "s", "d", "/status").is_none());
        assert!(record(EventKind::Kill, "zzznotacommand", "s", "d", "/x").is_none());
        // A SETTING is journalled, and NOT here: `/rounds` records a
        // `settings_receipt` row, so reaching for this recorder must refuse it
        // rather than write the same change into a second file.
        assert!(matches!(receipt_for("rounds"), Receipt::Journal));
        assert!(record(EventKind::Grant, "rounds", "s", "d", "/rounds").is_none());
    }

    /// Anti-vacuous twin: the gate is not simply refusing everything.
    #[test]
    fn an_event_row_does_record() {
        let _g = guard();
        assert!(matches!(receipt_for("remember"), Receipt::Event));
        assert!(record(EventKind::NoteAppend, "remember", "notes", "0 bytes", "/x").is_some());
    }

    #[test]
    fn the_route_of_a_bare_verb_is_the_verb() {
        assert_eq!(route_of("/resume"), "/resume");
        assert_eq!(route_of("  /resume   delete   x  "), "/resume delete");
    }
}
