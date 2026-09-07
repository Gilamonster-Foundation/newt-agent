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
