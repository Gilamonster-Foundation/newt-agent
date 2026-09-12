//! **The slash-command registry** (#1981 slice 1).
//!
//! One place that knows what commands exist. Before this, three lists did and
//! none of them agreed: `dispatch_slash`'s match (25 tokens), a long `if`
//! chain in `chat.rs` plus ten `parse_*` helpers and the navigator's verb
//! match (50 more), and `help_lines()` (61) — which had already drifted from
//! the dispatch by **eleven undocumented commands**. The full inventory and
//! how it was walked are in
//! `docs/decisions/slash_command_inventory.md`.
//!
//! This is pure data. It does not dispatch anything yet — wiring
//! `dispatch_slash` and `help_lines()` to derive from it is the follow-up
//! that makes the drift structurally impossible. What it does today is give
//! the consolidation a countable surface: `slash_registry_tests` reconciles
//! every entry against the real dispatch sources, and the ratchet arms on the
//! count so the surface can only shrink.
//!
//! # The dispositions
//!
//! The line the operator drew: **a verb that merely SETS A VALUE is absorbed
//! into `/settings`; a verb that PERFORMS something stays.** `Panel` is a
//! third case — a chooser that needs a real region to be usable, sequenced
//! behind #1979 (RegionLease) rather than shipped blind.
//!
//! # Receipts (#1965)
//!
//! `Receipt::Missing` is not a shrug, it is the audit finding: slash commands
//! never reach the receipt path, which is how a round-cap escalation to
//! unlimited left no durable record. Every state-mutating command carries it
//! until its receipt destination exists. Absorbing the knob families into one
//! `/settings` mutation path is what makes fixing them tractable — one path
//! to instrument instead of twenty.
//!
//! There are **two** destinations, and the column names which (#2085 PR-E2).
//! A SETTING has a from→to and lands as a `newt_core::settings_receipt` row
//! ([`Receipt::Journal`]); an OPERATION has no prior value and lands on the
//! chained `newt_core::event_journal` ([`Receipt::Event`]). Both writers read
//! this column, and neither reads the other's variant.

/// Which surface a command belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Family {
    Meta,
    Editor,
    Tuning,
    Model,
    Session,
    Memory,
    Navigator,
}

/// What the consolidation does with a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Disposition {
    /// Sets a value: becomes a field of the `/settings` form, and the verb
    /// becomes a shim that names its replacement.
    Absorb,
    /// Performs an action: stays a verb.
    Keep,
    /// A chooser that needs a usable region first (#1979).
    Panel,
}

/// **Where a registered thing LIVES** — the axis the ratchets count on.
///
/// The registry used to hold one kind of row: a top-level `/verb`. The radical
/// cut (#2009) turns most of those into fields and actions inside `/settings`,
/// and a register that can only describe verbs would have to DELETE a row to
/// record that — losing the pointer an operator's muscle memory still needs,
/// and losing the receipt destination the field still has.
///
/// So the register grows while the surface shrinks. That is the point, and it
/// is why the two ratchets below count `Slash` rows rather than all rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Surface {
    /// A top-level `/verb` an operator types. **The only surface the shrink
    /// ratchets count.**
    Slash,
    /// **A field of `/settings` that was never a top-level verb.**
    ///
    /// Reached as `/settings <field>` and as a row in the form — and, for
    /// `compaction`, still as `/context compaction`, the subcommand it was
    /// absorbed from. It has a value, a from→to and a receipt, but it is not a
    /// `/` command and must not be counted as one.
    ///
    /// # Why this variant existed, died, and came back
    ///
    /// PR1 removed it for having **no member**: `Disposition::Absorb` already
    /// recorded the plan, and an empty vocabulary beside it was the
    /// speculative API this repo keeps deleting. PR4 then declined to use it
    /// for `/markdown`, because `/markdown` is still a typed verb and marking
    /// it Native would drop a command an operator can type out of the surface
    /// count — the dishonesty PR1 existed to end.
    ///
    /// `compaction` is the member both were waiting for: a field whose only
    /// doors are `/settings compaction` and a subcommand. Register it `Slash`
    /// and the surface grows by a command nobody can type; leave it
    /// unregistered and the field↔row join has nothing to join to.
    Native,
    /// An action inside a `/settings` section — `/settings backends probe`.
    /// It PERFORMS rather than setting, so it has no from→to, but it is still
    /// a mutator and still owes a receipt destination.
    SectionAction,
    /// A permanent pointer to where the thing went — **carrying the
    /// destination**, so the pointer is data rather than prose someone has to
    /// keep in sync (§6 F6).
    ///
    /// **Never deleted.** §5: "No high-frequency verb ever answers 'unknown
    /// command' — retired rows are permanent pointers." A row here still
    /// occupies the register and still resolves; it just no longer occupies
    /// the surface.
    ///
    /// # A retired MUTATOR must not mutate; a retired READ may still read
    ///
    /// `/thinking` redirects and changes nothing, because a half-working
    /// mutator shim never gets to die. The nine reads folded into `/status`
    /// go on printing through the deprecation window, because printing twice
    /// harms nobody and §3.3 is explicit that reads must keep working on a
    /// pipe — `newt solve`, the eval harness and wyvern read `/version` and
    /// `/workspace` off one today. What retires now is the claim on the
    /// top-level surface and the help line, not the output.
    Retired(&'static str),
}

