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
    let held = crate::fs_cap::WorkspaceDir::open_root(&admin).unwrap();
    publish_detached_head(&held, &old, &new).unwrap();
    assert!(publish_detached_head(&held, &old, &old).is_err());
    assert_eq!(
        std::fs::read_to_string(admin.join("HEAD")).unwrap().trim(),
        new
    );
    assert!(!admin.join("HEAD.lock").exists());
    let moved = temp.path().join("held");
    std::fs::rename(&admin, &moved).unwrap();
    std::fs::create_dir(&admin).unwrap();
    std::fs::write(admin.join("HEAD"), "decoy").unwrap();
    publish_detached_head(&held, &new, &old).unwrap();
    assert_eq!(
        std::fs::read_to_string(moved.join("HEAD")).unwrap().trim(),
        old
    );
    assert_eq!(
        std::fs::read_to_string(admin.join("HEAD")).unwrap(),
        "decoy"
    );
}
