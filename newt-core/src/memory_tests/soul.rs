use super::*;

#[test]
fn default_soul_lists_all_current_tools() {
    // Regression: DEFAULT_SOUL went stale when use_skill (#135) and
    // web_fetch (#139) were added but the constant wasn't updated, so
    // default-identity sessions never learned those tools existed.
    // find (#496) is the latest such addition; render_report (#1004) is
    // the newest.
    for tool in [
        "run_command",
        "read_file",
        "write_file",
        "edit_file",
        "list_dir",
        "find",
        "use_skill",
        "web_fetch",
        "render_report",
    ] {
        assert!(
            DEFAULT_SOUL.contains(tool),
            "DEFAULT_SOUL must advertise `{tool}`"
        );
    }
}
/// FR-5 (#999) golden contract: the coach identity must instruct the model
/// to ADVISE, not act — name the mutating tools as things NOT to call, frame
/// the turn as advising, and drop the doer's imperative. This is the
/// deterministic essence of "a scripted incident turn emits no mutating tool
/// call": the model's behavior follows from this directive, so if COACH_SOUL
/// ever loses it the coach silently regresses into a doer.
#[test]
fn coach_soul_forbids_mutation_and_mandates_advice() {
    // Names the mutating tools it must not call.
    for mutating in ["write_file", "edit_file", "run_command"] {
        assert!(
            COACH_SOUL.contains(mutating),
            "COACH_SOUL must name `{mutating}` (as forbidden)"
        );
    }
    assert!(
        COACH_SOUL.contains("Do not call write_file"),
        "COACH_SOUL must carry the explicit no-mutation directive"
    );
    assert!(
        COACH_SOUL.to_lowercase().contains("advis"),
        "COACH_SOUL must frame the turn as advising"
    );
    // And it must NOT carry the doer's act-first imperative.
    assert!(
        !COACH_SOUL.contains("Never describe a code change — make it"),
        "COACH_SOUL must not inherit the doer imperative"
    );
}
