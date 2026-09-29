//! #2274 admission regressions with explicitly synthetic backend projections.
//!
//! Native old-0.8 red and candidate evidence tests live in the companion native
//! target. These lattice fixtures do not assert native policy application.

use agent_bridle_core::{
    empty_closure, enforcement_report, AdmittedFence, AxisEnforcement, BackendProjection, Caveats,
    ChildNetworkPolicy, ConfinementMechanism, EnforcementFloor, ResolvedAuthority, ResolvedScope,
    RuntimeClosure, SandboxKind, Scope,
};

const ROOT: &str = if cfg!(windows) {
    r"C:\tools\one\cargo.exe"
} else {
    "/tools/one/cargo"
};
const SIBLING: &str = if cfg!(windows) {
    r"C:\tools\two\cargo.exe"
} else {
    "/tools/two/cargo"
};
const PARENT_ROOT: &str = if cfg!(windows) {
    r"C:\tools\link\..\cargo.exe"
} else {
    "/tools/link/../cargo"
};

fn protected_roots() -> std::collections::BTreeSet<String> {
    [synthetic_path("bridle-admission-private/state")].into()
}

fn synthetic_path(tail: &str) -> String {
    std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(tail)
        .display()
        .to_string()
}

fn grant() -> Caveats {
    Caveats {
        fs_read: Scope::only(["/workspace".to_owned()]),
        fs_write: Scope::only(["/workspace".to_owned()]),
        exec: Scope::only([ROOT.to_owned()]),
        // Most synthetic comparisons need no network restriction/floor.
        // The net scope refusal tests below deliberately restrict this axis.
        net: Scope::All,
        ..Caveats::top()
    }
}

fn synthetic_tree_projection(caveats: &Caveats) -> BackendProjection {
    // This is a TEST fixture, not permission to replace the production
    // backend projection with a lift of the declared caveats.
    let mut resolved = ResolvedAuthority::from_delegated(caveats);
    resolved.exec = ResolvedScope::Unbounded;
    BackendProjection {
        resolved,
        runtime_closure: empty_closure(),
    }
}

fn named_mechanism() -> ConfinementMechanism {
    ConfinementMechanism::for_named_root(SandboxKind::Landlock, ChildNetworkPolicy::DenyDirect)
}

fn named_admission(caveats: &Caveats) -> AdmittedFence {
    AdmittedFence::admit_named_root(
        caveats,
        ROOT,
        &protected_roots(),
        named_mechanism(),
        EnforcementFloor::CONFINED,
        synthetic_tree_projection,
    )
    .expect("the explicit root contract admits the synthetic control")
}

#[test]
fn ordinary_admission_keeps_its_scope_obligation_and_interceptor_report() {
    let caveats = grant();
    let mechanism =
        ConfinementMechanism::new(SandboxKind::Landlock, ChildNetworkPolicy::DenyDirect);
    let report = enforcement_report(&caveats, mechanism);
    assert_eq!(report.exec, Some(AxisEnforcement::Interceptor));
    let result = AdmittedFence::admit(
        &caveats,
        RuntimeClosure::empty(),
        mechanism,
        EnforcementFloor::CONFINED,
        synthetic_tree_projection,
    );
    assert!(
        result.is_err(),
        "the default process-tree scope obligation remains"
    );
    // This does not claim a complete native executable-identity fence.
}

#[test]
fn named_root_admission_is_explicit_and_does_not_rewrite_caveats() {
    let caveats = grant();
    let admitted = named_admission(&caveats);
    assert_eq!(admitted.mechanism_caveats(), &caveats);
    assert_eq!(
        enforcement_report(&caveats, named_mechanism()).exec,
        Some(AxisEnforcement::Interceptor)
    );
    admitted
        .verify_named_root_applied(
            &caveats,
            ROOT,
            &protected_roots(),
            named_mechanism(),
            synthetic_tree_projection(&caveats),
        )
        .expect("a separately derived matching synthetic application verifies");
}

