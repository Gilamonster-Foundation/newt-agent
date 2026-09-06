use super::*;

#[test]
fn windows_wait_decision_only_treats_signalled_as_exited() {
    const WAIT_OBJECT_0_FIXTURE: u32 = 0;
    const WAIT_TIMEOUT_FIXTURE: u32 = 258;
    const WAIT_FAILED_FIXTURE: u32 = u32::MAX;

    assert!(wait_probe_reports_live_or_unknown(
        WAIT_TIMEOUT_FIXTURE,
        WAIT_OBJECT_0_FIXTURE
    ));
    assert!(!wait_probe_reports_live_or_unknown(
        WAIT_OBJECT_0_FIXTURE,
        WAIT_OBJECT_0_FIXTURE
    ));
    assert!(wait_probe_reports_live_or_unknown(
        WAIT_FAILED_FIXTURE,
        WAIT_OBJECT_0_FIXTURE
    ));
    assert!(wait_probe_reports_live_or_unknown(
        123_456,
        WAIT_OBJECT_0_FIXTURE
    ));
}

#[test]
fn windows_open_failure_only_treats_invalid_parameter_as_absent() {
    const ERROR_ACCESS_DENIED_FIXTURE: i32 = 5;
    const ERROR_INVALID_PARAMETER_FIXTURE: i32 = 87;

    assert!(!open_process_failure_reports_live_or_unknown(
        Some(ERROR_INVALID_PARAMETER_FIXTURE),
        ERROR_INVALID_PARAMETER_FIXTURE
    ));
    assert!(open_process_failure_reports_live_or_unknown(
        Some(ERROR_ACCESS_DENIED_FIXTURE),
        ERROR_INVALID_PARAMETER_FIXTURE
    ));
    assert!(open_process_failure_reports_live_or_unknown(
        Some(1_234_567),
        ERROR_INVALID_PARAMETER_FIXTURE
    ));
    assert!(open_process_failure_reports_live_or_unknown(
        None,
        ERROR_INVALID_PARAMETER_FIXTURE
    ));
}

#[test]
fn a_pid_reused_after_its_owner_died_is_not_the_owner() {
    // #1721 regression. `pid_is_alive` answers "SOME process holds this
    // pid", not "OUR owner is still running" — and pid_max wraps in hours
    // on a busy box. A live owner heartbeats while it runs, so its start
    // time always PRECEDES its own last heartbeat; a process that started
    // AFTER that heartbeat provably inherited the pid and is an impostor.
    const HEARTBEAT: i64 = 1_000;

    // The genuine owner: started before it last heartbeat.
    assert!(pid_identity_matches(Some(HEARTBEAT - 1), HEARTBEAT));
    // Boundary: starting exactly at the heartbeat is still the owner.
    assert!(pid_identity_matches(Some(HEARTBEAT), HEARTBEAT));

    // An impostor that took the pid after the owner's last heartbeat —
    // the case that today wedges a dead session's conversation as HeldBy.
    assert!(!pid_identity_matches(Some(HEARTBEAT + 1), HEARTBEAT));

    // An unreadable start time must fail CLOSED (judged the owner), so a
    // missing/racy /proc entry can never cause a wrongful reclaim.
    assert!(pid_identity_matches(None, HEARTBEAT));
}

/// GROUNDS `a_pid_reused_after_its_owner_died_is_not_the_owner` (#1721).
///
/// That test is pure — it asserts the DECISION given a start time. It cannot
/// tell whether `pid_start_unix_nanos` really produces a unix-epoch value on
/// the same scale as `now_claim_nanos`; if the two used different epochs the
/// comparison would be nonsense and the pure test would still pass. This
/// reads real `/proc` for the running process to prove the scales agree.
#[cfg(target_os = "linux")]
#[test]
fn pid_start_time_is_unix_nanos_on_the_same_scale_as_the_claim_clock() {
    let now = now_claim_nanos();
    let started = pid_start_unix_nanos(i64::from(std::process::id()))
        .expect("this process's own /proc/<pid>/stat is readable");

    // Our own start time is in the past...
    assert!(
        started <= now,
        "start {started} must not be after now {now}"
    );
    // ...and recent: a test binary is not days old. This is the assertion
    // that would fail loudly on an epoch/unit mismatch (a boot-relative or
    // seconds-scale value lands wildly outside this window).
    const ONE_DAY_NANOS: i64 = 24 * 3_600 * 1_000_000_000;
    assert!(
        now - started < ONE_DAY_NANOS,
        "start {started} implausibly far before now {now}"
    );

    // The decision function must therefore judge this LIVE process the owner
    // of a claim it heartbeat just now — the property #1721 depends on.
    assert!(pid_identity_matches(Some(started), now));
}
