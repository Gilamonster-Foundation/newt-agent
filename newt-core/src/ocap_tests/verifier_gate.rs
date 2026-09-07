use super::*;

#[test]
fn b1_verifier_absent_until_built() {
    // b1 remains fail-closed until the kernel floor lands (P5).
    assert!(!verify_b1().is_verified());
    assert_eq!(verify_b1().deviation(), Some("b1-os-isolation"));
}

#[test]
fn b1floor_live_path_reports_network_egress_properties_unmet() {
    let floor = B1Floor::live_attacker_path();
    // The live run_command -> bridle-shell path now PROVES direct-egress denial
    // (seccomp, slice 2), so it is no longer in the unmet set; the first
    // genuinely-absent property is mediated-egress-only (the deferred broker).
    assert!(
        !floor.is_complete(),
        "the live path is not fully fenced yet"
    );
    assert!(
        !floor.unmet.contains(&B1Property::DirectNetworkEgressDenied),
        "direct-egress denial is proven on the live path now — it must not be unmet"
    );
    assert_eq!(
        floor.first_unmet(),
        Some(B1Property::MediatedEgressOnly),
        "the credential-floor broker (mediated-egress-only) is the first remaining gap"
    );
}

#[test]
fn verify_b1_names_an_unmet_property_not_a_mechanism() {
    // Honest-as-code: the reason names a semantic PROPERTY that is unproven on
    // the live path — never the old blanket "no OS sandbox", and it states
    // the floor is property-based (uid-ns/netns are not mandatory clauses).
    let v = verify_b1();
    assert!(!v.is_verified());
    assert_eq!(v.deviation(), Some("b1-os-isolation"));
    let Verification::Absent { reason, .. } = &v else {
        unreachable!("b1 is Absent")
    };
    assert!(
        reason.contains(B1Property::MediatedEgressOnly.name()),
        "reason should name the unmet credential-floor property (mediated-egress-only): {reason}"
    );
    assert!(
        !reason.contains("seccomp egress floor / mediated broker / fd hygiene"),
        "reason must not carry the stale 'seccomp floor not established' wording — direct \
         egress IS proven now: {reason}"
    );
    assert!(
        reason.contains("not a fixed uid-ns/netns stack"),
        "reason must state b1 is property-based, not mechanism-locked: {reason}"
    );
    assert!(
        !reason.contains("no OS sandbox"),
        "reason must be property-specific, not the old blanket constant: {reason}"
    );
}

#[test]
fn b1property_required_is_credential_safety_ordered_and_first_unmet_follows_it() {
    // Filesystem confinement is the base; the egress/broker/credential legs
    // come before the lifecycle + disclosure + fail-closed legs.
    assert_eq!(B1Property::REQUIRED[0], B1Property::FilesystemConfinement);
    assert_eq!(
        B1Property::REQUIRED[1],
        B1Property::DirectNetworkEgressDenied
    );
    assert_eq!(B1Property::REQUIRED[2], B1Property::MediatedEgressOnly);
    assert_eq!(B1Property::REQUIRED[3], B1Property::CredentialIsolation);

    // A complete floor has no gap.
    let complete = B1Floor { unmet: vec![] };
    assert!(complete.is_complete());
    assert_eq!(complete.first_unmet(), None);

    // first_unmet follows REQUIRED order regardless of the Vec's order:
    // process-tree containment is last, so with only it missing it is named.
    let only_lifecycle = B1Floor {
        unmet: vec![B1Property::ProcessTreeContainment],
    };
    assert_eq!(
        only_lifecycle.first_unmet(),
        Some(B1Property::ProcessTreeContainment)
    );
    // With both egress denial and containment missing (Vec in reverse order),
    // the earlier-ordered egress-denial property is named first.
    let mixed = B1Floor {
        unmet: vec![
            B1Property::ProcessTreeContainment,
            B1Property::DirectNetworkEgressDenied,
        ],
    };
    assert_eq!(
        mixed.first_unmet(),
        Some(B1Property::DirectNetworkEgressDenied)
    );
}

/// A filter for probe tests. High-entropy, obviously synthetic.
fn probe_filter() -> DisclosureFilter {
    let mut f = DisclosureFilter::new();
    f.register("sk-probe-9f3a2b7c1d4e6a8b");
    f
}

