use super::*;

#[test]
fn elapsed_reads_in_the_register_the_operator_thinks_in() {
    use std::time::Duration;

    assert_eq!(super::fmt_duration(Duration::from_secs(0)), "0s");
    assert_eq!(super::fmt_duration(Duration::from_secs(41)), "41s");
    assert_eq!(super::fmt_duration(Duration::from_secs(59)), "59s");
    assert_eq!(super::fmt_duration(Duration::from_secs(60)), "1m 0s");
    assert_eq!(super::fmt_duration(Duration::from_secs(125)), "2m 5s");
    assert_eq!(super::fmt_duration(Duration::from_secs(3_600)), "1h 0m");
    assert_eq!(super::fmt_duration(Duration::from_secs(3_780)), "1h 3m");
}

/// The cadence is deliberately NON-catch-up: crossing many boundaries at
/// once yields one marker, not one per boundary. A turn that blocks for an
/// hour inside a single tool call must not return with twelve stamps at
/// exactly the moment the operator is trying to read what happened.
#[test]
fn the_cadence_boundary_is_shared_and_does_not_catch_up() {
    use std::time::Duration;

    // Off is off — this is the piped/headless posture and the default.
    assert_eq!(
        super::cadence_boundary(Duration::from_secs(9_999), Duration::ZERO),
        None
    );

    // Boundaries are intervals elapsed, so a caller comparing against what
    // it last announced advances by ONE however far the clock jumped.
    let five = Duration::from_secs(300);
    assert_eq!(
        super::cadence_boundary(Duration::from_secs(0), five),
        Some(0)
    );
    assert_eq!(
        super::cadence_boundary(Duration::from_secs(299), five),
        Some(0),
        "still inside the first interval"
    );
    assert_eq!(
        super::cadence_boundary(Duration::from_secs(300), five),
        Some(1)
    );
    assert_eq!(
        super::cadence_boundary(Duration::from_secs(3_600), five),
        Some(12),
        "an hour is one boundary number, not twelve emissions"
    );

    // A sub-second interval cannot divide by zero.
    assert_eq!(
        super::cadence_boundary(Duration::from_secs(7), Duration::from_millis(1)),
        Some(7)
    );
}

/// The marker must carry its meaning with `color: false`, because that is
/// the piped case — a bare grey `14:32` would read as output. Brackets are
/// already this transcript's timestamp vocabulary (the turn echo commits
/// `[YYYY-MM-DD HH:MM:SS]`), and they survive a monochrome capture.
#[test]
fn the_marker_is_a_bracketed_stamp_not_a_rule() {
    let line = super::time_marker_line("14:32");
    assert_eq!(line, "[14:32]");
    assert!(
        !line.contains('─') && !line.contains('-'),
        "no rule: a full-width one is a word-wrap hazard and says nothing \
         the brackets do not"
    );
}

/// Off by default, and the header is byte-identical when it is off — which
/// is what keeps every exact-transcript golden green and the
/// piped/headless path stable.
#[test]
fn no_cadence_means_the_header_bytes_are_unchanged() {
    super::set_time_marker_secs(0);
    let mut display = super::ToolDisplay::new(Vec::new(), false, 80, 3, false);
    display.call("run_command", "ls -la");
    let committed = String::from_utf8(display.into_inner()).unwrap();
    let expected = format!(
        "{}\n",
        tool_call_lines("run_command", "ls -la", 80).join("\n")
    );
    assert_eq!(committed, expected, "no marker, no change");
    assert!(!committed.contains('['), "nothing stamped: {committed:?}");
}