/// Where this command's state change is durably recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Receipt {
    /// Read-only: nothing to record.
    None_,
    /// Recorded as a content-addressed `newt_core::settings_receipt` line —
    /// a SETTING's from→to. `settings_form::change_for` reads this column to
    /// decide and `settings_receipt::record` writes: a command is receipted
    /// because the registry says where its receipt lands, not because a call
    /// site remembered to.
    Journal,
    /// Recorded on the chained `newt_core::event_journal` — an OPERATION
    /// (#2085 PR-E2). `event_receipt::record` is the writer, and it reads this
    /// column the same way `change_for` reads `Journal`.
    ///
    /// # Why this is a second variant and not the same one
    ///
    /// The two destinations are different files with different shapes, and
    /// `Surface::Retired` already sets the precedent that **a destination is
    /// data on the row, not prose someone keeps in sync** — the generated
    /// `slash_command_target_set.md` renders this column into a "where the
    /// receipt lands" cell, and one variant covering two files would make that
    /// cell name the wrong file for six rows.
    ///
    /// It also keeps the two writers from reading each other's rows. With one
    /// variant, `change_for` would mint a `SettingChange` for `/dock` and
    /// `event_receipt::record` would journal `/rounds` — each writing to a
    /// destination the row never declared, which is the failure
    /// [`receipt_for`] exists to prevent.
    ///
    /// The distinction is a today fact, not a permanent one: #2085 records
    /// that `settings_receipt` should join the chain, and when it does these
    /// two collapse back into one.
    Event,
    /// Mutates session state and records NOTHING today — #1965.
    Missing,
}

/// One registered top-level command.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SlashCommand {
    pub(crate) name: &'static str,
    /// Tokens that reach the same handler. Declared in the dispatch's own
    /// `|` groups, so they are part of the surface a ratchet must count.
    pub(crate) aliases: &'static [&'static str],
    pub(crate) family: Family,
    /// Read in PRODUCTION by `fallthrough_message`, which names where an
    /// absorbed verb's setting now lives. The exemption this carried in the
    /// previous commit is retired.
    pub(crate) disposition: Disposition,
    /// Read in PRODUCTION by [`receipt_for`], which is how
    /// `settings_form::apply_and_record` decides where a change is written.
    /// The scoped dead-code exemption this carried is retired: the column now
    /// drives behaviour, and `Receipt::Missing` is the remaining #1965 debt
    /// rather than the whole state of the world.
    pub(crate) receipt: Receipt,
    /// Which surface this row occupies — see [`Surface`]. Defaults to `Slash`
    /// via [`cmd`]; a row that has moved says so with [`cmd_on`].
    pub(crate) surface: Surface,
}

impl SlashCommand {
    /// Every token that reaches this command, canonical first.
    pub(crate) fn tokens(&self) -> impl Iterator<Item = &'static str> + '_ {
        std::iter::once(self.name).chain(self.aliases.iter().copied())
    }

    /// Whether this command changes session state.
    #[cfg(test)]
    pub(crate) fn mutates(&self) -> bool {
        !matches!(self.receipt, Receipt::None_)
    }
}

/// A row on the slash surface — the default, and still most of them.
const fn cmd(
    name: &'static str,
    aliases: &'static [&'static str],
    family: Family,
    disposition: Disposition,
    receipt: Receipt,
) -> SlashCommand {
    cmd_on(name, aliases, family, disposition, receipt, Surface::Slash)
}

/// A row on a named surface — a settings field, a section action, or a
/// retirement pointer.
const fn cmd_on(
    name: &'static str,
    aliases: &'static [&'static str],
    family: Family,
    disposition: Disposition,
    receipt: Receipt,
    surface: Surface,
) -> SlashCommand {
    SlashCommand {
        name,
        aliases,
        family,
        disposition,
        receipt,
        surface,
    }
}

