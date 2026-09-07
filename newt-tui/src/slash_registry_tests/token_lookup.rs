use super::*;

use std::collections::BTreeSet;

/// **Anti-vacuous twin.** A containment check over sources that contain
/// every short word would pass for anything. It does not.
#[test]
fn a_command_that_does_not_exist_is_not_found() {
    let sources = dispatch_sources();
    for absent in ["zzznotacommand", "quuxfrobnicate", "slash-registry-probe"] {
        assert!(
            !sources
                .iter()
                .any(|(_, src)| contains_dispatch_token(src, absent)),
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
