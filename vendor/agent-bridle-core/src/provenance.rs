//! provenance — the resolved-authority lattice + scope-relation admission,
//! re-exported from `agent-mesh-protocol` (the leash's single algebra home).
//!
//! As of `agent-mesh-protocol 0.6.4` the content-addressed authority provenance
//! API (mesh #72/#134) is published, so bridle ships **no copy** of the security
//! algebra: `ResolvedScope`, `ScopeRelation`, `relate`, `admit` and friends are
//! the mesh's — one implementation, one drift surface. (An earlier revision of
//! this module carried a temporary native lattice, written when the newest mesh
//! release predated #72; it is deleted, as designed.)
//!
//! What REMAINS bridle's is the meaning and the binding:
//!
//! - **Meaning (the #317 audit finding):** the admission witness used to carry
//!   only per-axis enforcement *strength*, never the resolved *scope*, so
//!   INV-BOUND (`effective ⊆ authorized`) could not be expressed and a widening
//!   was folded into a strength downgrade. These types are the scope operand:
//!   fidelity is a first-class relation, orthogonal to strength — a
//!   `Superset`/`Incomparable`/`Unknown` axis refuses regardless of how strongly
//!   the mechanism enforces it. Bridle's job is to **project** each native fence
//!   (Landlock ruleset / SBPL profile / AppContainer capability set) into this
//!   lattice and to declare its runtime closures explicitly.
//!
//! - **Binding (the conformance tier below):** the `#[cfg(test)]` suite pins the
//!   admission semantics bridle's security depends on — the join-semilattice
//!   laws, the widening refusals, the honest-parity closure mechanism, and the
//!   fail-closed `Unknown` rule. It runs against the *mesh implementation*, so a
//!   semantic drift in a future mesh release fails here at the pin bump instead
//!   of silently changing what a child process may do. The mesh's own proptests
//!   check these laws generically; the Lean spec
//!   (`newt-agent/formal/CaveatLattice/ResolvedLattice.lean`) proves the
//!   underlying order universally; this tier binds THIS consumer to them.

