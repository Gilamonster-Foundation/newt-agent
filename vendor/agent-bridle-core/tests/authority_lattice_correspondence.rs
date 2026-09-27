//! Correspondence tests: the PRODUCTION authority algebra (agent-mesh-protocol,
//! re-exported by agent-bridle-core) behaves as the formal model claims.
//!
//!   formal model  ≈  production semantic implementation   (this file)
//!   production semantic implementation  ≈  actual OS enforcement
//!       (the native hostile-child tests — a separate, per-backend tier)
//!
//! Each test below names the Lean theorem (formal/Ceremony/Assurance/
//! AuthorityLattice.lean) and/or TLA+ invariant (formal/tla/AuthorityLifecycle.tla)
//! it grounds. A divergence here means the Rust semantics drifted from the proof.
//!
//! These are pure, in-process, fs/network-free — the correspondence is about the
//! lattice, not any kernel. The Windows write⇒read case is tested at the SEMANTIC
//! layer (ResolvedScope::union + admit), which is exactly what the AppContainer
//! projection (#338) composes; the native DACL evidence is the separate tier.

use std::collections::BTreeSet;

use agent_bridle_core::{
    admit, empty_closure, relate, AdmissionDecision, Caveats, ConfinedAxis, ResolvedAuthority,
    ResolvedScope, Scope, ScopeRelation,
};

fn bounded(items: &[&str]) -> ResolvedScope {
    ResolvedScope::concrete(items.iter().map(|s| s.to_string()))
}

/// A ResolvedAuthority that restricts only `fs_read`; the other axes are the
/// ambient top (`Unbounded`) so they never drive an admission decision here.
fn only_fs_read(fs_read: ResolvedScope) -> ResolvedAuthority {
    ResolvedAuthority {
        fs_read,
        fs_write: ResolvedScope::Unbounded,
        exec: ResolvedScope::Unbounded,
        net: ResolvedScope::Unbounded,
    }
}

/// Delegated caveats granting exactly `read` on fs_read (everything else top).
fn delegated_fs_read(read: &[&str]) -> Caveats {
    Caveats {
        fs_read: Scope::only(read.iter().map(|s| s.to_string())),
        ..Caveats::top()
    }
}

// ── L1 / orientation: `relate(resolved, bound)` asks "resolved ⊆ bound",
//    accepting Equal|Subset. Grounds Lean `orientation_*` and `within`. ───────

#[test]
fn relate_orientation_matches_within() {
    // narrower fence within wider bound → Subset (fine).
    assert_eq!(
        relate(&bounded(&["a"]), &bounded(&["a", "b"])),
        ScopeRelation::Subset
    );
    // wider fence vs narrower bound → Superset (the widening this layer catches).
    assert_eq!(
        relate(&bounded(&["a", "b"]), &bounded(&["a"])),
        ScopeRelation::Superset
    );
    // equal → Equal.
    assert_eq!(
        relate(&bounded(&["a"]), &bounded(&["a"])),
        ScopeRelation::Equal
    );
    // Unbounded fence vs bounded bound is a widening (mirrors Lean
    // `orientation_unbounded_not_within_bounded`).
    assert_eq!(
        relate(&ResolvedScope::Unbounded, &bounded(&["a"])),
        ScopeRelation::Superset
    );
    // bounded fence vs Unbounded bound is within (mirrors
    // `orientation_bounded_within_unbounded`).
    assert_eq!(
        relate(&bounded(&["a"]), &ResolvedScope::Unbounded),
        ScopeRelation::Subset
    );
}

// ── L1: union monotone, ∅ identity, and it EXPANDS. Grounds `within_union_left`
//    and `ResolvedScope::union`. ────────────────────────────────────────────

#[test]
fn union_expands_and_absorbs() {
    let read = ResolvedScope::from_scope(&Scope::only(["a".to_string()]));
    let write = ResolvedScope::from_scope(&Scope::only(["b".to_string()]));
    let u = read.union(&write);
    // a ⊆ a ∪ b : the read grant is within the union.
    assert_eq!(relate(&read, &u), ScopeRelation::Subset);
    // ∅ ∪ X = X (identity).
    assert_eq!(
        relate(&ResolvedScope::empty().union(&read), &read),
        ScopeRelation::Equal
    );
    // Unbounded absorbs.
    assert_eq!(
        read.union(&ResolvedScope::Unbounded),
        ResolvedScope::Unbounded
    );
}

// ── L6 / T4: Unknown fails closed. Grounds `unknown_never_admissible`,
//    `unknown_closure_never_admissible`, TLA T4. ─────────────────────────────

#[test]
fn unknown_fails_closed() {
    // relate involving Unknown on either side is Unknown.
    assert_eq!(
        relate(&ResolvedScope::Unknown, &bounded(&["a"])),
        ScopeRelation::Unknown
    );
    assert_eq!(
        relate(&bounded(&["a"]), &ResolvedScope::Unknown),
        ScopeRelation::Unknown
    );
    // an Unknown axis is never admitted.
    let resolved = only_fs_read(ResolvedScope::Unknown);
    match admit(&resolved, &delegated_fs_read(&["a"]), &empty_closure()) {
        AdmissionDecision::Reject(r) => assert_eq!(r.axis, ConfinedAxis::FsRead),
        AdmissionDecision::Admit => panic!("Unknown axis must fail closed, not admit"),
    }
}

