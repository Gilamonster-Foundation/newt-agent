use super::*;

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
