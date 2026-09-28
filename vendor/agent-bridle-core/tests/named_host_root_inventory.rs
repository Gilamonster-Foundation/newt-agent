//! Inventory-relative closure admission; projections are explicitly synthetic.
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use agent_bridle_core::{
    empty_closure, AdmittedFence, BackendProjection, Caveats, ChildNetworkPolicy,
    ConfinementMechanism, EnforcementFloor, ResolvedAuthority, ResolvedScope, SandboxKind,
    SandboxPolicy, Scope,
};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "bridle-inventory-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        let fixture = Self(root.canonicalize().unwrap());
        for directory in ["store", "store-sibling", ".config/gh"] {
            std::fs::create_dir_all(fixture.0.join(directory)).unwrap();
        }
        std::fs::write(fixture.0.join("store/secret"), b"private").unwrap();
        fixture
    }
    fn path(&self, path: &str) -> String {
        self.0.join(path).display().to_string()
    }
    fn protected(&self) -> BTreeSet<String> {
        [self.path("store/secret"), self.path(".config/gh")].into()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn root() -> &'static str {
    if cfg!(windows) {
        r"C:\tools\root.exe"
    } else {
        "/tools/root"
    }
}
fn caveats() -> Caveats {
    Caveats {
        exec: Scope::only([root().to_owned()]),
        fs_read: Scope::only(["/workspace".to_owned()]),
        fs_write: Scope::only(["/workspace".to_owned()]),
        net: Scope::none(),
        ..Caveats::top()
    }
}
fn mechanism() -> ConfinementMechanism {
    ConfinementMechanism::for_named_root(SandboxKind::Landlock, ChildNetworkPolicy::DenyDirect)
}
fn projection() -> BackendProjection {
    let mut resolved = ResolvedAuthority::from_delegated(&caveats());
    resolved.exec = ResolvedScope::Unbounded;
    BackendProjection {
        resolved,
        runtime_closure: empty_closure(),
    }
}
fn admit(
    roots: &BTreeSet<String>,
    projection: BackendProjection,
) -> agent_bridle_core::ToolResult<AdmittedFence> {
    AdmittedFence::admit_named_root(
        &caveats(),
        root(),
        roots,
        mechanism(),
        EnforcementFloor::CONFINED,
        |_| projection,
    )
}
fn with_closure(mut p: BackendProjection, axis: &str, scope: ResolvedScope) -> BackendProjection {
    match axis {
        "fs_read" => {
            p.resolved.fs_read = p.resolved.fs_read.union(&scope);
            p.runtime_closure.fs_read = scope;
        }
        "fs_write" => {
            p.resolved.fs_write = p.resolved.fs_write.union(&scope);
            p.runtime_closure.fs_write = scope;
        }
        "exec" => p.runtime_closure.exec = scope,
        "net" => p.runtime_closure.net = scope,
        _ => unreachable!(),
    }
    p
}

#[test]
fn markerless_and_config_ancestors_refuse_but_component_siblings_admit() {
    let fixture = Fixture::new();
    for axis in ["fs_read", "fs_write", "exec"] {
        for path in [fixture.path("store"), fixture.path(".config")] {
            let p = with_closure(projection(), axis, ResolvedScope::concrete([path]));
            let error = admit(&fixture.protected(), p).unwrap_err();
            assert!(error.to_string().contains("closure"));
        }
        let sibling = with_closure(
            projection(),
            axis,
            ResolvedScope::concrete([fixture.path("store-sibling")]),
        );
        if axis == "exec" {
            assert!(
                admit(&fixture.protected(), sibling).is_err(),
                "all exec closure additions are unsupported"
            );
        } else {
            admit(&fixture.protected(), sibling).unwrap();
        }
    }
}

#[test]
fn every_runtime_class_and_bounded_descendant_exec_refuse() {
    let fixture = Fixture::new();
    admit(&fixture.protected(), projection()).unwrap();
    for axis in ["fs_read", "fs_write", "exec", "net"] {
        let p = with_closure(
            projection(),
            axis,
            ResolvedScope::class("unproven-authority"),
        );
        assert!(admit(&fixture.protected(), p)
            .unwrap_err()
            .to_string()
            .contains("classes"));
    }
    let mut bounded = projection();
    bounded.resolved.exec = ResolvedScope::concrete([root().to_owned()]);
    assert!(admit(&fixture.protected(), bounded)
        .unwrap_err()
        .to_string()
        .contains("exec"));
}

#[test]
fn named_root_does_not_admit_exec_or_net_runtime_additions() {
    let fixture = Fixture::new();
    admit(&fixture.protected(), projection()).unwrap();
    for (axis, entry) in [
        ("exec", fixture.path("store-sibling")),
        ("net", "remote.example".to_owned()),
    ] {
        let p = with_closure(projection(), axis, ResolvedScope::concrete([entry]));
        assert!(
            admit(&fixture.protected(), p).is_err(),
            "unsupported {axis} closure addition must refuse"
        );
    }
}

