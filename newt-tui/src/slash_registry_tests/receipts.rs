use super::*;

/// **The #1965 debt, counted so it can be paid down.**
///
/// Slash commands never reach the receipt path — that is how a round-cap
/// escalation to unlimited left no durable record. This is the size of
/// that hole, and like the sprawl ratchet it may only go DOWN.
///
/// **Counted across EVERY surface, unlike the shrink ratchets.** A mutator
/// does not stop owing a receipt by becoming a settings field or a section
/// action; it only stops being typed. Scoping this to `Surface::Slash`
/// would let the entire debt disappear as the cut proceeds, which is the
/// most tempting wrong answer available here.
///
/// # 24 → 18: the event journal pays six at once (#2085 PR-E2)
///
/// **The largest payment of the cut, and the first one that is not a
/// relocation.** Every reduction before this either moved a setting's
/// state somewhere `apply_and_record` could read a from→to, or found a row
/// that never owed. These six owed, and could not be paid at all, because
/// they are *operations*: a note is appended, a conversation is deleted, a
/// switch is thrown. There is no previous value to record, so no
/// `SettingValue` could ever have been minted for them — §4.4 parked them
/// against the event journal rather than handing them a fabricated
/// baseline, and #2087 landed the chain they were parked against.
///
/// Row by row, with the kind each now records and the route it records it
/// through (`event_receipt`):
///
/// - **`/remember`** → `NoteAppend`. `memory.add_note` appended and left
///   nothing durable behind it. Records the note's SIZE, never its text.
/// - **`/compress` (`/compact`)** → `Compression`. Records only a run that
///   FIRED, with the pipeline's own `how` and the token delta. The two
///   spellings are two events, which is what `via` is for.
/// - **`/undo-lock`** → `Reopen`. #1749's reversal of a decision the
///   harness adjudicated for itself. §7 Q7 named this and the kill-switch
///   as the two events whose absence made the parked set unacceptable.
/// - **`/dock`** → `Kill` on `disable`/`off`, `Grant` on `enable`/`on`.
///   Both directions, because a kill-switch journalled one way reads as
///   still shut forever. `status` records nothing; it is a read.
/// - **`/resume`** → `ConversationOp` for restore, rename and delete,
///   recorded inside `handle_conversation_command` — the one place both
///   doors converge, so a future door cannot bypass it.
/// - **`/rename` (`/name`)** → `ConversationOp`. A DIFFERENT mutator from
///   the one above, not a second door onto it: this retitles the
///   conversation you are IN, and on a conversation with no durable row it
///   creates the row titled — recorded as `title`, not `rename`, because
///   those are not the same fact.
///
/// **`/allow` did NOT flip, and the reason is the interesting one.**
/// #2085 leads with "permission grants", and the obvious row for it is
/// `/allow`. But `/allow` and `/permissions` share ONE arm in `chat.rs`
/// that prints the session's decisions and the audit tail and refuses
/// every other argument — its comment says *"Read-only by design"*, and
/// the code agrees. **The grant is not a slash command at all**: it is
/// minted in `permissions::PromptPermissionGate::record`, at the prompt,
/// which has no row in this register and already writes a FOURTH flat log
/// (`permission-log.jsonl`). Moving that onto the chain is the
/// `settings_receipt`/`denial_journal` migration #2085 records and does
/// not sequence, so it is not this slice's to make.
///
/// The two rows stay `Missing` rather than being reclassified `None_`,
/// which is the one direction a ratchet must be argued for: the plan's own
/// table (§3) routes `/allow` to `/settings permissions allow …` in PR10,
/// where it becomes a real mutator. Clearing the debt now would have to be
/// undone then, and a ratchet that moves down and back up teaches its
/// reader to edit the number. `/conversation` stays for the sibling
/// reason: its mutators redirect today, but its row is the parked pointer
/// §4.4 kept deliberately.
///
/// `EventKind::Grant` is therefore first used by the other ungating there
/// is — `/dock enable` — and not by a permission prompt.
///
/// # 25 → 24: `/posture` pays, and the ledger's exit opens (#2009 PR10c)
///
/// The last of the relocations §5.1 named. `ActivePosture` and
/// `build_posture` moved to `newt_core::posture`, so the value lives where
/// a pure `settings_form::apply` can install it and `apply_and_record` can
/// read a real from→to.
///
/// It was smaller than the ledger implied: `ActivePosture` is plain data
/// over `Caveats`, and `build_posture` already took the skill loader as a
/// CLOSURE — which is what let it move without dragging the skills path
/// with it. Recorded because the ledger's estimate was the thing that made
/// this look like a project.
///
/// # 27 → 26: `/retrieval` was never a mutator (#2009 PR11)
///
/// A truthing reclassification, and the decision doc predicted it: the row
/// was registered `Absorb`/`Missing` as though `/retrieval` set something,
/// but **its only live handler is the nav ledger** — `parse_retrieval`
/// produces a `NavCommand` that renders a view. It writes nothing, so it
/// never owed a receipt, and it is not a `/settings` field either.
///
/// Same shape as `/memory` and `/loadout` in PR3: the debt was never
/// theirs, and saying so needs the argument recorded, not just the number
/// lowered.
///
/// # 28 → 27: `/detail` pays (#2009 PR7b)
///
/// **Back below where the cut started.** The count was 27 when PR1 armed
/// it, rose to 33 as PR2 registered five ghosts that had never been
/// counted, and has been paid down since — by relocation, not by
/// reclassification, every time except PR3's two verified read-only rows.
///
/// `/detail`'s override was a `run_chat` local shared with `/spill`. It
/// lives in core now, so `apply_and_record` can read a real from→to.
///
/// # 29 → 28: `/prompt` pays (#2009 PR5)
///
/// Its state already lived in `NEWT_PROMPT`, so no relocation was needed —
/// what it lacked was a single writer. `/prompt set` open-coded its own
/// `set_var`, which is precisely the "one mutation path is aspirational
/// rather than true" the `/vi` arm's comment warns about. It routes
/// through `apply_and_record` now.
///
/// # 30 → 29: `/mode` pays the same way (#2009 PR4b)
///
/// Same shape as `/markdown` and for the same reason: absorbing it moved
/// `OperatingMode` down to core and the session value out of a `run_chat`
/// local, so `apply_and_record` can finally read a real from→to. The
/// relocation is the payment.
///
/// # 31 → 30: `/markdown` pays, rather than being reclassified (#2009 PR4)
///
/// The two before it came off by argument — they never owed. This one is
/// paid: `/markdown` mutates, still mutates, and now writes a receipt,
/// because absorbing it moved its state out of a `run_chat` local into
/// `session_markdown_mode` where `settings_form::apply_and_record` can
/// read a from→to. **That relocation IS the payment.** A field whose
/// previous value lives in a local can only be recorded as a guess.
///
/// # 33 → 31: two truthing reclassifications, verified (#2009 PR3)
///
/// `/memory` and `/loadout` were both registered `Missing` on a
/// **read-only description** — the doc flagged both as "verify; if it
/// writes nothing, reclassify `None_` with the argument recorded". Read,
/// and recorded here:
///
/// - `/memory` (`chat.rs:3279`) calls `memory.usage()` and prints the
///   compression counters. No store, no filesystem, no config write.
/// - `/loadout` (`chat.rs:5613`) renders a resolution view for
///   `""`/`show` and prints a refusal otherwise. No write on either path.
///
/// **This lowers the debt without paying anything, which is the one
/// direction a ratchet must be argued for rather than just taken.** The
/// argument is that the debt was never theirs: `Missing` means "mutates
/// and records nothing", and neither mutates. A row that cannot write
/// cannot owe a receipt.
///
/// # 28 → 33: five ghosts walk into the count (#2009 PR2)
///
/// `/new` (`/clear`), `/end`, `/restart`, `/start` and `/cd` are shipped,
/// advertised, state-mutating commands that were in NO register — so they
/// were outside this number while owing exactly what it measures. Four
/// finalize a conversation and write `end_reason`; `/cd` moves
/// `session_cwd`. None of them records a receipt.
///
/// **The debt did not grow by five; the instrument stopped under-reading
/// by five.** Per §4.4 these are operations, not settings — they have no
/// prior value for a `from→to` — so they park here, counted, against the
/// event journal (PR-E) rather than being handed a fabricated baseline.
///
/// # 27 → 28: the one declared raise (#2009 §7 Q8)
///
/// `/probe reset` wipes every learned capability — tool conformance,
/// context windows, calibration — and has never been registered. The
/// count was not 27 because the debt was 27; it was 27 because this row
/// was invisible.
///
/// Q8's recommendation, taken: *"Approve. The alternative is a mutator
/// that stays invisible because registering it would embarrass a number."*
/// Raising a ratchet is allowed exactly when the growth is the plan and
/// the item is named. It is never allowed to make a surprise go away, and
/// the itemization above is what separates the two.
#[test]
fn the_receiptless_state_mutators_are_counted_and_only_shrink() {
    let missing = COMMANDS
        .iter()
        .filter(|c| matches!(c.receipt, Receipt::Missing))
        .count();
    assert!(
        missing <= 18,
        "{missing} state-mutating commands record nothing durable — that \
         is more than when #1981 armed this. A new state mutator needs a \
         receipt destination, not another silent write"
    );
    // Anti-vacuous: the count is real, not zero-by-accident.
    assert!(
        missing > 0,
        "if this is 0 the debt is paid — lower the bound"
    );
}

