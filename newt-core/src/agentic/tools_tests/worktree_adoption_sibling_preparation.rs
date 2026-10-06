//! #2757: approval lifetime, destination identity and preparation regressions.
use super::*;

/// #2757: approval may not create a directory if it lacks either required axis.
#[test]
fn sibling_preparation_requires_read_and_write_before_mkdir() {
    let (_temp, policy, _) = crate::worktree_adoption::tests::fixture(false);
    std::fs::remove_dir(&policy.worktree).unwrap();
    let original = policy.worktree.parent().unwrap().join("main");
    let mut candidate = Creation::before(&original, &policy.worktree, &Caveats::top()).unwrap();
    for (read, write) in [(Scope::All, Scope::none()), (Scope::none(), Scope::All)] {
        let authority = Caveats {
            fs_read: read,
            fs_write: write,
            ..Caveats::top()
        };
        assert!(candidate.prepare(&authority).is_err());
        assert!(!policy.worktree.exists());
    }
}

/// #2757: a destination replaced with a symlink while the operator approves
/// cannot redirect the harness's directory creation into a neighbor.
#[test]
fn sibling_preparation_refuses_destination_symlink_swap() {
    let (_temp, policy, neighbor) = crate::worktree_adoption::tests::fixture(false);
    std::fs::remove_dir(&policy.worktree).unwrap();
    let original = policy.worktree.parent().unwrap().join("main");
    let mut candidate = Creation::before(&original, &policy.worktree, &Caveats::top()).unwrap();
    std::os::unix::fs::symlink(&neighbor, &policy.worktree).unwrap();
    assert!(candidate.prepare(&Caveats::top()).is_err());
    assert_eq!(std::fs::read_dir(neighbor).unwrap().count(), 0);
}

/// #2757: replacing the approved path's parent while prompting must not
/// redirect mkdir into a different directory that happens to share its name.
#[test]
fn sibling_preparation_refuses_replaced_parent() {
    let (_temp, policy, _) = crate::worktree_adoption::tests::fixture(true);
    std::fs::remove_dir(&policy.worktree).unwrap();
    let parent = policy.worktree.parent().unwrap();
    let original = parent.parent().unwrap();
    let mut candidate = Creation::before(original, &policy.worktree, &Caveats::top()).unwrap();
    std::fs::rename(parent, parent.with_extension("held")).unwrap();
    std::fs::create_dir(parent).unwrap();
    assert!(candidate.prepare(&Caveats::top()).is_err());
    assert!(!policy.worktree.exists());
}

/// #2757: approval for a leaf never authorizes missing ancestor creation.
#[test]
fn sibling_round2_missing_parent_refuses_without_mutation() {
    let (_temp, policy, _) = crate::worktree_adoption::tests::fixture(false);
    let original = policy.worktree.parent().unwrap().join("main");
    let target = policy.worktree.join("missing/task");
    let mut candidate = Creation::before(&original, &target, &Caveats::top()).unwrap();
    let authority = Caveats {
        fs_read: Scope::only([target.to_string_lossy().into_owned()]),
        fs_write: Scope::only([target.to_string_lossy().into_owned()]),
        ..Caveats::top()
    };
    assert!(
        candidate.prepare(&authority).is_err(),
        "missing parent must refuse"
    );
    assert!(!policy.worktree.join("missing").exists());
}

/// #2757: cancellation/refusal drops an owned empty leaf, never its siblings.
#[test]
fn sibling_round2_drop_rolls_back_only_owned_empty_leaf() {
    let (_temp, policy, neighbor) = crate::worktree_adoption::tests::fixture(false);
    std::fs::remove_dir(&policy.worktree).unwrap();
    std::fs::write(neighbor.join("sentinel"), "keep").unwrap();
    let original = policy.worktree.parent().unwrap().join("main");
    let mut candidate = Creation::before(&original, &policy.worktree, &Caveats::top()).unwrap();
    let authority = Caveats {
        fs_read: Scope::only([policy.worktree.to_string_lossy().into_owned()]),
        fs_write: Scope::only([policy.worktree.to_string_lossy().into_owned()]),
        ..Caveats::top()
    };
    candidate.prepare(&authority).unwrap();
    assert!(policy.worktree.is_dir());
    drop(candidate);
    assert!(
        !policy.worktree.exists(),
        "cancelled creation left its leaf behind"
    );
    assert_eq!(
        std::fs::read_to_string(neighbor.join("sentinel")).unwrap(),
        "keep"
    );
}

