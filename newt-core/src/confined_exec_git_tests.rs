//! #2835: pause after authority preparation, at the Landlock rule-install seam.
use super::*;
use agent_bridle::Sandbox;
#[path = "../tests/support/linked_git_fixture.rs"]
mod fixture_support;

#[test]
fn metadata_rule_install_never_follows_replacement_objects() {
    assert!(agent_bridle::landlock_is_supported());
    for component in [".git/worktrees/linked", ".git", "."] {
        let (_fixture, repo, linked) = fixture_support::fixture();
        crate::git_hardening::own_gitdir_grants(&linked);
        let request = build_tool_request(&linked, &linked, "/bin/cat", ["sentinel"], &Scope::All);
        let target = repo.join(component).canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let suffix = if component == "." {
            ".git/sentinel"
        } else {
            "sentinel"
        };
        let private = outside.path().join(suffix);
        std::fs::create_dir_all(private.parent().unwrap()).unwrap();
        std::fs::write(private, "outside secret\n").unwrap();
        let exposed = target.join(suffix);
        let moved = repo.parent().unwrap().join("moved");
        std::fs::rename(&target, &moved).unwrap();
        std::os::unix::fs::symlink(outside.path(), &target).unwrap();
        let denied = std::thread::spawn(move || {
            // Same kernel seam used by ConfinedCommand after admission. The
            // dedicated thread is disposable because Landlock is irreversible.
            agent_bridle::LandlockSandbox::new()
                .apply_with_held_roots(&request.caveats, &request.held_read_roots)
                .unwrap();
            std::fs::read(exposed).is_err()
        })
        .join()
        .unwrap();
        std::fs::remove_file(&target).unwrap();
        std::fs::rename(&moved, &target).unwrap();
        assert!(
            denied,
            "metadata replacement leaked an outside sentinel: {component}"
        );
    }
}