#[test]
fn exact_root_admission_refuses_ungranted_and_invalid_roots() {
    assert!(
        AdmittedFence::admit_named_root(
            &grant(),
            SIBLING,
            &protected_roots(),
            named_mechanism(),
            EnforcementFloor::CONFINED,
            synthetic_tree_projection,
        )
        .is_err(),
        "a same-basename sibling is not the granted root"
    );

    // Matching the raw entry cannot rescue an invalid absolute-root spelling.
    for root in ["", "cargo", "./cargo", "../cargo", PARENT_ROOT] {
        let mut caveats = grant();
        caveats.exec = Scope::only([root.to_owned()]);
        assert!(
            AdmittedFence::admit_named_root(
                &caveats,
                root,
                &protected_roots(),
                named_mechanism(),
                EnforcementFloor::CONFINED,
                synthetic_tree_projection,
            )
            .is_err(),
            "invalid root must refuse even with an exact raw entry: {root:?}"
        );
    }
    for scope in [Scope::none(), Scope::All] {
        let mut caveats = grant();
        caveats.exec = scope;
        assert!(
            AdmittedFence::admit_named_root(
                &caveats,
                ROOT,
                &protected_roots(),
                named_mechanism(),
                EnforcementFloor::CONFINED,
                synthetic_tree_projection,
            )
            .is_err(),
            "NamedRoot requires an explicit exact Only grant"
        );
    }
}

fn assert_projection_refuses_for(
    caveats: &Caveats,
    projection: BackendProjection,
    axis: &str,
    relation: &str,
) {
    let error = AdmittedFence::admit_named_root(
        caveats,
        ROOT,
        &protected_roots(),
        named_mechanism(),
        EnforcementFloor::CONFINED,
        |_| projection,
    )
    .expect_err("the changed synthetic projection must refuse");
    // A floor failure or unrelated denial is not evidence for the scope test.
    let reason = error.to_string().to_ascii_lowercase().replace('_', "");
    assert!(reason.contains(axis), "wrong refused axis: {reason}");
    assert!(
        reason.contains(relation),
        "wrong refused relation: {reason}"
    );
}

#[test]
fn each_inherited_axis_refuses_widening() {
    let caveats = grant();
    let _control = named_admission(&caveats);
    for axis in ["fsread", "fswrite", "net"] {
        let mut delegated = caveats.clone();
        if axis == "net" {
            delegated.net = Scope::none();
        }
        let mut projection = synthetic_tree_projection(&delegated);
        match axis {
            "fsread" => projection.resolved.fs_read = ResolvedScope::Unbounded,
            "fswrite" => projection.resolved.fs_write = ResolvedScope::Unbounded,
            "net" => projection.resolved.net = ResolvedScope::Unbounded,
            _ => unreachable!(),
        }
        assert_projection_refuses_for(&delegated, projection, axis, "widen");
    }
}

#[test]
fn unknown_on_every_inherited_axis_or_descendant_exec_refuses() {
    for axis in ["fsread", "fswrite", "net", "exec"] {
        let mut projection = synthetic_tree_projection(&grant());
        match axis {
            "fsread" => projection.resolved.fs_read = ResolvedScope::Unknown,
            "fswrite" => projection.resolved.fs_write = ResolvedScope::Unknown,
            "net" => projection.resolved.net = ResolvedScope::Unknown,
            "exec" => projection.resolved.exec = ResolvedScope::Unknown,
            _ => unreachable!(),
        }
        assert_projection_refuses_for(&grant(), projection, axis, "unknown");
    }
}

#[test]
fn runtime_closure_cannot_hide_recorded_private_unbounded_or_unknown_authority() {
    for axis in ["fsread", "fswrite", "exec"] {
        for scope in [
            ResolvedScope::Unbounded,
            ResolvedScope::Unknown,
            ResolvedScope::concrete(protected_roots()),
        ] {
            let mut projection = synthetic_tree_projection(&grant());
            match axis {
                "fsread" => projection.runtime_closure.fs_read = scope,
                "fswrite" => projection.runtime_closure.fs_write = scope,
                "exec" => projection.runtime_closure.exec = scope,
                _ => unreachable!(),
            }
            let error = AdmittedFence::admit_named_root(
                &grant(),
                ROOT,
                &protected_roots(),
                named_mechanism(),
                EnforcementFloor::CONFINED,
                |_| projection,
            )
            .expect_err("NamedRoot cannot launder authority through its closure");
            assert!(
                error.to_string().contains("closure"),
                "wrong refusal: {error}"
            );
        }
    }
}

#[test]
fn runtime_closure_cannot_hide_unknown_or_unbounded_network_authority() {
    for scope in [ResolvedScope::Unbounded, ResolvedScope::Unknown] {
        let mut caveats = grant();
        caveats.net = Scope::none();
        let mut projection = synthetic_tree_projection(&caveats);
        projection.runtime_closure.net = scope;
        let error = AdmittedFence::admit_named_root(
            &caveats,
            ROOT,
            &protected_roots(),
            named_mechanism(),
            EnforcementFloor::CONFINED,
            |_| projection,
        )
        .expect_err("an unbounded or unknown net closure cannot justify the fence");
        assert!(
            error.to_string().contains("closure"),
            "wrong refusal: {error}"
        );
    }
}

