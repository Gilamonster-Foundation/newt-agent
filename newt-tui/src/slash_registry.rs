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
    // Read by the ratchet and by the decision doc, not yet by the dispatch:
    // `disposition` is what `/settings` will consult to build its form, and
    // `receipt` is the #1965 debt it will pay down. `cfg_attr` rather than a
    // bare `allow` so the exemption disappears the moment a production reader
    // exists — and it is scoped to these two fields, not the module.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) disposition: Disposition,
    #[cfg_attr(not(test), allow(dead_code))]
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
        Receipt::Missing,
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
        Receipt::Missing,
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
        Receipt::Missing,
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
        Receipt::Missing,
    ),
    cmd(
        "tenacity",
        &[],
        Family::Tuning,
        Disposition::Absorb,
        Receipt::Missing,
    ),
    cmd(
        "thinking",
        &[],
        Family::Tuning,
        Disposition::Absorb,
        Receipt::Missing,
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
    #[test]
    fn the_registered_surface_only_shrinks() {
        assert!(
            COMMANDS.len() <= 64,
            "the slash surface GREW to {} commands. #1981 is a reduction: a \
             new command needs an argument for why it is not a field of \
             /settings or a subcommand of an existing verb",
            COMMANDS.len()
        );
        assert!(
            all_tokens().len() <= 78,
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
            missing <= 33,
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
}
