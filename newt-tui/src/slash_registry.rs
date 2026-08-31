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

const fn cmd(
    name: &'static str,
    aliases: &'static [&'static str],
    family: Family,
    disposition: Disposition,
    receipt: Receipt,
) -> SlashCommand {
    SlashCommand {
        name,
        aliases,
        family,
        disposition,
        receipt,
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
    cmd(
        "memory",
        &[],
        Family::Memory,
        Disposition::Keep,
        Receipt::Missing,
    ),
    cmd(
        "recall",
        &[],
        Family::Memory,
        Disposition::Keep,
        Receipt::None_,
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
    cmd(
        "byline",
        &[],
        Family::Meta,
        Disposition::Keep,
        Receipt::None_,
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
    cmd(
        "config",
        &[],
        Family::Meta,
        Disposition::Keep,
        Receipt::None_,
    ),
    cmd("docs", &[], Family::Meta, Disposition::Keep, Receipt::None_),
    cmd(
        "exit",
        &["quit"],
        Family::Meta,
        Disposition::Keep,
        Receipt::None_,
    ),
    cmd("help", &[], Family::Meta, Disposition::Keep, Receipt::None_),
    cmd("info", &[], Family::Meta, Disposition::Keep, Receipt::None_),
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
    cmd(
        "version",
        &[],
        Family::Meta,
        Disposition::Keep,
        Receipt::None_,
    ),
    cmd(
        "workspace",
        &[],
        Family::Meta,
        Disposition::Keep,
        Receipt::None_,
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
    cmd(
        "summarizer",
        &[],
        Family::Model,
        Disposition::Absorb,
        Receipt::Missing,
    ),
    cmd(
        "callees",
        &[],
        Family::Navigator,
        Disposition::Keep,
        Receipt::None_,
    ),
    cmd(
        "callers",
        &[],
        Family::Navigator,
        Disposition::Keep,
        Receipt::None_,
    ),
    cmd(
        "compare",
        &[],
        Family::Navigator,
        Disposition::Keep,
        Receipt::None_,
    ),
    cmd(
        "def",
        &["goto"],
        Family::Navigator,
        Disposition::Keep,
        Receipt::None_,
    ),
    cmd(
        "export",
        &[],
        Family::Navigator,
        Disposition::Keep,
        Receipt::None_,
    ),
    cmd(
        "hierarchy",
        &[],
        Family::Navigator,
        Disposition::Keep,
        Receipt::None_,
    ),
    cmd(
        "impact",
        &[],
        Family::Navigator,
        Disposition::Keep,
        Receipt::None_,
    ),
    cmd(
        "implementations",
        &["impls"],
        Family::Navigator,
        Disposition::Keep,
        Receipt::None_,
    ),
    cmd(
        "map",
        &[],
        Family::Navigator,
        Disposition::Keep,
        Receipt::None_,
    ),
    cmd(
        "tests",
        &[],
        Family::Navigator,
        Disposition::Keep,
        Receipt::None_,
    ),
    cmd(
        "text",
        &["grep"],
        Family::Navigator,
        Disposition::Keep,
        Receipt::None_,
    ),
    cmd(
        "type",
        &[],
        Family::Navigator,
        Disposition::Keep,
        Receipt::None_,
    ),
    cmd(
        "uses",
        &["refs"],
        Family::Navigator,
        Disposition::Keep,
        Receipt::None_,
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
    cmd(
        "conversation",
        &[],
        Family::Session,
        Disposition::Keep,
        Receipt::Missing,
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
    cmd(
        "cognition",
        &["psyche"],
        Family::Tuning,
        Disposition::Absorb,
        Receipt::Journal,
    ),
    cmd(
        "detail",
        &[],
        Family::Tuning,
        Disposition::Absorb,
        Receipt::Missing,
    ),
    cmd(
        "loadout",
        &[],
        Family::Tuning,
        Disposition::Absorb,
        Receipt::Missing,
    ),
    cmd(
        "markdown",
        &[],
        Family::Tuning,
        Disposition::Absorb,
        Receipt::Missing,
    ),
    cmd(
        "mode",
        &[],
        Family::Tuning,
        Disposition::Absorb,
        Receipt::Missing,
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
        "prompt",
        &[],
        Family::Tuning,
        Disposition::Absorb,
        Receipt::Missing,
    ),
    cmd(
        "retrieval",
        &[],
        Family::Tuning,
        Disposition::Absorb,
        Receipt::Missing,
    ),
    cmd(
        "rounds",
        &["tool-rounds", "max-rounds"],
        Family::Tuning,
        Disposition::Absorb,
        Receipt::Journal,
    ),
    cmd(
        "tenacity",
        &[],
        Family::Tuning,
        Disposition::Absorb,
        Receipt::Journal,
    ),
    cmd(
        "thinking",
        &[],
        Family::Tuning,
        Disposition::Absorb,
        Receipt::Journal,
    ),
];

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
        // An absorbed setting names its new home, not its old handler.
        Some(command) if command.disposition == Disposition::Absorb => format!(
            "/{token} sets a value that now lives in /settings {}",
            command.name
        ),
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
                src.contains("trim_start_matches('/')"),
                "{name}: no slash interception survived the cut"
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
    /// **Slice 1 raised these by one, and that is honest rather than a
    /// weakening.** `/settings` is a net ADDITION: it absorbs the editor-mode
    /// family, but `/vi`, `/emacs`, `/nano` and `/edit-mode` remain reachable
    /// as shims, because a removed command that answers "unknown" is worse
    /// than the four verbs were. The reduction lands when the deprecation
    /// window closes and the shims are retired — four tokens and one command
    /// come off then, and this bound comes down with them. Raising a ratchet
    /// is allowed exactly when the growth is the plan; it is not allowed to
    /// make a surprise go away.
    #[test]
    fn the_registered_surface_only_shrinks() {
        assert!(
            COMMANDS.len() <= 65,
            "the slash surface GREW to {} commands. #1981 is a reduction: a \
             new command needs an argument for why it is not a field of \
             /settings or a subcommand of an existing verb",
            COMMANDS.len()
        );
        assert!(
            all_tokens().len() <= 79,
            "the slash surface GREW to {} tokens",
            all_tokens().len()
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
    #[test]
    fn the_number_of_slash_interception_sites_is_pinned() {
        let counted: usize = dispatch_sources()
            .iter()
            .map(|(_, src)| src.matches("trim_start_matches('/')").count())
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
        for command in COMMANDS {
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
    #[test]
    fn the_receiptless_state_mutators_are_counted_and_only_shrink() {
        let missing = COMMANDS
            .iter()
            .filter(|c| matches!(c.receipt, Receipt::Missing))
            .count();
        assert!(
            missing <= 27,
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

    fn table() -> String {
        let mut rows: Vec<&SlashCommand> = COMMANDS.iter().collect();
        rows.sort_by_key(|c| (format!("{:?}", c.family), c.name));
        let mut out =
            String::from("| command | also typed as | family | disposition | receipt |\n");
        out.push_str("|---|---|---|---|---|\n");
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
            out.push_str(&format!(
                "| `/{}` | {aliases} | {:?} | {} | {} |\n",
                command.name,
                command.family,
                disposition_cell(command),
                receipt_cell(command),
            ));
        }
        let count = |d: Disposition| COMMANDS.iter().filter(|c| c.disposition == d).count();
        let receipts = |r: Receipt| COMMANDS.iter().filter(|c| c.receipt == r).count();
        out.push_str(&format!(
            "\n**{} commands, {} tokens.** Absorb {} · keep {} · panel {}. \
             Receipts: journalled {} · read-only {} · **missing {}**.\n",
            COMMANDS.len(),
            all_tokens().len(),
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
        for token in ["memory", "tab", "def"] {
            let msg = fallthrough_message(token);
            assert!(
                !msg.contains("unknown command"),
                "`/{token}` is registered; calling it unknown is the defect: {msg}"
            );
            assert!(msg.contains("routing bug"), "{msg}");
        }
    }

    /// Aliases resolve to their command, so a shim can name the replacement
    /// for the token the operator actually typed.
    #[test]
    fn lookup_resolves_aliases_and_is_case_insensitive() {
        assert_eq!(lookup("quit").map(|c| c.name), Some("exit"));
        assert_eq!(lookup("vi").map(|c| c.name), Some("edit-mode"));
        assert_eq!(lookup("PSYCHE").map(|c| c.name), Some("cognition"));
        assert!(lookup("zzznotacommand").is_none());
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

        let advertised: std::collections::BTreeSet<&str> = crate::help_lines()
            .iter()
            .filter_map(|l| l.trim_start().split_whitespace().next())
            .filter_map(|tok| tok.strip_prefix('/'))
            .collect();
        // Positive read assertion: an empty parse must fail, not pass.
        assert!(
            advertised.len() >= 20,
            "help_lines() parse collapsed: only {} slash tokens",
            advertised.len()
        );

        let missing: Vec<&str> = COMMANDS
            .iter()
            .map(|c| c.name)
            .filter(|n| !advertised.contains(n) && !KNOWN_UNADVERTISED.contains(n))
            .collect();
        assert!(
            missing.is_empty(),
            "registry commands with no help_lines() row (add the row, or argue \
             a KNOWN_UNADVERTISED entry in review): {missing:?}"
        );
        // The ratchet only shrinks: a row that gained a help line must leave.
        let stale: Vec<&str> = KNOWN_UNADVERTISED
            .iter()
            .copied()
            .filter(|n| advertised.contains(n))
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
