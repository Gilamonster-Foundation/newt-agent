use super::*;

/// `/remember` dispatches through an argument prefix, without a bare
/// `"remember"` literal. Its help-page match arm used to mask that spelling
/// from the containment guard while the catalog lived in `lib.rs`.
#[test]
fn argument_prefix_dispatch_does_not_need_a_help_catalog_match() {
    let dispatch = r#"task.trim_start_matches('/').strip_prefix("remember ")"#;
    assert!(contains_dispatch_token(dispatch, "remember"));
    assert!(!contains_dispatch_token(dispatch, "rem"));
    assert!(!contains_dispatch_token(
        r#"task.strip_prefix("remembered ")"#,
        "remember"
    ));
    assert!(!contains_dispatch_token(
        r#"task.strip_prefix("remember-more ")"#,
        "remember"
    ));
    assert!(!contains_dispatch_token(r#""remember ""#, "remember"));
    assert!(!contains_dispatch_token(
        r#""  /remember <fact> - add a note""#,
        "remember"
    ));
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
            assert!(
                sources
                    .iter()
                    .any(|(_, src)| contains_dispatch_token(src, token)),
                "`/{token}` is registered but no longer appears in any \
                 dispatch source — if it was removed, remove it here and \
                 lower the ratchet"
            );
        }
    }
}