pub use agent_mesh_protocol::{
    admit, empty_closure, relate, AdmissionDecision, AdmissionReject, ConfinedAxis,
    ResolvedAuthority, ResolvedScope, ScopeRelation,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Caveats, Scope};

    fn concrete(items: &[&str]) -> ResolvedScope {
        ResolvedScope::concrete(items.iter().map(|s| s.to_string()))
    }

    // ----- join-semilattice laws (mesh proptests check these generically; the
    // Lean ResolvedLattice proves them universally; these bind THIS consumer) -----

    #[test]
    fn empty_is_the_union_identity() {
        let x = ResolvedScope::concrete(["a".to_string(), "b".to_string()])
            .union(&ResolvedScope::class("cls"));
        assert_eq!(ResolvedScope::empty().union(&x), x);
        assert_eq!(x.union(&ResolvedScope::empty()), x);
    }

    #[test]
    fn union_is_commutative_associative_idempotent() {
        let a = concrete(&["a"]);
        let b = concrete(&["b"]).union(&ResolvedScope::class("k"));
        let c = concrete(&["c"]);
        assert_eq!(a.union(&b), b.union(&a));
        assert_eq!(a.union(&b).union(&c), a.union(&b.union(&c)));
        assert_eq!(a.union(&a), a);
    }

    #[test]
    fn unknown_propagates_and_unbounded_absorbs() {
        let a = concrete(&["a"]);
        assert_eq!(ResolvedScope::Unknown.union(&a), ResolvedScope::Unknown);
        assert_eq!(a.union(&ResolvedScope::Unknown), ResolvedScope::Unknown);
        assert_eq!(ResolvedScope::Unbounded.union(&a), ResolvedScope::Unbounded);
    }

    #[test]
    fn from_scope_lifts_the_delegated_lattice() {
        assert_eq!(
            ResolvedScope::from_scope(&Scope::<String>::All),
            ResolvedScope::Unbounded
        );
        assert_eq!(
            ResolvedScope::from_scope(&Scope::only(["sh".to_string()])),
            concrete(&["sh"])
        );
    }

    // ----- relate: the five-way classification (richer than a bare order) -----

    #[test]
    fn relate_classifies_equal_subset_superset() {
        let narrow = concrete(&["sh"]);
        let wide = concrete(&["sh", "bash"]);
        assert_eq!(relate(&narrow, &narrow), ScopeRelation::Equal);
        assert_eq!(relate(&narrow, &wide), ScopeRelation::Subset);
        assert_eq!(relate(&wide, &narrow), ScopeRelation::Superset);
    }

    #[test]
    fn relate_distinguishes_incomparable_from_superset() {
        // Neither contains the other — a DISTINCT reportable fact, not a widening
        // and not a subset. (The earlier native copy collapsed this into
        // `Widened`; the mesh algebra keeps it separate.)
        let a = concrete(&["a"]);
        let b = concrete(&["b"]);
        assert_eq!(relate(&a, &b), ScopeRelation::Incomparable);
    }

    #[test]
    fn relate_is_unknown_on_either_unknown_operand() {
        let a = concrete(&["a"]);
        assert_eq!(relate(&a, &ResolvedScope::Unknown), ScopeRelation::Unknown);
        assert_eq!(relate(&ResolvedScope::Unknown, &a), ScopeRelation::Unknown);
    }

    #[test]
    fn a_concrete_grant_is_not_covered_by_a_class() {
        // Containment is per dimension: a class the bound does not authorize is
        // extra authority even when the concrete dimension matches — the
        // single-address → whole-interface widening reads as Superset.
        let resolved = concrete(&["127.0.0.1"]).union(&ResolvedScope::class("loopback-interface"));
        let bound = concrete(&["127.0.0.1"]);
        assert_eq!(relate(&resolved, &bound), ScopeRelation::Superset);
    }

    // ----- admit: the INV-BOUND decision over whole authorities -----

    fn exec_only(programs: &[&str]) -> Caveats {
        Caveats {
            exec: Scope::only(programs.iter().map(|s| s.to_string())),
            ..Caveats::top()
        }
    }

    fn exec_authority(scope: ResolvedScope) -> ResolvedAuthority {
        let mut authority = empty_closure();
        authority.exec = scope;
        authority
    }

    #[test]
    fn sh_to_bash_launcher_widening_is_refused_without_a_closure() {
        // exec grant Only{sh}; the profile resolves to permit {sh, bash}. With no
        // closure authorizing bash this is a widening ⇒ Reject(Superset) on the
        // exec axis — the case the strength-only admission wrongly ADMITTED.
        let delegated = exec_only(&["sh"]);
        let mut resolved = ResolvedAuthority::from_delegated(&delegated);
        resolved.exec = concrete(&["sh", "bash"]);
        match admit(&resolved, &delegated, &empty_closure()) {
            AdmissionDecision::Reject(AdmissionReject { axis, relation }) => {
                assert_eq!(axis, ConfinedAxis::Exec);
                assert_eq!(relation, ScopeRelation::Superset);
            }
            AdmissionDecision::Admit => panic!("a silent exec widening must refuse"),
        }
    }

    #[test]
    fn extra_authority_is_admitted_only_via_an_explicit_closure() {
        // The honest-parity mechanism: bash IS admitted — but only because the
        // declared launcher closure authorizes it, never silently.
        let delegated = exec_only(&["sh"]);
        let mut resolved = ResolvedAuthority::from_delegated(&delegated);
        resolved.exec = concrete(&["sh", "bash"]);
        let closure = exec_authority(concrete(&["bash"]));
        assert!(matches!(
            admit(&resolved, &delegated, &closure),
            AdmissionDecision::Admit
        ));
    }

    #[test]
    fn an_undeclared_variant_still_refuses_under_the_declared_closure() {
        // The closure declares bash; the fence also permits zsh. A NOVEL widening
        // beyond the declared closure refuses under every policy ruling.
        let delegated = exec_only(&["sh"]);
        let mut resolved = ResolvedAuthority::from_delegated(&delegated);
        resolved.exec = concrete(&["sh", "bash", "zsh"]);
        let closure = exec_authority(concrete(&["bash"]));
        assert!(matches!(
            admit(&resolved, &delegated, &closure),
            AdmissionDecision::Reject(AdmissionReject {
                axis: ConfinedAxis::Exec,
                relation: ScopeRelation::Superset,
            })
        ));
    }

    #[test]
    fn an_unknown_resolution_fails_closed_even_under_top_delegation() {
        // Deliberately STRICTER than the Lean authority order (where `unknown`
        // sits at the top and is ⊑ an unbounded bound): an axis whose resolved
        // authority cannot be decided refuses even when the delegation is
        // unbounded. Decidability is an admission obligation, not an authority
        // one — the fail-closed L7 rule. This test PINS that strictness.
        let delegated = Caveats::top();
        let mut resolved = ResolvedAuthority::from_delegated(&delegated);
        resolved.net = ResolvedScope::Unknown;
        assert!(matches!(
            admit(&resolved, &delegated, &empty_closure()),
            AdmissionDecision::Reject(AdmissionReject {
                axis: ConfinedAxis::Net,
                relation: ScopeRelation::Unknown,
            })
        ));
    }

    #[test]
    fn an_unbounded_fence_over_a_bounded_grant_is_refused() {
        // Audit row 16: a fence that resolves unbounded on an axis the Caveat
        // bounded ⇒ Reject(Superset).
        let delegated = exec_only(&["git"]);
        let mut resolved = ResolvedAuthority::from_delegated(&delegated);
        resolved.exec = ResolvedScope::Unbounded;
        assert!(matches!(
            admit(&resolved, &delegated, &empty_closure()),
            AdmissionDecision::Reject(AdmissionReject {
                axis: ConfinedAxis::Exec,
                relation: ScopeRelation::Superset,
            })
        ));
    }

    #[test]
    fn a_fence_matching_the_delegated_grant_is_admitted() {
        let delegated = exec_only(&["sh"]);
        let resolved = ResolvedAuthority::from_delegated(&delegated);
        assert!(matches!(
            admit(&resolved, &delegated, &empty_closure()),
            AdmissionDecision::Admit
        ));
    }

    #[test]
    fn loopback_full_interface_widening_is_refused_without_a_closure() {
        // net grant Only{127.0.0.1}; the kernel fence permits the whole loopback
        // interface (a class). Same verdict as the exec widening — refused —
        // independent of enforcement strength; admitted only via a declared
        // net closure carrying the class.
        let delegated = Caveats {
            net: Scope::only(["127.0.0.1".to_string()]),
            ..Caveats::top()
        };
        let mut resolved = ResolvedAuthority::from_delegated(&delegated);
        resolved.net = concrete(&["127.0.0.1"]).union(&ResolvedScope::class("loopback-interface"));
        assert!(matches!(
            admit(&resolved, &delegated, &empty_closure()),
            AdmissionDecision::Reject(AdmissionReject {
                axis: ConfinedAxis::Net,
                relation: ScopeRelation::Superset,
            })
        ));
        let mut closure = empty_closure();
        closure.net = ResolvedScope::class("loopback-interface");
        assert!(matches!(
            admit(&resolved, &delegated, &closure),
            AdmissionDecision::Admit
        ));
    }
}
