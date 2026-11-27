use super::liveness::{boot_epoch, owner_coordinate};
use super::*;

#[test]
fn boot_epoch_subtracts_before_flooring_to_seconds() {
    use std::time::Duration;
    let real = Duration::new(100, 100);
    let boot = Duration::new(10, 200);
    assert_eq!(boot_epoch(Some(real), Some(boot)), Some(89));
    assert_eq!(boot_epoch(Some(real), Some(real)), Some(0));
    assert_eq!(boot_epoch(Some(real), Some(Duration::ZERO)), Some(100));
    // A later BOOTTIME read must only lower the estimate, including at a boundary.
    assert_eq!(
        boot_epoch(Some(real), Some(Duration::new(10, 100))),
        Some(90)
    );
    assert_eq!(boot_epoch(None, Some(boot)), None);
    assert_eq!(boot_epoch(Some(real), None), None);
    assert_eq!(boot_epoch(Some(boot), Some(real)), None);
    assert_eq!(boot_epoch(Some(Duration::MAX), Some(Duration::ZERO)), None);
    assert!(pid_identity_matches(boot_epoch(None, Some(boot)), 100));
}

#[test]
fn owner_coordinate_preserves_initial_and_scopes_private_namespaces() {
    assert_eq!(owner_coordinate("boot", Some("pid:[4026531836]")), "boot");
    assert_eq!(owner_coordinate("boot", None), "boot");
    assert_eq!(
        owner_coordinate("boot", Some("pid:[4026532001]")),
        "boot;pid:[4026532001]"
    );
    assert_eq!(
        owner_coordinate("", Some("pid:[4026532001]")),
        ";pid:[4026532001]"
    );
    // Only the exact initial link is exempt; PID 1 in a private namespace is not initial.
    assert_ne!(owner_coordinate("boot", Some("pid:[1]")), "boot");
}

fn memory_owner(boot_id: String) -> ConversationStore {
    let conn = Connection::open_in_memory().unwrap();
    create_schema(&conn).unwrap();
    ConversationStore {
        conn: Arc::new(Mutex::new(conn)),
        workspace: PathBuf::from("workspace"),
        workspace_id: "workspace-fixture".into(),
        writer_fingerprint: "writer-fixture".into(),
        max_per_workspace: 0,
        wal_fallback: None,
        claim_clock: || 100,
        host: "localhost".into(),
        boot_id,
        pid: 2,
        liveness: |_, _| true,
    }
}

#[test]
fn two_builds_with_the_same_local_pid_are_not_each_others_owner() {
    let first = memory_owner(owner_coordinate("", Some("pid:[4026532001]")));
    let second = ConversationStore {
        boot_id: owner_coordinate("", Some("pid:[4026532002]")),
        claim_clock: || 200,
        ..first.clone()
    };
    let id = "namespace-owner";
    assert_eq!(first.claim(id).unwrap(), ClaimOutcome::Claimed);
    let row = first.live_owner(id).unwrap();
    assert_eq!(
        second.claim(id).unwrap(),
        ClaimOutcome::HeldBy {
            host: "localhost".into(),
            pid: 2
        }
    );
    second.heartbeat(id).unwrap();
    second.release(id).unwrap();
    assert_eq!(first.live_owner(id).unwrap(), row);
    first.release(id).unwrap();
    assert_eq!(second.claim(id).unwrap(), ClaimOutcome::Claimed);
    assert!(matches!(
        first.claim(id).unwrap(),
        ClaimOutcome::HeldBy { .. }
    ));
}

#[test]
fn older_unqualified_owner_rows_require_liveness_before_reclaim() {
    let old = memory_owner("boot".into());
    let mut current = ConversationStore {
        boot_id: owner_coordinate("boot", Some("pid:[4026532001]")),
        ..old.clone()
    };
    let id = "older-owner";
    assert_eq!(old.claim(id).unwrap(), ClaimOutcome::Claimed);
    assert!(matches!(
        current.claim(id).unwrap(),
        ClaimOutcome::HeldBy { .. }
    ));
    current.set_liveness_for_test(|_, _| false);
    assert_eq!(current.claim(id).unwrap(), ClaimOutcome::Claimed);
    assert_eq!(
        current.live_owner(id).unwrap().unwrap().boot_id,
        current.boot_id
    );
}

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
/// reads real `/proc` for the running process to prove the scales agree, and
/// grounds `boot_epoch_subtracts_before_flooring_to_seconds` against the kernel's
/// whole-second `btime` when global procfs is available.
#[cfg(target_os = "linux")]
#[test]
fn pid_start_time_is_unix_nanos_on_the_same_scale_as_the_claim_clock() {
    use super::liveness::kernel_clock;

    // btime exposes only tv_sec. Bracket the reference read with both clocks:
    // scheduling delays widen the measured interval, not an arbitrary wall-time
    // tolerance. Flooring its endpoints compares the kernel's one-second bins.
    // The production pair uses REALTIME first and BOOTTIME second (lower bound).
    let boot_before = kernel_clock(libc::CLOCK_BOOTTIME).unwrap();
    let real_before = kernel_clock(libc::CLOCK_REALTIME).unwrap();
    let reference = std::fs::read_to_string("/proc/stat");
    let real_after = kernel_clock(libc::CLOCK_REALTIME).unwrap();
    let boot_after = kernel_clock(libc::CLOCK_BOOTTIME).unwrap();
    if let Ok(stat) = reference {
        let btime: i64 = stat
            .lines()
            .find_map(|line| line.strip_prefix("btime "))
            .expect("readable global procfs supplies btime")
            .trim()
            .parse()
            .unwrap();
        let earliest = boot_epoch(Some(real_before), Some(boot_after)).unwrap();
        let latest = boot_epoch(Some(real_after), Some(boot_before)).unwrap();
        assert!(
            (earliest..=latest).contains(&btime),
            "clock-derived boot epoch [{earliest}, {latest}] disagrees with procfs btime {btime}"
        );
        eprintln!("clock/proc boot epoch: reference agrees within whole-second sampling bounds");
    }

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
