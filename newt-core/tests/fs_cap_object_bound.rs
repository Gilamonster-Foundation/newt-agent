//! step-52.1 — object-bound containment proof for
//! [`newt_core::fs_cap::WorkspaceDir`].
//!
//! Real-resource tier (see CLAUDE.md "Testing strategy"): the invariant IS the
//! kernel's `openat2(RESOLVE_BENEATH)` behaviour, which no mock can stand in for —
//! a mock would only encode our *belief* about the syscall. These tests are the
//! ground truth that the object-bound capability behaves as claimed; the fs tool
//! arms and write primitives move onto it in step-52.2 / step-52.3, and this is
//! what proves that rewire will actually contain them.
//!
//! Linux (`openat2`) and macOS (descriptor-relative no-follow opens), `#[serial]` (real-fs tests
//! contend under parallel load — CLAUDE.md).

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::io::{Read, Write};
use std::os::unix::fs::symlink;
use std::path::Path;

use newt_core::fs_cap::WorkspaceDir;
use serial_test::serial;
use tempfile::tempdir;

// ---- positive controls: legitimate in-tree access works ----

#[test]
#[serial]
fn open_reads_a_contained_file() {
    let ws = tempdir().unwrap();
    std::fs::create_dir(ws.path().join("sub")).unwrap();
    std::fs::write(ws.path().join("sub/file.txt"), b"hello").unwrap();

    let dir = WorkspaceDir::open_root(ws.path()).unwrap();
    let mut f = dir.open(Path::new("sub/file.txt")).unwrap();
    let mut s = String::new();
    f.read_to_string(&mut s).unwrap();
    assert_eq!(s, "hello");
}

#[test]
#[serial]
fn create_writes_only_beneath_the_root() {
    let ws = tempdir().unwrap();
    let dir = WorkspaceDir::open_root(ws.path()).unwrap();
    let mut f = dir.create(Path::new("out.txt")).unwrap();
    f.write_all(b"written").unwrap();
    drop(f);
    // It landed inside the workspace, nowhere else.
    assert_eq!(
        std::fs::read_to_string(ws.path().join("out.txt")).unwrap(),
        "written"
    );
}

#[test]
#[serial]
fn open_dir_traverses_a_contained_subtree() {
    let ws = tempdir().unwrap();
    std::fs::create_dir(ws.path().join("sub")).unwrap();
    std::fs::write(ws.path().join("sub/inner.txt"), b"deep").unwrap();

    let dir = WorkspaceDir::open_root(ws.path()).unwrap();
    let sub = dir.open_dir(Path::new("sub")).unwrap();
    let mut f = sub.open(Path::new("inner.txt")).unwrap();
    let mut s = String::new();
    f.read_to_string(&mut s).unwrap();
    assert_eq!(s, "deep");
}

// ---- containment: every escape is refused at resolve time ----

#[test]
#[serial]
fn parent_traversal_escape_is_denied() {
    let root = tempdir().unwrap();
    let ws = root.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    std::fs::write(root.path().join("secret"), b"TOP SECRET").unwrap();

    let dir = WorkspaceDir::open_root(&ws).unwrap();
    assert!(
        dir.open(Path::new("../secret")).is_err(),
        "`..` must not climb above the workspace root"
    );
}

#[test]
#[serial]
fn absolute_path_is_denied() {
    let ws = tempdir().unwrap();
    let dir = WorkspaceDir::open_root(ws.path()).unwrap();
    assert!(
        dir.open(Path::new("/etc/hostname")).is_err(),
        "an absolute path must not resolve outside the root"
    );
}

#[test]
#[serial]
fn relative_symlink_under_workspace_escaping_is_denied() {
    // The named residual (#522): a symlink UNDER the workspace whose *relative*
    // target climbs out of it. A lexical check sees `ws/link/...` as inside; the
    // object resolver follows the link and refuses because resolution ascends
    // above the root.
    let root = tempdir().unwrap();
    let ws = root.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let outside = root.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("secret"), b"TOP SECRET").unwrap();
    symlink("../outside", ws.join("link")).unwrap(); // ws/link -> ../outside (climbs out)

    let dir = WorkspaceDir::open_root(&ws).unwrap();
    assert!(
        dir.open(Path::new("link/secret")).is_err(),
        "a symlink under the workspace resolving outside it must be denied"
    );
}

#[test]
#[serial]
fn absolute_symlink_escape_is_denied() {
    let ws = tempdir().unwrap();
    symlink("/etc", ws.path().join("etc")).unwrap(); // ws/etc -> /etc (absolute)
    let dir = WorkspaceDir::open_root(ws.path()).unwrap();
    assert!(
        dir.open(Path::new("etc/hostname")).is_err(),
        "an absolute symlink must not be followed out of the root"
    );
}

// ---- the honest contrast: why the object resolver, not a lexical predicate ----

#[test]
#[serial]
fn object_resolver_denies_what_a_lexical_prefix_check_would_admit() {
    // This is the whole point of step-52. A lexical `starts_with(root)` predicate
    // — the shape of every current fs gate — ADMITS `ws/link/secret` because the
    // string is under `ws/`. The object resolver DENIES it because the object the
    // kernel opens is outside. Same input, opposite (correct) verdict.
    let root = tempdir().unwrap();
    let ws = root.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let outside = root.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("secret"), b"TOP SECRET").unwrap();
    symlink("../outside", ws.join("link")).unwrap();

    // What a lexical predicate concludes about the *name*:
    let candidate = ws.join("link/secret");
    assert!(
        candidate.starts_with(&ws),
        "the lexical check is fooled — the name is under ws/"
    );

    // What the object resolver concludes about the *object*:
    let dir = WorkspaceDir::open_root(&ws).unwrap();
    assert!(
        dir.open(Path::new("link/secret")).is_err(),
        "the object resolver is not fooled — the opened object escapes ws"
    );
}

