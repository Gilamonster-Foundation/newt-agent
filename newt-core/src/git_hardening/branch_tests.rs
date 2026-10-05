// Real Git fixtures ground the native ref namespace/absence checks (#2748).
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod branch_namespace_tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let original = root.path().join("original");
        std::fs::create_dir(&original).unwrap();
        init_repo(&original);
        git(&original, &["branch", "-m", "original-topic"]);
        let task = root.path().join("task");
        git(
            &original,
            &[
                "worktree",
                "add",
                "--detach",
                task.to_str().unwrap(),
                "HEAD",
            ],
        );
        own_gitdir_grants(&task);
        (root, original, task)
    }

    fn conflict(existing: &str, requested: &str, packed: bool) {
        let (_root, original, task) = fixture();
        if existing != "original-topic" {
            git(&original, &["branch", existing]);
        }
        let common = original.join(".git");
        if packed {
            git(&original, &["pack-refs", "--all"]);
        }
        // A reflog D/F collision must not accidentally mask the ref bug.
        std::fs::remove_dir_all(common.join("logs/refs/heads")).unwrap();
        let (common_dir, admin) = git_dirs(&task).unwrap();
        let identity = BoundGitIdentity::bind(common_dir, admin.clone()).unwrap();
        let heads_before = identity
            .common_workspace
            .read_dir(Path::new("refs/heads"))
            .unwrap();
        let head_before = std::fs::read(admin.join("HEAD")).unwrap();
        let packed_before = std::fs::read(common.join("packed-refs")).ok();
        let original_before = git_output(&original, &["rev-parse", "original-topic"]);
        let result = create_worktree_branch(&task, requested);
        assert!(
            result.is_err(),
            "namespace collision {existing} -> {requested}: {result:?}"
        );
        assert_eq!(
            identity
                .common_workspace
                .read_dir(Path::new("refs/heads"))
                .unwrap(),
            heads_before,
            "refusal must precede parent directory creation"
        );
        assert_eq!(std::fs::read(admin.join("HEAD")).unwrap(), head_before);
        assert_eq!(
            std::fs::read(common.join("packed-refs")).ok(),
            packed_before
        );
        assert_eq!(
            git_output(&original, &["rev-parse", "original-topic"]),
            original_before
        );
        assert!(!common.join("logs/refs/heads").exists());
        assert!(!common.join("packed-refs.lock").exists());
    }

    /// #2748: a packed checked-out original branch must block a child ref.
    #[test]
    fn packed_original_ancestor_refuses_before_mkdir() {
        conflict("original-topic", "original-topic/child", true);
    }

    /// #2748: packed descendants also occupy the requested namespace.
    #[test]
    fn packed_descendant_refuses_before_mkdir() {
        conflict("topic/child", "topic", true);
    }

    /// #2748: an exact packed collision must not materialize its parent dirs.
    #[test]
    fn packed_exact_refuses_before_mkdir() {
        conflict("topic/child", "topic/child", true);
    }

    /// #2748: ordinary loose D/F collisions remain refused without mutation.
    #[test]
    fn loose_namespace_collisions() {
        conflict("topic", "topic/child", false);
        conflict("topic/child", "topic", false);
    }

    /// #2748: present invalid loose refs are not absence; preserve every byte.
    #[test]
    fn invalid_loose_ref_is_not_absence() {
        let (_root, original, task) = fixture();
        let (common, admin) = git_dirs(&task).unwrap();
        let identity = BoundGitIdentity::bind(common.clone(), admin.clone()).unwrap();
        let head_before = std::fs::read(admin.join("HEAD")).unwrap();
        for bytes in [
            "",
            "\n \t",
            "garbage\n",
            "0123\n",
            "0000000000000000000000000000000000000000\n",
            "ref: refs/heads/original-topic\n",
        ] {
            let target = original.join(".git/refs/heads/sentinel");
            std::fs::write(&target, bytes).unwrap();
            let result = create_worktree_branch(&task, "sentinel");
            assert!(
                result.is_err(),
                "invalid loose ref {bytes:?} overwritten: {result:?}"
            );
            assert!(read_ref_natively(&identity, "sentinel").is_err());
            assert_eq!(std::fs::read(&target).unwrap(), bytes.as_bytes());
            assert_eq!(std::fs::read(admin.join("HEAD")).unwrap(), head_before);
            assert!(!common.join("logs/refs/heads/sentinel").exists());
            assert!(!common.join("refs/heads/sentinel.lock").exists());
            assert!(!common.join("packed-refs.lock").exists());
        }
    }

    /// #2748: the held leaf lock excludes real Git exact-name and ancestor writers.
    #[test]
    fn native_creation_keeps_git_writers_out_until_publish() {
        let (root, original, task) = fixture();
        let (common, admin) = git_dirs(&task).unwrap();
        let identity = BoundGitIdentity::bind(common.clone(), admin).unwrap();
        let oid = git_output(&original, &["rev-parse", "HEAD"]);
        let competing = || {
            for name in ["refs/heads/topic/child", "refs/heads/topic"] {
                let result = hermetic_git(&original, root.path())
                    .args(["update-ref", name, &oid, ""])
                    .output()
                    .unwrap();
                assert!(!result.status.success(), "competing Git published {name}");
            }
        };
        advance_branch_ref_natively_seamed(
            &identity,
            "topic/child",
            None,
            &oid,
            "new branch",
            &competing,
        )
        .unwrap();
        assert_eq!(git_output(&original, &["rev-parse", "topic/child"]), oid);
        assert!(!common.join("refs/heads/topic/child.lock").exists());
        assert!(!common.join("packed-refs.lock").exists());
    }

    /// #2748: lock contention preserves the other writer and releases our lock.
    #[test]
    fn leaf_lock_contention_releases_namespace_lock() {
        let (_root, original, task) = fixture();
        let common = original.join(".git");
        let lock = common.join("refs/heads/fresh.lock");
        std::fs::write(&lock, "other writer").unwrap();
        assert!(create_worktree_branch(&task, "fresh").is_err());
        assert_eq!(std::fs::read_to_string(&lock).unwrap(), "other writer");
        assert!(!common.join("packed-refs.lock").exists());
        assert!(!common.join("logs/refs/heads/fresh").exists());
        std::fs::remove_file(lock).unwrap();
        create_worktree_branch(&task, "fresh/nested").unwrap();
        create_worktree_branch(&task, "fresh/peer").unwrap();
        create_worktree_branch(&task, "fresh-other").unwrap();
    }

    /// #2748: a pack writer's lock must exclude creation, without stealing it.
    #[test]
    fn packed_writer_lock_excludes_creation() {
        let (_root, original, task) = fixture();
        let lock = original.join(".git/packed-refs.lock");
        std::fs::write(&lock, "other writer").unwrap();
        assert!(create_worktree_branch(&task, "fresh/nested").is_err());
        assert_eq!(std::fs::read_to_string(&lock).unwrap(), "other writer");
        assert!(!original.join(".git/refs/heads/fresh").exists());
    }
}