/// #2757: an empty destination present at admission cannot be replaced while prompting.
#[test]
fn sibling_round2_existing_destination_swap_refuses() {
    let (_temp, policy, _) = crate::worktree_adoption::tests::fixture(false);
    let original = policy.worktree.parent().unwrap().join("main");
    let mut candidate = Creation::before(&original, &policy.worktree, &Caveats::top()).unwrap();
    std::fs::rename(&policy.worktree, policy.worktree.with_extension("held")).unwrap();
    std::fs::create_dir(&policy.worktree).unwrap();
    assert!(
        candidate.prepare(&Caveats::top()).is_err(),
        "replacement object accepted"
    );
}

/// #2757: a path swap after preparation must fail both pre-dispatch and
/// adoption validation, even if matching Git metadata is placed in the new leaf.
#[test]
fn sibling_round2_post_prepare_swap_cannot_adopt() {
    let (_temp, policy, _) = crate::worktree_adoption::tests::fixture(false);
    let original = policy.worktree.parent().unwrap().join("main");
    let mut candidate = Creation::before(&original, &policy.worktree, &Caveats::top()).unwrap();
    candidate.prepare(&Caveats::top()).unwrap();
    assert!(candidate.ready().is_ok());
    std::fs::rename(&policy.worktree, policy.worktree.with_extension("held")).unwrap();
    std::fs::create_dir(&policy.worktree).unwrap();
    crate::worktree_adoption::tests::link(&policy);
    assert!(candidate.ready().is_err());
    assert!(candidate.verify().is_none());
    assert!(policy.worktree.join(".git").is_file());
}

/// #2757: even a preparation refusal spends the queued call-only grant; fixing
/// the parent and retrying requires another operator decision, not queue reuse.
#[test]
fn sibling_round2_preparation_failure_spends_once_queue() {
    let (_temp, policy, _) = crate::worktree_adoption::tests::fixture(false);
    let original = policy.worktree.parent().unwrap().join("main");
    let target = policy.worktree.join("missing/task");
    let mut base = Caveats::top();
    crate::caveats::lock_fs_to_workspace(&mut base, original.to_str().unwrap(), &[], &[]);
    let args = serde_json::json!({"command":"git worktree add -b task ../task/missing/task"});
    let mut gate = AllowOnce::default();
    let mut candidate = Creation::before(&original, &target, &base).unwrap();
    let error = super::super::prepare::creation(
        &mut candidate,
        &args,
        original.to_str().unwrap(),
        &base,
        &mut Some(&mut gate),
    )
    .unwrap_err();
    assert!(error.contains("existing immediate parent"), "{error}");
    assert!(gate.2.is_empty());
    assert_eq!(gate.1.len(), 2);
    assert!(!target.parent().unwrap().exists());
    drop(candidate);
    std::fs::create_dir(target.parent().unwrap()).unwrap();
    let mut retry = Creation::before(&original, &target, &base).unwrap();
    let error = super::super::prepare::creation(
        &mut retry,
        &args,
        original.to_str().unwrap(),
        &base,
        &mut Some(&mut gate),
    )
    .unwrap_err();
    assert!(error.contains("not approved"), "{error}");
    assert!(!target.exists());
}

/// #2757: permission refresh happens after preparation; a swap there must
/// refuse before the refreshed authority is handed to the Git dispatch.
#[test]
fn sibling_round2_late_refresh_swap_refuses() {
    struct Swap(std::path::PathBuf);
    impl PermissionGate for Swap {
        fn ask(&mut self, _: &[PermissionRequest]) -> PermissionDecision {
            PermissionDecision::Deny
        }
        fn ask_question(&mut self, _: &str) -> HumanQuestionOutcome {
            HumanQuestionOutcome::Unavailable
        }
        fn refresh_caveats(&mut self, base: &Caveats) -> PermissionDecision {
            std::fs::rename(&self.0, self.0.with_extension("held")).unwrap();
            std::fs::create_dir(&self.0).unwrap();
            PermissionDecision::Allow(base.clone())
        }
    }
    let (_temp, policy, _) = crate::worktree_adoption::tests::fixture(false);
    let original = policy.worktree.parent().unwrap().join("main");
    let mut candidate = Creation::before(&original, &policy.worktree, &Caveats::top()).unwrap();
    candidate.prepare(&Caveats::top()).unwrap();
    let mut swap = Swap(policy.worktree);
    let mut guard = super::super::prepare::Guard {
        candidate: &candidate,
        inner: Some(&mut swap),
    };
    assert!(matches!(
        guard.refresh_caveats(&Caveats::top()),
        PermissionDecision::Deny
    ));
}