/// The gate answers the question it is asked — "is the backstop working
/// HERE" — rather than restating a build-time belief.
///
/// This one test walks all three states in sequence because the contrast
/// IS the assertion: the pre-fix implementation returned `Verified` in all
/// three, so any test that exercised only one of them would have passed
/// against a function that never looked at anything.
#[test]
fn the_gate_distinguishes_the_three_backstop_states() {
    // 1. Nothing installed → the identity function → refuse to claim.
    assert!(
        !verify_disclosure_gate().is_verified(),
        "with no filter on this thread, redact_session_ingress is identity"
    );
    assert_eq!(
        verify_disclosure_gate().deviation(),
        Some("disclosure-gate-live-path")
    );

    // 2. Installed but registering nothing → catches nothing → refuse.
    {
        let _g = scoped_session_disclosure(DisclosureFilter::new());
        assert!(!verify_disclosure_gate().is_verified());
    }

    // 3. Installed and provably redacting → verified, no deviation.
    {
        let _g = scoped_session_disclosure(probe_filter());
        let v = verify_disclosure_gate();
        assert!(v.is_verified(), "{v:?}");
        assert_eq!(v.deviation(), None);
    }

    // And the guard restores: back to state 1.
    assert!(!verify_disclosure_gate().is_verified());
}

/// The regression this fix exists for.
///
/// `verify_disclosure_gate` used to return `Verified` unconditionally,
/// with evidence text that explicitly cited the `scoped_session_disclosure`
/// TLS backstop — while performing no check at all. It would have reported
/// exactly the same thing if the backstop had never been installed
/// anywhere, which is the definition of a vacuous check.
#[test]
fn the_gate_refuses_to_claim_a_backstop_that_is_not_installed() {
    let v = verify_disclosure_gate();
    assert!(!v.is_verified());
    let reason = match &v {
        Verification::Absent { reason, .. } => reason.clone(),
        Verification::Verified { .. } => unreachable!("just asserted not verified"),
    };
    assert!(
        reason.contains("identity function"),
        "the reason must name the actual consequence, got: {reason}"
    );
}

/// The property that protects the concurrency work (#1669 / the cockpit
/// train): `ScopedSessionDisclosure` is `!Send` and thread-bound, so a
/// turn that migrates onto a thread which never installed the guard loses
/// value-filtering on the memory / observation / compaction / spill path.
/// The gate must report that, not inherit the installing thread's claim.
#[test]
fn the_backstop_does_not_follow_the_gate_onto_another_thread() {
    let _g = scoped_session_disclosure(probe_filter());
    assert!(
        verify_disclosure_gate().is_verified(),
        "installed on this thread"
    );

    let elsewhere = std::thread::spawn(|| verify_disclosure_gate().is_verified())
        .join()
        .expect("probe thread");
    assert!(
        !elsewhere,
        "the TLS backstop is per-thread — the gate must never claim it on a \
         thread that did not install it"
    );
}

/// The probe runs the real machinery, so it tracks `redact`'s documented
/// post-condition across every encoding rather than spot-checking the raw
/// form. A filter that only caught the raw value would not verify.
#[test]
fn the_probe_covers_every_tracked_encoding() {
    let f = probe_filter();
    assert!(f.redacts_what_it_registered());
    // Non-vacuous control: the same predicate is false when there is
    // nothing registered, so it is reading state rather than returning a
    // constant.
    assert!(!DisclosureFilter::new().redacts_what_it_registered());
    assert!(DisclosureFilter::new().is_empty());
    assert!(!f.is_empty());
}

#[test]
fn verified_reports_no_deviation() {
    let v = Verification::Verified {
        evidence: "synthetic".into(),
    };
    assert!(v.is_verified());
    assert_eq!(v.deviation(), None);
}

#[test]
fn network_confinement_is_the_basic_floor_not_the_credential_floor() {
    // The split: the basic network floor is Verified where the seccomp +
    // Landlock egress floor is enforceable, while the credential-bearing b1
    // floor stays Absent (so credential-seeding gates keep failing closed).
    let net = verify_network_confinement();
    if crate::confined_exec::kernel_fs_fence_available() {
        assert!(
            net.is_verified(),
            "basic network confinement should be enforced here"
        );
        assert_eq!(net.deviation(), None);
    } else {
        assert!(!net.is_verified());
        assert_eq!(net.deviation(), Some("b1-os-isolation"));
    }
    // The stronger credential-bearing floor is independent and still open.
    assert!(!verify_b1().is_verified());
    assert_eq!(verify_b1().deviation(), Some("b1-os-isolation"));
}

#[test]
fn seed_live_credential_fails_closed_on_b1() {
    let cred = ScopedCredential {
        label: "pa-token".into(),
    };
    let err = seed_live_credential(&cred).unwrap_err();
    assert_eq!(err.deviation, "b1-os-isolation");
    assert!(err.to_string().contains("fail-closed"));
}

#[test]
fn admit_untrusted_remote_fails_closed() {
    let err = admit_untrusted_remote("SHA256:deadbeef").unwrap_err();
    assert_eq!(err.deviation, "b1-os-isolation");
}

#[test]
fn require_passes_only_when_verified() {
    assert!(require(Verification::Verified {
        evidence: "ok".into()
    })
    .is_ok());
    assert!(require(verify_b1()).is_err());
}