#[test]
fn ancestor_read_and_write_closures_cannot_claim_harness_disjointness() {
    let caveats = grant();
    // Finite system substrate is the positive control. The projection and
    // closure are synthetic, not an assertion of native policy application.
    let mut control = synthetic_tree_projection(&caveats);
    let library = ResolvedScope::concrete([synthetic_path("bridle-admission-runtime")]);
    control.resolved.fs_read = control.resolved.fs_read.union(&library);
    control.runtime_closure.fs_read = library;
    AdmittedFence::admit_named_root(
        &caveats,
        ROOT,
        &protected_roots(),
        named_mechanism(),
        EnforcementFloor::CONFINED,
        |_| control,
    )
    .unwrap();
    for axis in ["fs_read", "fs_write"] {
        let private = std::path::PathBuf::from(synthetic_path("bridle-admission-private"));
        let ancestors = [
            private.ancestors().last().unwrap().to_path_buf(),
            private.parent().unwrap().to_path_buf(),
            private.clone(),
        ];
        for ancestor in ancestors {
            let mut projection = synthetic_tree_projection(&caveats);
            let scope = ResolvedScope::concrete([ancestor.display().to_string()]);
            if axis == "fs_read" {
                projection.resolved.fs_read = projection.resolved.fs_read.union(&scope);
                projection.runtime_closure.fs_read = scope;
            } else {
                projection.resolved.fs_write = projection.resolved.fs_write.union(&scope);
                projection.runtime_closure.fs_write = scope;
            }
            assert!(AdmittedFence::admit_named_root(&caveats, ROOT, &protected_roots(), named_mechanism(),
                EnforcementFloor::CONFINED, |_| projection).is_err(),
                "{axis} closure {} can enclose recorded private stores; marker absence is not a proof", ancestor.display());
        }
    }
}

#[test]
fn wrong_mode_missing_backend_and_kernel_exec_floor_refuse() {
    for mechanism in [
        ConfinementMechanism::new(SandboxKind::Landlock, ChildNetworkPolicy::DenyDirect),
        ConfinementMechanism::for_named_root(SandboxKind::None, ChildNetworkPolicy::DenyDirect),
    ] {
        assert!(
            AdmittedFence::admit_named_root(
                &grant(),
                ROOT,
                &protected_roots(),
                mechanism,
                EnforcementFloor::CONFINED,
                synthetic_tree_projection,
            )
            .is_err(),
            "named-root admission requires its explicit supported mechanism"
        );
    }
    assert!(
        AdmittedFence::admit_named_root(
            &grant(),
            ROOT,
            &protected_roots(),
            named_mechanism(),
            EnforcementFloor::from_scalar(AxisEnforcement::Kernel),
            synthetic_tree_projection,
        )
        .is_err(),
        "root admission cannot satisfy a Kernel exec requirement"
    );
}

#[test]
fn a_weak_caller_floor_cannot_remove_named_roots_confined_floor() {
    let mut caveats = grant();
    caveats.net = Scope::none();
    let weak = EnforcementFloor::from_scalar(AxisEnforcement::Advisory);
    AdmittedFence::admit_named_root(
        &caveats,
        ROOT,
        &protected_roots(),
        named_mechanism(),
        weak,
        synthetic_tree_projection,
    )
    .unwrap();
    let advisory_net = ConfinementMechanism::for_named_root(
        SandboxKind::Landlock,
        ChildNetworkPolicy::LandlockOnly,
    );
    let error = AdmittedFence::admit_named_root(
        &caveats,
        ROOT,
        &protected_roots(),
        advisory_net,
        weak,
        synthetic_tree_projection,
    )
    .unwrap_err();
    assert!(
        error.to_string().to_ascii_lowercase().contains("net"),
        "wrong refusal: {error}"
    );
}

#[test]
fn projector_receives_the_exact_effective_caveats_without_transient_widening() {
    let caveats = grant();
    let observed = std::cell::Cell::new(false);
    AdmittedFence::admit_named_root(
        &caveats,
        ROOT,
        &protected_roots(),
        named_mechanism(),
        EnforcementFloor::CONFINED,
        |actual| {
            assert_eq!(
                actual, &caveats,
                "the backend must never see a transient exec:All or another grant rewrite"
            );
            observed.set(true);
            synthetic_tree_projection(actual)
        },
    )
    .unwrap();
    assert!(observed.get());
}

