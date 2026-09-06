use super::*;
use crossterm::event::Event;
use test_support::{ctrl, key, special, RecordingSink};

/// G3(a), the registration conformance test (#2005): every claimant the
/// shipped table names must be REACHABLE from a real key sequence through
/// the editor's own accessors.
///
/// Reachability, not spelling, is the point. A test that asserted the five
/// names appear somewhere in the source would pass on a `claims` that can
/// never fire; this one types the keys an operator types and reads the
/// claim set back, so a rung with no accessor — or an accessor guarded on
/// a condition that is never true — fails the PR that adds it.
#[cfg(unix)]
#[test]
fn every_ladder_claimant_is_reachable_from_the_editors_own_state() {
    // The key sequence that puts a fresh vi mount into each claiming
    // state, straight out of `esc_and_vi_contract.md` §4.
    let reach: &[(&str, &[KeyEvent])] = &[
        // A fresh vi mount IS in INSERT, so no keys at all.
        ("vi-insert", &[]),
        ("palette", &[key('/')]),
        ("vi-pending", &[special(KeyCode::Esc), key('d')]),
        ("vi-ex", &[special(KeyCode::Esc), key(':')]),
        (
            "vi-confirm",
            &[
                special(KeyCode::Esc),
                key(':'),
                key('w'),
                key('q'),
                special(KeyCode::Enter),
            ],
        ),
    ];
    for (claimant, keys) in reach {
        let mut mounted = MountedEditor::new(Edit::Vi, Some(1), Vec::new(), "");
        let mut sink = RecordingSink::default();
        for k in *keys {
            mounted.on_event(Event::Key(*k), &mut sink).unwrap();
        }
        assert!(
            mounted.claim_set().is_live(claimant),
            "`{claimant}` is a rung in assets/esc_ladder.toml but nothing \
                 in Vi::claims / Editor::claims / MountedEditor::claim_set \
                 reports it — the rung can never fire"
        );
    }

    // Both directions. The loop above proves every listed name is
    // reachable; this proves the LIST is the table, so adding a rung
    // without an accessor (or an accessor without a rung) is a red PR
    // rather than a dead row nobody notices.
    let mut reachable: Vec<&str> = reach.iter().map(|(name, _)| *name).collect();
    reachable.sort_unstable();
    let mut table: Vec<&str> = crate::esc_ladder::ESC_LADDER.claimants().collect();
    table.sort_unstable();
    assert_eq!(
        reachable, table,
        "the ladder's claimants and the states this test can reach have \
             drifted apart"
    );

    // ANTI-VACUOUS TWIN: an idle vi mount in NORMAL claims NOTHING, so the
    // assertions above cannot be passing on a `claim_set` that names
    // everything unconditionally — which would swallow the interrupt at
    // every rung and reproduce exactly the defect #2005 fixes.
    let mut normal = MountedEditor::new(Edit::Vi, Some(1), Vec::new(), "");
    let mut sink = RecordingSink::default();
    normal
        .on_event(Event::Key(special(KeyCode::Esc)), &mut sink)
        .unwrap();
    assert_eq!(
        normal.claim_set().names().collect::<Vec<_>>(),
        Vec::<&str>::new(),
        "vi NORMAL with nothing pending must decline Esc — that decline IS \
             rung 7"
    );

    // ANTI-VACUOUS TWIN, second half: the `edit == Edit::Vi` gate in
    // `Editor::claims`. `Editor` carries a `Vi` in every mode and it
    // starts in INSERT, so without the gate an emacs mount would claim
    // `vi-insert` forever and Esc would never reach the hatch there.
    for (name, edit) in [("emacs", Edit::Emacs), ("nano", Edit::Nano)] {
        let mounted = MountedEditor::new(edit, Some(1), Vec::new(), "");
        assert_eq!(
            mounted.claim_set().names().collect::<Vec<_>>(),
            Vec::<&str>::new(),
            "{name} carries a Vi in INSERT; it must not claim Esc"
        );
    }
}

/// `vi-pending` is an OR of three separate fields, and the conformance
/// test above only reaches one of them — so dropping either of the other
/// two would go unnoticed there. Each is a live operator sequence: type a
/// count and press Esc, or use i_CTRL-O and press Esc, and rung 6 must
/// still outrank the interrupt. Without this, a mutation removing
/// `count > 0` means typing `2` mid-turn and pressing Esc kills the turn
/// instead of cancelling the count.
#[cfg(unix)]
#[test]
fn every_vi_pending_contributor_claims_esc() {
    for (what, keys) in [
        ("a pending operator", vec![special(KeyCode::Esc), key('d')]),
        ("a building count", vec![special(KeyCode::Esc), key('2')]),
        // i_CTRL-O leaves mode == Normal with the one-shot armed, so it
        // must land on `vi-pending` and NOT on `vi-insert`.
        ("i_CTRL-O", vec![ctrl('o')]),
    ] {
        let mut mounted = MountedEditor::new(Edit::Vi, Some(1), Vec::new(), "");
        let mut sink = RecordingSink::default();
        for k in keys {
            mounted.on_event(Event::Key(k), &mut sink).unwrap();
        }
        assert_eq!(
            mounted.claim_set().names().collect::<Vec<_>>(),
            vec!["vi-pending"],
            "{what} must claim rung 6 alone — not nothing (the turn dies \
                 mid-sequence) and not vi-insert as well"
        );
    }
}