// ── L2/L3 / T2/T3: admit accepts exactly resolved ⊆ delegated ∪ closure. ─────

#[test]
fn admit_admits_within_and_rejects_widening() {
    let delegated = delegated_fs_read(&["a"]);
    // resolved == delegated → Admit.
    let ok = only_fs_read(bounded(&["a"]));
    assert!(matches!(
        admit(&ok, &delegated, &empty_closure()),
        AdmissionDecision::Admit
    ));
    // resolved wider than delegated (adds "b"), empty closure → Reject on fs_read.
    let wide = only_fs_read(bounded(&["a", "b"]));
    match admit(&wide, &delegated, &empty_closure()) {
        AdmissionDecision::Reject(r) => {
            assert_eq!(r.axis, ConfinedAxis::FsRead);
            assert_eq!(r.relation, ScopeRelation::Superset);
        }
        AdmissionDecision::Admit => panic!("a fs_read widening must be refused"),
    }
}

// ── L5 / T2: the runtime closure — not a silent widening — is what makes an
//    extra resource admissible. Grounds `runtime_closure_not_hidden`. ─────────

#[test]
fn runtime_closure_is_the_only_way_extra_authority_admits() {
    let delegated = delegated_fs_read(&["a"]);
    // resolved needs "loader" which the grant does not name.
    let resolved = only_fs_read(bounded(&["a", "loader"]));
    // With no closure, that extra authority is a widening → Reject.
    assert!(matches!(
        admit(&resolved, &delegated, &empty_closure()),
        AdmissionDecision::Reject(_)
    ));
    // Declaring "loader" in the closure widens the BOUND visibly → Admit. The
    // authority is not hidden; it is accounted for in delegated ∪ closure.
    let closure = only_fs_read(bounded(&["loader"]));
    assert!(matches!(
        admit(&resolved, &delegated, &closure),
        AdmissionDecision::Admit
    ));
}

// ── L4 / #338: Windows write-implies-read, at the semantic layer the
//    AppContainer projection composes. Grounds
//    `windows_write_implies_read_widening` /
//    `windows_unrepresentable_narrowing_rejected`. ────────────────────────────

/// The Windows projection premise, expressed with the production union: the
/// resolved read axis is `delegated.read ∪ delegated.write` (the aclaunch DACL
/// grants read on every write path). This mirrors what #338's
/// `AppContainerSandbox::resolved_authority` will emit; the NATIVE DACL evidence
/// establishing that the premise actually holds is the separate tier.
fn windows_resolved_read(read: &[&str], write: &[&str]) -> ResolvedScope {
    ResolvedScope::from_scope(&Scope::only(read.iter().map(|s| s.to_string()))).union(
        &ResolvedScope::from_scope(&Scope::only(write.iter().map(|s| s.to_string()))),
    )
}

#[test]
fn windows_write_without_read_triggers_widening_and_is_rejected() {
    // write path "w" is NOT a read path → resolved read {r, w} ⊋ delegated read {r}.
    let resolved = only_fs_read(windows_resolved_read(&["r"], &["w"]));
    let delegated = delegated_fs_read(&["r"]);
    // The resolved read axis widens beyond the delegated read grant …
    assert_eq!(
        relate(
            &resolved.fs_read,
            &ResolvedAuthority::from_delegated(&delegated).fs_read
        ),
        ScopeRelation::Superset
    );
    // … so an exact/narrow admission (empty closure) REFUSES — #338's fail-closed
    // posture (Q3), matching Lean `windows_unrepresentable_narrowing_rejected`.
    match admit(&resolved, &delegated, &empty_closure()) {
        AdmissionDecision::Reject(r) => assert_eq!(r.axis, ConfinedAxis::FsRead),
        AdmissionDecision::Admit => panic!("write⊄read must be refused under an exact read bound"),
    }
}

#[test]
fn windows_write_subset_of_read_admits() {
    // when every write path IS already a read path, read ∪ write = read → Admit.
    let resolved = only_fs_read(windows_resolved_read(&["r", "w"], &["w"]));
    let delegated = delegated_fs_read(&["r", "w"]);
    assert!(matches!(
        admit(&resolved, &delegated, &empty_closure()),
        AdmissionDecision::Admit
    ));
}

/// Guard: the union really is set union (no dedup surprise, no dropped element)
/// — the concrete membership the widening argument rests on.
#[test]
fn windows_union_membership_is_exact() {
    let u = windows_resolved_read(&["r"], &["w"]);
    match u {
        ResolvedScope::Bounded { concrete, .. } => {
            let want: BTreeSet<String> = ["r", "w"].iter().map(|s| s.to_string()).collect();
            assert_eq!(concrete, want);
        }
        other => panic!("expected Bounded, got {other:?}"),
    }
}