/// Every top-level slash command, walked from the dispatch (#1981).
pub(crate) const COMMANDS: &[SlashCommand] = &[
    cmd(
        "edit-mode",
        &["vi", "emacs", "nano"],
        Family::Editor,
        Disposition::Absorb,
        Receipt::Journal,
    ),
    cmd_on(
        "memory",
        &[],
        Family::Memory,
        Disposition::Keep,
        Receipt::None_,
        Surface::Retired("/status memory"),
    ),
    cmd_on(
        "recall",
        &[],
        Family::Memory,
        Disposition::Keep,
        Receipt::None_,
        Surface::Retired("/resume find"),
    ),
    cmd(
        // #2085 PR-E2: a note append is one of the six event classes §4.4
        // parked. It has no from→to — a note is added, not set — which is why
        // it could never be a `/settings` field and needed the event journal.
        "remember",
        &[],
        Family::Memory,
        Disposition::Keep,
        Receipt::Event,
    ),
    cmd(
        "search",
        &[],
        Family::Memory,
        Disposition::Keep,
        Receipt::None_,
    ),
    cmd_on(
        "byline",
        &[],
        Family::Meta,
        Disposition::Keep,
        Receipt::None_,
        Surface::Retired("/status byline"),
    ),
    // #1981: the typed settings form the knob verbs are absorbed into. A
    // Keep: it PERFORMS (it asks and it writes), it does not merely hold a
    // value. Receipt::Missing because `apply` is the chokepoint where one
    // will land, and does not yet.
    cmd(
        "settings",
        &[],
        Family::Meta,
        Disposition::Keep,
        Receipt::Journal,
    ),
    cmd_on(
        "config",
        &[],
        Family::Meta,
        Disposition::Keep,
        Receipt::None_,
        Surface::Retired("/status config"),
    ),
    cmd_on(
        "docs",
        &[],
        Family::Meta,
        Disposition::Keep,
        Receipt::None_,
        Surface::Retired("/help docs"),
    ),
    cmd(
        "exit",
        &["quit"],
        Family::Meta,
        Disposition::Keep,
        Receipt::None_,
    ),
    cmd("help", &[], Family::Meta, Disposition::Keep, Receipt::None_),
    cmd_on(
        "info",
        &[],
        Family::Meta,
        Disposition::Keep,
        Receipt::None_,
        Surface::Retired("/status"),
    ),
    cmd(
        "setup",
        &[],
        Family::Meta,
        Disposition::Keep,
        Receipt::Missing,
    ),
    cmd(
        "status",
        &[],
        Family::Meta,
        Disposition::Keep,
        Receipt::None_,
    ),
    cmd_on(
        "version",
        &[],
        Family::Meta,
        Disposition::Keep,
        Receipt::None_,
        Surface::Retired("/status version"),
    ),
    cmd_on(
        "workspace",
        &[],
        Family::Meta,
        Disposition::Keep,
        Receipt::None_,
        Surface::Retired("/status workspace"),
    ),
    cmd(
        "backends",
        &["backend"],
        Family::Model,
        Disposition::Panel,
        Receipt::Missing,
    ),
    cmd("dgx", &[], Family::Model, Disposition::Keep, Receipt::None_),
    cmd(
        "model",
        &[],
        Family::Model,
        Disposition::Absorb,
        Receipt::Missing,
    ),
    // NOT retired. #2009 PR3 retired this row to `/status models`, but that
    // destination is `status_topics`' rewrite BACK to `/models`, which is the
    // live implementation and always has been (`lib.rs`'s dispatch match names
    // `"models"` explicitly, so the row never reached `fallthrough_message`).
    // The retirement therefore pointed a command at itself, and — because the
    // help corpus was pruned to match — took the only listing verb out of
    // `/help` and the palette. An operator could switch models but not see
    // them. Reads print (`status_topics`' own rule); this one is not retired.
    cmd(
        "models",
        &[],
        Family::Model,
        Disposition::Keep,
        Receipt::None_,
    ),
    cmd(
        "probe",
        &[],
        Family::Model,
        Disposition::Keep,
        Receipt::None_,
    ),
    // ── THE DECLARED TRUTHING RAISE (#2009 §7 Q8) ────────────────────────
    //
    // `/probe reset` wipes every learned capability: tool conformance,
    // context windows, calibration. It has always been a mutator and has
    // never been registered as one, so the receiptless-mutator count has been
    // understating itself by exactly this row.
    //
    // Registering it RAISES that count, which is why the doc made it an
    // operator question rather than a silent edit. Q8's recommendation, taken
    // here: "Approve. The alternative is a mutator that stays invisible
    // because registering it would embarrass a number."
    //
    // It is a `SectionAction` rather than a `Slash` row because `/probe`
    // itself already occupies the surface; this is the destructive verb
    // INSIDE it, and PR9 re-homes both to `/settings backends probe`. Being
    // off the slash surface is also why the raise costs the shrink ratchet
    // nothing — the register grows, the surface does not.
    cmd_on(
        "probe reset",
        &[],
        Family::Model,
        Disposition::Keep,
        Receipt::Missing,
        Surface::SectionAction,
    ),
    cmd(
        "summarizer",
        &[],
        Family::Model,
        Disposition::Absorb,
        Receipt::Missing,
    ),
    cmd(
        // #2009 PR11: thirteen navigator verbs plus `/retrieval` retire into
        // this one. It is the same parser — `parse_nav_command` strips the
        // `nav` and matches the verb it always matched — so the retired names
        // and their replacements cannot drift.
        "nav",
        &[],
        Family::Navigator,
        Disposition::Keep,
        Receipt::None_,
    ),
    cmd_on(
        "callees",
        &[],
        Family::Navigator,
        Disposition::Keep,
        Receipt::None_,
        Surface::Retired("/nav callees"),
    ),
    cmd_on(
        "callers",
        &[],
        Family::Navigator,
        Disposition::Keep,
        Receipt::None_,
        Surface::Retired("/nav callers"),
    ),
    cmd_on(
        "compare",
        &[],
        Family::Navigator,
        Disposition::Keep,
        Receipt::None_,
        Surface::Retired("/nav compare"),
    ),
    cmd_on(
        "def",
        &["goto"],
        Family::Navigator,
        Disposition::Keep,
        Receipt::None_,
        Surface::Retired("/nav def"),
    ),
    cmd_on(
        "export",
        &[],
        Family::Navigator,
        Disposition::Keep,
        Receipt::None_,
        Surface::Retired("/nav export"),
    ),
    cmd_on(
        "hierarchy",
        &[],
        Family::Navigator,
        Disposition::Keep,
        Receipt::None_,
        Surface::Retired("/nav hierarchy"),
    ),
    cmd_on(
        "impact",
        &[],
        Family::Navigator,
        Disposition::Keep,
        Receipt::None_,
        Surface::Retired("/nav impact"),
    ),
    cmd_on(
        "implementations",
        &["impls"],
        Family::Navigator,
        Disposition::Keep,
        Receipt::None_,
        Surface::Retired("/nav implementations"),
    ),
    cmd_on(
        "map",
        &[],
        Family::Navigator,
        Disposition::Keep,
        Receipt::None_,
        Surface::Retired("/nav map"),
    ),
    cmd_on(
        "tests",
        &[],
        Family::Navigator,
        Disposition::Keep,
        Receipt::None_,
        Surface::Retired("/nav tests"),
    ),
    cmd_on(
        "text",
        &["grep"],
        Family::Navigator,
        Disposition::Keep,
        Receipt::None_,
        Surface::Retired("/nav text"),
    ),
    cmd_on(
        "type",
        &["inspect"],
        Family::Navigator,
        Disposition::Keep,
        Receipt::None_,
        Surface::Retired("/nav type"),
    ),
    cmd_on(
        "uses",
        &["refs"],
        Family::Navigator,
        Disposition::Keep,
        Receipt::None_,
        Surface::Retired("/nav uses"),
    ),
    cmd(
        "allow",
        &[],
        Family::Session,
        Disposition::Keep,
        Receipt::Missing,
    ),
    cmd(
        // #2085 PR-E2. Both spellings journal, and as two different events:
        // `via` is the verb typed, so `/compress` and `/compact` are one
        // effect reached two ways and the record says which.
        "compress",
        &["compact"],
        Family::Session,
        Disposition::Keep,
        Receipt::Event,
    ),
    cmd(
        "context",
        &[],
        Family::Session,
        Disposition::Keep,
        Receipt::Missing,
    ),
    cmd_on(
        // Absorbed from `/context compaction` (#2009 PR7). Never a top-level
        // verb, so it is a field row rather than a slash row — see
        // `Surface::Native`.
        "compaction",
        &[],
        Family::Session,
        Disposition::Absorb,
        Receipt::Journal,
        Surface::Native,
    ),
    cmd_on(
        // Retired into `/resume` (#2009 PR6b). Its READS still read and
        // its MUTATORS redirect, so the row stays a permanent pointer
        // while the receipt debt it owes stays counted: the ops are
        // parked for the event journal (§4.4), not reclassified.
        "conversation",
        &[],
        Family::Session,
        Disposition::Keep,
        Receipt::Missing,
        Surface::Retired("/resume"),
    ),
    cmd(
        "crew",
        &[],
        Family::Session,
        Disposition::Keep,
        Receipt::Missing,
    ),
    cmd(
        // #2085 PR-E2 — **the security kill-switch**, and §7 Q7's reason for
        // landing the journal before the window closes. `disable` records a
        // `Kill`, `enable` a `Grant`: a switch journalled in one direction
        // reads as still shut forever.
        "dock",
        &[],
        Family::Session,
        Disposition::Keep,
        Receipt::Event,
    ),
    cmd(
        "mcp",
        &[],
        Family::Session,
        Disposition::Panel,
        Receipt::Missing,
    ),
    cmd(
        "permissions",
        &[],
        Family::Session,
        Disposition::Panel,
        Receipt::Missing,
    ),
    cmd(
        // #2085 PR-E2: retitling the ACTIVE conversation — a different mutator
        // from `/resume rename`, which retitles one the operator NAMES. Both
        // are `ConversationOp`; the two rows are two mutators, not two doors.
        "rename",
        &["name"],
        Family::Session,
        Disposition::Keep,
        Receipt::Event,
    ),
    cmd(
        // #2085 PR-E2: the conversation ops — restore, rename, delete — all
        // reach `handle_conversation_command`, which is where they journal.
        "resume",
        &[],
        Family::Session,
        Disposition::Keep,
        Receipt::Event,
    ),
    cmd(
        "roadmap",
        &[],
        Family::Session,
        Disposition::Keep,
        Receipt::None_,
    ),
    cmd(
        "spill",
        &[],
        Family::Session,
        Disposition::Keep,
        Receipt::None_,
    ),
    cmd(
        "tab",
        &[],
        Family::Session,
        Disposition::Keep,
        Receipt::Missing,
    ),
    cmd(
        "transcript",
        &[],
        Family::Session,
        Disposition::Keep,
        Receipt::None_,
    ),
    cmd_on(
        "tree",
        &[],
        Family::Session,
        Disposition::Keep,
        Receipt::None_,
        Surface::Retired("/roadmap tree"),
    ),
    cmd(
        // #2085 PR-E2: reopening a decision the harness adjudicated on its own
        // (#1749). §7 Q7's other named reason for the journal — a reversal
        // nobody witnessed is the one an audit most needs to see.
        "undo-lock",
        &[],
        Family::Session,
        Disposition::Keep,
        Receipt::Event,
    ),
    // ------------------------------------------------------------------
    // **The ghosts** (#2009 PR2). Five shipped, advertised, state-mutating
    // commands that were in no register at all — outside the shrink ratchets,
    // outside the receipt debt count, and invisible to every conformance test
    // in this file. Registering them RAISES three ratchets, and that raise is
    // the whole point: the numbers were low because they were not looking.
    //
    // Rows follow the code, not the help. `chat.rs:4506-4516` is one match on
    // the verb, and it is the authority for what is an alias and what is not.
    // ------------------------------------------------------------------
    cmd(
        // `/clear` and `/new` share ONE arm returning `Some("new")` — proven
        // identical, so a genuine alias.
        "new",
        &["clear"],
        Family::Session,
        Disposition::Keep,
        Receipt::Missing,
    ),
    cmd(
        // **NOT an alias of `/new`, though the help said so for two years.**
        // `end_reason` is a persisted column (`store.rs:2017`), and these
        // write different values into it. Two rows, because the difference
        // outlives the session that made it.
        "end",
        &[],
        Family::Session,
        Disposition::Keep,
        Receipt::Missing,
    ),
    cmd(
        "restart",
        &[],
        Family::Session,
        Disposition::Keep,
        Receipt::Missing,
    ),
    cmd(
        // The one that is obviously distinct: `/start` SWITCHES without
        // finalizing — it skips close-time note extraction, leaves the
        // outgoing conversation OPEN and resumable, and takes a title.
        "start",
        &[],
        Family::Session,
        Disposition::Keep,
        Receipt::Missing,
    ),
    cmd(
        // The one human navigation command (#1096). Moves `session_cwd`,
        // confined below the start dir.
        "cd",
        &[],
        Family::Session,
        Disposition::Keep,
        Receipt::Missing,
    ),
    // **`/cognition` redirects; `/psyche` performs.** They were one row with
    // an alias, which pointed the surviving verb at the retired one. The dial
    // panel is reached by `/psyche`, and `commands::settings` answers
    // `/cognition` with a redirect that mutates nothing.
    cmd_on(
        "cognition",
        &[],
        Family::Tuning,
        Disposition::Absorb,
        Receipt::Journal,
        Surface::Retired("/settings cognition"),
    ),
    cmd(
        "psyche",
        &[],
        Family::Tuning,
        Disposition::Keep,
        Receipt::Journal,
    ),
    cmd(
        // Absorbed as `/settings detail` (#2009 PR7b) and still a
        // typed verb: `/detail` toggles, the field sets a count.
        // Journal, because the write goes through `apply_and_record`
        // like every other field — which is what the relocation of
        // the override out of `run_chat` bought.
        "detail",
        &[],
        Family::Tuning,
        Disposition::Absorb,
        Receipt::Journal,
    ),
    cmd_on(
        "loadout",
        &[],
        Family::Tuning,
        Disposition::Keep,
        Receipt::None_,
        Surface::Retired("/status loadout"),
    ),
    cmd(
        // Absorbed as a `/settings` field in #2009 PR4 — and still a typed
        // verb, exactly like `/edit-mode`: absorbing moves the STATE, the
        // window close (PR14a) moves the row. Journal, because the field
        // writes a receipt through `apply_and_record` like every other.
        "markdown",
        &[],
        Family::Tuning,
        Disposition::Absorb,
        Receipt::Journal,
    ),
    cmd(
        // Absorbed as a `/settings` field in #2009 PR4b, and still a
        // typed verb until the window closes — same state as
        // `/edit-mode` and `/markdown`.
        "mode",
        &[],
        Family::Tuning,
        Disposition::Absorb,
        Receipt::Journal,
    ),
    cmd(
        "nudge",
        &[],
        Family::Tuning,
        Disposition::Absorb,
        Receipt::Journal,
    ),
    cmd(
        "persona",
        &[],
        Family::Tuning,
        Disposition::Absorb,
        Receipt::Missing,
    ),
    cmd_on(
        "plan",
        &[],
        Family::Tuning,
        Disposition::Keep,
        Receipt::None_,
        Surface::Retired("/roadmap"),
    ),
    cmd(
        // #2009 PR10c: absorbed as `/settings posture` once the
        // posture moved to core — the relocation is what lets
        // `apply_and_record` read a real from→to.
        "posture",
        &[],
        Family::Tuning,
        Disposition::Absorb,
        Receipt::Journal,
    ),
    cmd(
        // Absorbed as the form's first `Text` field in #2009 PR5, and
        // still a typed verb until the window closes.
        "prompt",
        &[],
        Family::Tuning,
        Disposition::Absorb,
        Receipt::Journal,
    ),
    cmd_on(
        "retrieval",
        &[],
        Family::Tuning,
        Disposition::Keep,
        Receipt::None_,
        Surface::Retired("/nav retrieval"),
    ),
    cmd(
        "rounds",
        &["tool-rounds", "max-rounds"],
        Family::Tuning,
        Disposition::Absorb,
        Receipt::Journal,
    ),
    cmd_on(
        "tenacity",
        &[],
        Family::Tuning,
        Disposition::Absorb,
        Receipt::Journal,
        Surface::Retired("/settings tenacity"),
    ),
    cmd_on(
        "thinking",
        &[],
        Family::Tuning,
        Disposition::Absorb,
        Receipt::Journal,
        Surface::Retired("/settings thinking"),
    ),
];

