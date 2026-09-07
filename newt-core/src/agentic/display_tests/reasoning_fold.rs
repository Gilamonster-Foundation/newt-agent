/// The reasoning block spends the SAME budget a committed tool result
/// spends, so `/spill 5` is one dial rather than two.
#[test]
fn a_reasoning_fold_shows_the_budget_and_retains_the_rest() {
    use std::time::Duration;

    let mut fold = super::ThinkingFold::default();
    let shown: Vec<bool> = (1..=10)
        .map(|i| fold.offer(&format!("line {i}"), 3))
        .collect();
    assert_eq!(
        shown,
        [true, true, true, false, false, false, false, false, false, false],
        "the first three print, the rest are held"
    );

    // Everything is retained, printed or not — the archived body is the
    // WHOLE block, which is what makes reopening it worth the offer.
    assert_eq!(fold.body().lines().count(), 10);
    assert!(fold.body().starts_with("line 1\n"));
    assert!(fold.body().ends_with("line 10\n"));

    let line = fold
        .closing_line(
            Duration::from_secs(41),
            super::Recovery::Command("/spill open 7"),
        )
        .expect("the model reasoned, so there is a line");
    assert_eq!(line, "Thought for 41s · 7 lines hidden  [/spill open 7]");
}

/// Nothing held back means nothing to announce: offering a handle that
/// opens what is already on screen is noise, not help.
#[test]
fn a_reasoning_fold_that_hid_nothing_says_only_how_long_it_thought() {
    use std::time::Duration;

    let mut fold = super::ThinkingFold::default();
    assert!(fold.offer("a short thought", 3));
    assert_eq!(
        fold.closing_line(Duration::from_secs(1), super::Recovery::default()),
        Some("Thought for 1s".to_string())
    );

    // And a model that did not reason at all closes nothing.
    let quiet = super::ThinkingFold::default();
    assert!(quiet.is_empty());
    assert_eq!(
        quiet.closing_line(Duration::from_secs(9), super::Recovery::default()),
        None
    );
}

/// `spill_lines = 0` is "unbounded" everywhere else in this file, and it
/// has to mean the same here — that is what `[tui] thinking = "stream"`
/// rides to keep the historical trickle.
#[test]
fn a_zero_budget_holds_nothing_back() {
    use std::time::Duration;

    let mut fold = super::ThinkingFold::default();
    for i in 0..50 {
        assert!(fold.offer(&format!("line {i}"), 0), "every line prints");
    }
    assert_eq!(
        fold.closing_line(Duration::from_secs(3), super::Recovery::default()),
        Some("Thought for 3s".to_string()),
        "no fold to announce"
    );
}
