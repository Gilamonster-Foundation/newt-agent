use super::{fold_observed_report, observed_report_folds};

const REPORT: &str = "## Observed\n\nReport `sample` — root `project`.\n\nFiles since objective/adoption snapshot (LF counts; absence is 0):\n- No content changes observed.\n\nLast observed checks per exact command/cwd (historical observations, not current-tree certification):\n- No check observation available.\n\nGoverned publication observations (not a live remote-state check):\n- No governed receipt available.\n\n## Model explanation\n\n";

#[test]
fn a_report_folds_to_one_marker_and_the_body_is_retained() {
    let mut retained = None;
    let folded = fold_observed_report(&format!("{REPORT}Hello again!"), |body| {
        retained = Some(body.to_string());
        Some("/spill open 3".into())
    })
    .expect("a complete report folds");
    assert_eq!(
        folded,
        "▸ Observed · 12 lines hidden  [/spill open 3]\n\nHello again!"
    );
    let body = retained.expect("the report body is archived");
    assert!(body.starts_with("## Observed") && body.contains("No governed receipt"));
    assert!(
        !body.contains("Hello again!"),
        "the explanation stays visible"
    );
}

#[test]
fn a_reply_without_a_report_or_without_retention_is_left_alone() {
    assert_eq!(
        fold_observed_report("Hello again!", |_| panic!("nothing to retain")),
        None
    );
    assert_eq!(fold_observed_report(&format!("{REPORT}Hi"), |_| None), None);
}

/// #3009: the fold follows the surface that can reopen the report, not the
/// global `spill_summary()` flag. Lean/piped keeps the report even after
/// `/spill summary` (its `/spill open` is refused); the Rich surface, cockpit
/// included, folds by default and `/spill excerpt` shows it.
#[test]
fn the_report_folds_on_the_rich_surface_and_never_on_lean() {
    assert!(!observed_report_folds(false, None), "lean default");
    assert!(
        !observed_report_folds(false, Some(true)),
        "lean + /spill summary: recovery is refused there, so the report stays"
    );
    assert!(
        observed_report_folds(true, None),
        "Rich (cockpit) default folds"
    );
    assert!(
        observed_report_folds(true, Some(true)),
        "Rich + /spill summary"
    );
    assert!(
        !observed_report_folds(true, Some(false)),
        "Rich + /spill excerpt shows the report"
    );
}

/// The turn loop must ask that policy, not read the global flag.
#[test]
fn the_turn_loop_gates_the_fold_on_the_surface_policy() {
    let src = include_str!("../chat.rs");
    let at = src
        .find("fold_observed_report(&display_reply")
        .expect("call site");
    let window = &src[at.saturating_sub(400)..at];
    assert!(window.contains("observed_report_folds("), "{window}");
    assert!(!window.contains("spill_summary()"), "{window}");
}