/// **The other half of the ratchet above: a paid row must actually pay.**
///
/// Lowering the debt count is a claim about the CODE, and a claim a doc
/// comment cannot check. This joins the register to the dispatch **both
/// ways**: every row #2085 PR-E2 flipped to `Event` names the
/// `event_receipt` constructor its arm must call, and every `Event` row in
/// the register is named here — so neither a deleted call nor a row
/// promoted without one can pass. Deleting a call turns this red with the
/// token in the message. Six mutators inside `run_chat` are not reachable
/// by a unit test; this is the guard that is, and its constructors carry
/// the kind/vocabulary/route assertions in `event_receipt`'s own tests.
///
/// A ratchet nobody can fail is the whole failure mode #1965 exists to
/// prevent, so the number and this test move together or not at all.
#[test]
fn every_event_journalled_mutator_reaches_the_journal() {
    // The registry row → the constructor that names its event.
    const WIRED: &[(&str, &str)] = &[
        ("remember", "event_receipt::note_appended("),
        ("compress", "event_receipt::compressed("),
        ("undo-lock", "event_receipt::decision_reopened("),
        ("dock", "event_receipt::dock_switched("),
        ("resume", "event_receipt::conversation_op("),
        ("rename", "event_receipt::conversation_titled("),
    ];
    let sources = dispatch_sources();
    for (token, call) in WIRED {
        assert!(
            matches!(receipt_for(token), Receipt::Event),
            "`/{token}` is wired to the event journal but its row does not \
             say `Receipt::Event` — the column is what production reads"
        );
        assert!(
            sources.iter().any(|(_, src)| src.contains(call)),
            "`/{token}` claims `Receipt::Event` but no dispatch source \
             calls `{call}` — the row says the mutation is witnessed and \
             nothing witnesses it, which is the #1965 defect with a nicer \
             label. Either restore the call or raise the debt count back."
        );
    }
    // **The reverse join.** Without it a seventh row could be promoted to
    // `Event` — paying the ratchet — with nothing wiring it, which is the
    // exact move the ratchet exists to catch.
    for command in COMMANDS.iter().filter(|c| c.receipt == Receipt::Event) {
        assert!(
            WIRED.iter().any(|(token, _)| *token == command.name),
            "`/{}` says `Receipt::Event` but is not in WIRED — a row cannot \
             pay the #1965 debt by declaring a destination it never reaches",
            command.name
        );
    }
    // Anti-vacuous: a containment check that finds everything finds
    // nothing. It does not find a recorder that was never written.
    assert!(
        !sources
            .iter()
            .any(|(_, src)| src.contains("event_receipt::zzz_not_a_recorder(")),
        "the containment check cannot fail and proves nothing"
    );
}

/// A read-only command must not claim to mutate, and vice versa: the
/// receipt field is what `mutates()` reports, so a wrong one silently
/// removes a command from the debt count above.
#[test]
fn mutation_and_receipt_agree() {
    for command in COMMANDS {
        assert_eq!(
            command.mutates(),
            !matches!(command.receipt, Receipt::None_),
            "`/{}` disagrees with its own receipt field",
            command.name
        );
    }
    assert!(
        COMMANDS.iter().any(SlashCommand::mutates),
        "no command mutates anything — the debt count is vacuous"
    );
    assert!(
        COMMANDS.iter().any(|c| !c.mutates()),
        "every command mutates — `mutates()` is constant and proves nothing"
    );
}