/// The rows an operator can type at the top level — what the shrink
/// ratchets count.
///
/// `#[cfg(test)]` for the same reason as `all_tokens` below: the register is
/// a conformance instrument today, and only `lookup` is on a runtime path.
/// This loses the gate when the completion source stops offering retired rows.
#[cfg(test)]
pub(crate) fn slash_commands() -> impl Iterator<Item = &'static SlashCommand> {
    COMMANDS.iter().filter(|c| c.surface == Surface::Slash)
}

/// Every token that reaches a `Surface::Slash` row.
#[cfg(test)]
pub(crate) fn slash_tokens() -> Vec<&'static str> {
    let mut tokens: Vec<&'static str> = slash_commands().flat_map(SlashCommand::tokens).collect();
    tokens.sort_unstable();
    tokens.dedup();
    tokens
}

/// Every token that reaches any registered command.
///
/// `#[cfg(test)]` to match its only caller — the ratchet. When `/settings`
/// and the completion source read the registry, this loses the gate.
#[cfg(test)]
pub(crate) fn all_tokens() -> Vec<&'static str> {
    COMMANDS.iter().flat_map(SlashCommand::tokens).collect()
}

/// The command `token` reaches, if any.
pub(crate) fn lookup(token: &str) -> Option<&'static SlashCommand> {
    COMMANDS
        .iter()
        .find(|c| c.tokens().any(|t| t.eq_ignore_ascii_case(token)))
}

