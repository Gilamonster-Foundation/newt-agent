use super::*;

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
/// # 44/54 → 42/52: `/roadmap` absorbs `/tree` and `/plan` (#2009 PR12)
///
/// The dispatch had already folded them — `/tree` ran `/roadmap show` and
/// `/plan` ran `/roadmap` — so what this slice does is make the REGISTER
/// say what the code had been doing, and give the fold the retirement rule
/// the others got.
///
/// # 26 → 25: `/plan` never wrote either
///
/// Its row was `Absorb`/`Missing`, as though it set something. It was an
/// alias that FORWARDED to `/roadmap` — `chat.rs` rewrote the line and
/// handed it on. A row that cannot write never owed a receipt, so this is
/// the same truthing shape as `/memory` and `/loadout` (PR3) and
/// `/retrieval` (PR11): the debt was never its own.
///
/// Four of the six reductions so far are this — a row that was counted
/// because nobody had read it, not because it stopped writing. That is
/// worth watching: the remaining 25 should be checked for the same before
/// anyone assumes they all need an event journal to clear.
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
///
/// # 42/52 → 43/53: `/models` was never actually retired
///
/// The same shape as PR2's "registering the ghosts": a command an operator
/// can type today, reaching a real handler (`lib.rs`'s dispatch match names
/// `"models"` explicitly), counted by neither number because its row claimed
/// `Surface::Retired("/status models")`. That destination is the rewrite BACK
/// to `/models`, so the row pointed at itself, and the help corpus was pruned
/// to match — leaving `/model <name>` as the only listed model verb and no
/// listed way to see the names. **A ratchet that does not know about a
/// command cannot stop it growing.** This raise makes the guarded surface the
/// surface that exists; it is honest rather than a weakening.
#[test]
fn the_registered_surface_only_shrinks() {
    assert!(
        slash_commands().count() <= 43,
        "the slash surface GREW to {} commands. #1981 is a reduction: a \
         new command needs an argument for why it is not a field of \
         /settings or a subcommand of an existing verb",
        slash_commands().count()
    );
    assert!(
        slash_tokens().len() <= 53,
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

/// **A command you can type must be listed where commands are listed.**
///
/// `/models` was typeable, dispatched, and absent from `help_lines()` — the
/// ONE corpus `/help` prints and the rich palette parses. So an operator
/// could switch models (`/model <name>` is listed) but had nowhere to learn
/// the names. Nothing failed: the palette's parity test builds its own
/// fixture entries, so the real corpus can lose a row without any test
/// noticing. That is the vacuous shape this guard exists to close.
///
/// A ratchet, not a wall, in the `KNOWN_VIOLATIONS` idiom (`CLAUDE.md`: the
/// mess is fixed by ratchet, not by rewrite). The five below are real and
/// may only go DOWN — document one and delete its row; never add a row to
/// make a new command pass.
#[test]
fn every_typeable_command_is_listed_in_the_help_corpus() {
    /// Typeable today, absent from the corpus. This list may only SHRINK.
    const KNOWN_UNDOCUMENTED: &[&str] = &["detail", "edit-mode", "markdown", "tab", "undo-lock"];

    let corpus = crate::help::help_lines().join("\n");
    let mut undocumented: Vec<&str> = slash_commands()
        .map(|c| c.name)
        .filter(|name| !corpus.contains(&format!("/{name}")))
        .collect();
    undocumented.sort_unstable();

    let mut allowed = KNOWN_UNDOCUMENTED.to_vec();
    allowed.sort_unstable();

    let fresh: Vec<&&str> = undocumented
        .iter()
        .filter(|n| !allowed.contains(n))
        .collect();
    assert!(
        fresh.is_empty(),
        "these commands can be typed but are listed nowhere an operator \
         looks: {fresh:?}. Add a line to `help_lines()` — it is the only \
         corpus /help and the palette read."
    );
    assert!(
        undocumented.len() <= KNOWN_UNDOCUMENTED.len(),
        "the undocumented set grew to {}",
        undocumented.len()
    );
    for known in &allowed {
        assert!(
            undocumented.contains(known),
            "`/{known}` is documented now — delete it from KNOWN_UNDOCUMENTED \
             so the count keeps falling"
        );
    }
}

/// Anti-vacuous twin: the guard above must actually be able to SEE a gap.
///
/// If `help_lines()` were empty, or the containment check always matched,
/// the ratchet would pass while documenting nothing. So prove the detector
/// detects: a name that is certainly not in the corpus must be reported as
/// missing, and a name that certainly is must not.
#[test]
fn the_help_corpus_guard_can_tell_listed_from_unlisted() {
    let corpus = crate::help::help_lines().join("\n");
    assert!(
        !corpus.is_empty(),
        "an empty corpus would pass the guard above vacuously"
    );
    assert!(
        !corpus.contains("/zzz-not-a-command"),
        "the detector must report an absent command as absent"
    );
    assert!(
        corpus.contains("/models"),
        "the detector must report a listed command as listed — and /models \
         is the row whose loss this guard was written for"
    );
}