// ---- read_dir (step-52.3): object-bound directory listing ----

#[test]
#[serial]
fn read_dir_lists_contained_entries() {
    let ws = tempdir().unwrap();
    std::fs::write(ws.path().join("a.txt"), b"").unwrap();
    std::fs::write(ws.path().join("b.txt"), b"").unwrap();
    std::fs::create_dir(ws.path().join("sub")).unwrap();

    let dir = WorkspaceDir::open_root(ws.path()).unwrap();
    let mut names: Vec<String> = dir
        .read_dir(Path::new("."))
        .unwrap()
        .into_iter()
        .map(|n| n.to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec!["a.txt", "b.txt", "sub"],
        "`.`/`..` filtered, all entries listed"
    );
}

#[test]
#[serial]
fn read_dir_denies_a_symlink_escape_directory() {
    let root = tempdir().unwrap();
    let ws = root.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let outside = root.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("secret"), b"x").unwrap();
    symlink("../outside", ws.join("link")).unwrap();

    let dir = WorkspaceDir::open_root(&ws).unwrap();
    assert!(
        dir.read_dir(Path::new("link")).is_err(),
        "listing a symlink-escape directory must be denied"
    );
}

// ---- create_dir_all (step-52.4): object-bound `mkdir -p` ----

#[test]
#[serial]
fn create_dir_all_makes_nested_dirs_beneath() {
    let ws = tempdir().unwrap();
    let dir = WorkspaceDir::open_root(ws.path()).unwrap();
    dir.create_dir_all(Path::new("a/b/c")).unwrap();
    assert!(
        ws.path().join("a/b/c").is_dir(),
        "nested dirs created beneath the root"
    );
    // Idempotent: an existing prefix is fine.
    dir.create_dir_all(Path::new("a/b")).unwrap();
}

#[test]
#[serial]
fn create_dir_all_denies_a_symlink_escape_component() {
    let root = tempdir().unwrap();
    let ws = root.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let outside = root.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    symlink("../outside", ws.join("link")).unwrap();

    let dir = WorkspaceDir::open_root(&ws).unwrap();
    assert!(
        dir.create_dir_all(Path::new("link/evil")).is_err(),
        "mkdir -p must not create dirs outside the workspace via a symlink"
    );
    assert!(!outside.join("evil").exists(), "no dir created outside");
}

// ---- unlink (step-52.6): object-bound file removal ----

#[test]
#[serial]
fn unlink_removes_a_contained_file() {
    let ws = tempdir().unwrap();
    std::fs::write(ws.path().join("a.txt"), b"x").unwrap();
    let dir = WorkspaceDir::open_root(ws.path()).unwrap();
    dir.unlink(Path::new("a.txt")).unwrap();
    assert!(
        !ws.path().join("a.txt").exists(),
        "the contained file is removed"
    );
}

#[test]
#[serial]
fn unlink_denies_a_symlink_escape_parent() {
    let root = tempdir().unwrap();
    let ws = root.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let outside = root.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("victim"), b"x").unwrap();
    symlink("../outside", ws.join("link")).unwrap();

    let dir = WorkspaceDir::open_root(&ws).unwrap();
    assert!(
        dir.unlink(Path::new("link/victim")).is_err(),
        "must not unlink through an escaping symlink parent"
    );
    assert!(
        outside.join("victim").exists(),
        "the outside file must survive"
    );
}

/// macOS uses a conservative descriptor-relative walk: even an in-tree link
/// is refused, so neither intermediate nor final links can redirect an open.
#[cfg(target_os = "macos")]
#[test]
fn macos_rejects_symlinks_without_truncating_their_targets() {
    let ws = tempdir().unwrap();
    std::fs::write(ws.path().join("target"), "unchanged").unwrap();
    symlink("target", ws.path().join("link")).unwrap();
    let dir = WorkspaceDir::open_root(ws.path()).unwrap();
    assert!(dir.open(Path::new("link")).is_err());
    assert!(dir.create(Path::new("link")).is_err());
    assert_eq!(
        std::fs::read_to_string(ws.path().join("target")).unwrap(),
        "unchanged"
    );
}

/// Ground descriptor authority against a namespace replacement: operations
/// continue on the granted object, never on an attacker-planted replacement.
#[test]
fn held_directory_survives_rename_and_replacement() {
    let temp = tempdir().unwrap();
    let original = temp.path().join("original");
    let moved = temp.path().join("moved");
    std::fs::create_dir(&original).unwrap();
    std::fs::write(original.join("file"), "granted").unwrap();
    let dir = WorkspaceDir::open_root(&original).unwrap();
    std::fs::rename(&original, &moved).unwrap();
    std::fs::create_dir(&original).unwrap();
    std::fs::write(original.join("file"), "replacement").unwrap();
    let mut value = String::new();
    dir.open(Path::new("file"))
        .unwrap()
        .read_to_string(&mut value)
        .unwrap();
    assert_eq!(value, "granted");
    dir.create(Path::new("new"))
        .unwrap()
        .write_all(b"written")
        .unwrap();
    assert!(moved.join("new").exists());
    assert!(!original.join("new").exists());
    dir.unlink(Path::new("file")).unwrap();
    assert_eq!(
        std::fs::read_to_string(original.join("file")).unwrap(),
        "replacement"
    );
}