/// Where `token`'s state change is durably recorded.
///
/// **The production reader of the receipt column.** An unregistered token has
/// no declared destination, which is the same answer as a registered one whose
/// destination does not exist yet: do not write. That is deliberate — a
/// receipt written to a destination nobody declared is worse than no receipt,
/// because it looks like coverage.
pub(crate) fn receipt_for(token: &str) -> Receipt {
    lookup(token).map_or(Receipt::Missing, |c| c.receipt)
}

/// What to tell an operator whose command fell through `dispatch_slash`.
///
/// `dispatch_slash` is the LAST resort: fifty of the seventy-eight tokens are
/// claimed earlier by the `chat.rs` interception chain, so reaching the
/// fallthrough with a *registered* token does not mean "unknown" — it means
/// the earlier handler declined it, which is a different thing and usually a
/// routing bug. Saying "unknown command" there sends the operator to `/help`
/// to look for something that is right there in the list.
///
/// This is also where absorbed commands will speak once `/settings` carries
/// them: a removed verb must name its replacement, never fall through to
/// "unknown" (#1981), because operators have muscle memory.
pub(crate) fn fallthrough_message(token: &str) -> String {
    match lookup(token) {
        // **A retired row is authoritative about where it went**, whatever its
        // disposition — the destination is data on the row, so this cannot
        // drift from the table the way a per-arm string would.
        Some(SlashCommand {
            surface: Surface::Retired(dest),
            ..
        }) => format!("/{token} is retired — use {dest}"),
        // An absorbed setting names its new home, not its old handler.
        Some(command) if command.disposition == Disposition::Absorb => format!(
            "/{token} sets a value that now lives in /settings {}",
            command.name
        ),
        // An action that lives inside a section says which door it is
        // behind. It is not a routing bug: nothing ever routed it at the top
        // level, and calling it one sends the operator to file an issue
        // instead of to the place the action actually is.
        Some(command) if command.surface == Surface::SectionAction => {
            format!("/{token} is an action inside /settings, not a top-level command")
        }
        Some(command) => format!(
            "/{token} is a known command ({:?} family) but nothing handled it \
             here — this is a routing bug, not a typo. Please report it.",
            command.family
        ),
        None => format!("unknown command: /{token}  (try /help)"),
    }
}

