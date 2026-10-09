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

/// The canonical prefilter rejects a physical escape before file capture.
/// Object binding still guards races after this check (#522).
#[cfg(unix)]
#[test]
fn tui_permits_path_rejects_a_physical_symlink_escape() {
    use crate::caveats::Scope;
    let outside = tempfile::TempDir::new().unwrap();
    std::fs::write(outside.path().join("secret"), b"x").unwrap();
    let ws = tempfile::TempDir::new().unwrap();
    // A symlink under the workspace whose target is OUTSIDE it.
    std::os::unix::fs::symlink(outside.path(), ws.path().join("link")).unwrap();

    let only = Scope::only([ws.path().to_string_lossy().into_owned()]);

    // What the read/write call sites feed the gate for model path "link/secret".
    let via_link = ws.path().join("link").join("secret");
    assert!(
        !tui_permits_path(&only, &via_link.to_string_lossy()),
        "canonical prefilter rejects a symlink outside the grant"
    );

    let dotdot = ws.path().join("..").join("etc").join("passwd");
    assert!(
        !tui_permits_path(&only, &dotdot.to_string_lossy()),
        "`..` escape is also denied"
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
        artifact_preimage_state(&crate::caveats::Scope::All, &path, true),
        crate::agentic::artifact_hooks::ArtifactFileState::from_bytes(&bytes)
    );
    assert!(artifact_file_matches(&crate::caveats::Scope::All, &path, &bytes).unwrap());
    let mut different = bytes.clone();
    different[64 * 1024] ^= 1;
    assert!(!artifact_file_matches(&crate::caveats::Scope::All, &path, &different).unwrap());
}

#[cfg(unix)]
#[test]
fn artifact_preimage_never_opens_non_regular_files() {
    let ws = tempfile::TempDir::new().unwrap();
    let socket = ws.path().join("local.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    assert_eq!(
        artifact_preimage_state(&crate::caveats::Scope::All, &socket, true),
        crate::agentic::artifact_hooks::ArtifactFileState::unavailable("preimage_not_regular_file")
    );
}

/// Ground artifact scope checks in an actual intermediate-directory symlink;
/// provenance must not hash a private file after a lexical authorization.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn artifact_capture_retains_the_file_scope() {
    let workspace = tempfile::tempdir().unwrap();
    let private = tempfile::tempdir().unwrap();
    std::fs::write(private.path().join("secret"), "private").unwrap();
    std::os::unix::fs::symlink(private.path(), workspace.path().join("link")).unwrap();
    let scope = crate::caveats::Scope::only([workspace.path().to_string_lossy().into_owned()]);
    let linked = workspace.path().join("link/secret");
    assert!(artifact_open_scoped_regular_file(&scope, &linked).is_err());
    assert!(artifact_file_matches(&scope, &linked, b"private").is_err());
    let inside = workspace.path().join("inside");
    std::fs::write(&inside, "visible").unwrap();
    assert!(artifact_file_matches(&scope, &inside, b"visible").unwrap());
}