#[test]
fn inherited_scope_accepts_subsets_and_refuses_incomparable_projections() {
    for axis in ["fs_read", "fs_write", "net"] {
        let mut caveats = grant();
        // Ambient network can conservatively resolve to a bounded subset.
        // Restricted hostname networking still has no native acceptance claim.
        let mut subset = synthetic_tree_projection(&caveats);
        match axis {
            "fs_read" => subset.resolved.fs_read = ResolvedScope::empty(),
            "fs_write" => subset.resolved.fs_write = ResolvedScope::empty(),
            "net" => subset.resolved.net = ResolvedScope::empty(),
            _ => unreachable!(),
        }
        AdmittedFence::admit_named_root(
            &caveats,
            ROOT,
            &protected_roots(),
            named_mechanism(),
            EnforcementFloor::CONFINED,
            |_| subset,
        )
        .unwrap();
        if axis == "net" {
            caveats.net = Scope::only(["permitted.example".to_owned()]);
        }
        let mut incomparable = synthetic_tree_projection(&caveats);
        let other = ResolvedScope::concrete(["ungranted-other".to_owned()]);
        match axis {
            "fs_read" => incomparable.resolved.fs_read = other,
            "fs_write" => incomparable.resolved.fs_write = other,
            "net" => incomparable.resolved.net = other,
            _ => unreachable!(),
        }
        let error = AdmittedFence::admit_named_root(
            &caveats,
            ROOT,
            &protected_roots(),
            named_mechanism(),
            EnforcementFloor::CONFINED,
            |_| incomparable,
        )
        .unwrap_err();
        let reason = error.to_string();
        assert!(
            reason.contains(axis) && reason.contains("Incomparable"),
            "{reason}"
        );
    }
}

#[test]
fn root_mode_and_caveats_are_bound_into_the_existing_fence_identity() {
    let mut caveats = grant();
    caveats.exec = Scope::only([ROOT.to_owned(), SIBLING.to_owned()]);
    let first = named_admission(&caveats);
    let second = AdmittedFence::admit_named_root(
        &caveats,
        SIBLING,
        &protected_roots(),
        named_mechanism(),
        EnforcementFloor::CONFINED,
        synthetic_tree_projection,
    )
    .unwrap();
    assert_ne!(first.fence_id(), second.fence_id());
    assert!(first
        .verify_named_root_applied(
            &caveats,
            SIBLING,
            &protected_roots(),
            named_mechanism(),
            synthetic_tree_projection(&caveats),
        )
        .is_err());
    assert!(first
        .verify_named_root_applied(
            &caveats,
            ROOT,
            &protected_roots(),
            ConfinementMechanism::new(SandboxKind::Landlock, ChildNetworkPolicy::DenyDirect),
            synthetic_tree_projection(&caveats),
        )
        .is_err());
    let mut widened = caveats.clone();
    widened.fs_write = Scope::All;
    assert!(first
        .verify_named_root_applied(
            &widened,
            ROOT,
            &protected_roots(),
            named_mechanism(),
            synthetic_tree_projection(&widened),
        )
        .is_err());
}

#[test]
fn fresh_applied_projection_substitution_cannot_reuse_cached_admission() {
    let caveats = grant();
    let admitted = named_admission(&caveats);
    let mut freshly_derived = synthetic_tree_projection(&caveats);
    freshly_derived.resolved.fs_write = ResolvedScope::Unbounded;
    assert!(
        admitted
            .verify_named_root_applied(
                &caveats,
                ROOT,
                &protected_roots(),
                named_mechanism(),
                freshly_derived
            )
            .is_err(),
        "unchanged root/mode/caveats cannot hide a changed backend projection"
    );
    let mut narrower = synthetic_tree_projection(&caveats);
    narrower.resolved.fs_read = ResolvedScope::empty();
    // This fresh projection passes every scope/floor/inventory check. Only
    // comparison with the ORIGINAL admitted CID can refuse its substitution.
    AdmittedFence::admit_named_root(
        &caveats,
        ROOT,
        &protected_roots(),
        named_mechanism(),
        EnforcementFloor::CONFINED,
        |_| narrower.clone(),
    )
    .unwrap();
    assert!(
        admitted
            .verify_named_root_applied(
                &caveats,
                ROOT,
                &protected_roots(),
                named_mechanism(),
                narrower
            )
            .is_err(),
        "a valid different projection still cannot reuse the original fence CID"
    );
}

// Real producers, native controls and consumer tampering are tested in the
// companion native target and production Brush harness. Prepared-Command and
// native-apply mutations have measured receipts. Newt Cargo/full gates remain
// integration acceptance work; these synthetic projections do not replace it.

// Model: gpt-6-astra | Harness: Codex 0.153.4 | Operator: Shawn Hartsock | Time: 22:29 UTC | Date: 2026-09-12
