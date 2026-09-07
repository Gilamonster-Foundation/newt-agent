use super::*;

/// Evidence with everything Verified — for proving the ceiling still wins.
fn all_verified() -> RuntimeEvidence {
    let v = || Verification::Verified {
        evidence: "synthetic".into(),
    };
    RuntimeEvidence {
        b1: v(),
        disclosure: v(),
        fs_object_bound: v(),
        constrained_executor: v(),
        fail_closed: v(),
    }
}

#[test]
fn linux_report_matches_live_verifier_state() {
    // Reporting derives from the SAME verifiers the gates use (#11).
    // Disclosure (+ fs/fail-closed on Linux) report enforced; EnvIsolation and
    // NetworkConfinement track their own verifiers (Enforced where the kernel
    // fence is available); process / credential still name b1 (their meet
    // includes the open credential-bearing b1 half).
    // Disclosure filtering is now a LIVE, per-thread probe, so the report
    // tracks it in both directions rather than restating a constant.
    let bare = SecurityReport::from_parts(&LINUX_CEILING, &RuntimeEvidence::current());
    assert!(
        matches!(
            bare.achieved(Guarantee::DisclosureFiltering),
            Achieved::Unverified {
                deviation: "disclosure-gate-live-path",
                ..
            }
        ),
        "no session filter on this thread ⇒ the row must be honest"
    );
    {
        let mut f = DisclosureFilter::new();
        f.register("sk-probe-9f3a2b7c1d4e6a8b");
        let _g = scoped_session_disclosure(f);
        let live = SecurityReport::from_parts(&LINUX_CEILING, &RuntimeEvidence::current());
        assert!(matches!(
            live.achieved(Guarantee::DisclosureFiltering),
            Achieved::Enforced { .. }
        ));
    }
    let report = bare;
    // NetworkConfinement still names the full credential-bearing b1 floor
    // (the seccomp egress deny is opt-in and does not cover run_command).
    assert!(matches!(
        report.achieved(Guarantee::NetworkConfinement),
        Achieved::Unverified {
            deviation: "b1-os-isolation",
            ..
        }
    ));
    // EnvIsolation is the executor's own single-half guarantee: Enforced when
    // the kernel fence is available on this host, else fail-closed Absent.
    if crate::confined_exec::kernel_fs_fence_available() {
        assert!(matches!(
            report.achieved(Guarantee::EnvIsolation),
            Achieved::Enforced { .. }
        ));
    } else {
        assert!(matches!(
            report.achieved(Guarantee::EnvIsolation),
            Achieved::Unverified {
                deviation: "p4-constrained-executor",
                ..
            }
        ));
    }
    // Process + credential confinement take the meet with the still-open b1,
    // so they stay Unverified regardless of the executor half.
    assert!(matches!(
        report.achieved(Guarantee::ProcessConfinement),
        Achieved::Unverified { .. }
    ));
    assert!(matches!(
        report.achieved(Guarantee::CredentialIsolation),
        Achieved::Unverified { .. }
    ));
}

#[test]
fn ceiling_never_rounds_up() {
    // Adversarial #12: even with EVERY runtime verifier Verified, a
    // platform whose ceiling says "cannot provide" must report Unsupported
    // — an unsupported platform can never claim Linux-equivalent OCAP.
    for ceiling in [&MACOS_CEILING, &WINDOWS_CEILING] {
        let report = SecurityReport::from_parts(ceiling, &all_verified());
        for g in [
            Guarantee::FsConfinement,
            Guarantee::ProcessConfinement,
            Guarantee::NetworkConfinement,
            Guarantee::CredentialIsolation,
        ] {
            assert!(
                matches!(report.achieved(g), Achieved::Unsupported { .. }),
                "{} must be Unsupported on {}",
                g.label(),
                ceiling.platform
            );
        }
    }
}

#[test]
fn unknown_platform_is_fully_unsupported() {
    // The default arm of the platform axis is the MOST restrictive.
    let report = SecurityReport::from_parts(&UNKNOWN_CEILING, &all_verified());
    for g in Guarantee::ALL {
        assert!(
            matches!(report.achieved(g), Achieved::Unsupported { .. }),
            "{} must be Unsupported on unknown platforms",
            g.label()
        );
    }
}

