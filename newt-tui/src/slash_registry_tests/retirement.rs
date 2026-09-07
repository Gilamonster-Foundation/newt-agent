use super::*;

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