#[test]
fn absent_empty_and_relative_inventory_refuse_ordinary_missing_tails_resolve() {
    let fixture = Fixture::new();
    let default = SandboxPolicy::default();
    assert!(default.resolve_named_root_protected_roots().is_err());
    assert!(serde_json::to_value(default)
        .unwrap()
        .get("named_root_protected_roots")
        .is_none());
    assert!(admit(&BTreeSet::new(), projection()).is_err());
    assert!(admit(&["relative/private".to_owned()].into(), projection()).is_err());
    for entry in [".", ".."] {
        for axis in ["fs_read", "fs_write", "exec", "net"] {
            assert!(admit(
                &fixture.protected(),
                with_closure(
                    projection(),
                    axis,
                    ResolvedScope::concrete([entry.to_owned()])
                )
            )
            .is_err());
        }
    }
    let missing: BTreeSet<String> = [fixture.path("store/not-created/state")].into();
    let admitted = admit(&missing, projection()).unwrap();
    assert_eq!(admitted.admitted_body().unwrap().protected_roots, missing);
    assert!(admitted
        .verify_named_root_applied(
            &caveats(),
            root(),
            &fixture.protected(),
            mechanism(),
            projection()
        )
        .is_err());
}

#[cfg(unix)]
#[test]
fn canonical_aliases_overlap_and_dangling_aliases_refuse() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    symlink(fixture.0.join("store"), fixture.0.join("alias")).unwrap();
    let alias = with_closure(
        projection(),
        "fs_read",
        ResolvedScope::concrete([fixture.path("alias")]),
    );
    assert!(admit(&fixture.protected(), alias).is_err());
    let aliased_inventory = [fixture.path("alias/secret")].into();
    let admitted = admit(&aliased_inventory, projection()).unwrap();
    assert_eq!(
        admitted.admitted_body().unwrap().protected_roots,
        [fixture.path("store/secret")].into()
    );
    symlink(fixture.0.join("missing-target"), fixture.0.join("dangling")).unwrap();
    assert!(admit(&[fixture.path("dangling/private")].into(), projection()).is_err());
}

#[test]
fn inventory_does_not_revoke_explicit_delegation() {
    let fixture = Fixture::new();
    let mut delegated = caveats();
    delegated.fs_read = Scope::only([fixture.path("store")]);
    let mut p = projection();
    p.resolved.fs_read = ResolvedScope::from_scope(&delegated.fs_read);
    AdmittedFence::admit_named_root(
        &delegated,
        root(),
        &fixture.protected(),
        mechanism(),
        EnforcementFloor::CONFINED,
        |_| p,
    )
    .unwrap();
}

#[test]
fn duplicate_closure_entry_does_not_revoke_explicit_protected_access() {
    let fixture = Fixture::new();
    for unrestricted in [false, true] {
        let mut delegated = caveats();
        delegated.fs_read = if unrestricted {
            Scope::All
        } else {
            Scope::only([fixture.path("store")])
        };
        let mut p = projection();
        p.resolved.fs_read = ResolvedScope::from_scope(&delegated.fs_read);
        let duplicate = ResolvedScope::concrete([fixture.path("store")]);
        p.runtime_closure.fs_read = duplicate;
        AdmittedFence::admit_named_root(
            &delegated,
            root(),
            &fixture.protected(),
            mechanism(),
            EnforcementFloor::CONFINED,
            |_| p,
        )
        .expect("the closure adds no access beyond this explicit grant");
    }
}

#[test]
fn two_valid_inventories_for_the_same_fence_have_distinct_content_ids() {
    let fixture = Fixture::new();
    let first = admit(&fixture.protected(), projection()).unwrap();
    let other_inventory = [fixture.path("another-state-root")].into();
    let second = admit(&other_inventory, projection()).unwrap();
    assert_ne!(first.fence_id(), second.fence_id());
    assert_eq!(first.mechanism_caveats(), second.mechanism_caveats());
}

#[test]
fn marker_named_runtime_additions_outside_the_inventory_are_not_private_by_name() {
    let fixture = Fixture::new();
    for marker in [".newt", ".ssh"] {
        let path = fixture.0.join("public-runtime").join(marker);
        std::fs::create_dir_all(&path).unwrap();
        for axis in ["fs_read", "fs_write"] {
            let p = with_closure(
                projection(),
                axis,
                ResolvedScope::concrete([path.display().to_string()]),
            );
            admit(&fixture.protected(), p).expect("NamedRoot protection is relative to the trusted inventory, not a basename heuristic");
        }
    }
}

// Model: gpt-6-astra | Harness: Codex 0.153.4 | Operator: Shawn Hartsock | Time: 22:29 UTC | Date: 2026-09-12
