use super::*;
use crate::git_staging::read_head_branch;
use crate::git_staging::tests::{held, scope, tempdir, tools_with};

/// `require_kernel_fence` refuses an advisory sandbox regardless of outcome.
/// This is the descendant-fence proof: a `SandboxKind::None` result means
/// no kernel fence was applied, and the copy is refused even on success.
#[test]
fn require_kernel_fence_refuses_advisory_sandbox() {
    assert!(require_kernel_fence(crate::sandbox_seam::SandboxKind::None).is_err());
}

#[test]
fn an_advisory_sandbox_is_refused_even_after_a_successful_run() {
    assert!(require_kernel_fence(crate::sandbox_seam::SandboxKind::None).is_err());
    // The expected kernel-fence kind is platform-specific: Landlock on Linux,
    // Seatbelt on macOS — the other kind is wrong and must also be refused.
    #[cfg(target_os = "linux")]
    assert_eq!(
        require_kernel_fence(crate::sandbox_seam::SandboxKind::Landlock),
        Ok(())
    );
    #[cfg(target_os = "macos")]
    assert_eq!(
        require_kernel_fence(crate::sandbox_seam::SandboxKind::Seatbelt),
        Ok(())
    );
    // Wrong sandbox kind on the current platform is also refused.
    #[cfg(target_os = "linux")]
    assert!(require_kernel_fence(crate::sandbox_seam::SandboxKind::Seatbelt).is_err());
    #[cfg(target_os = "macos")]
    assert!(require_kernel_fence(crate::sandbox_seam::SandboxKind::Landlock).is_err());
}

/// The canonical read root is REPLACED after binding: deleted and
/// recreated at the same pathname with an imposter `HEAD`. Pathname
/// re-acquisition would read the imposter (positive control); the held
/// handle names the unlinked original and fails closed, so the planning
/// read refuses. The copy path refuses too: the handle's identity no
/// longer matches what its path names, checked before any git runs.
///
/// Measured red (mutation): with `HeldRoots::open_read` re-acquiring the
/// anchor by pathname, the planning read returns `imposter`; with
/// `verify_identities` removed from `confined_fetch`, the copy proceeds
/// against the replacement.
#[cfg(unix)]
#[test]
fn replaced_canonical_root_refuses_the_planning_read_and_the_copy() {
    let dir = tempdir();
    let root = dir.path().join("repo");
    let git_dir = root.join(".git");
    std::fs::create_dir_all(&git_dir).unwrap();
    std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/task\n").unwrap();
    let held = held(&[root.to_str().unwrap()]);
    assert_eq!(read_head_branch(&git_dir, &held).unwrap(), "task");

    // Replace the root itself: same pathname, a different directory object.
    std::fs::remove_dir_all(&root).unwrap();
    std::fs::create_dir_all(&git_dir).unwrap();
    std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/imposter\n").unwrap();
    assert_eq!(
        std::fs::read_to_string(git_dir.join("HEAD"))
            .unwrap()
            .trim(),
        "ref: refs/heads/imposter",
        "positive control: the pathname now names the imposter"
    );

    let err = read_head_branch(&git_dir, &held).unwrap_err().to_string();
    assert!(
        !err.contains("imposter"),
        "the imposter must never be read: {err}"
    );
    assert!(err.contains("refused"), "{err}");

    let tools = tools_with(dir.path(), None, &["/some/other/root"]);
    let staging = StagingRepo::create(&dir.path().join("state"), tools.ctx()).unwrap();
    let err = confined_fetch(
        &tools,
        &staging,
        &git_dir,
        &"a".repeat(40),
        &Caveats::top(),
        &held,
    )
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("no longer names the directory bound at plan time"),
        "{err}"
    );
}

/// `held_read_roots` itself refuses a root swapped after `bind`, even with
/// no call to `verify_identities` in between: `HeldReadRoot::bind` (vendor
/// crate) performs its own `fstat`/`stat` identity check at construction,
/// so there is no path through this crate that can produce a held root
/// whose label and descriptor disagree about which object they name.
///
/// Measured red (#2674 P1): before `HeldReadRoot::bind` validated its
/// arguments, `held_read_roots()` alone silently built a held root pairing
/// the REPLACEMENT directory's pathname with the ORIGINAL directory's
/// descriptor — no error, no call to `verify_identities` required to
/// demonstrate it.
#[cfg(target_os = "linux")]
#[test]
fn held_read_roots_refuses_a_swapped_root_even_without_verify_identities() {
    let dir = tempdir();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let held = held(&[root.to_str().unwrap()]);

    // Swap the root for a new object at the same pathname — deliberately
    // skip verify_identities() to isolate held_read_roots()'s own check.
    std::fs::remove_dir_all(&root).unwrap();
    std::fs::create_dir_all(&root).unwrap();

    let err = held.held_read_roots().unwrap_err().to_string();
    assert!(err.contains("failed its identity check"), "{err}");
}

/// Release regression: file read grants bind without widening to siblings;
/// absent grants remain inert and replacement is refused before admission.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn held_roots_bind_regular_files_without_granting_the_parent() {
    let dir = tempdir();
    let root = dir.path().canonicalize().unwrap();
    let path = root.join("gitconfig");
    let absent = HeldRoots::bind(&scope(&[path.to_str().unwrap()])).unwrap();
    assert!(absent.canonical().is_empty());
    std::fs::write(&path, "[fixture]\n").unwrap();
    let held = HeldRoots::bind(&scope(&[path.to_str().unwrap()])).unwrap();
    assert_eq!(held.canonical(), std::slice::from_ref(&path));
    for _ in 0..2 {
        assert_eq!(
            held.read_to_string(&root, Path::new("gitconfig"))
                .unwrap()
                .as_deref(),
            Some("[fixture]\n")
        );
    }
    assert!(held.is_regular_file(&path));
    assert!(!held.is_regular_file(&root));
    std::fs::write(root.join("sibling"), "private").unwrap();
    assert!(held.read_to_string(&root, Path::new("sibling")).is_err());
    assert!(held.read_to_string(&path, Path::new("child")).is_err());
    held.verify_identities().unwrap();
    std::fs::rename(&path, root.join("old")).unwrap();
    std::fs::write(&path, "replacement").unwrap();
    assert!(held.verify_identities().is_err());
    #[cfg(target_os = "linux")]
    assert!(held.held_read_roots().is_err());
}
