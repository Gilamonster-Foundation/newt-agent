use super::*;

#[test]
fn tui_permits_path_prefix_semantics() {
    use crate::caveats::Scope;
    assert!(tui_permits_path(&Scope::All, "/anything/at/all"));
    assert!(!tui_permits_path(&Scope::<String>::none(), "/ws/file"));
    let only = Scope::only(["/ws".to_string()]);
    assert!(tui_permits_path(&only, "/ws/sub/file.rs"));
    assert!(tui_permits_path(&only, "/ws"), "the workspace root itself");
    assert!(!tui_permits_path(&only, "/elsewhere/file.rs"));
    // `..` traversal must NOT escape: a path that lexically resolves outside
    // the workspace is denied even though it textually begins with it.
    assert!(
        !tui_permits_path(&only, "/ws/../etc/passwd"),
        "`..` traversal escapes the workspace"
    );
    assert!(
        !tui_permits_path(&only, "/ws/../../etc/passwd"),
        "repeated `..` traversal escapes the workspace"
    );
    // A sibling dir that merely shares the string prefix is not under /ws.
    assert!(
        !tui_permits_path(&only, "/ws-secret/file.rs"),
        "sibling-prefix collision escapes the workspace"
    );
    // A `..` that stays inside the workspace is still permitted.
    assert!(tui_permits_path(&only, "/ws/sub/../file.rs"));
}

/// Ratchet for the OPEN `fs-canonical-containment` deviation (issue #522,
/// `docs/security/ocap-deviations.md`). `tui_permits_path` is string-lexical:
/// it collapses `..` but does NOT resolve symlinks, so a link *inside* the
/// workspace pointing OUT is permitted even though the OS would read the
/// outside target. This test builds the path the call sites do
/// (`workspace.join(model_path)`) over a REAL symlink and PINS that residual.
///
/// When canonicalize-then-contain lands (the deviation's closure criterion),
/// the gate will deny the symlinked path and this assertion MUST flip to
/// `!tui_permits_path(...)` — that break is the signal to close the deviation.
/// Unix-only: Windows symlinks need privileges (mirrors
/// `find_does_not_follow_symlinks_out_of_workspace`).
#[cfg(unix)]
#[test]
fn tui_permits_path_is_a_lexical_prefilter_not_the_fence() {
    use crate::caveats::Scope;
    let outside = tempfile::TempDir::new().unwrap();
    std::fs::write(outside.path().join("secret"), b"x").unwrap();
    let ws = tempfile::TempDir::new().unwrap();
    // A symlink under the workspace whose target is OUTSIDE it.
    std::os::unix::fs::symlink(outside.path(), ws.path().join("link")).unwrap();

    let only = Scope::only([ws.path().to_string_lossy().into_owned()]);

    // What the read/write call sites feed the gate for model path "link/secret".
    let via_link = ws.path().join("link").join("secret");
    // `tui_permits_path` is a cheap LEXICAL PRE-FILTER — it still admits the
    // symlinked name (it cannot see through the link, and is not meant to).
    // The authoritative fence is now object-bound: the fs tool arms resolve
    // through `WorkspaceDir` (openat2 RESOLVE_BENEATH), so the *arm* denies
    // this escape even though the predicate admits the name — proven by
    // `{read_file,list_dir,write_file,edit_file,delete_file}_symlink_under_
    // workspace_escaping_is_denied` and `apply_whole_files_denies_symlink_
    // escape_object_bound`. This test therefore pins that the predicate stays
    // a prefilter (NOT that a residual is open — #522 is CLOSED, step-52.7).
    assert!(
        tui_permits_path(&only, &via_link.to_string_lossy()),
        "the lexical prefilter admits the name; object-binding is the fence"
    );

    // Contrast: a plain `..` escape through the SAME root is already denied
    // (lexical containment, the part #502 did fix) — so this isn't a blanket
    // hole, only the symlink-resolution gap.
    let dotdot = ws.path().join("..").join("etc").join("passwd");
    assert!(
        !tui_permits_path(&only, &dotdot.to_string_lossy()),
        "`..` escape is denied even though symlink escape is not"
    );
}

/// The file tools retain the lexical OCAP residual above, but their
/// provenance hook must fail closed so it never labels an outside target as
/// a workspace artifact.
#[cfg(unix)]
#[test]
fn artifact_provenance_rejects_physical_symlink_escapes() {
    let outside = tempfile::TempDir::new().unwrap();
    std::fs::write(outside.path().join("existing"), b"x").unwrap();
    let ws = tempfile::TempDir::new().unwrap();
    std::os::unix::fs::symlink(outside.path(), ws.path().join("link")).unwrap();

    assert!(artifact_path_is_physically_within_workspace(
        ws.path(),
        &ws.path().join("new/leaf.txt")
    ));
    assert!(!artifact_path_is_physically_within_workspace(
        ws.path(),
        &ws.path().join("link/existing")
    ));
    assert!(!artifact_path_is_physically_within_workspace(
        ws.path(),
        &ws.path().join("link/new-file")
    ));

    std::os::unix::fs::symlink(outside.path().join("missing"), ws.path().join("dangling")).unwrap();
    assert!(!artifact_path_is_physically_within_workspace(
        ws.path(),
        &ws.path().join("dangling")
    ));
}

#[test]
fn artifact_file_streaming_hash_and_postcondition_are_exact() {
    let ws = tempfile::TempDir::new().unwrap();
    let bytes = vec![0x5a; 3 * 64 * 1024 + 17];
    let path = ws.path().join("large.bin");
    std::fs::write(&path, &bytes).unwrap();

    assert_eq!(
        artifact_preimage_state(&path, true),
        crate::agentic::artifact_hooks::ArtifactFileState::from_bytes(&bytes)
    );
    assert!(artifact_file_matches(&path, &bytes).unwrap());
    let mut different = bytes.clone();
    different[64 * 1024] ^= 1;
    assert!(!artifact_file_matches(&path, &different).unwrap());
}

#[cfg(unix)]
#[test]
fn artifact_preimage_never_opens_non_regular_files() {
    let ws = tempfile::TempDir::new().unwrap();
    let socket = ws.path().join("local.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    assert_eq!(
        artifact_preimage_state(&socket, true),
        crate::agentic::artifact_hooks::ArtifactFileState::unavailable("preimage_not_regular_file")
    );
}
