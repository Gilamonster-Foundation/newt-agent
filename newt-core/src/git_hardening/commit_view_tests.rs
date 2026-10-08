use super::*;

/// #2813: publishing a private commit must reject a moved HEAD and must use
/// the held admin directory even if its pathname is replaced by another actor.
#[test]
fn private_commit_head_publication_is_cas_and_descriptor_bound() {
    let temp = tempfile::tempdir().unwrap();
    let admin = temp.path().join("admin");
    std::fs::create_dir(&admin).unwrap();
    let old = "1".repeat(40);
    let new = "2".repeat(40);
    std::fs::write(admin.join("HEAD"), format!("{old}\n")).unwrap();
    let held = agent_bridle_fdguard::GrantedRoot::acquire(&admin).unwrap();
    publish_detached_head(&held, &old, &new, "verified first\n").unwrap();
    assert!(publish_detached_head(&held, &old, &old, "must not append\n").is_err());
    assert_eq!(
        std::fs::read_to_string(admin.join("HEAD")).unwrap().trim(),
        new
    );
    assert!(!admin.join("HEAD.lock").exists());
    assert_eq!(
        std::fs::read_to_string(admin.join("logs/HEAD")).unwrap(),
        "verified first\n"
    );
    let moved = temp.path().join("held");
    std::fs::rename(&admin, &moved).unwrap();
    std::fs::create_dir(&admin).unwrap();
    std::fs::write(admin.join("HEAD"), "decoy").unwrap();
    publish_detached_head(&held, &new, &old, "verified second\n").unwrap();
    assert_eq!(
        std::fs::read_to_string(moved.join("HEAD")).unwrap().trim(),
        old
    );
    assert_eq!(
        std::fs::read_to_string(admin.join("HEAD")).unwrap(),
        "decoy"
    );
    assert!(!admin.join("logs").exists());
    assert_eq!(
        std::fs::read_to_string(moved.join("logs/HEAD")).unwrap(),
        "verified first\nverified second\n"
    );
}

/// #2813: HEAD-log failure must leave HEAD unchanged and never append through
/// a child-planted link into a shared/sibling log outside the held admin root.
#[test]
fn private_commit_reflog_refuses_escape_before_publishing_head() {
    let temp = tempfile::tempdir().unwrap();
    let admin = temp.path().join("admin");
    std::fs::create_dir_all(admin.join("logs")).unwrap();
    let outside = temp.path().join("outside-log");
    std::fs::write(&outside, "untouched\n").unwrap();
    std::os::unix::fs::symlink(&outside, admin.join("logs/HEAD")).unwrap();
    let old = "1".repeat(40);
    let new = "2".repeat(40);
    std::fs::write(admin.join("HEAD"), &old).unwrap();
    let held = agent_bridle_fdguard::GrantedRoot::acquire(&admin).unwrap();
    let error = publish_detached_head(&held, &old, &new, "verified entry\n").unwrap_err();
    assert!(
        error.contains("cannot append worktree HEAD reflog"),
        "{error}"
    );
    assert_eq!(std::fs::read_to_string(admin.join("HEAD")).unwrap(), old);
    assert_eq!(std::fs::read_to_string(outside).unwrap(), "untouched\n");
    assert!(!admin.join("HEAD.lock").exists());
}
