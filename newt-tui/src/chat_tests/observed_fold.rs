use super::fold_observed_report;

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