/// Drop every `#[cfg(test)]` item.
///
/// Not `crate::production_source`, which splits on
/// `"\n#[cfg(test)]\nmod tests {"` — a marker `lib.rs` and `chat.rs` no
/// longer contain, because #1949 extracted their test bodies to
/// `lib_tests/`. It would panic here rather than truncate, which is the
/// right failure but not a usable one.
///
/// The subtlety that cost a re-walk: `#[cfg(test)]` does NOT always
/// introduce a brace block. In `lib.rs` it precedes `use …;` and
/// `#[path = "…"] mod x;` declarations. A skipper that assumed a block
/// scanned to the next `{` anywhere and ate ~760 lines of production.
#[cfg(test)]
fn production(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim() == "#[cfg(test)]" {
            let mut j = i + 1;
            while j < lines.len() && !lines[j].contains('{') && !lines[j].trim_end().ends_with(';')
            {
                j += 1;
            }
            if j < lines.len() && lines[j].contains('{') {
                let mut depth: i64 = 0;
                while j < lines.len() {
                    depth +=
                        lines[j].matches('{').count() as i64 - lines[j].matches('}').count() as i64;
                    if depth <= 0 {
                        break;
                    }
                    j += 1;
                }
            }
            i = j + 1;
            continue;
        }
        out.push(lines[i]);
        i += 1;
    }
    out.join("\n")
}