#[test]
fn require_achieved_refuses_unverified_and_unsupported() {
    // The refusal primitive: Enforced proceeds; everything else refuses.
    // Mock the evidence (like `compound_guarantees_take_the_meet`) rather than
    // the live `RuntimeEvidence::current()` host probe: `verify_constrained_executor`
    // is now honest (Verified only when `kernel_fs_fence_available()`), so off
    // Linux the `meet(constrained_executor, b1)` half flips to
    // `p4-constrained-executor` and this deviation assertion would be
    // platform-dependent. With `constrained_executor` synthetically Verified,
    // b1 is the only Absent half, so the deviation is `b1` everywhere.
    let mut ev = all_verified();
    ev.b1 = verify_b1();
    let linux = SecurityReport::from_parts(&LINUX_CEILING, &ev);
    assert!(require_achieved(&linux, Guarantee::DisclosureFiltering).is_ok());
    let err = require_achieved(&linux, Guarantee::CredentialIsolation).unwrap_err();
    assert_eq!(err.deviation, "b1-os-isolation");

    let mac = SecurityReport::from_parts(&MACOS_CEILING, &all_verified());
    let err = require_achieved(&mac, Guarantee::FsConfinement).unwrap_err();
    assert_eq!(err.deviation, "platform-unsupported");
    assert!(err.to_string().contains("fail-closed"));
}

#[test]
fn compound_guarantees_take_the_meet() {
    // Credential isolation needs BOTH the executor and b1: one Absent half
    // keeps the guarantee Unverified even when the other half is Verified.
    let mut ev = all_verified();
    ev.b1 = verify_b1(); // Absent
    let report = SecurityReport::from_parts(&LINUX_CEILING, &ev);
    assert!(matches!(
        report.achieved(Guarantee::CredentialIsolation),
        Achieved::Unverified {
            deviation: "b1-os-isolation",
            ..
        }
    ));
    // And with both halves Verified it is enforced.
    let report = SecurityReport::from_parts(&LINUX_CEILING, &all_verified());
    assert!(report.is_enforced(Guarantee::CredentialIsolation));
}

#[test]
fn summary_lines_cover_every_guarantee_honestly() {
    let report = SecurityReport::from_parts(&LINUX_CEILING, &RuntimeEvidence::current());
    let lines = report.summary_lines();
    assert_eq!(lines.len(), Guarantee::ALL.len());
    // Honest means honest in both directions: outside a live turn there is
    // no thread-local backstop, and the summary says so rather than
    // reporting a filter that is not there.
    assert!(lines
        .iter()
        .any(|l| l.contains("disclosure-filtering: OPEN (disclosure-gate-live-path)")));
    {
        let mut f = DisclosureFilter::new();
        f.register("sk-probe-9f3a2b7c1d4e6a8b");
        let _g = scoped_session_disclosure(f);
        let live = SecurityReport::from_parts(&LINUX_CEILING, &RuntimeEvidence::current());
        assert!(live
            .summary_lines()
            .iter()
            .any(|l| l == "disclosure-filtering: enforced"));
    }
    // Network confinement still names the full credential-bearing b1 floor
    // (the seccomp egress deny is opt-in and does not cover run_command).
    assert!(lines
        .iter()
        .any(|l| l.contains("network-confinement: OPEN (b1-os-isolation)")));
}

#[test]
fn current_report_reflects_build_platform() {
    // On the Linux CI/dev platform the live report enforces fs +
    // fail-closed; on non-Linux builds those same rows must be honest
    // (Absent/Unsupported), never a silent Linux-equivalent claim.
    let report = SecurityReport::current();
    #[cfg(target_os = "linux")]
    {
        assert_eq!(report.platform, "linux");
        assert!(report.is_enforced(Guarantee::FsConfinement));
        assert!(report.is_enforced(Guarantee::FailClosedExecution));
    }
    #[cfg(not(target_os = "linux"))]
    {
        assert_ne!(report.platform, "linux");
        assert!(!report.is_enforced(Guarantee::FsConfinement));
        assert!(!report.is_enforced(Guarantee::FailClosedExecution));
    }
    // Everywhere: disclosure filtering is a per-THREAD probe, not a
    // process-wide constant. `SecurityReport::current()` runs on a thread
    // with no session filter installed, so the honest answer is that the
    // backstop is not in effect here.
    assert!(!report.is_enforced(Guarantee::DisclosureFiltering));
}
