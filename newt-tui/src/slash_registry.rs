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
    /// Recorded as a content-addressed `newt_core::settings_receipt` line.
    /// `settings_form::apply_and_record` is the writer, and it reads this
    /// column to decide — a command is receipted because the registry says
    /// where its receipt lands, not because a call site remembered to.
    Journal,
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
        "remember",
        &[],
        Family::Memory,
        Disposition::Keep,
        Receipt::Missing,
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
    cmd_on(
        "models",
        &[],
        Family::Model,
        Disposition::Keep,
        Receipt::None_,
        Surface::Retired("/status models"),
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
        "compress",
        &["compact"],
        Family::Session,
        Disposition::Keep,
        Receipt::Missing,
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
        "dock",
        &[],
        Family::Session,
        Disposition::Keep,
        Receipt::Missing,
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
        "rename",
        &["name"],
        Family::Session,
        Disposition::Keep,
        Receipt::Missing,
    ),
    cmd(
        "resume",
        &[],
        Family::Session,
        Disposition::Keep,
        Receipt::Missing,
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
    cmd(
        "tree",
        &[],
        Family::Session,
        Disposition::Keep,
        Receipt::None_,
    ),
    cmd(
        "undo-lock",
        &[],
        Family::Session,
        Disposition::Keep,
        Receipt::Missing,
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
    cmd(
        "plan",
        &[],
        Family::Tuning,
        Disposition::Absorb,
        Receipt::Missing,
    ),
    cmd(
        "posture",
        &[],
        Family::Tuning,
        Disposition::Absorb,
        Receipt::Missing,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

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
    fn production(text: &str) -> String {
        let lines: Vec<&str> = text.split('\n').collect();
        let mut out = Vec::new();
        let mut i = 0;
        while i < lines.len() {
            if lines[i].trim() == "#[cfg(test)]" {
                let mut j = i + 1;
                while j < lines.len()
                    && !lines[j].contains('{')
                    && !lines[j].trim_end().ends_with(';')
                {
                    j += 1;
                }
                if j < lines.len() && lines[j].contains('{') {
                    let mut depth: i64 = 0;
                    while j < lines.len() {
                        depth += lines[j].matches('{').count() as i64
                            - lines[j].matches('}').count() as i64;
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
    fn count_sites(src: &str) -> usize {
        src.lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .map(|line| line.matches("trim_start_matches('/')").count())
            .sum()
    }

    fn dispatch_sources() -> Vec<(&'static str, String)> {
        vec![
            ("lib.rs", production(include_str!("lib.rs"))),
            ("chat.rs", production(include_str!("chat.rs"))),
            (
                "navigator_cmds.rs",
                production(include_str!("navigator_cmds.rs")),
            ),
        ]
    }

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

    /// **THE RATCHET (#1981).** The slash surface may only shrink.
    ///
    /// Two numbers, because they answer different questions: `COMMANDS` is
    /// what the consolidation must reduce, `all_tokens` is what an operator
    /// can actually type — aliases included, since a removed alias is a
    /// removed affordance.
    ///
    /// Walked from the dispatch, not from `help_lines()`: the help had
    /// already drifted by eleven undocumented commands when this was armed.
    ///
    /// # 57/72 → 44/54: the navigator folds into `/nav` (#2009 PR11)
    ///
    /// **Thirteen commands and five aliases, in one slice** — the largest
    /// reduction of the cut, and the cheapest, because they were already one
    /// parser with one verb match. `parse_nav_command` strips the `nav` and
    /// matches the verb it always matched, so the retired names and their
    /// replacements are the same line by the time anything decides what to do.
    ///
    /// Every verb keeps its OWN help line, spelled `/nav <verb>` — the doc is
    /// explicit about that, and it is why the fold is a subcommand rather than
    /// a single opaque entry point. Thirteen discoverable rows became thirteen
    /// discoverable rows under one name; what left is thirteen top-level
    /// claims on the operator's memory.
    ///
    /// # 58/73 → 57/72: `/conversation` folds into `/resume` (#2009 PR6b)
    ///
    /// One conversation surface: list, show, restore, rename and delete are
    /// `/resume` subcommands now, sharing the retired verb's parser and
    /// handler so the two doors cannot drift.
    ///
    /// **The row keeps `Receipt::Missing`.** Retiring the verb does not pay
    /// its debt — a delete still records nothing durable, and §4.4 parks the
    /// conversation operations for the event journal rather than minting
    /// `SettingValue` variants for them. A retirement that quietly cleared the
    /// count would be the most tempting wrong answer available here.
    ///
    /// # 59/74 → 58/73: `/recall` folds into `/resume find` (#2009 PR6)
    ///
    /// `/resume <token>` already ran the same FTS5 search; what `/recall` had
    /// that it lacked was *searching without reopening*, since a token that
    /// resolves as an id reopens that conversation. That is now `find`, a
    /// subcommand of the verb the operator already reaches for — so the
    /// capability survives while the top-level name does not.
    ///
    /// # 68/83 → 59/74: the `/status` fold (#2009 PR3)
    ///
    /// Nine reads stop being top-level verbs and become topics of one:
    /// `/info` `/config` `/version` `/workspace` `/byline` `/memory`
    /// `/loadout` `/models` retire into `/status <topic>`, and `/docs` into
    /// `/help docs`. **The first real reduction of the cut**, and it pays back
    /// PR2's raise with one to spare.
    ///
    /// The verbs still work — see `Surface::Retired`: a retired READ may
    /// still read, because §3.3 requires reads to keep working on a pipe.
    /// What retires is the claim on the surface, which is what these two
    /// numbers measure.
    ///
    /// # 63/76 → 68/83: registering the ghosts (#2009 PR2)
    ///
    /// Five commands an operator could type today, advertised in
    /// `help_lines()`, reaching real handlers, and counted by neither of these
    /// numbers: `/new` (`/clear`), `/end`, `/restart`, `/start`, `/cd` — plus
    /// `inspect`, a proven alias of `/type` (`navigator_cmds.rs:105` matches
    /// both in one arm).
    ///
    /// **A shrink ratchet that does not know about a command cannot stop it
    /// growing.** This raise buys the ratchet its teeth: the surface it now
    /// guards is the surface that exists. Every later slice pays it back —
    /// PR3 alone retires nine rows.
    ///
    /// **Slice 1 raised these by one, and that is honest rather than a
    /// weakening.** `/settings` is a net ADDITION: it absorbs the editor-mode
    /// family, but `/vi`, `/emacs`, `/nano` and `/edit-mode` remain reachable
    /// as shims, because a removed command that answers "unknown" is worse
    /// than the four verbs were. The reduction lands when the deprecation
    /// window closes and the shims are retired — four tokens and one command
    /// come off then, and this bound comes down with them. Raising a ratchet
    /// is allowed exactly when the growth is the plan; it is not allowed to
    /// make a surprise go away.
    /// **Counted on `Surface::Slash`, not on the register (#2009 PR1).**
    ///
    /// The register GROWS as the cut proceeds — fields, section actions and
    /// permanent retirement pointers all keep their rows, because a deleted
    /// row loses both the pointer an operator's muscle memory needs and the
    /// receipt destination the setting still has. Counting every row would
    /// therefore turn the plan into a ratchet violation.
    ///
    /// What may only shrink is what an operator can TYPE at the top level.
    #[test]
    fn the_registered_surface_only_shrinks() {
        assert!(
            slash_commands().count() <= 44,
            "the slash surface GREW to {} commands. #1981 is a reduction: a \
             new command needs an argument for why it is not a field of \
             /settings or a subcommand of an existing verb",
            slash_commands().count()
        );
        assert!(
            slash_tokens().len() <= 54,
            "the slash surface GREW to {} tokens",
            slash_tokens().len()
        );
    }

    /// **The register may grow; only the surface may not.**
    ///
    /// Anti-vacuous guard on the ratchet above: if `slash_commands()` ever
    /// returned everything, the two numbers would coincide and the ratchet
    /// would silently become the old one again — which is exactly the shape
    /// the cut needs it not to be.
    #[test]
    fn the_register_is_allowed_to_be_larger_than_the_surface() {
        assert!(
            COMMANDS.len() >= slash_commands().count(),
            "the surface cannot exceed the register"
        );
        assert!(
            all_tokens().len() >= slash_tokens().len(),
            "typed tokens cannot exceed registered ones"
        );
    }

    /// **The exact guard: a new interception SITE forces a registry review.**
    ///
    /// Containment (below) catches a command that disappears. Nothing catches
    /// one that APPEARS, because a new `if slash_body == "whatever"` is
    /// invisible to a scan that only knows the tokens it was told about. The
    /// site count is the proxy that is exact: every top-level command reaches
    /// its handler through one of these, so a new one means either a new
    /// command or a refactor, and both deserve a look at this file.
    /// # PR6 DID free one: 22 → 21
    ///
    /// The `/recall` arm is gone, not redirected. `parse_resume_command` reads
    /// `/recall` as the `/resume find` it retired into, so the retired verb
    /// runs the replacement's code instead of a second copy — which is what
    /// makes the site removable rather than merely renamed. This is the first
    /// real consolidation of the cut, and §5's rule is satisfied: a recount
    /// says so, rather than a forecast.
    ///
    /// # PR3 did NOT free a site, and says so
    ///
    /// The train predicted the `/status` fold would kill the `/info` site. It
    /// did not: a retired READ keeps reading, so the `/status || /info` arm is
    /// still there and still needed. §5's site-count honesty rule is explicit
    /// that a shared binding survives until its LAST command dies, and that a
    /// slice may only lower this number when a real recount says so — so it
    /// stays 22, and the site dies with the shims in PR14b.
    #[test]
    fn the_number_of_slash_interception_sites_is_pinned() {
        let counted: usize = dispatch_sources()
            .iter()
            .map(|(_, src)| count_sites(src))
            .sum();
        assert_eq!(
            counted, 21,
            "the number of slash interception sites moved to {counted}. If a \
             command was added, register it here. If sites were consolidated \
             — which is #1981's goal — lower this number and the ratchet above."
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

    /// Every registered token is still present in the dispatch.
    ///
    /// Deliberately a weak check, and named as one: containment cannot tell a
    /// dispatch arm from an unrelated string, so it catches REMOVAL and not
    /// much else. The site count above is the exact half.
    #[test]
    fn every_registered_token_still_appears_in_the_dispatch() {
        let sources = dispatch_sources();
        // `Surface::Slash` only. A `SectionAction` is reached through its
        // parent verb's ARGUMENT (`/probe reset`), so it has no dispatch token
        // of its own and never will — asking for one would force every future
        // section action to be registered as a fake top-level command, which
        // is the fiction #2009 PR1 exists to end.
        for command in slash_commands() {
            for token in command.tokens() {
                let needle = format!("\"{token}\"");
                assert!(
                    sources.iter().any(|(_, src)| src.contains(&needle)),
                    "`/{token}` is registered but no longer appears in any \
                     dispatch source — if it was removed, remove it here and \
                     lower the ratchet"
                );
            }
        }
    }

    /// **Anti-vacuous twin.** A containment check over sources that contain
    /// every short word would pass for anything. It does not.
    #[test]
    fn a_command_that_does_not_exist_is_not_found() {
        let sources = dispatch_sources();
        for absent in ["zzznotacommand", "quuxfrobnicate", "slash-registry-probe"] {
            let needle = format!("\"{absent}\"");
            assert!(
                !sources.iter().any(|(_, src)| src.contains(&needle)),
                "`{absent}` was 'found' in the dispatch — the containment \
                 check cannot fail and proves nothing"
            );
        }
    }

    #[test]
    fn no_token_is_registered_twice() {
        let mut seen = BTreeSet::new();
        for command in COMMANDS {
            for token in command.tokens() {
                assert!(
                    seen.insert(token),
                    "`/{token}` is registered twice — two entries claiming one \
                     token means the ratchet counts a command that cannot be \
                     reached"
                );
            }
        }
        assert_eq!(seen.len(), all_tokens().len());
    }

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
            missing <= 26,
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

    /// The absorb set is what `/settings` must carry. Named here so the form's
    /// own tests can be checked against it rather than a second hand-list.
    #[test]
    fn the_absorb_set_is_the_settings_form_contract() {
        let absorbed: Vec<&str> = COMMANDS
            .iter()
            .filter(|c| c.disposition == Disposition::Absorb)
            .map(|c| c.name)
            .collect();
        assert!(
            absorbed.contains(&"edit-mode") && absorbed.contains(&"tenacity"),
            "the two families slice 1 absorbs must be marked Absorb: {absorbed:?}"
        );
        assert!(
            COMMANDS.iter().any(|c| c.disposition == Disposition::Keep),
            "nothing is kept — the absorb rule would be 'absorb everything'"
        );
    }

    /// **Every field the form carries is marked absorbed here.**
    ///
    /// The form's `Field::name()` IS the registry's command name, so this is a
    /// real join rather than two lists that look alike. It catches the drift in
    /// the direction that actually happens: a knob gets added to `/settings`
    /// and the registry keeps calling its verb a `Keep`, so the consolidation
    /// count never moves.
    #[test]
    fn every_settings_field_is_registered_as_absorbed() {
        for field in crate::settings_form::Field::ALL {
            let command = lookup(field.name())
                .unwrap_or_else(|| panic!("/settings {} is not registered", field.name()));
            assert_eq!(
                command.disposition,
                Disposition::Absorb,
                "`/{}` is a field of /settings but the registry still calls it \
                 {:?} — the surface never shrinks if absorbing does not count",
                command.name,
                command.disposition
            );
        }
    }

    /// **And the other direction: every absorbed row names a field that
    /// exists.**
    ///
    /// The join above catches a knob added to the form and forgotten by the
    /// registry — a MISCOUNT. This one catches the failure that reaches the
    /// operator: `unknown_command_hint` answers a retired verb with
    /// "/{token} sets a value that now lives in /settings {name}", built from
    /// the registry alone. If no such field exists, the redirect sends someone
    /// to a door that is not there, and the message is confident about it.
    ///
    /// **Scoped to RETIRED rows, and that scope is the point.** A
    /// `Disposition::Absorb` on a live verb is a PLAN — eleven of them are
    /// still waiting on their slice of #2009, and asserting against a plan
    /// would only pressure someone to mark the plan differently. Once the verb
    /// retires, the pointer is no longer a plan: it is the entire remaining
    /// behaviour, and it is spoken to an operator.
    ///
    /// One join is two lists agreeing about their overlap. Two joins is the
    /// same set.
    #[test]
    fn every_absorbed_row_points_at_a_field_that_exists() {
        let fields: std::collections::BTreeSet<&str> = crate::settings_form::Field::ALL
            .iter()
            .map(|f| f.name())
            .collect();
        let dangling: Vec<&str> = COMMANDS
            .iter()
            .filter(|c| {
                c.disposition == Disposition::Absorb && matches!(c.surface, Surface::Retired(_))
            })
            .map(|c| c.name)
            .filter(|name| !fields.contains(name))
            .collect();
        assert!(
            dangling.is_empty(),
            "these rows are marked absorbed, so their shim tells the operator \
             the setting lives at `/settings <name>` — but /settings carries \
             no such field: {dangling:?}"
        );
    }

    /// **A retired row still resolves, and still says where to go.**
    ///
    /// Retiring a verb is the one moment a row stops being reachable by the
    /// thing that names it, so it is the moment a pointer can rot unobserved:
    /// the arm is gone, no dispatch test covers it, and the only surviving
    /// behaviour is the hint. `/thinking` retired in #2045 precisely because a
    /// half-working shim never gets to die — a shim that redirects nowhere is
    /// the same defect wearing the opposite face.
    #[test]
    fn a_retired_row_still_resolves_to_its_replacement() {
        for command in COMMANDS {
            let Surface::Retired(dest) = command.surface else {
                continue;
            };
            // The no-dangling guard (§6 F6): a pointer to nowhere is worse
            // than no pointer, because it is confident.
            assert!(
                dest.starts_with('/'),
                "`/{}` retires to {dest:?}, which is not a command",
                command.name
            );
            let target = dest.trim_start_matches('/');
            let target = target.split_whitespace().next().unwrap_or(target);
            assert!(
                lookup(target).is_some(),
                "`/{}` retires to `/{target}`, which is not registered",
                command.name
            );
            for token in command.tokens() {
                let hint = fallthrough_message(token);
                assert!(
                    lookup(token).is_some(),
                    "`/{token}` is retired but no longer resolves — the hint \
                     path cannot find the row that explains it"
                );
                assert!(
                    hint.contains(dest),
                    "`/{token}` is retired and its hint does not name its \
                     declared destination {dest:?}: {hint:?}"
                );
            }
        }
    }

    /// Anti-vacuous: the guard above is worthless if nothing is retired yet.
    #[test]
    fn something_is_actually_retired() {
        assert!(
            COMMANDS
                .iter()
                .any(|c| matches!(c.surface, Surface::Retired(_))),
            "no row is retired, so the retired-pointer guard proves nothing"
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
             Receipts: journalled {} · read-only {} · **missing {}**.\n",
            COMMANDS.len(),
            slash_commands().count(),
            slash_tokens().len(),
            count(Disposition::Absorb),
            count(Disposition::Keep),
            count(Disposition::Panel),
            receipts(Receipt::Journal),
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
        ] {
            assert!(block.contains(needle), "the doc never says {needle}");
        }
    }
}

#[cfg(test)]
mod fallthrough_tests {
    use super::*;

    /// A genuine typo still gets the plain answer and the pointer to `/help`.
    #[test]
    fn an_unregistered_token_is_reported_as_unknown() {
        let msg = fallthrough_message("zzznotacommand");
        assert!(msg.contains("unknown command: /zzznotacommand"), "{msg}");
        assert!(msg.contains("/help"), "{msg}");
    }

    /// **A registered command reaching the fallthrough is a routing bug, and
    /// is named as one.** Fifty of the seventy-eight tokens never reach
    /// `dispatch_slash` at all; telling the operator one of them is "unknown"
    /// sends them to `/help` to find something already listed there.
    #[test]
    fn a_registered_token_is_not_called_unknown() {
        // Live `Surface::Slash` rows only — a RETIRED token correctly gets
        // its pointer instead of this message, which is what
        // `a_retired_row_still_resolves_to_its_replacement` pins.
        for token in ["remember", "tab", "crew"] {
            let msg = fallthrough_message(token);
            assert!(
                !msg.contains("unknown command"),
                "`/{token}` is registered; calling it unknown is the defect: {msg}"
            );
            assert!(msg.contains("routing bug"), "{msg}");
        }
    }

    /// **The lifecycle family is four rows, not one row with three aliases.**
    ///
    /// The help called `/end` and `/restart` "aliases of /new" and the
    /// registry knew nothing about any of them, so nothing contradicted it.
    /// `chat.rs:4512-4515` does: `/new` and `/clear` share one arm returning
    /// `Some("new")`, while `/end` and `/restart` return their own words, and
    /// that word is written to the persisted `end_reason` column
    /// (`store.rs:2017`). A difference that outlives the session that made it
    /// is not an alias.
    ///
    /// `/start` is further out still — it skips close-time note extraction
    /// entirely, leaves the outgoing conversation OPEN and resumable, and
    /// takes a title.
    ///
    /// This test exists because the three tokens LOOK interchangeable, which
    /// is exactly the argument that would collapse them in a later cleanup.
    #[test]
    fn the_lifecycle_verbs_that_differ_are_separate_rows() {
        for token in ["new", "end", "restart", "start"] {
            let row = lookup(token).unwrap_or_else(|| panic!("/{token} is registered"));
            assert_eq!(
                row.name, token,
                "`/{token}` resolves to `/{}` — it was made an alias of a \
                 command it does not behave like",
                row.name
            );
        }
        // ...and the one pair that IS proven identical stays one row.
        assert_eq!(
            lookup("clear").map(|c| c.name),
            Some("new"),
            "`/clear` and `/new` share a dispatch arm; two rows would claim a \
             difference the code does not have"
        );
    }

    /// Every ghost registered by PR2 is reachable, receipted honestly, and
    /// advertised — the three things being unregistered let them skip.
    #[test]
    fn the_registered_ghosts_are_typed_mutators_that_owe_a_receipt() {
        for token in ["new", "clear", "end", "restart", "start", "cd"] {
            let row = lookup(token).unwrap_or_else(|| panic!("/{token} is registered"));
            assert_eq!(row.surface, Surface::Slash, "/{token} is typed today");
            assert_eq!(
                row.receipt,
                Receipt::Missing,
                "/{token} mutates durable state and records no receipt; \
                 saying otherwise hides it from the #1965 debt"
            );
        }
    }

    /// **A section action is told where it lives, not accused of being a
    /// bug.**
    ///
    /// `/probe reset` never routed at the top level and never will — it is an
    /// action inside a section. The generic arm calls any registered token
    /// that falls through a routing bug and asks for a report, which for this
    /// row is both false and a dead end: it names no destination.
    #[test]
    fn a_section_action_names_the_door_it_is_behind() {
        let msg = fallthrough_message("probe reset");
        assert!(msg.contains("/settings"), "names where it lives: {msg}");
        assert!(!msg.contains("routing bug"), "it is not a bug: {msg}");
        assert!(!msg.contains("unknown command"), "{msg}");
    }

    /// Aliases resolve to their command, so a shim can name the replacement
    /// for the token the operator actually typed.
    #[test]
    fn lookup_resolves_aliases_and_is_case_insensitive() {
        assert_eq!(lookup("quit").map(|c| c.name), Some("exit"));
        assert_eq!(lookup("vi").map(|c| c.name), Some("edit-mode"));
        assert_eq!(lookup("NUDGE").map(|c| c.name), Some("nudge"));
        assert!(lookup("zzznotacommand").is_none());
    }

    /// **`/psyche` is not an alias of `/cognition`; it is what `/cognition`
    /// redirects TO.**
    ///
    /// The registry had the arrow backwards — one row named `cognition` with
    /// `psyche` in its alias list — which made the surviving verb resolve to
    /// the retired one. Two rows now, and each says what it does.
    #[test]
    fn psyche_performs_and_cognition_points_at_it() {
        let psyche = lookup("psyche").expect("/psyche is registered");
        assert_eq!(
            psyche.name, "psyche",
            "resolves to itself, not to cognition"
        );
        assert_eq!(psyche.surface, Surface::Slash, "it is still typed");
        assert_eq!(psyche.disposition, Disposition::Keep, "and it performs");

        let cognition = lookup("cognition").expect("/cognition is registered");
        assert_eq!(
            cognition.surface,
            Surface::Retired("/settings cognition"),
            "`commands::settings` answers /cognition with a redirect that \
             mutates nothing — the registry has to say so"
        );
    }

    /// #2001: `/settings` shipped in #1994 reachable but ADVERTISED NOWHERE —
    /// absent from `help_lines()`, which also seeds the palette, so typing it
    /// got palette-completed into `/crew edit`. The inventory called this
    /// drift class out ("the dispatch outgrew the help") and #1994 then added
    /// an instance of it. This ratchet makes the drift a test failure: a
    /// registry command either LEADS a help line or is enumerated below, and
    /// the list may only shrink.
    #[test]
    fn every_registry_command_is_advertised_or_ratcheted() {
        // Exact set, not a count: membership names the debt (F0d discipline).
        // Remove rows as commands gain help lines; NEVER add one for a new
        // command — new commands ship advertised.
        const KNOWN_UNADVERTISED: &[&str] = &[
            // The inventory's "advertised nowhere" set (#1994 §1), verbatim.
            "callees",
            "callers",
            "cognition",
            "detail",
            "edit-mode",
            "hierarchy",
            "implementations",
            "markdown",
            "rename",
            "tab",
            "tenacity",
            "undo-lock",
        ];

        // **Matched by NAME, not by word count.** This took the first
        // whitespace-delimited token of each help line, which cannot see a
        // `SectionAction` — `/probe reset` advertised itself as `probe`, so a
        // registered two-word row could never be found however plainly it was
        // documented. A registry that grows subcommand rows (#2009 PR1) needs
        // the check to read the name it is looking for.
        let is_advertised = |name: &str| -> bool {
            let needle = format!("/{name}");
            crate::help_lines().iter().any(|line| {
                line.trim_start().strip_prefix(&needle).is_some_and(|rest| {
                    // A real boundary, so `/model` is not advertised by
                    // `/models` and `/probe` is not advertised by
                    // `/probe reset`.
                    rest.is_empty() || rest.starts_with(char::is_whitespace)
                })
            })
        };

        // Positive read assertion: an empty parse must fail, not pass.
        let advertised_count = COMMANDS.iter().filter(|c| is_advertised(c.name)).count();
        assert!(
            advertised_count >= 20,
            "help_lines() parse collapsed: only {advertised_count} commands \
             matched a help row"
        );

        // **A retired row must NOT be advertised — that is what retiring
        // it means.** The help teaches the surface, and a help line for a
        // command the cut just folded would teach the fold away. The verb
        // goes on working (a retired read still reads); it stops being
        // taught, and `fallthrough_message` carries the pointer for muscle
        // memory. `no_retired_row_is_still_advertised` is the other half.
        let missing: Vec<&str> = slash_commands()
            .chain(
                COMMANDS
                    .iter()
                    .filter(|c| c.surface == Surface::SectionAction),
            )
            .map(|c| c.name)
            .filter(|n| !is_advertised(n) && !KNOWN_UNADVERTISED.contains(n))
            .collect();
        assert!(
            missing.is_empty(),
            "registry commands with no help_lines() row (add the row, or argue \
             a KNOWN_UNADVERTISED entry in review): {missing:?}"
        );
        // ...and the paired direction, so "not advertised" cannot quietly
        // become the way to dodge the check above.
        let taught: Vec<&str> = COMMANDS
            .iter()
            .filter(|c| matches!(c.surface, Surface::Retired(_)))
            .map(|c| c.name)
            .filter(|n| is_advertised(n))
            .collect();
        assert!(
            taught.is_empty(),
            "these rows are retired but the help still teaches them as \
             top-level commands, which teaches the fold away: {taught:?}"
        );

        // The ratchet only shrinks: a row that gained a help line must leave.
        let stale: Vec<&str> = KNOWN_UNADVERTISED
            .iter()
            .copied()
            .filter(|n| is_advertised(n))
            .collect();
        assert!(
            stale.is_empty(),
            "now advertised — remove from the list: {stale:?}"
        );
    }

    /// The slash surface must be deduplicated: one token, one command. The
    /// registry's `lookup` is first-match, so a duplicate token would win
    /// silently by declaration order — this makes it a test failure instead.
    /// Mutation-proved: `settings` claiming `config`'s token goes red with
    /// "token `/config` claimed by both `settings` and `config`".
    #[test]
    fn no_token_resolves_to_two_commands() {
        let mut seen: std::collections::BTreeMap<&str, &str> = Default::default();
        for c in COMMANDS {
            for t in c.tokens() {
                if let Some(prev) = seen.insert(t, c.name) {
                    panic!("token `/{t}` claimed by both `{prev}` and `{}`", c.name);
                }
            }
        }
    }
}