/// Interception sites in `src`, **not** mentions of one.
///
/// The count skips comment lines. Writing the doc comment that explains
/// why `/cd` joined this shape moved the pin by two without adding a
/// single interception — a guard that a comment can trip teaches its
/// reader to edit the number instead of reading the code, which is the
/// one failure mode a ratchet cannot survive.
#[cfg(test)]
fn count_sites(src: &str) -> usize {
    src.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .map(|line| line.matches("trim_start_matches('/')").count())
        .sum()
}

#[cfg(test)]
fn dispatch_sources() -> Vec<(&'static str, String)> {
    vec![
        ("lib.rs", production(include_str!("lib.rs"))),
        ("chat.rs", production(include_str!("chat.rs"))),
        (
            "navigator_cmds.rs",
            production(include_str!("navigator_cmds.rs")),
        ),
        // The `/roadmap` family moved out of `lib.rs`; its interception
        // site moved with it, so the inventory follows the code. Adding
        // the file keeps the pinned total whole — a REPOINT, not a
        // lowering.
        (
            "roadmap_cmds.rs",
            production(include_str!("roadmap_cmds.rs")),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    /// **The production cut reads something.** Every count below is over these
    /// strings; a cut that returned "" would make all of them pass having read
    /// nothing, which is exactly how the first version of this scanner looked
    /// clean while eating 760 lines.
    #[test]
    fn the_production_cut_is_not_vacuous() {
        for (name, src) in dispatch_sources() {
            assert!(
                src.len() > 5_000,
                "{name}: the production cut read {} bytes",
                src.len()
            );
            assert!(
                count_sites(src.as_str()) > 0,
                "{name}: no slash interception survived the cut — counted over \
                 CODE, so a surviving comment cannot stand in for one"
            );
        }
        // ...and it really does remove test code.
        let lib = production(include_str!("lib.rs"));
        assert!(
            lib.len() < include_str!("lib.rs").len(),
            "the cut removed nothing; it is not cutting at all"
        );
    }

    /// **The session must not write the round cap behind the recorder** (#1998).
    ///
    /// `/rounds` is the one journalled setting whose verb still performs real
    /// work before the write — it doubles, it resolves `unlimited`, it releases
    /// — so the write is a separate step a future edit could quietly inline.
    /// The other four fields are protected by `settings_form::apply` being
    /// private; this one needs a count, because the setter it would call lives
    /// in another crate and no visibility rule can reach it.
    ///
    /// ONE call is expected: `run_chat`'s reset-to-default at session start,
    /// which is not an operator decision and correctly records nothing. A
    /// second one means someone put the mutation back in the dispatch, and the
    /// escalation stops leaving a receipt again.
    #[test]
    fn the_session_writes_the_round_cap_only_through_the_recorder() {
        let chat = production(include_str!("chat.rs"));
        let direct = chat.matches("set_session_tool_rounds(").count();
        assert_eq!(
            direct, 1,
            "chat.rs writes the /rounds override directly {direct} times — exactly \
             one is expected (the session-start reset). An operator's change goes \
             through settings_form::apply_and_record or it leaves no receipt"
        );
        // Anti-vacuous: the recorded route really is the one in use, so the
        // count above is not 1 because the feature was removed.
        assert!(
            chat.contains("settings_form::Field::Rounds"),
            "no production caller applies the round cap through the form"
        );
    }
}

/// **The decision doc's table, rendered from this registry** (#1981
/// deliverable 2).
///
/// `docs/decisions/slash_command_target_set.md` records one row per command:
/// absorb / keep / delete, and where every surviving state-mutator's receipt
/// lands. It is GENERATED, never hand-written — a second hand-maintained list
/// of sixty-five commands is precisely the drift this slice exists to kill,
/// and it would be stale within a PR.
///
/// Rows are sorted by family then name so that reordering `COMMANDS` does not
/// churn the document.
#[cfg(test)]
mod target_set_doc {
    use super::*;

    const DOC: &str = include_str!("../../docs/decisions/slash_command_target_set.md");
    const BEGIN: &str = "<!-- BEGIN GENERATED: slash_registry::COMMANDS -->";
    const END: &str = "<!-- END GENERATED -->";

    fn disposition_cell(command: &SlashCommand) -> String {
        match command.disposition {
            Disposition::Absorb => format!("absorb → `/settings {}`", command.name),
            Disposition::Keep => "keep — it performs".to_string(),
            Disposition::Panel => "panel — a chooser, needs a region (#1979)".to_string(),
        }
    }

    fn receipt_cell(command: &SlashCommand) -> &'static str {
        match command.receipt {
            Receipt::None_ => "— read-only",
            Receipt::Journal => "`~/.newt/receipts.jsonl`",
            Receipt::Event => "`~/.newt/events.jsonl` (chained)",
            Receipt::Missing => "**none — #1965**",
        }
    }

    /// How a row is REACHED, so the doc cannot be read as "everything here is
    /// typed with a slash". That was true when the register and the surface
    /// were the same set; #2009 exists to make them differ.
    fn surface_cell(command: &SlashCommand) -> String {
        match command.surface {
            Surface::Slash => "`/` command".to_string(),
            Surface::Native => "field of `/settings`".to_string(),
            Surface::SectionAction => "action inside a section".to_string(),
            Surface::Retired(dest) => format!("retired → `{dest}`"),
        }
    }

    fn table() -> String {
        let mut rows: Vec<&SlashCommand> = COMMANDS.iter().collect();
        rows.sort_by_key(|c| (format!("{:?}", c.family), c.name));
        let mut out = String::from(
            "| command | also typed as | reached by | family | disposition | receipt |\n",
        );
        out.push_str("|---|---|---|---|---|---|\n");
        for command in &rows {
            let aliases = if command.aliases.is_empty() {
                "—".to_string()
            } else {
                command
                    .aliases
                    .iter()
                    .map(|a| format!("`/{a}`"))
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            // A `Native` row is NOT typeable as `/name`, so it is not
            // rendered as though it were — the table is read by people, and a
            // leading slash is a promise that the token works.
            let shown = match command.surface {
                Surface::Native => format!("`/settings {}`", command.name),
                _ => format!("`/{}`", command.name),
            };
            out.push_str(&format!(
                "| {shown} | {aliases} | {} | {:?} | {} | {} |\n",
                surface_cell(command),
                command.family,
                disposition_cell(command),
                receipt_cell(command),
            ));
        }
        let count = |d: Disposition| COMMANDS.iter().filter(|c| c.disposition == d).count();
        let receipts = |r: Receipt| COMMANDS.iter().filter(|c| c.receipt == r).count();
        out.push_str(&format!(
            "\n**{} registered, {} of them typed as `/` commands ({} tokens).** \
             Absorb {} · keep {} · panel {}. \
             Receipts: settings {} · events {} · read-only {} · **missing {}**.\n",
            COMMANDS.len(),
            slash_commands().count(),
            slash_tokens().len(),
            count(Disposition::Absorb),
            count(Disposition::Keep),
            count(Disposition::Panel),
            receipts(Receipt::Journal),
            receipts(Receipt::Event),
            receipts(Receipt::None_),
            receipts(Receipt::Missing),
        ));
        out
    }

    fn generated_block(doc: &str) -> &str {
        let start = doc.find(BEGIN).expect("the doc has no generated block") + BEGIN.len();
        let end = doc.find(END).expect("the generated block is not closed");
        doc[start..end].trim_matches('\n')
    }

    /// **The doc and the registry cannot disagree.**
    ///
    /// Run with `UPDATE_DOCS=1` to regenerate after changing `COMMANDS`. The
    /// default path reads the doc through `include_str!`, so the check itself
    /// touches no filesystem.
    #[test]
    fn the_target_set_doc_is_generated_from_this_registry() {
        let want = table();
        if std::env::var_os("UPDATE_DOCS").is_some() {
            let path = concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../docs/decisions/slash_command_target_set.md"
            );
            let current = std::fs::read_to_string(path).expect("the doc exists");
            let start = current.find(BEGIN).expect("marker") + BEGIN.len();
            let end = current.find(END).expect("marker");
            let updated = format!("{}\n{want}\n{}", &current[..start], &current[end..]);
            std::fs::write(path, updated).expect("writable");
            return;
        }
        assert_eq!(
            generated_block(DOC),
            want.trim_end(),
            "docs/decisions/slash_command_target_set.md is stale — regenerate \
             it with `UPDATE_DOCS=1 cargo test -p newt-tui \
             the_target_set_doc_is_generated_from_this_registry`"
        );
    }

    /// **Anti-vacuous twin.** Comparing an empty block to an empty table would
    /// pass forever. The generated block is real, and it says the things the
    /// decision doc exists to say.
    #[test]
    fn the_generated_block_is_not_empty() {
        let block = generated_block(DOC);
        assert!(block.len() > 2_000, "{} bytes is not 65 rows", block.len());
        for needle in [
            "`/settings edit-mode`",
            "keep — it performs",
            "**none — #1965**",
            "`~/.newt/receipts.jsonl`",
            // #2085 PR-E2: the second destination is real in the doc too, so
            // this guard cannot pass over a table that lost it.
            "`~/.newt/events.jsonl` (chained)",
        ] {
            assert!(block.contains(needle), "the doc never says {needle}");
        }
    }
}

#[cfg(test)]
#[path = "slash_registry_tests/mod.rs"]
mod slash_registry_tests;
